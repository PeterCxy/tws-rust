/*
 * Shared implementation details between server and client
 */
use bytes::BytesMut;
use errors::*;
use futures::{Future, Stream};
use futures::stream::SplitSink;
use protocol::protocol as proto;
use protocol::udp::{SharedUdpHandle, UdpStream};
use protocol::util::{self, Boxable, BoxFuture, HeartbeatAgent, SharedWriter,
    SharedSpeedometer, Speedometer, StreamThrottler, ThrottlingHandler};
use websocket::OwnedMessage;
use websocket::async::Client;
use websocket::stream::async::Stream as WsStream;
use std::cell::RefCell;
use std::collections::HashMap;
use std::marker::PhantomData;
use std::net::SocketAddr;
use std::rc::Rc;
use std::time::{Instant, Duration};
use tokio_codec::{BytesCodec, Decoder, Framed};
use tokio::net::TcpStream;
use tokio::executor::current_thread;
use tokio_timer;

pub trait TwsServiceState<C: TwsConnection, U: TwsUdpConnection>: 'static + Sized {
    fn get_connections(&mut self) -> &mut HashMap<String, C>;
    fn get_udp_connections(&mut self) -> &mut HashMap<String, U>;

    /*
     * The `paused` state of a TwsService session
     * indicates whether the underlying WebSocket
     * connection has been congested or not.
     */
    fn set_paused(&mut self, paused: bool);
    fn get_paused(&self) -> bool;
}

macro_rules! make_tws_service_state {
    (
        $name:ident;
        $conn_type:ty, $udp_conn_type:ty;
        $conn_field_name:ident, $udp_conn_field_name:ident;
        { $($field:ident: $type:ty),*}
    ) => (
        struct $name {
            paused: bool,
            $conn_field_name: HashMap<String, $conn_type>,
            $udp_conn_field_name: HashMap<String, $udp_conn_type>,
            $( $field: $type ),*
        }

        impl TwsServiceState<$conn_type, $udp_conn_type> for $name {
            #[inline(always)]
            fn get_connections(&mut self) -> &mut HashMap<String, $conn_type> {
                &mut self.$conn_field_name
            }

            #[inline(always)]
            fn get_udp_connections(&mut self) -> &mut HashMap<String, $udp_conn_type> {
                &mut self.$udp_conn_field_name
            }

            #[inline(always)]
            fn get_paused(&self) -> bool {
                self.paused
            }

            #[inline(always)]
            fn set_paused(&mut self, paused: bool) {
                self.paused = paused;
            }
        }
    )
}

/*
 * Throttle the TCP connection between
 *  1. server and remote
 *  2. local and client
 * based on the state of the WebSocket stream.
 * If the stream is congested, mark the corresponding
 * TCP connections as paused.
 */
pub struct TwsTcpReadThrottler<C: TwsConnection, U: TwsUdpConnection, T: TwsServiceState<C, U>> {
    _marker: PhantomData<C>,
    _marker_2: PhantomData<U>,
    state: Rc<RefCell<T>>,
    pause_state: HashMap<String, bool>
}

impl<C, U, T> ThrottlingHandler for TwsTcpReadThrottler<C, U, T>
    where C: TwsConnection,
          U: TwsUdpConnection,
          T: TwsServiceState<C, U>
{
    fn pause(&mut self, max_speed: u64) {
        let mut state = self.state.borrow_mut();
        state.set_paused(true);

        // Pause all UDP connections before pausing TCP ones
        // Don't need anything special for UDP ones,
        // pausing just causes all packet to be lost
        for (_, v) in state.get_udp_connections() {
            v.get_handle().borrow_mut().pause();
        }

        // The stream is now at its full speed.
        // Try to pause the fastest substreams first
        let mut it = state.get_connections().iter_mut()
            .map(|(c, v)| {
                let speed = v.get_speedometer().borrow().speed();
                (c, v, speed)
            })
            .collect::<Vec<_>>();
        it.sort_by(|(_, _, s1), (_, _, s2)| s1.cmp(s2));
        let mut sum = 0;
        for (_, _, s) in &it {
            sum += *s;
        }
        for (c, v, s) in it {
            if *self.pause_state.get(c).unwrap_or(&false) {
                v.pause();
                self.pause_state.insert(c.clone(), true);
                sum -= s;

                if sum < max_speed {
                    break;
                }
            }
        }
    }

    fn resume(&mut self) {
        let mut state = self.state.borrow_mut();
        state.set_paused(false);

        // Resume all UDP connections before TCP ones
        for (_, v) in state.get_udp_connections() {
            v.get_handle().borrow_mut().resume();
        }

        // Resume only the paused streams
        for (c, s) in &self.pause_state {
            if *s {
                state.get_connections().get_mut(c).map_or((), |s| s.resume());
            }
        }
        self.pause_state.clear();
    }

    fn is_paused(&self) -> bool {
        self.state.borrow().get_paused()
    }

    #[inline(always)]
    fn allow_pause_multiple_times(&self) -> bool {
        true
    }
}

/*
 * Shared logic abstracted from both the server and the client
 * should be implemented only on structs
 */
pub trait TwsService<C: TwsConnection, U: TwsUdpConnection, T: TwsServiceState<C, U>, S: 'static + WsStream>: 'static + Sized {
    /*
     * Required fields simulated by required methods.
     */
    fn get_passwd(&self) -> &str;
    fn get_writer(&self) -> &SharedWriter<SplitSink<Client<S>>>;
    fn get_heartbeat_agent(&self) -> &HeartbeatAgent<SplitSink<Client<S>>>;
    fn get_logger(&self) -> &util::Logger;
    fn get_state(&self) -> &Rc<RefCell<T>>;
    fn get_udp_timeout(&self) -> u64;

    /*
     * Execute this service.
     * More precisely, execute the WebSocket-related services.
     * Receive from WebSocket and parse the TWS protocol packets.
     * This will consume the ownership of self. Please spawn
     * the returned future on an event loop.
     */
    fn run_service<'a>(self, client: Client<S>) -> BoxFuture<'a, ()> {
        let logger = self.get_logger().clone();
        let state = self.get_state().clone();
        let (sink, stream) = client.split();

        self.get_writer().set_throttling_handler(TwsTcpReadThrottler {
            _marker: PhantomData,
            _marker_2: PhantomData,
            state: self.get_state().clone(),
            pause_state: HashMap::new()
        });

        // Obtain a future representing the writing tasks.
        let sink_write = self.get_writer().run(sink).map_err(clone!(logger; |e| {
            do_log!(logger, ERROR, "{:?}", e);
        }));

        // Obtain a future to do heartbeats.
        let heartbeat_work = self.get_heartbeat_agent().run().map_err(clone!(logger; |_| {
            do_log!(logger, ERROR, "Session timed out.");
        }));

        // UDP cleanup work
        let udp_cleanup_work = tokio_timer::Interval::new(Instant::now(), Duration::from_millis(self.get_udp_timeout()))
            .for_each(clone!(state; |_| {
                for (_, conn) in state.borrow_mut().get_udp_connections() {
                    conn.get_handle().borrow().notify();
                }
                Ok(())
            }));

        // The main task
        // 3 combined streams. Will finish once one of them finish.
        // i.e. when the connection closes, everything here should finish.
        util::AlternatingStream::new(stream)
            .map_err(clone!(logger; |e| {
                do_log!(logger, ERROR, "{:?}", e);
                "session failed.".into()
            }))
            .for_each(move |msg| {
                // Process each message from the client
                // Note that this does not return a future.
                // Anything that happens for processing
                // the message and requires doing things
                // on the event loop should spawn a task
                // instead of returning it.
                // In order not to block the main WebSocket
                // stream.
                self.on_message(msg);
                Ok(()) as Result<()>
            })
            .select2(sink_write) // Execute task for the writer
            .select2(heartbeat_work) // Execute task for heartbeats
            .select2(udp_cleanup_work) // Execute task for cleaning up UDP connections
            .then(clone!(logger; |_| {
                do_log!(logger, INFO, "Session finished.");

                // Clean-up job
                // Drop all the connections
                // will be closed by the implementation of Drop
                //state.borrow_mut().remote_connections.clear();

                Ok(())
            }))
            ._box()
    }

    /*
     * Process new WebSocket packets
     */
    fn on_message(&self, msg: OwnedMessage) {
        match msg {
            // Control / Data packets can be in either Text or Binary form.
            OwnedMessage::Text(text) => self.on_packet(proto::parse_packet(&self.get_passwd(), text.as_bytes())),
            OwnedMessage::Binary(bytes) => self.on_packet(proto::parse_packet(&self.get_passwd(), &bytes)),

            // Send pong back to keep connection alive
            OwnedMessage::Ping(msg) => self.get_writer().feed(OwnedMessage::Pong(msg)),

            // Notify the heartbeat agent that a pong is received.
            OwnedMessage::Pong(_) => self.get_heartbeat_agent().set_heartbeat_received(),
            OwnedMessage::Close(_) => self.get_writer().close()
        };
    }

    /*
     * Process TWS protocol packets
     */
    fn on_packet(&self, packet: proto::Packet) {
        //do_log!(self.get_logger(), DEBUG, "{:?}", packet);
        match packet {
            // Call corresponding event methods.
            // Implementations can override these to control event handling.
            proto::Packet::Handshake(addr) => self.on_handshake(addr),
            proto::Packet::Connect(conn_id) => self.on_connect(conn_id),
            proto::Packet::UdpConnect(conn_id) => self.on_udp_connect(conn_id),
            proto::Packet::ConnectionState((conn_id, state)) => self.on_connect_state(conn_id, state),
            proto::Packet::Data((conn_id, data)) => self.on_data(conn_id, data),
            proto::Packet::UdpData((conn_id, data)) => self.on_udp_data(conn_id, data),

            // Process unknown packets
            _ => self.on_unknown()
        }
    }

    /*
     * Process `Pause` and `Resume` states
     * which is identical between the client and the server
     * 
     * Pause or resume the `read` part of the corresponding connection
     * on request.
     */
    fn _on_connect_state(&self, conn_id: &str, conn_state: &proto::ConnectionState) {
        if conn_state.is_pause() {
            self.get_state().borrow_mut().get_connections().get_mut(conn_id)
                .map_or((), |c| c.pause());
        } else if conn_state.is_resume() {
            self.get_state().borrow_mut().get_connections().get_mut(conn_id)
                .map_or((), |c| c.resume());
        }
    }

    /*
     * Overridable events
     */
    fn on_unknown(&self) {}
    fn on_handshake(&self, _addr: SocketAddr) {}
    fn on_connect(&self, _conn_id: &str) {}
    fn on_udp_connect(&self, _conn_id: &str) {}
    fn on_connect_state(&self, _conn_id: &str, _state: proto::ConnectionState) {
        // If this method is overridden, implementations must call self._on_connect_state()
        self._on_connect_state(_conn_id, &_state);
    }
    fn on_data(&self, _conn_id: &str, _data: &[u8]) {}
    fn on_udp_data(&self, _conn_id: &str, _data: &[u8]) {}
}

macro_rules! make_tws_service {
    (
        $name:ident;
        $conn_type:ty, $udp_conn_type:ty, $state_type:ty, $stream_type:ty;
        { $($field:ident: $type:ty),*};
        $(override fn $fn_name:ident (&$s:ident $(, $param:ident: $ptype:ty)*) -> $ret_type:ty $block:block)*
    ) => {
        struct $name {
            logger: util::Logger,
            writer: SharedWriter<SplitSink<Client<$stream_type>>>,
            heartbeat_agent: HeartbeatAgent<SplitSink<Client<$stream_type>>>,
            state: Rc<RefCell<$state_type>>,
            $( $field: $type ),*
        }

        impl TwsService<$conn_type, $udp_conn_type, $state_type, $stream_type> for $name {
            #[inline(always)]
            fn get_writer(&self) -> &SharedWriter<SplitSink<Client<$stream_type>>> {
                &self.writer
            }

            #[inline(always)]
            fn get_heartbeat_agent(&self) -> &HeartbeatAgent<SplitSink<Client<$stream_type>>> {
                &self.heartbeat_agent
            }

            #[inline(always)]
            fn get_logger(&self) -> &util::Logger {
                &self.logger
            }

            #[inline(always)]
            fn get_state(&self) -> &Rc<RefCell<$state_type>> {
                &self.state
            }

            $(
                fn $fn_name(&$s, $($param: $ptype),*) -> $ret_type { $block }
            )*
        }
    };
}

// Splitted Sink for Tcp byte streams
pub type TcpSink = SplitSink<Framed<TcpStream, BytesCodec>>;

/*
 * Handler of throttling events from the writing part of
 * the TCP connection
 *  1. from server to remote
 *  2. from local to client
 * and convert them into TWS Connection State messages
 * to instruct the other side to block or resume
 * the reading part.
 */
pub struct TwsTcpWriteThrottlingHandler<S: 'static + WsStream> {
    conn_id: String,
    ws_writer: SharedWriter<SplitSink<Client<S>>>,
    paused: bool
}

impl<S: 'static + WsStream> ThrottlingHandler for TwsTcpWriteThrottlingHandler<S> {
    fn pause(&mut self, _max_speed: u64) {
        self.paused = true;
        self.ws_writer.feed(OwnedMessage::Text(proto::connect_state_build(&self.conn_id, proto::ConnectionState::Pause)));
    }

    fn resume(&mut self) {
        self.paused = false;
        self.ws_writer.feed(OwnedMessage::Text(proto::connect_state_build(&self.conn_id, proto::ConnectionState::Resume)));
    }

    fn is_paused(&self) -> bool {
        self.paused
    }
}

/*
 * Handler of data and close events emitted by
 * TCP connections transmitted on TWS
 * responsible for forwarding these events to remote
 */
pub trait TwsConnectionHandler: 'static + Sized {
    fn on_data(&self, _d: BytesMut);
    fn on_close(&self);
}

/*
 * Shared logic for TCP connection
 *  1. from server to remote
 *  2. from client to local (which is TWS client)
 */
pub trait TwsConnection: 'static + Sized {
    #[inline(always)]
    fn get_endpoint_descriptors() -> (&'static str, &'static str) {
        ("remote", "client")
    }
    
    /*
     * Static method to bootstrap the connection
     * set up the reading and writing part of the connection
     */
    fn create<S: 'static + WsStream, H: TwsConnectionHandler>(
        conn_id: String, logger: util::Logger,
        client: TcpStream, ws_writer: SharedWriter<SplitSink<Client<S>>>,
        conn_handler: H
    ) -> (SharedSpeedometer, SharedWriter<TcpSink>, StreamThrottler) {
        let (a, b) = Self::get_endpoint_descriptors();
        let conn_handler = Rc::new(conn_handler);
        let speedometer = Rc::new(RefCell::new(Speedometer::new()));

        let read_throttler = StreamThrottler::new();
        let (sink, stream) = BytesCodec::new().framed(client).split();
        // SharedWriter for sending to remote
        let remote_writer = SharedWriter::new();
        remote_writer.set_throttling_handler(TwsTcpWriteThrottlingHandler {
            conn_id: conn_id.clone(),
            ws_writer,
            paused: false
        });

        // Forward remote packets to client
        let stream_work = read_throttler.wrap_stream(util::AlternatingStream::new(stream)).for_each(clone!(conn_handler, speedometer; |p| {
            // Calculate the speed of the TCP stream (read part)
            speedometer.borrow_mut().feed_counter(p.len() as u64);
            // Forward
            conn_handler.on_data(p);
            Ok(())
        })).map_err(clone!(a, b, logger, conn_id; |e| {
            do_log!(logger, ERROR, "[{}] {} => {} error {:?}", conn_id, a, b, e);
        })).map(|_| ());

        // Forward client packets to remote
        // Client packets should be sent through `send` method.
        let sink_work = remote_writer.run(sink)
            .map_err(clone!(a, b, logger, conn_id; |e| {
                do_log!(logger, ERROR, "[{}] {} => {} error {:?}", conn_id, b, a, e);
            }));

        // Schedule the two jobs on the event loop
        // Use `select` to wait one of the jobs to finish.
        // This is often the `sink_work` if no error on remote side
        // has happened.
        // Once one of them is finished, just tear down the whole
        // channel.
        current_thread::spawn(stream_work.select(sink_work)
            .then(clone!(logger, conn_id, conn_handler; |_| {
                // Clean-up job upon finishing
                // No matter if there is any error.
                do_log!(logger, INFO, "[{}] Channel closing.", conn_id);
                conn_handler.on_close();
                Ok(())
            })));

        (speedometer, remote_writer, read_throttler)
    }

    fn get_writer(&self) -> &SharedWriter<TcpSink>;
    fn get_conn_id(&self) -> &str;
    fn get_logger(&self) -> &util::Logger;
    fn get_speedometer(&self) -> &SharedSpeedometer;
    fn get_read_throttler(&mut self) -> &mut StreamThrottler;
    fn get_read_pause_counter(&self) -> usize;
    fn set_read_pause_counter(&mut self, counter: usize);

    fn close(&self) {
        self.get_writer().close();
    }

    /*
     * Pause the reading part if it is not paused yet
     */
    fn pause(&mut self) {
        let counter = self.get_read_pause_counter();
        if counter == 0 {
            self.get_read_throttler().pause(0);
        }
        self.set_read_pause_counter(counter + 1);
    }

    /*
     * Resume the reading part if no one requires it
     * to be paused
     */
    fn resume(&mut self) {
        let counter = self.get_read_pause_counter();
        if counter == 1 {
            self.get_read_throttler().resume();
        }

        if counter > 0 {
            self.set_read_pause_counter(counter - 1);
        }
    }
}

macro_rules! make_tws_connection {
    (
        $name:ident; $writer_name:ident;
        ($endpoint_1:expr, $endpoint_2:expr)
    ) => (
        struct $name {
            conn_id: String,
            logger: util::Logger,
            $writer_name: SharedWriter<TcpSink>,
            read_throttler: StreamThrottler,
            speedometer: SharedSpeedometer,
            read_pause_counter: usize
        }

        impl TwsConnection for $name {
            #[inline(always)]
            fn get_endpoint_descriptors() -> (&'static str, &'static str) {
                ($endpoint_1, $endpoint_2)
            }

            #[inline(always)]
            fn get_logger(&self) -> &util::Logger {
                &self.logger
            }

            #[inline(always)]
            fn get_conn_id(&self) -> &str {
                &self.conn_id
            }

            #[inline(always)]
            fn get_writer(&self) -> &SharedWriter<TcpSink> {
                &self.$writer_name
            }

            #[inline(always)]
            fn get_read_throttler(&mut self) -> &mut StreamThrottler {
                &mut self.read_throttler
            }

            #[inline(always)]
            fn get_read_pause_counter(&self) -> usize {
                self.read_pause_counter
            }

            #[inline(always)]
            fn get_speedometer(&self) -> &SharedSpeedometer {
                &self.speedometer
            }

            #[inline(always)]
            fn set_read_pause_counter(&mut self, counter: usize) {
                self.read_pause_counter = counter;
            }
        }

        impl Drop for $name {
            fn drop(&mut self) {
                /*
                 * Close immediately on drop.
                */
                self.close();
            }
        }
    )
}

pub trait TwsUdpConnectionHandler: 'static + Sized {
    fn on_data(&self, _d: Vec<u8>);
    fn on_close(&self);
}

pub trait TwsUdpConnection: 'static + Sized {
    fn create<H: TwsUdpConnectionHandler>(
        conn_id: String, logger: util::Logger,
        client: UdpStream, handler: H
    ) {
        let handler = Rc::new(handler);
        let stream_work = util::AlternatingStream::new(client).for_each(clone!(handler; |data| {
            handler.on_data(data);
            Ok(())
        }));
        
        current_thread::spawn(stream_work.then(clone!(conn_id, logger, handler; |_| {
            do_log!(logger, INFO, "[{}] UDP connection idle. Closing.", conn_id);
            handler.on_close();
            Ok(())
        })))
    }

    fn get_handle(&self) -> &SharedUdpHandle;

    fn pause(&self) {
        self.get_handle().borrow_mut().pause();
    }

    fn resume(&self) {
        self.get_handle().borrow_mut().resume();
    }
}

macro_rules! make_tws_udp_connection {
    (
        $name:ident;
        { $($field:ident: $type:ty),*}
    ) => (
        #[allow(dead_code)]
        struct $name {
            conn_id: String,
            handle: SharedUdpHandle,
            logger: util::Logger,
            $( $field: $type ),*
        }

        impl TwsUdpConnection for $name {
            #[inline(always)]
            fn get_handle(&self) -> &SharedUdpHandle {
                &self.handle
            }
        }
    )
}