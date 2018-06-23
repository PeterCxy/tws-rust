/*
 * Server-side concrete implementation of TWS
 * 
 * Refer to `protocol.rs` for detailed description
 * of the protocol.
 */
use errors::*;
use bytes::{Bytes, BytesMut};
use futures::Stream;
use futures::future::{Future, IntoFuture};
use futures::stream::SplitSink;
use protocol::protocol as proto;
use protocol::shared::{TwsServiceState, TwsService, TwsConnection, TwsConnectionHandler, TcpSink};
use protocol::util::{self, FutureChainErr, HeartbeatAgent, SharedWriter, StreamThrottler};
use std::cell::RefCell;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::rc::Rc;
use tokio::executor::current_thread;
use tokio::net::TcpStream;
use tokio::reactor::Handle;
use tokio_codec::Framed;
use websocket::OwnedMessage;
use websocket::async::{Client, Server, MessageCodec};

#[derive(Clone, Serialize, Deserialize)]
pub struct TwsServerOption {
    pub listen: SocketAddr,
    pub passwd: String,
    #[serde(default = "util::default_timeout")]
    pub timeout: u64
}

/*
 * A TWS Server instance.
 * 
 * Listens for incoming WebSocket connections
 * and establishes individual TWS sessions
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
    logger: util::Logger // An abstract logger (see `util.rs` for details)
}

impl TwsServer {
    pub fn new(option: TwsServerOption) -> TwsServer {
        TwsServer {
            option,
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
    pub fn run<'a>(&self) -> impl Future<Error=Error, Item=()> {
        clone!(self, option, logger);

        // Bind the port first
        // this is a Result, we convert it to Future.
        Server::bind(self.option.listen, &Handle::current())
            .into_future()
            .chain_err(|| "Failed to bind to server")
            .map(move |server| {
                // The WebSocket server is now listening.
                // Retrieve the incoming connections as a stream
                server.incoming()
                    .map_err(|_| "Invalid Websocket connection".into())
            })
            .flatten_stream() // Convert the future to the stream of connections it contains.
            .map(|t| Some(t))
            .or_else(clone!(logger; |e| {
                do_log!(logger, WARNING, "{:?}, continuing anyway", e);
                Ok(None) // Recover from any error that might occur: just resume the stream
            }))
            .filter_map(|t| t)
            .for_each(move |(upgrade, addr)| {
                let mut addr = upgrade.stream.peer_addr().unwrap_or(addr);
                if addr.ip().is_loopback() {
                    // Trust the "X-Real-IP" header from loopback interface
                    let real_ip_res = upgrade.request.headers.get("x-real-ip").ok_or(())
                        .and_then(|x| x.to_str().map_err(|_| ()))
                        .and_then(|x| x.parse().map_err(|_| ()));
                    if let Ok(real_ip) = real_ip_res {
                        addr.set_ip(real_ip);
                    }
                }
                // Spawn a separate task for every incoming connection
                // on the event loop.
                let work = upgrade.accept()
                    .chain_err(|| "Failed to accept connection.")
                    .and_then(clone!(option, logger; |(client, _)| {
                        // Create a new ServerSession object
                        // in charge of every connection.
                        ServerSession::new(option, logger)
                            .run(client, addr)
                    }))
                    .map_err(clone!(logger; |e| {
                        do_log!(logger, WARNING, "{:?}", e)
                    }));
                current_thread::spawn(work);
                Ok(())
            })
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
    paused: bool,
    remote_connections: HashMap<String, RemoteConnection> // Map from connection ids to remote connection objects.
}

impl TwsServiceState<RemoteConnection> for ServerSessionState {
    #[inline(always)]
    fn get_connections(&mut self) -> &mut HashMap<String, RemoteConnection> {
        &mut self.remote_connections
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
    writer: SharedWriter<ClientSink>, // Writer for sending packets to the client side
    heartbeat_agent: HeartbeatAgent<ClientSink>, // Agent for doing heartbeats
    state: Rc<RefCell<ServerSessionState>> // The mutable state of this session
}

impl ServerSession {
    fn new(option: TwsServerOption, logger: util::Logger) -> ServerSession {
        let writer = SharedWriter::new();
        ServerSession {
            heartbeat_agent: HeartbeatAgent::new(option.timeout, writer.clone()),
            option,
            logger,
            writer,
            state: Rc::new(RefCell::new(ServerSessionState {
                remote: None,
                client: None,
                handshaked: false,
                paused: false,
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
    fn run<'a>(self, client: Client<TcpStream>, addr: SocketAddr) -> impl Future<Error=Error, Item=()> {
        clone!(self, state);

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
    }

    fn check_handshaked(&self) -> bool {
        let state = self.state.borrow();
        if !state.handshaked {
            // Treat as authentication failure if we receive anything
            // before handshake.
            do_log!(self.logger, WARNING, "Authentication / Protocol failure. Client: {}", state.client.unwrap());
            self.writer.close();
        }
        state.handshaked
    }

    // Clean-up job after a logical connection is closed.
    fn close_conn(state: &Rc<RefCell<ServerSessionState>>, writer: &SharedWriter<ClientSink>, conn_id: &str) {
        //let ref mut conns = state.remote_connections;
        let has_conn = state.borrow().remote_connections.contains_key(conn_id);
        if has_conn {
            state.borrow_mut().remote_connections.remove(conn_id);

            // Notify the client that this logical connection has been closed.
            writer.feed(OwnedMessage::Text(proto::connect_state_build(conn_id, proto::ConnectionState::Closed)));
        }
    }
}

impl TwsService<RemoteConnection, ServerSessionState, TcpStream> for ServerSession {
    #[inline(always)]
    fn get_passwd(&self) -> &str {
        &self.option.passwd
    }

    #[inline(always)]
    fn get_writer(&self) -> &SharedWriter<ClientSink> {
        &self.writer
    }

    #[inline(always)]
    fn get_heartbeat_agent(&self) -> &HeartbeatAgent<ClientSink> {
        &self.heartbeat_agent
    }

    #[inline(always)]
    fn get_logger(&self) -> &util::Logger {
        &self.logger
    }

    #[inline(always)]
    fn get_state(&self) -> &Rc<RefCell<ServerSessionState>> {
        &self.state
    }

    fn on_unknown(&self) {
        // If we have not handshaked and received unknown packet
        // then close the connection.
        self.check_handshaked();

        // TODO: Support adding garbage to obfuscate.
    }

    fn on_handshake(&self, addr: SocketAddr) {
        {
            let mut state = self.state.borrow_mut();
            do_log!(self.logger, INFO, "New session: {} <=> {}", state.client.unwrap(), addr);

            // Remote address is now available.
            state.remote = Some(addr);

            // Set handshake flag to true
            state.handshaked = true;
        }

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
        let conn_work = RemoteConnection::connect(
            conn_id, logger.clone(), &self.state.borrow().remote.unwrap(), writer.clone(),
            RemoteConnectionHandler {
                conn_id: conn_id_owned.clone(),
                ws_writer: writer.clone(),
                state: state.clone()
            }
        );
        let conn_work = conn_work
            .map(clone!(state, writer, logger, conn_id_owned; |conn| {
                // Notify the client that this connection is now up.
                do_log!(logger, INFO, "[{}] {} <=> {}, connection estabilished.", conn_id_owned, state.borrow().client.unwrap(), state.borrow().remote.unwrap());
                writer.feed(OwnedMessage::Text(proto::connect_state_build(&conn_id_owned, proto::ConnectionState::Ok)));

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
                    writer.feed(OwnedMessage::Text(proto::connect_state_build(&conn_id_owned, proto::ConnectionState::Closed)));
                }
                Ok(())
            }));
        current_thread::spawn(conn_work);
    }

    fn on_connect_state(&self, conn_id: &str, conn_state: proto::ConnectionState) {
        if !self.check_handshaked() { return; }
        self._on_connect_state(conn_id, &conn_state);

        if conn_state.is_closed() {
            // Call shared clean-up code to clean up the logical connection.
            Self::close_conn(&self.state, &self.writer, conn_id);
        }
        // Client side will not send ok = false.
    }

    fn on_data(&self, conn_id: &str, data: &[u8]) {
        if !self.check_handshaked() { return; }
        
        let writer = self.state.borrow().remote_connections.get(conn_id)
            .map(|conn| conn.get_writer().clone());
        match writer {
            Some(writer) => writer.feed(Bytes::from(data)),
            None => ()
        }
    }
}

/*
 * Forward TCP stream from remote to client
 */
struct RemoteConnectionHandler {
    conn_id: String,
    ws_writer: SharedWriter<ClientSink>,
    state: Rc<RefCell<ServerSessionState>>
}

impl TwsConnectionHandler for RemoteConnectionHandler {
    fn on_data(&self, data: BytesMut) {
        self.ws_writer.feed(OwnedMessage::Binary(proto::data_build(&self.conn_id, &data)));
    }

    fn on_close(&self) {
        ServerSession::close_conn(&self.state, &self.ws_writer, &self.conn_id);
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
    read_throttler: StreamThrottler,
    read_pause_counter: usize
}

impl RemoteConnection {
    fn connect<'a>(
        conn_id: &str,
        logger: util::Logger,
        addr: &SocketAddr,
        ws_writer: SharedWriter<ClientSink>,
        conn_handler: RemoteConnectionHandler
    ) -> impl Future<Error=Error, Item=RemoteConnection> {
        let conn_id_owned = String::from(conn_id);

        TcpStream::connect(addr)
            .chain_err(|| "Connection failed")
            .map(move |s| {
                let (remote_writer, read_throttler) =
                    Self::create(conn_id_owned.clone(), logger.clone(), s, ws_writer, conn_handler);

                // Create RemoteConnection object
                // To be used in ServerSession to forward data
                RemoteConnection {
                    conn_id: conn_id_owned,
                    logger,
                    remote_writer,
                    read_throttler,
                    read_pause_counter: 0
                }
            })
    }
}

impl TwsConnection for RemoteConnection {
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
        &self.remote_writer
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
    fn set_read_pause_counter(&mut self, counter: usize) {
        self.read_pause_counter = counter;
    }
}

impl Drop for RemoteConnection {
    fn drop(&mut self) {
        // Once a connection is dropped, close it immediately.
        self.close();
    }
}