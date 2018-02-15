/*
 * Client-side concrete implementation of TWS
 * 
 * Refer to `protocol.rs` for detailed description
 * of the protocol.
 */
use errors::*;
use futures::future::{Future, IntoFuture};
use futures::stream::{Stream, SplitSink};
use protocol::util::{self, BoxFuture, Boxable, FutureChainErr, HeartbeatAgent, SharedWriter};
use protocol::shared::TwsService;
use std::cell::RefCell;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::rc::Rc;
use tokio_core::net::{TcpListener, TcpStream};
use tokio_core::reactor::Handle;
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
    sessions: Vec<Option<ClientSession>>
}

impl TwsClient {
    pub fn new(handle: Handle, option: TwsClientOption) -> TwsClient {
        let capacity = option.connections;
        TwsClient {
            handle,
            option,
            logger: Rc::new(util::default_logger),
            sessions: Vec::with_capacity(capacity) // TODO: Maintain a list of connections
        }
    }

    pub fn on_log<F>(&mut self, logger: F) where F: 'static + Fn(util::LogLevel, &str) {
        self.logger = Rc::new(logger);
    }

    pub fn run<'a>(&self) -> BoxFuture<'a, ()> {
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
                Ok(())
            })
            ._box()
    }
}

type ServerConn = Framed<Box<WsStream + Send>, MessageCodec<OwnedMessage>>;
type ServerSink = SplitSink<ServerConn>;

struct ClientSessionState {
    connected: bool,
    connections: HashMap<String, ClientConnection>,
}

// TODO: Need a handle class to manipulate state from outside the session
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

    fn is_connected(&self) -> bool {
        self.state.borrow().connected
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
                        state.borrow_mut().connections.clear();
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

// TODO: Should be an EventSource. Emits events to ClientSession
// and allows ClientSession to send data on it.
struct ClientConnection {

}