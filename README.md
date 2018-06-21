tws-rust
===

`tws-rust` is a fast TCP tunnel over Websocket with multiplexing support, written in blazing fast systems programming language Rust. `tws-rust` can be used to tunnel any TCP connection through a Websocket channel, which is a message protocol based on HTTP. This is mainly useful for bypassing firewalls that allow only 80 / 443 ports, though combined with `HTTPS` it can also help secure your connection. You should always use `tws-rust` behind `HTTPS` even if you are just a little concerned about security.

**Please carefully read this document before actually using this program!**

**Disclaimer: This project was built with the intention of personal use, and was released in the hope that it would help others. There is ABSOLUTELY NO WARRANTY on this project, to the extent permitted by applicable laws. You should evaluate for yourself before using this project in production.**

Features
===

- Easy to use
- Performant
- [Multiplexing](https://en.wikipedia.org/wiki/Multiplexing): To avoid the overhead of HTTP(S) requests, `tws-rust` generates a fixed (configurable) number of Websocket connections, on which it can create logical channels to carry **any** number of TCP connections (M:N multiplexing).
- SSL/TLS through reverse proxy: Though the server side of `tws-rust` itself does not handle SSL/TLS, it is always recommended to place a SSL-capable reverse proxy (like Nginx) in front of `tws-rust` for maximum security. The client side supports SSL handling out of the box.
- Self-contained binaries: we provide statically-linked self-contained binaries for releases.
- Written in Rust!

Installation
===

You could download pre-built binaries from the [Releases](https://git.angry.im/PeterCxy/tws-rust/releases) page. Or, you can build a release yourself by cloning this repository and

```bash
cargo build --release
```

You will need Rust **Nightly** and Cargo to build this program. However, the binaries produced by such a command will not be self-contained: it will be linked against the system `libc` and `OpenSSL`.

To build a self-contained binary release, `musl`, `kernel-headers-musl` and `upx` (package names for ArchLinux) must be installed. You should also check if `musl-gcc` is available. The Rust target `x86_64-unknown-linux-musl` must also be installed via `rustup`. Then, simply run

```bash
./build-static.sh
```

and you will find the self-contained binary in the `release/` folder. The script will download `OpenSSL` and compile it statically, then build `tws-rust` and link all the things statically. It will also `strip` the binary and use `upx` to compress the final output.

Usage
===

Server:

```
tws-rust-server
Run in server mode

USAGE:
    tws-rust server [FLAGS] [OPTIONS]

FLAGS:
    -h, --help       Prints help information
    -V, --version    Prints version information
    -v               Verbose logging

OPTIONS:
        --config <FILE_NAME>    Load a configuration file in yaml format that overrides all the command-line options
    -l, --listen <ADDR>         Address to listen on (e.g. 127.0.0.1:8080)
    -p, --passwd <SECRET>       Shared password with the client
    -t, --timeout <TIMEOUT>     Time in milliseconds before considering inactive clients as disconnected [default: 5000]
```

Client:

```
tws-rust-client
Run in client mode

USAGE:
    tws-rust client [FLAGS] [OPTIONS]

FLAGS:
    -h, --help       Prints help information
    -V, --version    Prints version information
    -v               Verbose logging

OPTIONS:
        --config <FILE_NAME>         Load a configuration file in yaml format that overrides all the command-line
                                     options
    -c, --connections <NUM>          Number of concurrent WebSocket connections to maintain [default: 2]
    -l, --listen <ADDR>              Address to listen on (e.g. 127.0.0.1:8080)
    -p, --passwd <SECRET>            Shared password with the server
    -r, --remote <ADDR>              Address of the target host to forward connections to (e.g. 4.5.6.7:3000)
    -e, --retry_timeout <TIMEOUT>    Time in milliseconds in which interrupted sessions will retry [default: 5000]
    -s, --server <URL>               URL of TWS server (e.g. wss://example.com/my_tws_server)
    -t, --timeout <TIMEOUT>          Time in milliseconds before considering the server as disconnected [default: 5000]
```

Note that due to an early design issue, it is actually the **client** of `tws-rust` that could decide which address to forward the connections to (the `--remote` option of `tws-rust client`), not the server side, as opposed to other TCP tunnels.

You can also use `--config` to pass a configuration file instead of passing everything through command line. You can find the examples in the `example/` folder.

**Please note: if you use the self-contained version of tws-rust binary, you have to set the following environment variable correctly:**

Due to the configuration of the statically linked OpenSSL, you must set `SSL_CERT_DIR` to your system's certificate store, on ArchLinux it is `/etc/ssl/certs`.

On CentOS 7, `SSL_CERT_FILE=/etc/pki/tls/cert.pem` should be set instead. I'm not sure exactly why this would happen.

Reverse Proxy with Nginx
===

As mentioned before, `tws-rust` **must** be run behind a SSL-capable reverse proxy to ensure security. This can be done by Nginx: you could just add some configuration like below to any of your Nginx `server` blocks with SSL enabled.

```nginx
location /some/path {
  if ($http_upgrade !~* ^WebSocket$) {
    return 404;
  }
  proxy_pass http://127.0.0.1:TWS_PORT/;
  proxy_http_version 1.1;
  proxy_set_header Upgrade $http_upgrade;
  proxy_set_header Connection "Upgrade";
}
```

In this case, the `tws-rust` must be listening on some port on `127.0.0.1` instead of your public IP. After configuring a reverse proxy, simply point the `--server` option of your client to `wss://your_domain/some/path` (note the `wss://` instead of `ws://`).

Systemd
===

Systemd example is under `example` directory. You can copy `tws-rust` `tws-server@service` `tws-client@.service` and the `.yaml` configuration files to the follow paths:

```shell
.
├── etc
│   ├── systemd
│   │   └── system
│   │       ├── tws-server@.service
│   │       └── tws-client@.service
│   └── tws
│       ├── client.yaml
│       └── server.yaml
└── usr
    └── bin
        └── tws-rust
```

Start `tws-rust` server service

```shell
systemctl enable tws-server@server
systemctl start tws-server@server
```

Start `tws-rust` client service

```shell
systemctl enable tws-client@client
systemctl start tws-client@client
```

You can change the part after `@` for your custom configuration file names.

Benchmark
===

The following benchmarks are run on `lo` on an XPS15-9550 with Intel i7-6700HQ. The connection was not behind SSL. Since the implementation of the client-side SSL completely depends on `OpenSSL`, the performance of SSL-enabled connections are mostly limited by the performance of `OpenSSL` on your hardware, and `OpenSSL` can be benchmarked separately from this program.

Server command:

```bash
tws-rust server --config example/server.yaml
```

Client command:

```bash
tws-rust client --config example/client.yaml
```

I have a `iperf3 -s` running on the default port `5201` and the `iperf3 -c` connects to the client endpoint of `tws-rust` on port `5203`.

```
$ iperf3 -c 127.0.0.1 -p 5203 -R
Connecting to host 127.0.0.1, port 5203
Reverse mode, remote host 127.0.0.1 is sending
[  5] local 127.0.0.1 port 58660 connected to 127.0.0.1 port 5203
[ ID] Interval           Transfer     Bitrate
[  5]   0.00-1.00   sec   867 MBytes  7.27 Gbits/sec
[  5]   1.00-2.00   sec   896 MBytes  7.52 Gbits/sec
[  5]   2.00-3.00   sec   876 MBytes  7.35 Gbits/sec
[  5]   3.00-4.00   sec   895 MBytes  7.51 Gbits/sec
[  5]   4.00-5.00   sec   846 MBytes  7.10 Gbits/sec
[  5]   5.00-6.00   sec   883 MBytes  7.41 Gbits/sec
[  5]   6.00-7.00   sec   886 MBytes  7.43 Gbits/sec
[  5]   7.00-8.00   sec   845 MBytes  7.09 Gbits/sec
[  5]   8.00-9.00   sec   852 MBytes  7.15 Gbits/sec
[  5]   9.00-10.00  sec   886 MBytes  7.43 Gbits/sec
- - - - - - - - - - - - - - - - - - - - - - - - -
[ ID] Interval           Transfer     Bitrate         Retr
[  5]   0.00-10.04  sec  8.54 GBytes  7.31 Gbits/sec    0             sender
[  5]   0.00-10.00  sec  8.53 GBytes  7.33 Gbits/sec                  receiver

iperf Done.
```

```
$ iperf3 -c 127.0.0.1 -p 5203
Connecting to host 127.0.0.1, port 5203
[  5] local 127.0.0.1 port 51076 connected to 127.0.0.1 port 5203
[ ID] Interval           Transfer     Bitrate         Retr  Cwnd
[  5]   0.00-1.00   sec   830 MBytes  6.96 Gbits/sec    0    639 KBytes
[  5]   1.00-2.00   sec   835 MBytes  7.00 Gbits/sec    0   1023 KBytes
[  5]   2.00-3.00   sec   851 MBytes  7.14 Gbits/sec    0   1023 KBytes
[  5]   3.00-4.00   sec   822 MBytes  6.90 Gbits/sec    0   1.06 MBytes
[  5]   4.00-5.00   sec   814 MBytes  6.83 Gbits/sec    0   1.06 MBytes
[  5]   5.00-6.00   sec   826 MBytes  6.93 Gbits/sec    0   1.06 MBytes
[  5]   6.00-7.00   sec   836 MBytes  7.01 Gbits/sec    0   1.25 MBytes
[  5]   7.00-8.00   sec   834 MBytes  6.99 Gbits/sec    0   1.25 MBytes
[  5]   8.00-9.00   sec   830 MBytes  6.96 Gbits/sec    0   1.25 MBytes
[  5]   9.00-10.00  sec   818 MBytes  6.86 Gbits/sec    0   1.25 MBytes
- - - - - - - - - - - - - - - - - - - - - - - - -
[ ID] Interval           Transfer     Bitrate         Retr
[  5]   0.00-10.00  sec  8.10 GBytes  6.96 Gbits/sec    0             sender
[  5]   0.00-10.05  sec  8.09 GBytes  6.92 Gbits/sec                  receiver

iperf Done.
```

This is much faster than [chisel](https://github.com/jpillora/chisel), which is also a multiplexed TCP over Websocket implementation. Their README stated a performance of ~100MB/s which is ~800Mbps. Note that the main logic of `tws-rust` only runs on a **single** thread, in contrast to `goroutine` which can be scheduled on multiple OS threads. The performance of `tws-rust` could be improved on multi-core systems by introducing multi-threading, but I decided that it was just unnecessary since I don't have an uplink of over 1Gbps anyway :)

Since the `chisel` implementation used SSH by default to encrypt the packets, I have also done a test on the loopback interface of my Vultr instance with HTTPS enabled by Nginx:

```
Connecting to host 127.0.0.1, port 5203
Reverse mode, remote host 127.0.0.1 is sending
[  5] local 127.0.0.1 port 42662 connected to 127.0.0.1 port 5203
[ ID] Interval           Transfer     Bitrate
[  5]   0.00-1.00   sec   123 MBytes  1.03 Gbits/sec
[  5]   1.00-2.00   sec   132 MBytes  1.10 Gbits/sec
[  5]   2.00-3.00   sec   124 MBytes  1.04 Gbits/sec
[  5]   3.00-4.00   sec   129 MBytes  1.08 Gbits/sec
[  5]   4.00-5.00   sec   125 MBytes  1.05 Gbits/sec
[  5]   5.00-6.00   sec   126 MBytes  1.06 Gbits/sec
[  5]   6.00-7.00   sec   129 MBytes  1.08 Gbits/sec
[  5]   7.00-8.00   sec   130 MBytes  1.09 Gbits/sec
[  5]   8.00-9.00   sec   132 MBytes  1.11 Gbits/sec
[  5]   9.00-10.00  sec   126 MBytes  1.05 Gbits/sec
- - - - - - - - - - - - - - - - - - - - - - - - -
[ ID] Interval           Transfer     Bitrate         Retr
[  5]   0.00-10.11  sec  1.27 GBytes  1.08 Gbits/sec    0             sender
[  5]   0.00-10.00  sec  1.25 GBytes  1.07 Gbits/sec                  receiver

iperf Done.
```

Still faster than `chisel`: and in this test, `tws-rust` was only able to eat up 45% of the CPU because my Vultr instance was single core and it has to share with the server side and the Nginx.
