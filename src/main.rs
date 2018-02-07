extern crate base64;
extern crate futures;
extern crate hmac;
extern crate rand;
extern crate sha2;
extern crate time;
extern crate tokio_core;
extern crate websocket;

#[macro_use]
extern crate error_chain;

mod protocol;

mod errors {
    error_chain! {
        foreign_links {
            AddrParseError(::std::net::AddrParseError);
            ParseIntError(::std::num::ParseIntError);
            Utf8Error(::std::str::Utf8Error);
            WebSocketError(::websocket::WebSocketError);
        }
    }
}

use protocol::server::{TwsServerOption, TwsServer};
use tokio_core::reactor::Core;

fn main() {
    //println!("Hello, world!");
    let mut core = Core::new().unwrap();
    let mut server = TwsServer::new(core.handle(), TwsServerOption {
        listen: "127.0.0.1:23356".parse().unwrap(),
        passwd: String::from("testpassword")
    });
    server.on_log(|l, m| {
        println!("{:?}: {:?}", l, m);
    });
    core.run(server.run()).unwrap();
}
