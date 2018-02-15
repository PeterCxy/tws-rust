/*
 * Server-side concrete implementation of TWS
 * 
 * Refer to `protocol.rs` for detailed description
 * of the protocol.
 */
use bytes::{Bytes, BytesMut};
use futures::Stream;
use futures::future::{Future, IntoFuture};
use futures::stream::SplitSink;
use protocol::protocol as proto;
use protocol::shared::{TwsService, TwsConnection, TcpSink, ConnectionEvents, ConnectionValues};
use protocol::util::{self, BoxFuture, Boxable, RcEventEmitter, EventSource, FutureChainErr, HeartbeatAgent, SharedWriter};
use std::cell::RefCell;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::rc::Rc;
use tokio_core::net::TcpStream;
use tokio_core::reactor::Handle;
use tokio_io::AsyncRead;
use tokio_io::codec::BytesCodec;
use tokio_io::codec::Framed;
use websocket::OwnedMessage;
use websocket::async::{Server, Client, MessageCodec};

#[derive(Clone)]
pub struct TwsServerOption {
    pub listen: SocketAddr,
    pub passwd: String,
    pub timeout: u64
}

/*
 * A TWS Server instance.
 * 
 * Listens for incoming WebSocket connections
 * and estabilishes individual TWS sessions
 * on each connection. It then forwards data
 * packets from the client to the designated
 * remote server as is requested by the client.
 * 
 * Each TWS connection can handle multiple
 * `logical connections` which correspond
 * to one TCP connection to the remote.
 */
pub struct TwsServer {
    option: TwsServerOption,
    handle: Handle, // Tokio Handle
    logger: util::Logger // An abstract logger (see `util.rs` for details)
}

impl TwsServer {
    pub fn new(handle: Handle, option: TwsServerOption) -> TwsServer {
        TwsServer {
            option,
            handle,
            logger: Rc::new(util::default_logger)
        }
    }

    pub fn on_log<F>(&mut self, logger: F) where F: 'static + Fn(util::LogLevel, &str) {
        self.logger = Rc::new(logger);
    }

    /*
     * Execute this instance and returns a Future
     * that represents the execution.
     * 
     * The future should be polled in order to have
     * the server working correctly.
     */
    pub fn run<'a>(&self) -> BoxFuture<'a, ()> {
        clone!(self, option, logger, handle);

        // Bind the port first
        // this is a Result, we convert it to Future.
        Server::bind(self.option.listen, &self.handle)
            .into_future()
            .chain_err(|| "Failed to bind to server")
            .map(move |server| {
                // The WebSocket server is now listening.
                // Retrieve the incoming connections as a stream
                server.incoming()
                    .map_err(|_| "Failed to accept connections".into())
            })
            .flatten_stream() // Convert the future to the stream of connections it contains.
            .for_each(move |(upgrade, addr)| {
                // Spawn a separate task for every incoming connection
                // on the event loop.
                let work = upgrade.accept()
                    .chain_err(|| "Failed to accept connection.")
                    .and_then(clone!(option, logger, handle; |(client, _)| {
                        // Create a new ServerSession object
                        // in charge of every connection.
                        ServerSession::new(option, logger, handle)
                            .run(client, addr)
                    }))
                    .map_err(|_| ());
                handle.spawn(work);
                Ok(())
            })
            ._box()
    }
}

// Shorthands for sending sides of the streams
type ClientSink = SplitSink<Framed<TcpStream, MessageCodec<OwnedMessage>>>;

/*
 * The state of a single TWS session.
 */
struct ServerSessionState {
    remote: Option<SocketAddr>, // The remote address (will be available after handshake)
    client: Option<SocketAddr>, // The client address (will be available when spawned)
    handshaked: bool, // Have we finished the handshake?
    remote_connections: HashMap<String, RemoteConnection> // Map from connection ids to remote connection objects.
}

/*
 * A single TWS session.
 * 
 * This manages communication within a single client
 * connection. The session has a designated remote
 * server, which should be determined by the client.
 */
struct ServerSession {
    option: TwsServerOption, // Options should be passed from TwsServer instance.
    logger: util::Logger,
    handle: Handle, // Tokio handle
    writer: SharedWriter<ClientSink>, // Writer for sending packets to the client side
    heartbeat_agent: HeartbeatAgent<ClientSink>, // Agent for doing heartbeats
    state: Rc<RefCell<ServerSessionState>> // The mutable state of this session
}

impl ServerSession {
    fn new(option: TwsServerOption, logger: util::Logger, handle: Handle) -> ServerSession {
        let writer = SharedWriter::new();
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

    /*
     * Start this session.
     * 
     * Consumes self, the stream representing the client,
     * and the client address. Returns a future representing
     * this session, which should be spawned on the event
     * loop.
     */
    fn run<'a>(self, client: Client<TcpStream>, addr: SocketAddr) -> BoxFuture<'a, ()> {
        clone!(self, state, logger);

        // Now we have the client address
        state.borrow_mut().client = Some(addr);

        self.run_service(client)
            .then(clone!(state; |_| {
                // Clean-up job
                // Drop all the connections
                // will be closed by the implementation of Drop
                state.borrow_mut().remote_connections.clear();

                Ok(())
            }))
            ._box()
    }

    fn check_handshaked(&self) -> bool {
        let state = self.state.borrow();
        if !state.handshaked {
            // Treat as authentication failure if we receive anything
            // before handshake.
            do_log!(self.logger, WARNING, "Authentication / Protocol failure. Client: {}", state.client.unwrap());
            self.writer.feed(OwnedMessage::Close(None));
        }
        state.handshaked
    }

    // Clean-up job after a logical connection is closed.
    fn close_conn(state: &mut ServerSessionState, writer: &SharedWriter<ClientSink>, conn_id: &str) {
        let ref mut conns = state.remote_connections;
        if conns.contains_key(conn_id) {
            conns.remove(conn_id);

            // Notify the client that this logical connection has been closed.
            writer.feed(OwnedMessage::Text(proto::connect_state_build(conn_id, false)));
        }
    }
}

impl TwsService<TcpStream> for ServerSession {
    fn get_passwd(&self) -> &str {
        &self.option.passwd
    }

    fn get_writer(&self) -> &SharedWriter<ClientSink> {
        &self.writer
    }

    fn get_heartbeat_agent(&self) -> &HeartbeatAgent<ClientSink> {
        &self.heartbeat_agent
    }

    fn get_logger(&self) -> &util::Logger {
        &self.logger
    }

    fn on_unknown(&self) {
        // If we have not handshaked and received unknown packet
        // then close the connection.
        self.check_handshaked();

        // TODO: Support adding garbage to obfuscate.
    }

    fn on_handshake(&self, addr: SocketAddr) {
        let mut state = self.state.borrow_mut();
        do_log!(self.logger, INFO, "New session: {} <=> {}", state.client.unwrap(), addr);

        // Remote address is now available.
        state.remote = Some(addr);

        // Set handshake flag to true
        state.handshaked = true;

        // Send anything back to activate the connection
        self.writer.feed(OwnedMessage::Text(String::from("hello")));
    }

    /*
     * Open a new logical connection inside this channel.
     */
    fn on_connect(&self, conn_id: &str) {
        if !self.check_handshaked() { return; }

        clone!(self, state, writer, logger);
        let conn_id_owned = String::from(conn_id);
        let conn_work = RemoteConnection::connect(conn_id, logger.clone(), &self.state.borrow().remote.unwrap(), self.handle.clone())
            .map(clone!(writer, logger, state, conn_id_owned; |conn| {
                // Subscribe to remote events
                // Remote => Client
                conn.subscribe(ConnectionEvents::Data, clone!(writer, conn_id_owned; |data| {
                    unwrap!(&ConnectionValues::Packet(ref data), data, ());

                    // Forward data to client.
                    writer.feed(OwnedMessage::Binary(proto::data_build(&conn_id_owned, data)))
                }));

                // Remote connection closed
                conn.subscribe(ConnectionEvents::Close, clone!(writer, state, conn_id_owned; |_| {
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
            }));
        self.handle.spawn(conn_work);
    }

    fn on_connect_state(&self, conn_id: &str, ok: bool) {
        if !self.check_handshaked() { return; }

        if !ok {
            // Call shared clean-up code to clean up the logical connection.
            Self::close_conn(&mut self.state.borrow_mut(), &self.writer, conn_id);
        }
        // Client side will not send ok = false.
    }

    fn on_data(&self, conn_id: &str, data: &[u8]) {
        if !self.check_handshaked() { return; }
        
        let ref conns = self.state.borrow().remote_connections;
        if let Some(conn) = conns.get(conn_id) {
            // If the designated connection exists, just forward the data.
            conn.send(data);
        }
    }
}

/*
 * A connection to remote server
 * corresponding to a logical connection
 * inside one WebSocket session.
 * 
 * This emits events for remote packets.
 * Should be subscribed to by the ServerSession.
 */
struct RemoteConnection {
    conn_id: String,
    logger: util::Logger,
    remote_writer: SharedWriter<TcpSink>, // Writer of remote connection
    event_emitter: RcEventEmitter<ConnectionEvents, ConnectionValues>
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
                let (emitter, remote_writer) =
                    Self::create(conn_id_owned.clone(), handle, logger.clone(), s);

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
}

impl TwsConnection for RemoteConnection {
    fn get_logger(&self) -> &util::Logger {
        &self.logger
    }

    fn get_conn_id(&self) -> &str {
        &self.conn_id
    }

    fn get_writer(&self) -> &SharedWriter<TcpSink> {
        &self.remote_writer
    }
}

impl EventSource<ConnectionEvents, ConnectionValues> for RemoteConnection {
    fn get_event_emitter(&self) -> RcEventEmitter<ConnectionEvents, ConnectionValues> {
        self.event_emitter.clone()
    }
}

impl Drop for RemoteConnection {
    fn drop(&mut self) {
        // Once a connection is dropped, close it immediately.
        self.close();
    }
}