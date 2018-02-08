/*
 * Server-side concrete implementation of TWS
 */
use bytes::Bytes;
use futures::{Stream};
use futures::future::{Future, IntoFuture};
use futures::stream::{SplitSink};
use protocol::protocol as proto;
use protocol::util::{self, BoxFuture, FutureChainErr};
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
                    .for_each(move |(upgrade, addr)| {
                        let local_option = self.option.clone();
                        let local_logger = self.logger.clone();
                        let local_handle = self.handle.clone();
                        let work = upgrade.accept()
                            .chain_err(|| "Failed to accept connection.")
                            .and_then(move |(client, _)| {
                                ServerSession::new(local_option, local_logger, local_handle)
                                    .run(client, addr)
                            })
                            .map_err(|_| ());
                        self.handle.spawn(work);
                        Ok(())
                    })
            }))
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
    state: Rc<RefCell<ServerSessionState>>
}

impl ServerSession {
    pub fn new(option: TwsServerOption, logger: util::Logger, handle: Handle) -> ServerSession {
        ServerSession {
            option,
            logger,
            handle,
            writer: Rc::new(util::BufferedWriter::new()),
            state: Rc::new(RefCell::new(ServerSessionState {
                remote: None,
                client: None,
                handshaked: false,
                remote_connections: HashMap::new()
            }))
        }
    }

    pub fn run<'a>(self, client: Client<TcpStream>, addr: SocketAddr) -> BoxFuture<'a, ()> {
        self.state.borrow_mut().client = Some(addr);
        let logger = self.logger.clone();
        let (sink, stream) = client.split();
        let sink_write = self.writer.run(sink).map_err(clone!(logger; |e| {
            do_log!(logger, ERROR, "{:?}", e);
            "session failed.".into()
        }));
        Box::new(stream
            .map_err(clone!(logger; |e| {
                do_log!(logger, ERROR, "{:?}", e);
                "session failed.".into()
            }))
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
            OwnedMessage::Ping(msg) => {
                self.writer.feed(OwnedMessage::Pong(msg));
                Box::new(Ok(()).into_future())
            },
            _ => Box::new(Ok(()).into_future())
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
        let ref state = self.state.borrow();
        if !state.handshaked {
            // Unknown packet received while not handshaked yet.
            // Close the connection.
            do_log!(self.logger, WARNING, "Authentication failure. Client: {}", state.client.unwrap());
            self.writer.feed(OwnedMessage::Close(None));
        }
        Box::new(Ok(()).into_future())
    }

    fn on_handshake<'a>(&self, addr: SocketAddr) -> BoxFuture<'a, ()> {
        let ref mut state = self.state.borrow_mut();
        do_log!(self.logger, INFO, "New session: {} <=> {}", state.client.unwrap(), addr);
        state.remote = Some(addr);
        state.handshaked = true;

        // Send anything back to activate the connection
        self.writer.feed(OwnedMessage::Text(String::from("hello")));

        Box::new(Ok(()).into_future())
    }

    fn on_connect<'a>(&self, conn_id: &str) -> BoxFuture<'a, ()> {
        let state = self.state.clone();
        let conn_id_owned = String::from(conn_id);
        let writer = self.writer.clone();
        Box::new(
            RemoteConnection::connect(conn_id, self.logger.clone(), self.writer.clone(), &self.state.borrow().remote.unwrap(), self.handle.clone())
                .map(clone!(writer, conn_id_owned; |conn| {
                    writer.feed(OwnedMessage::Text(proto::connect_state_build(&conn_id_owned, true)));
                    state.borrow_mut().remote_connections.insert(conn_id_owned, conn);
                }))
                .then(clone!(writer, conn_id_owned; |r| {
                    // TODO: Log
                    if r.is_err() {
                        writer.feed(OwnedMessage::Text(proto::connect_state_build(&conn_id_owned, false)));
                    }
                    Ok(())
                }))
        )
    }

    fn on_connect_state<'a>(&self, conn_id: &str, ok: bool) -> BoxFuture<'a, ()> {
        if !ok {
            let ref mut conns = self.state.borrow_mut().remote_connections;
            if conns.contains_key(conn_id) {
                conns.remove(conn_id);
            }
        }
        // Client side will not send ok = false.
        Box::new(Ok(()).into_future())
    }

    fn on_data<'a>(&self, conn_id: &str, data: &[u8]) -> BoxFuture<'a, ()> {
        let ref conns = self.state.borrow().remote_connections;
        if let Some(conn) = conns.get(conn_id) {
            conn.send(data);
        }
        Box::new(Ok(()).into_future())
    }
}

struct RemoteConnection {
    conn_id: String,
    logger: util::Logger,
    remote_writer: Rc<util::BufferedWriter<RemoteSink>>
}

impl RemoteConnection {
    fn connect<'a>(
        conn_id: &str,
        logger: util::Logger,
        client_writer: Rc<util::BufferedWriter<ClientSink>>,
        addr: &SocketAddr,
        handle: Handle
    ) -> BoxFuture<'a, RemoteConnection> {
        let conn_id_owned = String::from(conn_id);

        Box::new(TcpStream::connect(addr, &handle.clone())
            .chain_err(|| "Connection failed")
            .map(move |s| {
                // Convert the client into two halves
                let (sink, stream) = s.framed(BytesCodec::new()).split();

                // BufferedWriter for sending to remote
                let remote_writer = Rc::new(util::BufferedWriter::new());

                // Forward remote packets to client
                let stream_work = stream.for_each(clone!(client_writer, conn_id_owned; |p| {
                    client_writer.feed(OwnedMessage::Binary(proto::data_build(&conn_id_owned, &p)));
                    Ok(()).into_future()
                })).map_err(clone!(logger, conn_id_owned; |e| {
                    do_log!(logger, ERROR, "[{}] Remote => Client side error {:?}", conn_id_owned, e);
                }));

                // Forward client packets to remote
                let sink_work = remote_writer.run(sink)
                    .map_err(clone!(logger, conn_id_owned; |e| {
                        do_log!(logger, ERROR, "[{}] Client => Remote side error {:?}", conn_id_owned, e);
                    }));

                // Schedule the two jobs on the event loop
                handle.spawn(stream_work.join(sink_work)
                    .then(clone!(client_writer, logger, conn_id_owned; |_r| {
                        // Clean-up job upon finishing
                        do_log!(logger, INFO, "[{}] Channel closing.", conn_id_owned);
                        client_writer.feed(OwnedMessage::Text(proto::connect_state_build(&conn_id_owned, false)));
                        Ok(())
                    })));

                // Create RemoteConnection object
                // To be used in ServerSession to forward data
                RemoteConnection {
                    conn_id: conn_id_owned,
                    logger,
                    remote_writer
                }
            }))
    }

    /*
     * Send a data buffer to remote via the BufferedWriter
     * created while connecting
     */
    fn send(&self, data: &[u8]) {
        do_log!(self.logger, INFO, "[{}]: sending {} bytes to remote", self.conn_id, data.len());
        self.remote_writer.feed(Bytes::from(data));
    }
}