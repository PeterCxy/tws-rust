/*
 * Shared implementation details between server and client
 */
use futures::Sink;
use protocol::protocol as proto;
use protocol::util::{self, HeartbeatAgent, SharedWriter};
use websocket::OwnedMessage;
use std::fmt::Debug;
use std::net::SocketAddr;

pub trait TwsService<S: 'static + Sink<SinkItem=OwnedMessage>> {
    fn get_passwd(&self) -> &str;
    fn get_writer(&self) -> &SharedWriter<S>;
    fn get_heartbeat_agent(&self) -> &HeartbeatAgent<S>;
    fn get_logger(&self) -> &util::Logger;
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

    fn on_packet(&self, packet: proto::Packet) {
        do_log!(self.get_logger(), DEBUG, "{:?}", packet);
        match packet {
            proto::Packet::Handshake(addr) => self.on_handshake(addr),
            proto::Packet::Connect(conn_id) => self.on_connect(conn_id),
            proto::Packet::ConnectionState((conn_id, ok)) => self.on_connect_state(conn_id, ok),
            proto::Packet::Data((conn_id, data)) => self.on_data(conn_id, data),

            // Process unknown packets
            _ => self.on_unknown()
        }
    }
    fn on_unknown(&self);
    fn on_handshake(&self, addr: SocketAddr);
    fn on_connect(&self, conn_id: &str);
    fn on_connect_state(&self, conn_id: &str, ok: bool);
    fn on_data(&self, conn_id: &str, data: &[u8]);
}