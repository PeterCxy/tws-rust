/*
 * Client-side concrete implementation of TWS
 * 
 * Refer to `protocol.rs` for detailed description
 * of the protocol.
 */
use errors::*;
use bytes::{Bytes, BytesMut};
use futures;
use futures::future::{Either, Future, IntoFuture};
use futures::stream::{Stream, SplitSink};
use protocol::protocol as proto;
use protocol::udp::{UdpDatagram, UdpStream, SharedUdpHandle};
use protocol::util::{self, BoxFuture, Boxable, FutureChainErr, HeartbeatAgent,
    SharedWriter, SharedSpeedometer, StreamThrottler};
use protocol::shared::{TwsServiceState, TwsService, TwsConnection,
    TwsConnectionHandler, TcpSink, TwsUdpConnection, TwsUdpConnectionHandler};
use rand::{self, Rng};
use std::cell::RefCell;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::rc::Rc;
use std::time::{Duration, Instant};
use tokio::reactor::Handle;
use tokio::net::{TcpListener, TcpStream};
use tokio::executor::current_thread;
use tokio_codec::Framed;
use tokio_timer;
use websocket::{ClientBuilder, OwnedMessage};
use websocket::async::{Client, MessageCodec};
use websocket::stream::async::Stream as WsStream;

#[derive(Clone, Serialize, Deserialize)]
pub struct TwsClientOption {
    #[serde(default = "util::default_connections")]
    pub connections: usize,
    pub listen: SocketAddr,
    pub remote: SocketAddr,
    pub server: String,
    pub passwd: String,
    #[serde(default = "util::default_timeout")]
    pub timeout: u64,
    #[serde(default = "util::default_retry_timeout")]
    pub retry_timeout: u64,
    #[serde(default = "util::default_no_udp")]
    pub no_udp: bool,
    #[serde(default = "util::default_udp_timeout")]
    pub udp_timeout: u64
}

/*
 * Client object for TWS protocol
 */
pub struct TwsClient {
    option: TwsClientOption,
    logger: util::Logger,
    sessions: Rc<RefCell<Vec<Option<ClientSessionHandle>>>>
}

impl TwsClient {
    pub fn new(option: TwsClientOption) -> TwsClient {
        let capacity = option.connections;
        TwsClient {
            option,
            logger: Rc::new(util::default_logger),
            sessions: Rc::new(RefCell::new(Vec::with_capacity(capacity))) // TODO: Maintain a list of connections
        }
    }

    pub fn on_log<F>(&mut self, logger: F) where F: 'static + Fn(util::LogLevel, &str) {
        self.logger = Rc::new(logger);
    }

    pub fn run<'a>(&self) -> impl Future<Error = Error, Item=()> {
        if self.option.no_udp {
            Either::A(self.run_tcp().then(|_| Ok(())))
        } else {
            Either::B(self.run_tcp().select(self.run_udp()).then(|_| Ok(())))
        }
    }

    pub fn run_tcp<'a>(&self) -> impl Future<Error=Error, Item=()> {
        clone!(self, sessions, logger);
        let task_maintain_sessions = self.maintain_sessions();
        TcpListener::bind(&self.option.listen)
            .into_future()
            .chain_err(|| "Failed to bind to local port")
            .map(move |server| {
                current_thread::spawn(task_maintain_sessions);
                server.incoming()
                    .map_err(|_| "Failed to listen for connections".into())
            })
            .flatten_stream()
            .for_each(move |client| {
                // Randomly choose a session to assign the new connection to.
                let session_id = Self::choose_session(&sessions);
                if let Some(id) = session_id {
                    if let Some(ref conn) = sessions.borrow()[id] {
                        let conn_id = conn.add_pending_connection(client);
                        do_log!(logger, INFO, "[{}] new connection assigned to session {}", conn_id, id);
                    }
                } else {
                    do_log!(logger, WARNING, "Failed to assign an active session to the new connection");
                }
                Ok(())
            })
    }

    pub fn run_udp<'a>(&self) -> impl Future<Error=Error, Item=()> {
        clone!(self, sessions, logger);
        UdpDatagram::bind(&self.option.listen, Duration::from_millis(self.option.udp_timeout))
            .into_future()
            .chain_err(|| "Failed to bind to local UDP port")
            .map(move |server| {
                let handle = server.get_handle();
                server
                    .map_err(|_| "err".into())
                    .map(move |a| (handle.clone(), a))
            })
            .flatten_stream()
            .for_each(move |(handle, (addr, client))| {
                let session_id = Self::choose_session(&sessions);
                if let Some(id) = session_id {
                    if let Some(ref conn) = sessions.borrow()[id] {
                        let conn_id = conn.add_udp_connection(addr, handle, client);
                        do_log!(logger, INFO, "[{}] new UDP connection assigned to session {}", conn_id, id);
                    }
                }
                Ok(())
            })
    }

    /*
     * Spawn a task for each session
     * maintain the session, retry when closed.
     */
    fn maintain_sessions(&self) -> impl Future<Error=(), Item=()> {
        clone!(self, option, sessions, logger);
        futures::lazy(move || {
            for _ in 0..option.connections {
                sessions.borrow_mut().push(None);
            }

            for i in 0..option.connections {
                current_thread::spawn(
                    Self::run_session(sessions.clone(), logger.clone(), option.clone(), i, 0)
                        .map_err(|_| ())
                )
            }

            futures::future::ok(())
        })
    }

    /*
     * Tail-recursive Future to maintain a ClientSession
     * automatically reconnect on close.
     */
    fn run_session<'a>(
        sessions: Rc<RefCell<Vec<Option<ClientSessionHandle>>>>,
        logger: util::Logger, option: TwsClientOption, id: usize,
        mut retry_count: u32
    ) -> BoxFuture<'a, ()> {
        let s = ClientSession::new(id, logger.clone(), option.clone());
        sessions.borrow_mut()[id] = Some(s.get_handle());
        s.run()
            .then(move |r| {
                if r.is_err() && retry_count < u32::max_value() - 1 {
                    retry_count += 1;
                } else {
                    retry_count = 1;
                }

                // Exponential backoff
                let retry_time = option.retry_timeout * rand::thread_rng().gen_range(0, u64::pow(2, retry_count) - 1);
                do_log!(logger, WARNING, "Session {} closed. Retrying after {} ms...", id, retry_time);

                // Get rid of the handle first.
                sessions.borrow_mut()[id] = None;

                // Restart this session (wait for some timeout)
                // +1 to avoid waiting for 0 seconds
                tokio_timer::sleep(Duration::from_millis(retry_time + 1))
                    .then(move |_| Self::run_session(sessions, logger, option, id, retry_count))
            })
            ._box()
    }

    /*
     * Choose a random working session from all the available sessions.
     * If nothing could be chosen, return None.
     */
    fn choose_session(sessions: &Rc<RefCell<Vec<Option<ClientSessionHandle>>>>) -> Option<usize> {
        let _sessions = sessions.borrow();
        let l = _sessions.len();
        let mut rng = rand::thread_rng();
        for _ in 0..l {
            let i = rng.gen_range(0, l);
            if let Some(ref s) = _sessions[i] {
                if s.is_connected() {
                    return Some(i);
                }
            }
        }
        None
    }
}

// Type shorthands
type ServerConn = Framed<Box<dyn WsStream + Send>, MessageCodec<OwnedMessage>>;
type ServerSink = SplitSink<ServerConn>;

make_tws_service_state!(
    ClientSessionState;
    ClientConnection, ClientUdpConnection;
    connections, udp_connections;
    {
        connected: bool,
        pending_connections: HashMap<String, PendingClientConnection>
    }
);

/*
 * Handle that can be used to communicate
 * to a ClientSession.
 * Used to add new pending connections.
 */
#[allow(dead_code)]
struct ClientSessionHandle {
    id: usize,
    option: TwsClientOption,
    logger: util::Logger,
    writer: SharedWriter<ServerSink>,
    state: Rc<RefCell<ClientSessionState>>
}

impl ClientSessionHandle {
    /*
     * Return true if the associated ClientSession is active.
     */
    fn is_connected(&self) -> bool {
        self.state.borrow().connected
    }

    /*
     * Add a pending connection to the session.
     * The connection will be active once the server finishes
     * connecting to remote.
     */
    fn add_pending_connection(&self, client: TcpStream) -> String {
        let conn_id = util::rand_str(6);
        self.state.borrow_mut().pending_connections.insert(conn_id.clone(), PendingClientConnection {
            created: Instant::now(),
            conn_id: conn_id.clone(),
            logger: self.logger.clone(),
            client,
            ws_writer: self.writer.clone(),
            state: self.state.clone()
        });

        // Send CONNECT request to server.
        // Wait for response.
        self.writer.feed(OwnedMessage::Text(proto::connect_build(&self.option.passwd, &conn_id).unwrap()));
        conn_id
    }

    fn add_udp_connection(&self, addr: SocketAddr, handle: SharedUdpHandle, client: UdpStream) -> String {
        let conn_id = util::rand_str(6);
        let handler = ClientUdpConnectionHandler {
            conn_id: conn_id.clone(),
            ws_writer: self.writer.clone(),
            state: self.state.clone()
        };
        self.state.borrow_mut().udp_connections.insert(conn_id.clone(),
            ClientUdpConnection::new(conn_id.clone(), self.logger.clone(), client, addr, handle, handler));
        self.writer.feed(OwnedMessage::Text(proto::udp_connect_build(&self.option.passwd, &conn_id).unwrap()));
        return conn_id;
    }
}

/*
 * A ClientSession is one WebSocket connection
 * between the client and the server.
 * 
 * Once established, it can forward traffic from
 * multiple TCP clients, through server to remote.
 */
make_tws_service!(
    ClientSession;
    ClientConnection, ClientUdpConnection, ClientSessionState, Box<WsStream + Send>;
    { id: usize, option: TwsClientOption };
    override fn get_passwd(&self) -> &str {
        &self.option.passwd
    }

    override fn get_udp_timeout(&self) -> u64 {
        self.option.udp_timeout
    }

    override fn on_unknown(&self) -> () {
        // TODO: Use a dedicated packet type rather than `unknown`
        // to signal successful handshake.
        let mut state = self.state.borrow_mut();
        if !state.connected {
            do_log!(self.logger, INFO, "[{}] Session up.", self.id);
            state.connected = true;
        }
    }

    override fn on_connect_state(&self, conn_id: &str, conn_state: proto::ConnectionState) -> () {
        self._on_connect_state(conn_id, &conn_state);
        if conn_state.is_closed() {
            Self::close_conn(&self.state, &self.writer, conn_id);
        } else if conn_state.is_ok() {
            if self.state.borrow().pending_connections.contains_key(conn_id) {
                // If there is a corresponding pending connection, activate it.
                self.activate_connection(conn_id);
            } else {
                // Else we instruct the server to close the unknown connection
                self.writer.feed(OwnedMessage::Text(proto::connect_state_build(conn_id, proto::ConnectionState::Closed)));
            }
        }
    }

    override fn on_data(&self, conn_id: &str, data: &[u8]) -> () {
        let writer = self.state.borrow().connections.get(conn_id)
            .map(|conn| conn.get_writer().clone());
        match writer {
            Some(writer) => writer.feed(Bytes::from(data)),
            None => ()
        }
    }

    override fn on_udp_data(&self, conn_id: &str, data: &[u8]) -> () {
        let handle = self.state.borrow().udp_connections.get(conn_id)
            .map(|conn| (conn.addr.clone(), conn.get_handle().clone()));
        match handle {
            Some((addr, handle)) => handle.borrow_mut().send_to(&addr, data),
            None => ()
        }
    }
);

impl ClientSession {
    fn new(id: usize, logger: util::Logger, option: TwsClientOption) -> ClientSession {
        let writer = SharedWriter::new();
        ClientSession {
            heartbeat_agent: HeartbeatAgent::new(option.timeout, writer.clone()),
            id,
            option,
            logger,
            writer,
            state: Rc::new(RefCell::new(ClientSessionState {
                connected: false,
                connections: HashMap::new(),
                pending_connections: HashMap::new(),
                udp_connections: HashMap::new(),
                paused: false
            }))
        }
    }

    /*
     * Get a handle associated with this session
     * This is necessary because run() takes ownership of self.
     */
    fn get_handle(&self) -> ClientSessionHandle {
        ClientSessionHandle {
            id: self.id,
            logger: self.logger.clone(),
            option: self.option.clone(),
            writer: self.writer.clone(),
            state: self.state.clone()
        }
    }

    /*
     * Spin up the session
     * Try to connect and start to accept traffic.
     */
    fn run<'a>(self) -> impl Future<Error=Error, Item=()> {
        // Create the WebSocket client.
        ClientBuilder::new(&self.option.server)
            .into_future()
            .chain_err(|| "Parse failure")
            .and_then(move |builder| {
                builder.async_connect(None, &Handle::current())
                    .chain_err(|| "Connect failure")
            })
            .and_then(move |(client, _headers)| {
                clone!(self, state, option, logger);

                // Send handshake before anything happens
                self.writer.feed(OwnedMessage::Text(proto::handshake_build(&option.passwd, option.remote.clone()).unwrap()));

                // Spin up the service
                self.run_service(client)
                    .select2(
                        // Periodically remove all pending connections.
                        tokio_timer::Interval::new(Instant::now(), Duration::from_millis(option.timeout))
                            .for_each(clone!(state; |_| {
                                //do_log!(logger, DEBUG, "Periodic cleanup of dead pending connections");
                                let to_remove: Vec<_> = state.borrow_mut().pending_connections.iter()
                                    .filter(|&(_, conn)| conn.created.elapsed() > Duration::from_millis(option.timeout))
                                    .map(|(id, _)| id.clone())
                                    .collect();
                                for id in to_remove {
                                    do_log!(logger, INFO, "[{}] timed out", id);
                                    state.borrow_mut().pending_connections.remove(&id);
                                }
                                Ok(())
                            }))
                    )
                    .then(move |_| {
                        // Cleanup job
                        let mut _state = state.borrow_mut();
                        _state.connected = false;
                        _state.connections.clear();
                        _state.pending_connections.clear();
                        Ok(())
                    })
            })
            // When this future finish, everything should end here.
    }

    /*
     * Call this to activate a pending connection.
     * NOTE: This method does not check the validity. It is 
     *      up to the caller to make sure that conn_id is valid.
     */
    fn activate_connection(&self, conn_id: &str) {
        let conn = self.state.borrow_mut().pending_connections.remove(conn_id).unwrap().connect();
        let conn_id = String::from(conn_id);

        // Add the connection to active connection list
        self.state.borrow_mut().connections.insert(conn_id, conn);
    }

    /*
     * Static method to close a connection (either an active one or a pending one)
     */
    fn close_conn(state: &Rc<RefCell<ClientSessionState>>, writer: &SharedWriter<ServerSink>, conn_id: &str) {
        let is_activated = state.borrow().connections.contains_key(conn_id);
        let is_pending = state.borrow().pending_connections.contains_key(conn_id);
        if is_activated {
            state.borrow_mut().connections.remove(conn_id);

            // Notify the server about closing this connection
            // TODO: Notify only when needed.
            writer.feed(OwnedMessage::Text(proto::connect_state_build(conn_id, proto::ConnectionState::Closed)));
        } else if is_pending {
            state.borrow_mut().pending_connections.remove(conn_id);
        }
    }
}

/*
 * Forward TCP stream from client to remote
 */
struct ClientConnectionHandler {
    conn_id: String,
    ws_writer: SharedWriter<ServerSink>,
    state: Rc<RefCell<ClientSessionState>>
}

impl TwsConnectionHandler for ClientConnectionHandler {
    fn on_data(&self, data: BytesMut) {
        self.ws_writer.feed(OwnedMessage::Binary(proto::data_build(&self.conn_id, &data)));
    }

    fn on_close(&self) {
        ClientSession::close_conn(&self.state, &self.ws_writer, &self.conn_id);
    }
}

/*
 * A ClientConnection that is still pending
 * waiting for the server to connect to remote.
 * 
 * Temporarily holds information about the future
 * ClientConnection.
 */
struct PendingClientConnection {
    created: Instant,
    conn_id: String,
    logger: util::Logger,
    client: TcpStream,
    ws_writer: SharedWriter<ServerSink>,
    state: Rc<RefCell<ClientSessionState>>
}

impl PendingClientConnection {
    /*
     * Upgrade this connection to an actual ClientConnection
     */
    fn connect(self) -> ClientConnection {
        ClientConnection::new(
            self.conn_id.clone(), self.logger, self.client, self.ws_writer.clone(),
            ClientConnectionHandler {
                conn_id: self.conn_id,
                ws_writer: self.ws_writer,
                state: self.state
            }
        )
    }
}

/*
 * Model of a TCP connection from client to local.
 */
make_tws_connection!(
    ClientConnection; client_writer;
    ("client", "server")
);

impl ClientConnection {
    /*
     * Create the connection and start to forward traffic.
     * Do not use this if the connection should be pending.
     */
    fn new(
        conn_id: String, logger: util::Logger, client: TcpStream,
        ws_writer: SharedWriter<ServerSink>, conn_handler: ClientConnectionHandler
    ) -> ClientConnection {
        let (speedometer, writer, read_throttler) =
            Self::create(conn_id.clone(), logger.clone(), client, ws_writer, conn_handler);

        ClientConnection {
            conn_id,
            logger,
            client_writer: writer,
            read_throttler,
            speedometer,
            read_pause_counter: 0
        }
    }
}

struct ClientUdpConnectionHandler {
    conn_id: String,
    ws_writer: SharedWriter<ServerSink>,
    state: Rc<RefCell<ClientSessionState>>
}

impl TwsUdpConnectionHandler for ClientUdpConnectionHandler {
    fn on_data(&self, d: Vec<u8>) {
        self.ws_writer.feed(OwnedMessage::Binary(proto::udp_data_build(&self.conn_id, &d)));
    }

    fn on_close(&self) {
        self.state.borrow_mut().udp_connections.remove(&self.conn_id);
    }
}

make_tws_udp_connection!(
    ClientUdpConnection;
    {
        addr: SocketAddr
    }
);

impl ClientUdpConnection {
    fn new(
        conn_id: String, logger: util::Logger, client: UdpStream, addr: SocketAddr,
        handle: SharedUdpHandle, handler: ClientUdpConnectionHandler
    ) -> ClientUdpConnection {
        Self::create(conn_id.clone(), logger.clone(), client, handler);
        ClientUdpConnection {
            conn_id,
            handle,
            logger,
            addr
        }
    }
}