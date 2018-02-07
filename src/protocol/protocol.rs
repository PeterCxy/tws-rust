/*
 * This submodule contains implementation of basic
 * elements of the TWS protocol.
 * 
 * TWS protocol is a TCP forwarding protocol through
 * one or more WebSocket channels. A WebSocket channel
 * can contain multiple logical TCP connections to the
 * same remote, while a forwarded TCP connection can only
 * be attached to one WebSocket channel.
 * 
 * In this submodule, code is only concerned with basic
 * building / parsing packets.
 * 
 * TODO: Better documentation
 * TODO: Randomize packet length
 *      or try to add random meaningless
 *      packets during the session.
 */
use errors::*;
use base64::encode;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use std::net::SocketAddr;
use std::str;
use protocol::util;

/*
 * Enum to represent different parsed packets
 */
#[derive(Debug)]
pub enum Packet<'a> {
    Handshake(SocketAddr),
    Connect(&'a str),
    ConnectionState((&'a str, bool)),
    Data((&'a str, &'a [u8])),
    Unrecognized
}

/*
 * Try to parse a packet
 * If it is a recognized packet in the TWS protocol,
 * return the result in a Packet struct.
 * Otherwise, return Packet::Unrecognized.
 * For details on parsing, see below in this module.
 */
pub fn parse_packet<'a>(passwd: &str, packet: &'a [u8]) -> Packet<'a> {
    if let Ok(res) = handshake_parse(passwd, packet) {
        Packet::Handshake(res)
    } else if let Ok(res) = connect_parse(passwd, packet) {
        Packet::Connect(res)
    } else if let Ok(res) = connect_state_parse(packet) {
        Packet::ConnectionState(res)
    } else if let Ok(res) = data_parse(packet) {
        Packet::Data(res)
    } else {
        Packet::Unrecognized
    }
}

/*
 * HMAC_SHA256 authentication wrapper
 * This is used for HANDSHAKE and CONNECT packets
 */
pub fn hmac_sha256(passwd: &str, data: &str) -> Result<String> {
    Hmac::<Sha256>::new(passwd.as_bytes())
        .map(|mut mac| {
            mac.input(data.as_bytes());
            encode(mac.result().code().as_slice())
        })
        .map_err(|_| "HMAC_SHA256 failed".into())
}

/*
 * Add HMAC_SHA256 AUTH field to a text-based control packet.
 * i.e. Add a line "AUTH [code]" in front of the packet
 *      where [code] is the HMAC_SHA256 authentication code
 *      generated from the packet body.
 */
fn build_authenticated_packet(passwd: &str, msg: &str) -> Result<String> {
    hmac_sha256(passwd, msg)
        .map(|auth| format!("AUTH {}\n{}", auth, msg))
}

/*
 * Parse an HMAC_SHA256 authenticated text-based control packet.
 * i.e. Verify if the packet starts with a line "AUTH [code]"
 *      if so, verify the line against the packet body,
 *      and split the packet into lines.
 */
fn parse_authenticated_packet<'a>(passwd: &str, packet: &'a [u8]) -> Result<Vec<&'a str>> {
    if packet[0..4] != "AUTH".as_bytes()[0..4] {
        return Err("Not a proper authenticated packet.".into());
    }

    str::from_utf8(packet)
        .chain_err(|| "Illegal packet")
        .and_then(|packet_str| {
            let lines = packet_str.lines()
                .skip(1)
                .collect::<Vec<&str>>();
            packet_str.find("\n").ok_or("Not a proper packet".into())
                .map(|index| packet_str.split_at(index))
                .and_then(|(l, r)| {
                    hmac_sha256(passwd, &r[1..])
                        .map(|auth| (l, auth))
                })
                .map(|(first_line, auth)| (first_line, lines, auth))
        })
        .and_then(|(first_line, lines, auth)| {
            if first_line == format!("AUTH {}", auth) {
                Ok(lines)
            } else {
                Err("Illegal packet".into())
            }
        })
}

/* --- Control packets --- */
/*
 * All control packets are text-based,
 * transmitting control information of
 * the TWS protocol.
 * 
 * Some control packets need to be 
 * authenticated with HMAC_SHA256,
 * see above for authentication process.
 */

/*
 * Handshake packet (authenticated, including time)
 * The first control packet to send when opening
 * a new WebSocket channel. Sets the forwarding
 * destination and authenticates the client.
 * 
 * > AUTH [authentication code]
 * > NOW [current timestamp (UTC)]
 * > TARGET [targetHost]:[targetPort]
 * 
 * Packets older than 5 seconds (TODO: make this configurable)
 * should be dropped to prevent replaying.
 * (Even though this protocol should work behind TLS)
 */
pub fn handshake_build(passwd: &str, target: SocketAddr) -> Result<String> {
    _handshake_build(passwd, util::time_ms(), target)
}

fn _handshake_build(passwd: &str, time: i64, target: SocketAddr) -> Result<String> {
    build_authenticated_packet(
        passwd,
        &format!("NOW {}\nTARGET {}", time, util::addr_to_str(target))
    )
}

pub fn handshake_parse(passwd: &str, packet: &[u8]) -> Result<SocketAddr> {
    _handshake_parse(passwd, util::time_ms(), packet)
}

fn _handshake_parse(passwd: &str, time: i64, packet: &[u8]) -> Result<SocketAddr> {
    parse_authenticated_packet(passwd, packet)
        .and_then(|lines| {
            if lines.len() < 2 {
                return Err("Not a handshake packet".into());
            }
            if !(lines[0].starts_with("NOW ") && lines[0].len() > 4) {
                return Err("Not a handshake packet".into());
            }
            if !(lines[1].starts_with("TARGET ") && lines[1].len() > 7) {
                return Err("Not a handshake packet".into());
            }
            lines[0][4..].parse::<i64>()
                .chain_err(|| "Illegal handshake packet")
                .and_then(|packet_time| Ok((packet_time, lines)))
        })
        .and_then(|(packet_time, lines)| {
            if time - packet_time > 5 * 1000 {
                return Err("Protocol handshake timed out".into());
            }
            util::str_to_addr(&lines[1][7..])
                .chain_err(|| "Illegal host name")
        })
}

/*
 * Connect packet (authenticated)
 * Request to open a new logical TCP connection
 * to the remote (specified in the Handshake packet)
 * inside a WebSocket channel.
 * The server should respond with a connection state
 * packet.
 * 
 * > AUTH [authentication code]
 * > NEW CONNECTION [conn id]
 * 
 * [conn id] should be a random 6-char string
 * generated by the client side.
 * TODO: Should we make authentication for this
 *  kind of packets more strict? i.e. include time
 */
fn connect_build(passwd: &str) -> Result<(String, String)> {
    let conn_id = util::rand_str(6);
    _connect_build(passwd, &conn_id)
        .map(|packet| (conn_id, packet))
}

fn _connect_build(passwd: &str, conn_id: &str) -> Result<String> {
    build_authenticated_packet(
        passwd,
        &format!("NEW CONNECTION {}", conn_id)
    )
}

fn connect_parse<'a>(passwd: &str, packet: &'a [u8]) -> Result<&'a str> {
    parse_authenticated_packet(passwd, packet)
        .and_then(|lines| {
            if lines.len() < 1 {
                return Err("Not a Connect packet".into());
            }

            if !(lines[0].starts_with("NEW CONNECTION ") && lines[0].len() == 21) {
                return Err("Not a Connect packet".into());
            }

            Ok(&lines[0][15..])
        })
}

/*
 * Connection State Packet (or Connect-Response packet)
 * Can be sent by either the client or the server
 * to notify changes on the state of a logical
 * connection in the current WebSocket channel.
 * 
 * > CONNECTION [conn id] <OK|CLOSED>
 * 
 */
pub fn connect_state_build(conn_id: &str, ok: bool) -> String {
    format!("CONNECTION {} {}", conn_id, if ok { "OK" } else { "CLOSED" })
}

pub fn connect_state_parse<'a>(packet: &'a [u8]) -> Result<(&'a str, bool)> {
    if packet[0..10] != "CONNECTION".as_bytes()[0..10] {
        return Err("Not a Connect State packet".into());
    }

    str::from_utf8(packet)
        .chain_err(|| "Not a Connect State packet")
        .and_then(|s| {
            let arr = s.split(" ").collect::<Vec<&str>>();
            if arr.len() != 3 {
                return Err("Not a Connect State packet".into());
            }

            let conn_id = arr[1];
            if arr[2] == "OK" {
                Ok((conn_id, true))
            } else if arr[2] == "CLOSED" {
                Ok((conn_id, false))
            } else {
                Err("Not a Connect State packet".into())
            }
        })
}

/* --- Data packet --- */
/*
 * Data packet
 * Just wraps the actual data to forward
 * through a logical connection to
 * the remote / client.
 * 
 * > P[conn id][binary data]
 * 
 */
pub fn data_build(conn_id: &str, data: &[u8]) -> Vec<u8> {
    [format!("P{}", conn_id).as_bytes(), data].concat()
}

pub fn data_parse<'a>(packet: &'a [u8]) -> Result<(&'a str, &'a [u8])> {
    if packet[0] != "P".as_bytes()[0] || packet.len() < 8 {
        Err("Not a Data packet".into())
    } else {
        str::from_utf8(&packet[1..7])
            .chain_err(|| "Failed to decode conn_id")
            .map(|s| (s, &packet[7..]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hmac_sha256_1() {
        assert_eq!("pOWtIY65MVjolOXjrIkpNH72V95kfBGN9zL1OJdUZOY=", hmac_sha256("testpasswd", "testdata").unwrap());
    }

    #[test]
    fn hmac_sha256_2() {
        assert_eq!("3c/Z/9/7ZqSfddwILUTheauyZe7YdDCRRtOArSRo9bc=", hmac_sha256("testpasswd2", "testdata2").unwrap());
    }

    #[test]
    fn handshake_build_1() {
        assert_eq!(
            "AUTH s4V0i9Lwlm6eve7JftwGEgKN20mgtbSW3uacxIuh0Fo=\nNOW 1517476212983\nTARGET 192.168.1.1:443",
            _handshake_build("bscever", 1517476212983, util::str_to_addr("192.168.1.1:443").unwrap()).unwrap()
        );
    }

    #[test]
    fn handshake_build_2() {
        assert_eq!(
            "AUTH wrhyAKqrQKln+Jj9rSlpiDC1+/gw8vi5o6yIMnB5oOM=\nNOW 1517476367329\nTARGET 8.8.4.4:62311",
            _handshake_build("0o534hn045", 1517476367329, util::str_to_addr("8.8.4.4:62311").unwrap()).unwrap()
        );
    }

    #[test]
    fn handshake_build_parse_1() {
        let t = util::time_ms();
        let handshake = _handshake_build("evbie", t, util::str_to_addr("233.233.233.233:456").unwrap()).unwrap();
        assert_eq!("233.233.233.233:456", util::addr_to_str(_handshake_parse("evbie", t, handshake.as_bytes()).unwrap()));
    }

    #[test]
    fn handshake_build_parse_2() {
        let t = util::time_ms();
        let handshake = _handshake_build("43g,poe3w", t, util::str_to_addr("fe80::dead:beef:2333:8080").unwrap()).unwrap();
        assert_eq!("fe80::dead:beef:2333:8080", util::addr_to_str(_handshake_parse("43g,poe3w", t, handshake.as_bytes()).unwrap()));
    }

    #[test]
    #[should_panic]
    fn handshake_build_parse_fail_1() {
        let t = util::time_ms();
        let handshake = _handshake_build("43g,poe3w", t, util::str_to_addr("fe80::dead:beef:2333:8080").unwrap()).unwrap();
        _handshake_parse("43g,poe3", t, handshake.as_bytes()).unwrap();
    }

    #[test]
    fn connect_build_1() {
        assert_eq!(
            "AUTH +cdQQVGtyqj7KxTS5mPEwvpRGhRuctCM3pa9GsTYGZA=\nNEW CONNECTION XnjEa2",
            _connect_build("eeovgrg", "XnjEa2").unwrap()
        );
    }

    #[test]
    fn connect_parse_1() {
        assert_eq!(
            "37keeU",
            connect_parse("fneo0ivb", b"AUTH +l0yOYsTR0oqvj7//0iO24WjmdxRKNmMwVhXZpVLwvY=\nNEW CONNECTION 37keeU").unwrap()
        );
    }

    #[test]
    #[should_panic]
    fn connect_parse_fail_1() {
        connect_parse("fneo0ib", b"AUTH +l0yOYsTR0oqvj7//0iO24WjmdxRKNmMwVhXZpVLwvY=\nNEW CONNECTION 37keeU").unwrap();
    }

    #[test]
    #[should_panic]
    fn connect_parse_fail_2() {
        connect_parse("fneo0ivb", b"AUTH +l0yOYsTR0oqvj77/0iO24WjmdxRKNmMwVhXZpVLwvY=\nNEW CONNECTION 37keeU").unwrap();
    }

    #[test]
    fn connect_state_build_1() {
        assert_eq!("CONNECTION abcde OK", connect_state_build("abcde", true));
    }

    #[test]
    fn connect_state_build_2() {
        assert_eq!("CONNECTION cbdea CLOSED", connect_state_build("cbdea", false));
    }

    #[test]
    fn connect_state_parse_1() {
        assert_eq!(("abcde", true), connect_state_parse("CONNECTION abcde OK".as_bytes()).unwrap());
    }

    #[test]
    fn connect_state_parse_2() {
        assert_eq!(("cbdae", false), connect_state_parse("CONNECTION cbdae CLOSED".as_bytes()).unwrap());
    }

    #[test]
    #[should_panic]
    fn connect_state_parse_fail_1() {
        connect_state_parse("CCNNECTION cbdae CLOSED".as_bytes()).unwrap();
    }

    #[test]
    fn data_parse_1() {
        assert_eq!(("acedef", "HelloWorld".as_bytes()), data_parse("PacedefHelloWorld".as_bytes()).unwrap());
    }
}