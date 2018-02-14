/*
 * Client-side concrete implementation of TWS
 * 
 * Refer to `protocol.rs` for detailed description
 * of the protocol.
 */
use errors::*;
use futures::future::{Future, IntoFuture};
use futures::stream::{Stream, SplitSink};
use protocol::util::{self, BoxFuture, Boxable, FutureChainErr, SharedWriter};
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

#[derive(Clone)]
struct ClientSession {
    id: usize,
    option: TwsClientOption,
    handle: Handle,
    logger: util::Logger,
    writer: SharedWriter<ServerSink>,
    state: Rc<RefCell<ClientSessionState>>
}

impl ClientSession {
    fn new(id: usize, handle: Handle, logger: util::Logger, option: TwsClientOption) -> ClientSession {
        ClientSession {
            id,
            option,
            handle,
            logger,
            writer: SharedWriter::new(),
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
                self.on_connect(client)
            })
            ._box() // When this future finish, everything should end here.
    }

    fn on_connect<'a>(self, client: ServerConn) -> BoxFuture<'a, ()> {
        clone!(self, logger);
        let (sink, stream) = client.split();
        let sink_write = self.writer.run(sink).map_err(clone!(logger; |e| {
            do_log!(logger, ERROR, "{:?}", e);
        }));
        // TODO: Heartbeat
        // TODO: Extract shared code with the server
        stream
            .map_err(clone!(logger; |e| {
                do_log!(logger, ERROR, "{:?}", e);
                "session failed.".into()
            }))
            .for_each(move |msg| {
                self.on_message(msg);
                Ok(()) as Result<()>
            })
            .select2(sink_write)
            .then(clone!(logger; |_| {
                do_log!(logger, INFO, "Session finished.");
                Ok(())
            }))
            ._box()
    }

    fn on_message(&self, msg: OwnedMessage) {

    }
}

// TODO: Should be an EventSource. Emits events to ClientSession
// and allows ClientSession to send data on it.
struct ClientConnection {

}