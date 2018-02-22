/*
 * Client-side concrete implementation of TWS
 * 
 * Refer to `protocol.rs` for detailed description
 * of the protocol.
 */
use futures::future::{Future, IntoFuture};
use futures::stream::{Stream, SplitSink};
use protocol::protocol as proto;
use protocol::util::{self, BoxFuture, Boxable, FutureChainErr, HeartbeatAgent, SharedWriter, RcEventEmitter, EventSource, StreamThrottler};
use protocol::shared::{
    TwsServiceState, TwsService, TwsConnection,
    TcpSink, ConnectionEvents, ConnectionValues};
use rand::{self, Rng};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::rc::Rc;
use tokio_core::net::{TcpListener, TcpStream};
use tokio_core::reactor::Handle;
use tokio_io::codec::Framed;
use websocket::{ClientBuilder, OwnedMessage};
use websocket::async::MessageCodec;
use websocket::stream::async::Stream as WsStream;

#[derive(Clone)]
pub struct TwsClientOption {
    pub connections: usize,
    pub listen: SocketAddr,
    pub remote: SocketAddr,
    pub server: String,
    pub passwd: String,
    pub timeout: u64
}

/*
 * Client object for TWS protocol
 */
pub struct TwsClient {
    option: TwsClientOption,
    handle: Handle,
    logger: util::Logger,
    sessions: Rc<RefCell<Vec<Option<ClientSessionHandle>>>>
}

impl TwsClient {
    pub fn new(handle: Handle, option: TwsClientOption) -> TwsClient {
        let capacity = option.connections;
        TwsClient {
            handle,
            option,
            logger: Rc::new(util::default_logger),
            sessions: Rc::new(RefCell::new(Vec::with_capacity(capacity))) // TODO: Maintain a list of connections
        }
    }

    pub fn on_log<F>(&mut self, logger: F) where F: 'static + Fn(util::LogLevel, &str) {
        self.logger = Rc::new(logger);
    }

    pub fn run<'a>(&self) -> BoxFuture<'a, ()> {
        clone!(self, sessions, handle, logger);
        self.maintain_sessions();
        TcpListener::bind(&self.option.listen, &self.handle)
            .into_future()
            .chain_err(|| "Failed to bind to local port")
            .map(move |server| {
                server.incoming()
                    .map_err(|_| "Failed to listen for connections".into())
            })
            .flatten_stream()
            .for_each(move |(client, _addr)| {
                // Randomly choose a session to assign the new connection to.
                let session_id = Self::choose_session(&sessions);
                if let Some(id) = session_id {
                    if let Some(ref conn) = sessions.borrow()[id] {
                        let conn_id = conn.add_pending_connection(client, handle.clone());
                        do_log!(logger, INFO, "[{}] new connection assigned to session {}", conn_id, id);
                    }
                } else {
                    do_log!(logger, WARNING, "Failed to assign an active session to the new connection");
                }
                Ok(())
            })
            ._box()
    }

    /*
     * Spawn a task for each session
     * maintain the session, retry when closed.
     */
    fn maintain_sessions(&self) {
        for _ in 0..self.option.connections {
            self.sessions.borrow_mut().push(None);
        }

        for i in 0..self.option.connections {
            self.handle.spawn(
                Self::run_session(self.sessions.clone(), self.handle.clone(), self.logger.clone(), self.option.clone(), i)
                    .map_err(|_| ())
            )
        }
    }

    /*
     * Tail-recursive Future to maintain a ClientSession
     * automatically reconnect on close.
     */
    fn run_session<'a>(
        sessions: Rc<RefCell<Vec<Option<ClientSessionHandle>>>>,
        handle: Handle, logger: util::Logger, option: TwsClientOption, id: usize
    ) -> BoxFuture<'a, ()> {
        let s = ClientSession::new(id, handle.clone(), logger.clone(), option.clone());
        sessions.borrow_mut()[id] = Some(s.get_handle());
        s.run()
            .then(clone!(handle; |_| {
                do_log!(logger, WARNING, "Session {} closed. Retrying...", id);

                // Get rid of the handle first.
                sessions.borrow_mut()[id] = None;

                // Restart this session
                // TODO: Retry after a certain timeout.
                Self::run_session(sessions, handle, logger, option, id)
            }))
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
type ServerConn = Framed<Box<WsStream + Send>, MessageCodec<OwnedMessage>>;
type ServerSink = SplitSink<ServerConn>;

struct ClientSessionState {
    connected: bool,
    connections: HashMap<String, ClientConnection>,
    pending_connections: HashMap<String, PendingClientConnection>,
    paused: bool
}

impl TwsServiceState<ClientConnection> for ClientSessionState {
    fn get_connections(&self) -> &HashMap<String, ClientConnection> {
        &self.connections
    }

    fn get_paused(&self) -> bool {
        self.paused
    }

    fn set_paused(&mut self, paused: bool) {
        self.paused = paused;
    }
}

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
    fn add_pending_connection(&self, client: TcpStream, handle: Handle) -> String {
        let conn_id = util::rand_str(6);
        self.state.borrow_mut().pending_connections.insert(conn_id.clone(), PendingClientConnection {
            conn_id: conn_id.clone(),
            handle,
            logger: self.logger.clone(),
            client
        });

        // Send CONNECT request to server.
        // Wait for response.
        self.writer.feed(OwnedMessage::Text(proto::connect_build(&self.option.passwd, &conn_id).unwrap()));
        conn_id
    }
}

/*
 * A ClientSession is one WebSocket connection
 * between the client and the server.
 * 
 * Once established, it can forward traffic from
 * multiple TCP clients, through server to remote.
 */
struct ClientSession {
    id: usize,
    option: TwsClientOption,
    handle: Handle,
    logger: util::Logger,
    writer: SharedWriter<ServerSink>,
    heartbeat_agent: HeartbeatAgent<ServerSink>,
    state: Rc<RefCell<ClientSessionState>>
}

impl ClientSession {
    fn new(id: usize, handle: Handle, logger: util::Logger, option: TwsClientOption) -> ClientSession {
        let writer = SharedWriter::new();
        ClientSession {
            heartbeat_agent: HeartbeatAgent::new(option.timeout, writer.clone()),
            id,
            option,
            handle,
            logger,
            writer,
            state: Rc::new(RefCell::new(ClientSessionState {
                connected: false,
                connections: HashMap::new(),
                pending_connections: HashMap::new(),
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
    fn run<'a>(self) -> BoxFuture<'a, ()> {
        clone!(self, handle);

        // Create the WebSocket client.
        ClientBuilder::new(&self.option.server)
            .into_future()
            .chain_err(|| "Parse failure")
            .and_then(move |builder| {
                builder.async_connect(None, &handle)
                    .chain_err(|| "Connect failure")
            })
            .and_then(move |(client, _headers)| {
                clone!(self, state);

                // Send handshake before anything happens
                self.writer.feed(OwnedMessage::Text(proto::handshake_build(&self.option.passwd, self.option.remote.clone()).unwrap()));

                // Spin up the service
                // TODO: Add a periodic job to clear the pending connection list.
                self.run_service(client)
                    .then(move |_| {
                        // TODO: Cleanup job
                        let mut _state = state.borrow_mut();
                        _state.connected = false;
                        _state.connections.clear();
                        Ok(())
                    })
            })
            ._box() // When this future finish, everything should end here.
    }

    /*
     * Call this to activate a pending connection.
     * NOTE: This method does not check the validity. It is 
     *      up to the caller to make sure that conn_id is valid.
     */
    fn activate_connection(&self, conn_id: &str) {
        clone!(self, writer, state);
        let conn = self.state.borrow_mut().pending_connections.remove(conn_id).unwrap().connect();
        let conn_id = String::from(conn_id);

        // Forward client packet to server
        conn.subscribe(ConnectionEvents::Data, clone!(conn_id, writer; |data| {
            unwrap!(&ConnectionValues::Packet(ref data), data, ());
            writer.feed(OwnedMessage::Binary(proto::data_build(&conn_id, data)));
        }));

        // Listen to close event
        conn.subscribe(ConnectionEvents::Close, clone!(conn_id, writer; |_| {
            ClientSession::close_conn(&state, &writer, &conn_id);
        }));

        // Add the connection to active connection list
        self.state.borrow_mut().connections.insert(String::from(conn.get_conn_id()), conn);
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

impl TwsService<ClientConnection, ClientSessionState, Box<WsStream + Send>> for ClientSession {
    fn get_passwd(&self) -> &str {
        &self.option.passwd
    }

    fn get_logger(&self) -> &util::Logger {
        &self.logger
    }

    fn get_writer(&self) -> &SharedWriter<ServerSink> {
        &self.writer
    }

    fn get_heartbeat_agent(&self) -> &HeartbeatAgent<ServerSink> {
        &self.heartbeat_agent
    }

    fn get_state(&self) -> &Rc<RefCell<ClientSessionState>> {
        &self.state
    }

    fn on_unknown(&self) {
        // TODO: Use a dedicated packet type rather than `unknown`
        // to signal successful handshake.
        let mut state = self.state.borrow_mut();
        if !state.connected {
            do_log!(self.logger, INFO, "[{}] Session up.", self.id);
            state.connected = true;
        }
    }

    fn on_connect_state(&self, conn_id: &str, state: proto::ConnectionState) {
        if state.is_closed() {
            Self::close_conn(&self.state, &self.writer, conn_id);
        } else if state.is_ok() {
            if self.state.borrow().pending_connections.contains_key(conn_id) {
                // If there is a corresponding pending connection, activate it.
                self.activate_connection(conn_id);
            } else {
                // Else we instruct the server to close the unknown connection
                self.writer.feed(OwnedMessage::Text(proto::connect_state_build(conn_id, proto::ConnectionState::Closed)));
            }
        }
    }

    fn on_data(&self, conn_id: &str, data: &[u8]) {
        let state = self.state.borrow();
        if let Some(conn) = state.connections.get(conn_id) {
            conn.send(data);
        }
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
    conn_id: String,
    handle: Handle,
    logger: util::Logger,
    client: TcpStream
}

impl PendingClientConnection {
    /*
     * Upgrade this connection to an actual ClientConnection
     */
    fn connect(self) -> ClientConnection {
        ClientConnection::new(self.conn_id, self.logger, self.handle, self.client)
    }
}

/*
 * Model of a TCP connection from client to local.
 */
struct ClientConnection {
    conn_id: String,
    logger: util::Logger,
    event_emitter: RcEventEmitter<ConnectionEvents, ConnectionValues>,
    client_writer: SharedWriter<TcpSink>,
    read_throttler: StreamThrottler,
    read_pause_counter: Cell<usize>
}

impl ClientConnection {
    /*
     * Create the connection and start to forward traffic.
     * Do not use this if the connection should be pending.
     */
    fn new(conn_id: String, logger: util::Logger, handle: Handle, client: TcpStream) -> ClientConnection {
        let (emitter, writer, read_throttler) = Self::create(conn_id.clone(), handle, logger.clone(), client);

        ClientConnection {
            conn_id,
            logger,
            event_emitter: emitter,
            client_writer: writer,
            read_throttler,
            read_pause_counter: Cell::new(0)
        }
    }
}

impl TwsConnection for ClientConnection {
    fn get_endpoint_descriptors() -> (&'static str, &'static str) {
        ("client", "server")
    }

    fn get_logger(&self) -> &util::Logger {
        &self.logger
    }

    fn get_conn_id(&self) -> &str {
        &self.conn_id
    }

    fn get_writer(&self) -> &SharedWriter<TcpSink> {
        &self.client_writer
    }

    fn get_read_throttler(&self) -> &StreamThrottler {
        &self.read_throttler
    }

    fn get_read_pause_counter(&self) -> &Cell<usize> {
        &self.read_pause_counter
    }
}

impl EventSource<ConnectionEvents, ConnectionValues> for ClientConnection {
    fn get_event_emitter(&self) -> RcEventEmitter<ConnectionEvents, ConnectionValues> {
        self.event_emitter.clone()
    }
}

impl Drop for ClientConnection {
    fn drop(&mut self) {
        /*
         * Close immediatly on drop.
         */
        self.close();
    }
}