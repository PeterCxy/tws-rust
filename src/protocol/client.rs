/*
 * Client-side concrete implementation of TWS
 * 
 * Refer to `protocol.rs` for detailed description
 * of the protocol.
 */
use bytes::Bytes;
use errors::*;
use futures::future::{Future, IntoFuture};
use futures::stream::{Stream, SplitSink};
use protocol::util::{self, BoxFuture, Boxable, FutureChainErr, HeartbeatAgent, SharedWriter, RcEventEmitter, EventSource};
use protocol::shared::{TwsService, TwsConnection, TcpSink, ConnectionEvents, ConnectionValues};
use rand::{self, Rng};
use std::cell::RefCell;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::rc::Rc;
use tokio_core::net::{TcpListener, TcpStream};
use tokio_core::reactor::Handle;
use tokio_io::AsyncRead;
use tokio_io::codec::BytesCodec;
use tokio_io::codec::Framed;
use websocket::{ClientBuilder, OwnedMessage};
use websocket::async::{Client, MessageCodec};
use websocket::stream::async::Stream as WsStream;

#[derive(Clone)]
pub struct TwsClientOption {
    pub connections: usize,
    pub listen: SocketAddr,
    pub server: String,
    pub passwd: String,
    pub timeout: u64
}

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
        clone!(self, sessions);
        self.maintain_sessions();
        TcpListener::bind(&self.option.listen, &self.handle)
            .into_future()
            .chain_err(|| "Failed to bind to local port")
            .map(move |server| {
                server.incoming()
                    .map_err(|_| "Failed to listen for connections".into())
            })
            .flatten_stream()
            .for_each(move |(client, addr)| {
                // TODO: Randomly choose a working session
                let session_id = Self::choose_session(&sessions);
                if let Some(id) = session_id {

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

type ServerConn = Framed<Box<WsStream + Send>, MessageCodec<OwnedMessage>>;
type ServerSink = SplitSink<ServerConn>;

struct ClientSessionState {
    connected: bool,
    connections: HashMap<String, ClientConnection>,
}

struct ClientSessionHandle {
    id: usize,
    writer: SharedWriter<ServerSink>,
    state: Rc<RefCell<ClientSessionState>>
}

impl ClientSessionHandle {
    fn is_connected(&self) -> bool {
        self.state.borrow().connected
    }
}

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
            }))
        }
    }

    fn get_handle(&self) -> ClientSessionHandle {
        ClientSessionHandle {
            id: self.id,
            writer: self.writer.clone(),
            state: self.state.clone()
        }
    }

    fn run<'a>(self) -> BoxFuture<'a, ()> {
        clone!(self, handle);
        ClientBuilder::new(&self.option.server)
            .into_future()
            .chain_err(|| "Parse failure")
            .and_then(move |builder| {
                builder.async_connect(None, &handle)
                    .chain_err(|| "Connect failure")
            })
            .and_then(move |(client, _headers)| {
                clone!(self, state);
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
}

impl TwsService<Box<WsStream + Send>> for ClientSession {
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
}

struct ClientConnection {
    conn_id: String,
    logger: util::Logger,
    event_emitter: RcEventEmitter<ConnectionEvents, ConnectionValues>,
    client_writer: SharedWriter<TcpSink>
}

impl ClientConnection {
    fn new(logger: util::Logger, handle: Handle, client: TcpStream) -> ClientConnection {
        let conn_id = util::rand_str(6);
        let (emitter, writer) = Self::create(conn_id.clone(), handle, logger.clone(), client);
        /*let emitter = util::new_emitter();
        let (sink, stream) = client.framed(BytesCodec::new()).split();
        let writer = SharedWriter::new();
        let sink_work = writer.run(sink).map_err(clone!(conn_id, logger; |e| {
            do_log!(logger, ERROR, "[{}] Local => Client error: {:?}", conn_id, e);
        }));
        let stream_work = stream.for_each(clone!(conn_id, logger, emitter; |p| {
            do_log!(logger, INFO, "[{}] Received {} bytes from client", conn_id, p.len());
            emitter.borrow().emit(ConnectionEvents::Data, ConnectionValues::Packet(p));
            Ok(())
        })).map_err(clone!(conn_id, logger; |e| {
            do_log!(logger, ERROR, "[{}] Client => Local error: {:?}", conn_id, e);
        }));

        handle.spawn(sink_work.select(stream_work)
            .then(clone!(conn_id, logger, emitter; |_| {
                do_log!(logger, INFO, "[{}] Connection finished.", conn_id);
                emitter.borrow().emit(ConnectionEvents::Close, ConnectionValues::Nothing);
                Ok(())
            })));*/

        ClientConnection {
            conn_id,
            logger,
            event_emitter: emitter,
            client_writer: writer
        }
    }

    /*fn send(&self, data: &[u8]) {
        do_log!(self.logger, INFO, "[{}] sending {} bytes to client", self.conn_id, data.len());
        self.client_writer.feed(Bytes::from(data));
    }

    fn close(&self) {
        self.client_writer.close();
    }*/
}

impl TwsConnection for ClientConnection {
    fn get_logger(&self) -> &util::Logger {
        &self.logger
    }

    fn get_conn_id(&self) -> &str {
        &self.conn_id
    }

    fn get_writer(&self) -> &SharedWriter<TcpSink> {
        &self.client_writer
    }
}

impl EventSource<ConnectionEvents, ConnectionValues> for ClientConnection {
    fn get_event_emitter(&self) -> RcEventEmitter<ConnectionEvents, ConnectionValues> {
        self.event_emitter.clone()
    }
}

impl Drop for ClientConnection {
    fn drop(&mut self) {
        self.close();
    }
}