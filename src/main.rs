extern crate base64;
extern crate hmac;
extern crate sha2;

#[macro_use]
extern crate error_chain;

mod protocol;

mod errors {
    error_chain! {
        foreign_links {
            
        }
    }
}

fn main() {
    println!("Hello, world!");
}
