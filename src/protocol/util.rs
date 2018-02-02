use errors::*;
use rand::{self, Rng};
use std::net::SocketAddr;
use std::str;
use time;

/*
 * Modified version of SocketAddr stringifier
 * Omits brackets for IPv6 addresses.
 */
pub fn addr_to_str(addr: SocketAddr) -> String {
    format!("{}", addr).replace("[", "").replace("]", "")
}

/*
 * Modified version of SocketAddr parser
 * Omits brackets needed by the official SocketAddr
 * when parsing IPv6 addresses.
 * 
 * e.g.
 *   official: [fe80::dead:beef:2333]:8080
 *   our: fe80::dead:beef:2333:8080
 * 
 * we always treat the number after the last column
 * as the port when parsing an IPv6 address
 */
pub fn str_to_addr(addr: &str) -> Result<SocketAddr> {
    let mut my_addr = String::from(addr);
    if !addr.contains(".") {
        // Assume it's IPv6
        let index = addr.rfind(":").ok_or::<Error>("Illegal address: Neither IPv4 nor IPv6".into())?;
        let (host, port) = addr.split_at(index);
        my_addr = format!("[{}]:{}", host, port.replace(":", ""));
    }
    my_addr.parse()
        .chain_err(|| "Illegal address: Failed to parse")
}

pub fn time_ms() -> i64 {
    let t = time::now_utc().to_timespec();
    (t.sec as i64) * 1000 + (t.nsec as i64) / 1000 / 1000
}

/*
 * Generate a random alphanumeric string of a specified length
 */
const DICTIONARY: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ1234567890";
pub fn rand_str(len: usize) -> String {
    let mut rng = rand::thread_rng();
    let mut ret: Vec<u8> = Vec::with_capacity(len);
    for i in 0..len {
        ret[i] = DICTIONARY[rng.gen_range(0, DICTIONARY.len())];
    }
    str::from_utf8(ret.as_slice()).unwrap().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn str_to_addr_1() {
        assert_eq!("192.168.1.1:443", addr_to_str(str_to_addr("192.168.1.1:443").unwrap()));
    }

    #[test]
    fn str_to_addr_2() {
        assert_eq!("fe80::dead:beef:8080", addr_to_str(str_to_addr("fe80::dead:beef:8080").unwrap()));
    }
}