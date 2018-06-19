use clap::ArgMatches;
use protocol::server::TwsServerOption;
use protocol::client::TwsClientOption;
use serde::Deserialize;
use serde_yaml;
use std::convert::TryFrom;
use std::fs::File;
use std::io::Read;

impl<'a> TryFrom<&'a ArgMatches<'a>> for TwsClientOption {
    type Error = String;
    fn try_from(args: &'a ArgMatches<'a>) -> Result<TwsClientOption, String> {
        let conf = load_config_file(args);
        if conf.is_some() {
            return Ok(conf.unwrap());
        }
        Ok(TwsClientOption {
            connections: args.value_of("connections")
                .ok_or("WTF")?
                .parse()
                .map_err(|_| "Invalid value of `connections`. Must be a number")?,
            listen: args.value_of("listen")
                .ok_or("`listen` is required")?
                .parse()
                .map_err(|_| "Invalid value of `listen`. Must be an address.")?,
            server: args.value_of("server")
                .ok_or("`server` is required")?.to_string(),
            remote: args.value_of("remote")
                .ok_or("`remote` is required")?
                .parse()
                .map_err(|_| "Invalid value of `remote`. Must be an address.")?,
            passwd: args.value_of("passwd")
                .ok_or("`passwd` is required")?.to_string(),
            timeout: args.value_of("timeout")
                .ok_or("WTF")?
                .parse()
                .map_err(|_| "Invalid value of `timeout`. Must be a number")?,
            retry_timeout: args.value_of("retry_timeout")
                .ok_or("WTF")?
                .parse()
                .map_err(|_| "Invalid value of `retry_timeout`. Must be a number")?
        })
    }
}

impl<'a> TryFrom<&'a ArgMatches<'a>> for TwsServerOption {
    type Error = String;
    fn try_from(args: &'a ArgMatches<'a>) -> Result<TwsServerOption, String> {
        let conf = load_config_file(args);
        if conf.is_some() {
            return Ok(conf.unwrap());
        }
        Ok(TwsServerOption {
            listen: args.value_of("listen")
                .ok_or("`listen` is required")?
                .parse()
                .map_err(|_| "Invalid value of `listen`. Must be an address.")?,
            passwd: args.value_of("passwd")
                .ok_or("`passwd` is required")?.to_string(),
            timeout: args.value_of("timeout")
                .ok_or("WTF")?
                .parse()
                .map_err(|_| "Invalid value of `timeout`. Must be a number")?,
        })
    }
}

fn load_config_file<'a, T>(args: &'a ArgMatches<'a>) -> Option<T>
    where for<'de> T: Deserialize<'de> {
    args.value_of("config")
        .and_then(|config_file| {
            let mut buf = String::new();
            File::open(config_file).expect("Config file not found")
                .read_to_string(&mut buf).expect("Cannot read config file");
            serde_yaml::from_str(&buf).expect("Invalid config file")
        })
}