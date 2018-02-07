/*
 * Server-side concrete implementation of TWS
 */
use futures::{Stream, Sink};
use futures::future::{Future, IntoFuture};
use futures::stream::{SplitSink};
use protocol::protocol as proto;
use protocol::util::{self, BoxFuture, FutureChainErr};
use std::cell::RefCell;
use std::net::SocketAddr;
use std::rc::Rc;
use tokio_core::net::TcpStream;
use tokio_core::reactor::Handle;
use websocket::OwnedMessage;
use websocket::async::{Server, Client, MessageCodec};
use websocket::client::async::Framed;

#[derive(Clone)]
pub struct TwsServerOption {
    pub listen: SocketAddr,
    pub passwd: String
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
        Box::new(Server::bind(self.option.listen, &self.handle)
            .into_future()
            .chain_err(|| "Failed to bind to server")
            .and_then(move |server| {
                server.incoming()
                    .map_err(|_| "Failed to accept connections".into())
                    .for_each(move |(upgrade, _addr)| {
                        let local_option = self.option.clone();
                        let local_logger = self.logger.clone();
                        let local_handle = self.handle.clone();
                        let work = upgrade.accept()
                            .chain_err(|| "Failed to accept connection.")
                            .and_then(move |(client, _)| {
                                ServerSession::new(local_option, local_logger, local_handle)
                                    .run(client)
                            })
                            .map_err(|_| ());
                        self.handle.spawn(work);
                        Ok(())
                    })
            }))
    }
}

type ClientSink = SplitSink<Framed<TcpStream, MessageCodec<OwnedMessage>>>;

struct ServerSessionState {
    remote: Option<SocketAddr>
}

struct ServerSession {
    option: TwsServerOption,
    logger: util::Logger,
    handle: Handle,
    state: RefCell<ServerSessionState>
}

impl ServerSession {
    pub fn new(option: TwsServerOption, logger: util::Logger, handle: Handle) -> ServerSession {
        ServerSession {
            option,
            logger,
            handle,
            state: RefCell::new(ServerSessionState {
                remote: None
            })
        }
    }

    pub fn run<'a>(self, client: Client<TcpStream>) -> BoxFuture<'a, ()> {
        let local_logger = self.logger.clone();
        let (sink, stream) = client.split();
        Box::new(stream
            .map_err(move |e| {
                do_log!(local_logger, ERROR, "{:?}", e);
                "session failed.".into()
            })
            .for_each(move |msg| {
                self.on_message(&sink, msg)
            })
            .into_future()
            //.map_err(|_| "session failed.".into())
            .map(|_| ()))
    }

    fn on_message<'a>(&self, sink: &ClientSink, msg: OwnedMessage) -> BoxFuture<'a, ()> {
        match msg {
            OwnedMessage::Text(text) => self.on_packet(sink, proto::parse_packet(&self.option.passwd, text.as_bytes())),
            OwnedMessage::Binary(bytes) => self.on_packet(sink, proto::parse_packet(&self.option.passwd, &bytes)),
            // TODO: Implement Close / Ping events
            _ => Box::new(Ok(()).into_future())
        }
    }

    fn on_packet<'a>(&self, sink: &ClientSink, packet: proto::Packet) -> BoxFuture<'a, ()> {
        do_log!(self.logger, DEBUG, "{:?}", packet);
        match packet {
            proto::Packet::Handshake(addr) => self.on_handshake(sink, addr),
            _ => Box::new(Ok(()).into_future()) // TODO: Close on failure?
        }
    }

    fn on_handshake<'a>(&self, sink: &ClientSink, addr: SocketAddr) -> BoxFuture<'a, ()> {
        self.state.borrow_mut().remote = Some(addr);
        Box::new(Ok(()).into_future())
    }
}