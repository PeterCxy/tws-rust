extern crate base64;
extern crate bytes;
#[macro_use]
extern crate clap;
extern crate futures;
extern crate hmac;
extern crate rand;
extern crate sha2;
extern crate time;
extern crate tokio;
extern crate tokio_io;
extern crate tokio_timer;
extern crate tokio_executor;
extern crate tokio_codec;
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
use futures::future::{Either, Future};
use protocol::server::TwsServer;
use protocol::client::TwsClient;
use protocol::util::LogLevel;
use tokio::executor::current_thread;
use tokio_timer::timer::{Handle, Timer};
use std::thread;

fn main() {
    // Set up tokio_timer environment first
    let mut timer = Timer::default();
    let handle = timer.handle();
    
    // Execute the program in a new thread.
    thread::spawn(move || {
        main_thread(&handle)
    });

    // Current thread will be used as the driver for the timer
    #[allow(while_true)]
    while true {
        if timer.turn(None).is_err() {
            break;
        }
    }
}

#[allow(unreachable_code)]
fn main_thread(handle: &Handle) {
    // Load cli argument definitions
    // TODO: Support appending options from config file
    let cli_def = load_yaml!("cli.yaml");
    let mut app = App::from_yaml(cli_def);
    let matches = app.clone().get_matches();

    let mut ent = tokio_executor::enter().unwrap();

    tokio_timer::with_default(handle, &mut ent, |ent| {
        // Get task based on subcommand
        let task = {
            if let Some(subapp) = matches.subcommand_matches("server") {
                Either::A(server(subapp))
            } else if let Some(subapp) = matches.subcommand_matches("client") {
                Either::B(client(subapp))
            } else {
                // No subcommand provided, print help and exit
                app.print_help().unwrap();
                std::process::exit(1);
                unreachable!();
            }
        };

        current_thread::CurrentThread::new().enter(ent).block_on(task).unwrap();
    });
}

fn server(matches: &ArgMatches) -> impl Future<Error=errors::Error, Item=()> {
    let mut server = TwsServer::new(matches.into());
    server.on_log(logger);
    server.run()
}

fn client(matches: &ArgMatches) -> impl Future<Error=errors::Error, Item=()> {
    let mut client = TwsClient::new(matches.into());
    client.on_log(logger);
    client.run()
}

// TODO: Support specifying log level from cli
fn logger(level: LogLevel, message: &str) {
    println!("{:?}: {}", level, message);
}
