use clap::ArgMatches;
use protocol::server::TwsServerOption;
use protocol::client::TwsClientOption;

impl<'a> From<&'a ArgMatches<'a>> for TwsClientOption {
    fn from(args: &'a ArgMatches<'a>) -> TwsClientOption {
        TwsClientOption {
            connections: args.value_of("connections").unwrap().parse().unwrap(),
            listen: args.value_of("listen").unwrap().parse().unwrap(),
            server: args.value_of("server").unwrap().to_string(),
            remote: args.value_of("remote").unwrap().parse().unwrap(),
            passwd: args.value_of("passwd").unwrap().to_string(),
            timeout: args.value_of("timeout").unwrap().parse().unwrap(),
            retry_timeout: args.value_of("retry_timeout").unwrap().parse().unwrap()
        }
    }
}

impl<'a> From<&'a ArgMatches<'a>> for TwsServerOption {
    fn from(args: &'a ArgMatches<'a>) -> TwsServerOption {
        TwsServerOption {
            listen: args.value_of("listen").unwrap().parse().unwrap(),
            passwd: args.value_of("passwd").unwrap().to_string(),
            timeout: args.value_of("timeout").unwrap().parse().unwrap(),
        }
    }
}