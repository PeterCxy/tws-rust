/*
 * This submodule contains implementation of basic
 * elements of the TWS protocol.
 * TODO: Better documentation
 */
use errors::*;
use base64::encode;
use hmac::{Hmac, Mac};
use sha2::Sha256;

/*
 * HMAC_SHA256 authentication wrapper
 * This is used for HANDSHAKE and CONNECT packets
 */
pub fn hmac_sha256(passwd: &str, data: &str) -> Result<String> {
    Hmac::<Sha256>::new(passwd.as_bytes())
        .and_then(|mut mac| {
            mac.input(data.as_bytes());
            Ok(encode(mac.result().code().as_slice()))
        })
        .map_err(|_| "HMAC_SHA256 failed".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hmac_sha256_should_work_1() {
        assert_eq!("pOWtIY65MVjolOXjrIkpNH72V95kfBGN9zL1OJdUZOY=", hmac_sha256("testpasswd", "testdata").unwrap());
    }

    #[test]
    fn hmac_sha256_should_work_2() {
        assert_eq!("3c/Z/9/7ZqSfddwILUTheauyZe7YdDCRRtOArSRo9bc=", hmac_sha256("testpasswd2", "testdata2").unwrap());
    }
}