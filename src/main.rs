extern crate base64;
extern crate bytes;
extern crate futures;
extern crate hmac;
extern crate rand;
extern crate sha2;
extern crate time;
extern crate tokio_core;
extern crate tokio_io;
extern crate tokio_timer;
extern crate websocket;

#[macro_use]
extern crate error_chain;

mod protocol;

mod errors {
    error_chain! {
        foreign_links {
            IoError(::std::io::Error);
            AddrParseError(::std::net::AddrParseError);
            ParseIntError(::std::num::ParseIntError);
            Utf8Error(::std::str::Utf8Error);
            WebSocketError(::websocket::WebSocketError);
        }
    }
}

use protocol::server::{TwsServerOption, TwsServer};
use std::env;
use tokio_core::reactor::Core;

fn main() {
    match &env::args().nth(1).expect("Argument needed")[..] {
        "server" => test_server(),
        _ => println!("Unkown argument")
    }
}

// TEMPORARY TEST CODE FOR SERVER
fn test_server() {
    //println!("Hello, world!");
    let mut core = Core::new().unwrap();
    let mut server = TwsServer::new(core.handle(), TwsServerOption {
        listen: "127.0.0.1:23356".parse().unwrap(),
        passwd: String::from("testpassword"),
        timeout: 1000
    });
    server.on_log(|l, m| {
        println!("{:?}: {:?}", l, m);
    });
    core.run(server.run()).unwrap();
}
