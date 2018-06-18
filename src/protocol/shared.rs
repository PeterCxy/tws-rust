/*
 * Shared implementation details between server and client
 */
use bytes::BytesMut;
use errors::*;
use futures::{Future, Stream};
use futures::stream::SplitSink;
use protocol::protocol as proto;
use protocol::util::{self, Boxable, BoxFuture, HeartbeatAgent, SharedWriter, StreamThrottler, ThrottlingHandler};
use websocket::OwnedMessage;
use websocket::stream::async::Stream as WsStream;
use websocket::codec::ws::MessageCodec;
use std::cell::RefCell;
use std::collections::HashMap;
use std::marker::PhantomData;
use std::net::SocketAddr;
use std::rc::Rc;
use tokio_codec::{BytesCodec, Decoder, Framed};
use tokio::net::TcpStream;
use tokio::executor::current_thread;

pub type Client<S> = Framed<S, MessageCodec<OwnedMessage>>;

pub trait TwsServiceState<C: TwsConnection>: 'static + Sized {
    fn get_connections(&mut self) -> &mut HashMap<String, C>;

    /*
     * The `paused` state of a TwsService session
     * indicates whether the underlying WebSocket
     * connection has been congested or not.
     */
    fn set_paused(&mut self, paused: bool);
    fn get_paused(&self) -> bool;
}

/*
 * Throttle the TCP connection between
 *  1. server and remote
 *  2. local and client
 * based on the state of the WebSocket stream.
 * If the stream is congested, mark the corresponding
 * TCP connections as paused.
 */
pub struct TwsTcpReadThrottler<C: TwsConnection, T: TwsServiceState<C>> {
    _marker: PhantomData<C>,
    state: Rc<RefCell<T>>
}

impl<C, T> ThrottlingHandler for TwsTcpReadThrottler<C, T>
    where C: TwsConnection,
          T: TwsServiceState<C>
{
    fn pause(&mut self) {
        let mut state = self.state.borrow_mut();
        state.set_paused(true);
        for (_, v) in state.get_connections() {
            v.pause();
        }
    }

    fn resume(&mut self) {
        let mut state = self.state.borrow_mut();
        state.set_paused(false);
        for (_, v) in state.get_connections() {
            v.resume();
        }
    }

    fn is_paused(&self) -> bool {
        self.state.borrow().get_paused()
    }
}

/*
 * Shared logic abstracted from both the server and the client
 * should be implemented only on structs
 */
pub trait TwsService<C: TwsConnection, T: TwsServiceState<C>, S: 'static + WsStream>: 'static + Sized {
    /*
     * Required fields simulated by required methods.
     */
    fn get_passwd(&self) -> &str;
    fn get_writer(&self) -> &SharedWriter<SplitSink<Client<S>>>;
    fn get_heartbeat_agent(&self) -> &HeartbeatAgent<SplitSink<Client<S>>>;
    fn get_logger(&self) -> &util::Logger;
    fn get_state(&self) -> &Rc<RefCell<T>>;

    /*
     * Execute this service.
     * More precisely, execute the WebSocket-related services.
     * Receive from WebSocket and parse the TWS protocol packets.
     * This will consume the ownership of self. Please spawn
     * the returned future on an event loop.
     */
    fn run_service<'a>(self, client: Client<S>) -> BoxFuture<'a, ()> {
        let logger = self.get_logger().clone();
        let (sink, stream) = client.split();

        self.get_writer().set_throttling_handler(TwsTcpReadThrottler {
            _marker: PhantomData,
            state: self.get_state().clone()
        });

        // Obtain a future representing the writing tasks.
        let sink_write = self.get_writer().run(sink).map_err(clone!(logger; |e| {
            do_log!(logger, ERROR, "{:?}", e);
        }));

        // Obtain a future to do heartbeats.
        let heartbeat_work = self.get_heartbeat_agent().run().map_err(clone!(logger; |_| {
            do_log!(logger, ERROR, "Session timed out.");
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
            proto::Packet::ConnectionState((conn_id, state)) => self.on_connect_state(conn_id, state),
            proto::Packet::Data((conn_id, data)) => self.on_data(conn_id, data),

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
    fn on_connect_state(&self, _conn_id: &str, _state: proto::ConnectionState) {
        // If this method is overridden, implementations must call self._on_connect_state()
        self._on_connect_state(_conn_id, &_state);
    }
    fn on_data(&self, _conn_id: &str, _data: &[u8]) {}
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
    fn pause(&mut self) {
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
    ) -> (SharedWriter<TcpSink>, StreamThrottler) {
        let (a, b) = Self::get_endpoint_descriptors();
        let conn_handler = Rc::new(conn_handler);

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
        let stream_work = read_throttler.wrap_stream(util::AlternatingStream::new(stream)).for_each(clone!(conn_handler; |p| {
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

        (remote_writer, read_throttler)
    }

    fn get_writer(&self) -> &SharedWriter<TcpSink>;
    fn get_conn_id(&self) -> &str;
    fn get_logger(&self) -> &util::Logger;
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
            self.get_read_throttler().pause();
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