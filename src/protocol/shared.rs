/*
 * Shared implementation details between server and client
 */
use errors::*;
use futures::{Future, Stream};
use futures::stream::SplitSink;
use protocol::protocol as proto;
use protocol::util::{self, Boxable, BoxFuture, HeartbeatAgent, SharedWriter};
use websocket::OwnedMessage;
use websocket::async::Client;
use websocket::stream::async::Stream as WsStream;
use std::net::SocketAddr;

/*
 * Shared logic abstracted from both the server and the client
 * should be implemented only on structs
 */
pub trait TwsService<S: 'static + WsStream>: 'static + Sized {
    /*
     * Required fields simulated by required methods.
     */
    fn get_passwd(&self) -> &str;
    fn get_writer(&self) -> &SharedWriter<SplitSink<Client<S>>>;
    fn get_heartbeat_agent(&self) -> &HeartbeatAgent<SplitSink<Client<S>>>;
    fn get_logger(&self) -> &util::Logger;

    /*
     * Execute this service.
     * More precisely, execute the WebSocket-related services.
     * Receive from WebSocket and parse the TWS protocol packets.
     * This will consume the ownership of self. Please spawn
     * the returned future on an event loop.
     */
    fn run_service<'a>(self, client: Client<S>) -> BoxFuture<'a, ()> {
        let logger = self.get_logger().clone();
        let (sink, stream) = client.split();

        // Obtain a future representing the writing tasks.
        let sink_write = self.get_writer().run(sink).map_err(clone!(logger; |e| {
            do_log!(logger, ERROR, "{:?}", e);
        }));

        // Obtain a future to do heartbeats.
        let heartbeat_work = self.get_heartbeat_agent().run().map_err(clone!(logger; |_| {
            do_log!(logger, ERROR, "Session timed out.");
        }));

        // The main task
        // 3 combined streams. Will finish once one of them finish.
        // i.e. when the connection closes, everything here should finish.
        stream
            .map_err(clone!(logger; |e| {
                do_log!(logger, ERROR, "{:?}", e);
                "session failed.".into()
            }))
            .for_each(move |msg| {
                // Process each message from the client
                // Note that this does not return a future.
                // Anything that happens for processing
                // the message and requires doing things
                // on the event loop should spawn a task
                // instead of returning it.
                // In order not to block the main WebSocket
                // stream.
                self.on_message(msg);
                Ok(()) as Result<()>
            })
            .select2(sink_write) // Execute task for the writer
            .select2(heartbeat_work) // Execute task for heartbeats
            .then(clone!(logger; |_| {
                do_log!(logger, INFO, "Session finished.");

                // Clean-up job
                // Drop all the connections
                // will be closed by the implementation of Drop
                //state.borrow_mut().remote_connections.clear();

                Ok(())
            }))
            ._box()
    }

    /*
     * Process new WebSocket packets
     */
    fn on_message(&self, msg: OwnedMessage) {
        match msg {
            // Control / Data packets can be in either Text or Binary form.
            OwnedMessage::Text(text) => self.on_packet(proto::parse_packet(&self.get_passwd(), text.as_bytes())),
            OwnedMessage::Binary(bytes) => self.on_packet(proto::parse_packet(&self.get_passwd(), &bytes)),

            // Send pong back to keep connection alive
            OwnedMessage::Ping(msg) => self.get_writer().feed(OwnedMessage::Pong(msg)),

            // Notify the heartbeat agent that a pong is received.
            OwnedMessage::Pong(_) => self.get_heartbeat_agent().set_heartbeat_received(),
            _ => ()
        };
    }

    /*
     * Process TWS protocol packets
     */
    fn on_packet(&self, packet: proto::Packet) {
        do_log!(self.get_logger(), DEBUG, "{:?}", packet);
        match packet {
            // Call corresponding event methods.
            // Implementations can override these to control event handling.
            proto::Packet::Handshake(addr) => self.on_handshake(addr),
            proto::Packet::Connect(conn_id) => self.on_connect(conn_id),
            proto::Packet::ConnectionState((conn_id, ok)) => self.on_connect_state(conn_id, ok),
            proto::Packet::Data((conn_id, data)) => self.on_data(conn_id, data),

            // Process unknown packets
            _ => self.on_unknown()
        }
    }

    /*
     * Overridable events
     */
    fn on_unknown(&self) {}
    fn on_handshake(&self, _addr: SocketAddr) {}
    fn on_connect(&self, _conn_id: &str) {}
    fn on_connect_state(&self, _conn_id: &str, _ok: bool) {}
    fn on_data(&self, _conn_id: &str, _data: &[u8]) {}
}