/*
 * Server-side concrete implementation of TWS
 */
use futures::{Stream};
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
    remote: Option<SocketAddr>,
    handshaked: bool
}

struct ServerSession {
    option: TwsServerOption,
    logger: util::Logger,
    handle: Handle,
    writer: Rc<util::BufferedWriter<ClientSink>>,
    state: RefCell<ServerSessionState>
}

impl ServerSession {
    pub fn new(option: TwsServerOption, logger: util::Logger, handle: Handle) -> ServerSession {
        ServerSession {
            option,
            logger,
            handle,
            writer: Rc::new(util::BufferedWriter::new()),
            state: RefCell::new(ServerSessionState {
                remote: None,
                handshaked: false
            })
        }
    }

    pub fn run<'a>(self, client: Client<TcpStream>) -> BoxFuture<'a, ()> {
        let local_logger1 = self.logger.clone();
        let local_logger2 = self.logger.clone();
        let (sink, stream) = client.split();
        let sink_write = self.writer.run(sink).map_err(move |e| {
            do_log!(local_logger1, ERROR, "{:?}", e);
            "session failed.".into()
        });
        Box::new(stream
            .map_err(move |e| {
                do_log!(local_logger2, ERROR, "{:?}", e);
                "session failed.".into()
            })
            .for_each(move |msg| {
                self.on_message(msg)
            })
            .into_future()
            .map(|_| ())
            .join(sink_write)
            .map(|_| ()))
    }

    fn on_message<'a>(&self, msg: OwnedMessage) -> BoxFuture<'a, ()> {
        match msg {
            OwnedMessage::Text(text) => self.on_packet(proto::parse_packet(&self.option.passwd, text.as_bytes())),
            OwnedMessage::Binary(bytes) => self.on_packet(proto::parse_packet(&self.option.passwd, &bytes)),
            // TODO: Implement Close / Ping events
            _ => Box::new(Ok(()).into_future())
        }
    }

    fn on_packet<'a>(&self, packet: proto::Packet) -> BoxFuture<'a, ()> {
        do_log!(self.logger, DEBUG, "{:?}", packet);
        match packet {
            proto::Packet::Handshake(addr) => self.on_handshake(addr),
            _ => {
                if !self.state.borrow().handshaked {
                    // Unknown packet received while not handshaked yet.
                    // Close the connection.
                    do_log!(self.logger, WARNING, "Authentication failure.");
                    self.writer.feed(OwnedMessage::Close(None));
                }
                Box::new(Ok(()).into_future())
            }
        }
    }

    fn on_handshake<'a>(&self, addr: SocketAddr) -> BoxFuture<'a, ()> {
        let ref mut state = self.state.borrow_mut();
        state.remote = Some(addr);
        state.handshaked = true;

        // Send anything back to activate the connection
        self.writer.feed(OwnedMessage::Text(String::from("hello")));

        Box::new(Ok(()).into_future())
    }
}