/*
 * Server-side concrete implementation of TWS
 */
use bytes::{Bytes, BytesMut};
use futures::{Stream};
use futures::future::{Future, IntoFuture};
use futures::stream::{SplitSink};
use protocol::protocol as proto;
use protocol::util::{self, BoxFuture, Boxable, RcEventEmitter, EventSource, FutureChainErr, HeartbeatAgent};
use std::cell::RefCell;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::rc::Rc;
use tokio_core::net::TcpStream;
use tokio_core::reactor::Handle;
use tokio_io::AsyncRead;
use tokio_io::codec::BytesCodec;
use websocket::OwnedMessage;
use websocket::async::{Server, Client, MessageCodec};
use websocket::client::async::Framed;

#[derive(Clone)]
pub struct TwsServerOption {
    pub listen: SocketAddr,
    pub passwd: String,
    pub timeout: u64
}

pub struct TwsServer {
    option: TwsServerOption,
    handle: Handle,
    logger: util::Logger
}

impl TwsServer {
    pub fn new(handle: Handle, option: TwsServerOption) -> TwsServer {
        TwsServer {
            option,
            handle,
            logger: Rc::new(util::default_logger)
        }
    }

    pub fn on_log<F: 'static + Fn(util::LogLevel, &str)>(&mut self, logger: F) {
        self.logger = Rc::new(logger);
    }

    pub fn run<'a>(self) -> BoxFuture<'a, ()> {
        let option = self.option.clone();
        let logger = self.logger.clone();
        let handle = self.handle.clone();
        Server::bind(self.option.listen, &self.handle)
            .into_future()
            .chain_err(|| "Failed to bind to server")
            .and_then(move |server| {
                server.incoming()
                    .map_err(|_| "Failed to accept connections".into())
                    .for_each(move |(upgrade, addr)| {
                        let work = upgrade.accept()
                            .chain_err(|| "Failed to accept connection.")
                            .and_then(clone!(option, logger, handle; |(client, _)| {
                                ServerSession::new(option, logger, handle)
                                    .run(client, addr)
                            }))
                            .map_err(|_| ());
                        self.handle.spawn(work);
                        Ok(())
                    })
            })
            ._box()
    }
}

type ClientSink = SplitSink<Framed<TcpStream, MessageCodec<OwnedMessage>>>;
type RemoteSink = SplitSink<Framed<TcpStream, BytesCodec>>;

struct ServerSessionState {
    remote: Option<SocketAddr>,
    client: Option<SocketAddr>,
    handshaked: bool,
    remote_connections: HashMap<String, RemoteConnection>
}

struct ServerSession {
    option: TwsServerOption,
    logger: util::Logger,
    handle: Handle,
    writer: Rc<util::BufferedWriter<ClientSink>>,
    heartbeat_agent: HeartbeatAgent<ClientSink>,
    state: Rc<RefCell<ServerSessionState>>
}

impl ServerSession {
    pub fn new(option: TwsServerOption, logger: util::Logger, handle: Handle) -> ServerSession {
        let writer = Rc::new(util::BufferedWriter::new());
        ServerSession {
            heartbeat_agent: HeartbeatAgent::new(option.timeout, writer.clone()),
            option,
            logger,
            handle,
            writer,
            state: Rc::new(RefCell::new(ServerSessionState {
                remote: None,
                client: None,
                handshaked: false,
                remote_connections: HashMap::new()
            }))
        }
    }

    pub fn run<'a>(self, client: Client<TcpStream>, addr: SocketAddr) -> BoxFuture<'a, ()> {
        let state = self.state.clone();
        state.borrow_mut().client = Some(addr);
        let logger = self.logger.clone();
        let (sink, stream) = client.split();
        let sink_write = self.writer.run(sink).map_err(clone!(logger; |e| {
            do_log!(logger, ERROR, "{:?}", e);
        }));
        let heartbeat_work = self.heartbeat_agent.run().map_err(clone!(logger; |_| {
            do_log!(logger, ERROR, "Session timed out.");
        }));
        
        stream
            .map_err(clone!(logger; |e| {
                do_log!(logger, ERROR, "{:?}", e);
                "session failed.".into()
            }))
            .for_each(move |msg| {
                self.on_message(msg)
            })
            .select2(sink_write)
            .select2(heartbeat_work)
            .then(clone!(state, logger; |_| {
                do_log!(logger, INFO, "Session finished.");

                // Clean-up job
                // Drop all the connections
                state.borrow_mut().remote_connections.clear();

                Ok(())
            }))
            ._box()
    }

    fn on_message<'a>(&self, msg: OwnedMessage) -> BoxFuture<'a, ()> {
        match msg {
            OwnedMessage::Text(text) => self.on_packet(proto::parse_packet(&self.option.passwd, text.as_bytes())),
            OwnedMessage::Binary(bytes) => self.on_packet(proto::parse_packet(&self.option.passwd, &bytes)),
            OwnedMessage::Ping(msg) => {
                self.writer.feed(OwnedMessage::Pong(msg));
                empty_future!()
            },
            OwnedMessage::Pong(_) => {
                self.heartbeat_agent.set_heartbeat_received();
                empty_future!()
            }
            _ => empty_future!()
        }
    }

    fn on_packet<'a>(&self, packet: proto::Packet) -> BoxFuture<'a, ()> {
        do_log!(self.logger, DEBUG, "{:?}", packet);
        match packet {
            proto::Packet::Handshake(addr) => self.on_handshake(addr),
            proto::Packet::Connect(conn_id) => self.on_connect(conn_id),
            proto::Packet::ConnectionState((conn_id, ok)) => self.on_connect_state(conn_id, ok),
            proto::Packet::Data((conn_id, data)) => self.on_data(conn_id, data),
            _ => self.on_unknown()
        }
    }

    fn on_unknown<'a>(&self) -> BoxFuture<'a, ()> {
        let state = self.state.borrow();
        if !state.handshaked {
            // Unknown packet received while not handshaked yet.
            // Close the connection.
            do_log!(self.logger, WARNING, "Authentication failure. Client: {}", state.client.unwrap());
            self.writer.feed(OwnedMessage::Close(None));
        }
        empty_future!()
    }

    fn on_handshake<'a>(&self, addr: SocketAddr) -> BoxFuture<'a, ()> {
        let mut state = self.state.borrow_mut();
        do_log!(self.logger, INFO, "New session: {} <=> {}", state.client.unwrap(), addr);
        state.remote = Some(addr);
        state.handshaked = true;

        // Send anything back to activate the connection
        self.writer.feed(OwnedMessage::Text(String::from("hello")));

        empty_future!()
    }

    fn on_connect<'a>(&self, conn_id: &str) -> BoxFuture<'a, ()> {
        let state = self.state.clone();
        let conn_id_owned = String::from(conn_id);
        let writer = self.writer.clone();
        let logger = self.logger.clone();
        RemoteConnection::connect(conn_id, logger.clone(), &self.state.borrow().remote.unwrap(), self.handle.clone())
            .map(clone!(writer, logger, state, conn_id_owned; |conn| {
                // Subscribe to remote events
                // Remote => Client
                conn.subscribe(RemoteConnectionEvents::Data, clone!(writer, conn_id_owned; |data| {
                    unwrap!(&RemoteConnectionValues::Packet(ref data), data, ());

                    // Forward data to client.
                    writer.feed(OwnedMessage::Binary(proto::data_build(&conn_id_owned, data)))
                }));

                // Remote connection closed
                conn.subscribe(RemoteConnectionEvents::Close, clone!(writer, state, conn_id_owned; |_| {
                    // Call shared clean-up code for this.
                    Self::close_conn(&mut state.borrow_mut(), &writer, &conn_id_owned);
                }));

                // Notify the client that this connection is now up.
                do_log!(logger, INFO, "[{}] Connection estabilished.", conn_id_owned);
                writer.feed(OwnedMessage::Text(proto::connect_state_build(&conn_id_owned, true)));

                // Store the connection inside the table.
                state.borrow_mut().remote_connections.insert(conn_id_owned, conn);
            }))
            .then(clone!(writer, logger, conn_id_owned; |r| {
                if r.is_err() {
                    // The connection has failed.
                    // If it fails here, then the connection
                    // has not been set up yet.
                    // Thus, we only need to notify the client
                    // about this.
                    // We do not need any clean-up job.
                    do_log!(logger, ERROR, "[{}] Failed to establish connection: {:?}", conn_id_owned, r.unwrap_err());
                    writer.feed(OwnedMessage::Text(proto::connect_state_build(&conn_id_owned, false)));
                }
                Ok(())
            }))
            ._box()
    }

    fn on_connect_state<'a>(&self, conn_id: &str, ok: bool) -> BoxFuture<'a, ()> {
        if !ok {
            // Call shared clean-up code to clean up the connection.
            Self::close_conn(&mut self.state.borrow_mut(), &self.writer, conn_id);
        }
        // Client side will not send ok = false.
        empty_future!()
    }

    fn on_data<'a>(&self, conn_id: &str, data: &[u8]) -> BoxFuture<'a, ()> {
        let ref conns = self.state.borrow().remote_connections;
        if let Some(conn) = conns.get(conn_id) {
            conn.send(data);
        }
        empty_future!()
    }

    // Clean-up job after a connection is closed.
    fn close_conn(state: &mut ServerSessionState, writer: &util::BufferedWriter<ClientSink>, conn_id: &str) {
        let ref mut conns = state.remote_connections;
        if conns.contains_key(conn_id) {
            conns.remove(conn_id);

            // Notify the client that this connection has been closed.
            writer.feed(OwnedMessage::Text(proto::connect_state_build(conn_id, false)));
        }
    }
}

#[derive(PartialEq, Eq)]
enum RemoteConnectionEvents {
    Data,
    Close
}

#[derive(Debug)]
enum RemoteConnectionValues {
    Nothing,
    Packet(BytesMut)
}

struct RemoteConnection {
    conn_id: String,
    logger: util::Logger,
    remote_writer: Rc<util::BufferedWriter<RemoteSink>>,
    event_emitter: RcEventEmitter<RemoteConnectionEvents, RemoteConnectionValues>
}

impl RemoteConnection {
    fn connect<'a>(
        conn_id: &str,
        logger: util::Logger,
        addr: &SocketAddr,
        handle: Handle
    ) -> BoxFuture<'a, RemoteConnection> {
        let conn_id_owned = String::from(conn_id);

        TcpStream::connect(addr, &handle.clone())
            .chain_err(|| "Connection failed")
            .map(move |s| {
                // The communication channel between this connection
                // and the parent ServerSession
                let emitter = util::new_emitter();

                // Convert the client into two halves
                let (sink, stream) = s.framed(BytesCodec::new()).split();

                // BufferedWriter for sending to remote
                let remote_writer = Rc::new(util::BufferedWriter::new());

                // Forward remote packets to client
                let stream_work = stream.for_each(clone!(emitter, logger, conn_id_owned; |p| {
                    do_log!(logger, INFO, "[{}] received {} bytes from remote", conn_id_owned, p.len());
                    emitter.borrow().emit(RemoteConnectionEvents::Data, RemoteConnectionValues::Packet(p));
                    Ok(())
                })).map_err(clone!(logger, conn_id_owned; |e| {
                    do_log!(logger, ERROR, "[{}] Remote => Client side error {:?}", conn_id_owned, e);
                })).map(|_| ());

                // Forward client packets to remote
                // Client packets should be sent through `send` method.
                let sink_work = remote_writer.run(sink)
                    .map_err(clone!(logger, conn_id_owned; |e| {
                        do_log!(logger, ERROR, "[{}] Client => Remote side error {:?}", conn_id_owned, e);
                    }));

                // Schedule the two jobs on the event loop
                // Use `select` to wait one of the jobs to finish.
                // This is often the `sink_work` if no error on remote side
                // has happened.
                // Once one of them is finished, just tear down the whole
                // channel.
                handle.spawn(stream_work.select(sink_work)
                    .then(clone!(emitter, logger, conn_id_owned; |_| {
                        // Clean-up job upon finishing
                        // No matter if there is any error.
                        do_log!(logger, INFO, "[{}] Channel closing.", conn_id_owned);
                        emitter.borrow().emit(RemoteConnectionEvents::Close, RemoteConnectionValues::Nothing);
                        Ok(())
                    })));

                // Create RemoteConnection object
                // To be used in ServerSession to forward data
                RemoteConnection {
                    conn_id: conn_id_owned,
                    logger,
                    remote_writer,
                    event_emitter: emitter
                }
            })
            ._box()
    }

    /*
     * Send a data buffer to remote via the BufferedWriter
     * created while connecting
     */
    fn send(&self, data: &[u8]) {
        do_log!(self.logger, INFO, "[{}]: sending {} bytes to remote", self.conn_id, data.len());
        self.remote_writer.feed(Bytes::from(data));
    }

    fn close(&self) {
        self.remote_writer.close();
    }
}

impl EventSource<RemoteConnectionEvents, RemoteConnectionValues> for RemoteConnection {
    fn get_event_emitter(&self) -> RcEventEmitter<RemoteConnectionEvents, RemoteConnectionValues> {
        self.event_emitter.clone()
    }
}

impl Drop for RemoteConnection {
    fn drop(&mut self) {
        self.close();
    }
}