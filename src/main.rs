extern crate base64;
extern crate bytes;
#[macro_use]
extern crate clap;
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
mod parser;

mod errors {
    error_chain! {
        foreign_links {
            IoError(::std::io::Error);
            AddrParseError(::std::net::AddrParseError);
            ParseIntError(::std::num::ParseIntError);
            Utf8Error(::std::str::Utf8Error);
            WebSocketError(::websocket::WebSocketError);
            ParseError(::websocket::client::builder::ParseError);
        }
    }
}

use clap::{App, ArgMatches};
use protocol::server::TwsServer;
use protocol::client::TwsClient;
use protocol::util::{BoxFuture, LogLevel};
use tokio_core::reactor::{Core, Handle};

#[allow(unreachable_code)]
fn main() {
    let mut core = Core::new().unwrap();

    // Load cli argument definitions
    // TODO: Support appending options from config file
    let cli_def = load_yaml!("cli.yaml");
    let mut app = App::from_yaml(cli_def);
    let matches = app.clone().get_matches();

    // Get task based on subcommand
    let task = {
        if let Some(subapp) = matches.subcommand_matches("server") {
            server(core.handle(), subapp)
        } else if let Some(subapp) = matches.subcommand_matches("client") {
            client(core.handle(), subapp)
        } else {
            // No subcommand provided, print help and exit
            app.print_help().unwrap();
            std::process::exit(1);
            unreachable!();
        }
    };
    core.run(task).unwrap();
}

fn server(handle: Handle, matches: &ArgMatches) -> BoxFuture<'static, ()> {
    let mut server = TwsServer::new(handle, matches.into());
    server.on_log(logger);
    server.run()
}

fn client(handle: Handle, matches: &ArgMatches) -> BoxFuture<'static, ()> {
    let mut client = TwsClient::new(handle, matches.into());
    client.on_log(logger);
    client.run()
}

// TODO: Support specifying log level from cli
fn logger(level: LogLevel, message: &str) {
    println!("{:?}: {}", level, message);
}
