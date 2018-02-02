extern crate base64;
extern crate hmac;
extern crate rand;
extern crate sha2;
extern crate time;

#[macro_use]
extern crate error_chain;

mod protocol;

mod errors {
    error_chain! {
        foreign_links {
            AddrParseError(::std::net::AddrParseError);
            ParseIntError(::std::num::ParseIntError);
        }
    }
}

fn main() {
    println!("Hello, world!");
}
