/*
 * This module provides a wrapper over the UDP interface
 * of Tokio that (to some degree) resembles the TCP counterpart
 */
use futures::{Async, Poll, Stream};
use futures::task::{self, Task};
use tokio::net::UdpSocket;
use tokio::io as iot;
use std::cell::RefCell;
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::rc::Rc;
use std::io;
use std::time::{Duration, Instant};

macro_rules! try_ready {
    ($x:expr) => (
        {
            let res = $x;
            match res {
                Err(e) => return Err(e),
                Ok(res) => match res {
                    Async::NotReady => return Ok(Async::NotReady),
                    Async::Ready(item) => item
                }
            }
        }
    );
}

const UDP_QUEUE_LEN: usize = 4;

struct UdpState {
    remote: SocketAddr,
    buf: Vec<Vec<u8>>,
    err: Option<io::Error>, // unused for now
    task: Option<Task>,
    timeout: Duration,
    last_activity: Instant
}

impl UdpState {
    fn new(remote: &SocketAddr, timeout: Duration) -> UdpState {
        UdpState {
            remote: *remote,
            buf: Vec::with_capacity(UDP_QUEUE_LEN),
            err: None,
            task: None,
            timeout,
            last_activity: Instant::now()
        }
    }
}

type SharedUdpState = Rc<RefCell<UdpState>>;

impl UdpState {
    fn write(&mut self, buf: Vec<u8>) {
        self.last_activity = Instant::now();
        if self.buf.len() <= UDP_QUEUE_LEN {
            self.buf.push(buf);
        }
    }

    fn notify(&mut self) {
        if let Some(ref t) = self.task {
            t.notify();
        }
    }
}

/*
 * A stream of UDP messages
 * that come from the same client (SocketAddr)
 * This will close once both sides haven't communicated
 * for `timeout`.
 * This stream would not emit any error for now.
 */
pub struct UdpStream {
    state: SharedUdpState
}

impl Stream for UdpStream {
    type Item = Vec<u8>;
    type Error = io::Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        let mut state = self.state.borrow_mut();
        if state.task.is_none() {
            state.task = Some(task::current());
        }

        if state.err.is_some() {
            let err = ::std::mem::replace(&mut state.err, None);
            return Err(err.unwrap());
        } else if state.buf.len() > 0 {
            let b = state.buf.remove(0);
            return Ok(Async::Ready(Some(b)));
        } else {
            if Instant::now().duration_since(state.last_activity) >= state.timeout {
                return Ok(Async::Ready(None));
            } else {
                return Ok(Async::NotReady)
            }
        }
    }
}

/*
 * A handle to operate on a UdpDatagram
 * mainly sending packets and notifying for polling
 */
pub struct UdpDatagramHandle {
    buf: Vec<(Option<SocketAddr>, Vec<u8>)>,
    task: Option<Task>,
    paused: bool, // On paused, new packets are simply dropped
}

impl UdpDatagramHandle {
    /*
     * Send a packet, through the UDP socket, to a target address.
     * If the current send queue is full, this function will not
     * block, but instead, the packet is dropped.
     * This only puts the packet into the queue if possible.
     * The underlying UdpDatagram must be polled for this to work.
     * This is used when the socket is a "server"
     */
    pub fn send_to(&mut self, target: &SocketAddr, data: &[u8]) {
        if self.buf.len() <= UDP_QUEUE_LEN {
            self.buf.push((Some(*target), Vec::from(data)));
            self.notify();
        }
    }

    /*
     * Send a packet, through the UDP socket, to the default target.
     * If the current send queue is full, this function will not
     * block, but instead, the packet is dropped.
     * This only puts the packet into the queue if possible.
     * The underlying UdpDatagram must be polled for this to work.
     * This is used when the socket is a "client"
     */
    pub fn send(&mut self, data: &[u8]) {
        if self.buf.len() <= UDP_QUEUE_LEN {
            self.buf.push((None, Vec::from(data)));
            self.notify();
        }
    }

    /*
     * Set the socket as paused
     * when paused, all packet will be droppped.
     */
    pub fn pause(&mut self) {
        self.paused = true;
    }

    /*
     * Undo what `pause()` does.
     */
    pub fn resume(&mut self) {
        self.paused = false;
    }

    /*
     * Allow the task to be polled on the next tick
     * This is used to ensure that the socket is polled
     * at least every `timeout` in order to remove all
     * idle connections.
     */
    pub fn notify(&self) {
        if let Some(ref task) = self.task {
            task.notify();
        }
    }
}

pub type SharedUdpHandle = Rc<RefCell<UdpDatagramHandle>>;

/*
 * A wrapper over UdpSocket
 * It is a stream that emits (SocketAddr, UdpStream) pairs
 * each time a unique new client sends a packet to a server.
 * All the subsequent packets are emitted through the
 * UdpStream object.
 * This is used as a shorthand for mapping UDP clients
 * to TWS logical connections.
 */
pub struct UdpDatagram {
    socket: UdpSocket,
    conn_states: HashMap<usize, SharedUdpState>,
    conn_states_reverse_map: HashMap<SocketAddr, usize>,
    handle: SharedUdpHandle,
    counter: usize,
    read_buf: Vec<u8>,
    // If the UdpDatagram is a client, this should be set to true
    // Because a client-mode UDP socket can only receive from one
    // address and if that closes, everything is done
    close_if_no_conn: bool,
    // The maximum idle interval allowed
    timeout: Duration
}

impl UdpDatagram {
    pub fn wrap_socket(socket: UdpSocket, timeout: Duration, close_if_no_conn: bool) -> UdpDatagram {
        UdpDatagram {
            socket,
            conn_states: HashMap::new(),
            conn_states_reverse_map: HashMap::new(),
            counter: 0,
            close_if_no_conn,
            timeout,
            read_buf: vec![0; 65535],
            handle: Rc::new(RefCell::new(UdpDatagramHandle {
                buf: Vec::with_capacity(UDP_QUEUE_LEN),
                paused: false,
                task: None
            }))
        }
    }

    /*
     * Obtain a handle to operate on this socket
     * after being consumed by the event loop
     */
    pub fn get_handle(&self) -> SharedUdpHandle {
        self.handle.clone()
    }

    pub fn bind(addr: &SocketAddr, timeout: Duration) -> Result<UdpDatagram, iot::Error> {
        UdpSocket::bind(addr)
            .map(|s| Self::wrap_socket(s, timeout, false))
    }

    pub fn connect(addr: &SocketAddr, timeout: Duration) -> Result<(UdpDatagram, UdpStream), iot::Error> {
        UdpSocket::bind(&build_unspecified_addr(addr))
            .and_then(|s| s.connect(addr).map(|_| s))
            .map(|s| {
                let mut socket = Self::wrap_socket(s, timeout, true);
                let state = Rc::new(RefCell::new(UdpState::new(addr, timeout)));
                socket.add_conn(state.clone());
                (socket, UdpStream {
                    state
                })
            })
    }

    fn add_conn(&mut self, conn: SharedUdpState) {
        let addr = conn.borrow().remote.clone();
        self.conn_states.insert(self.counter, conn);
        self.conn_states_reverse_map.insert(addr, self.counter);
        if self.counter >= ::std::usize::MAX {
            self.counter = 0;
        } else {
            self.counter += 1;
        }
    }

    fn remove_conn(&mut self, id: usize) {
        let conn = self.conn_states.remove(&id);
        if let Some(conn) = conn {
            // Poll the connection and let the connection to close itself
            conn.borrow_mut().notify();
            self.conn_states_reverse_map.remove(&conn.borrow().remote);
        }
    }
}

fn build_unspecified_addr(addr: &SocketAddr) -> SocketAddr {
    if addr.is_ipv4() {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)), 0)
    } else {
        SocketAddr::new(IpAddr::V6(Ipv6Addr::new(0, 0, 0, 0, 0, 0, 0, 0)), 0)
    }
}

impl Stream for UdpDatagram {
    type Item = (SocketAddr, UdpStream);
    type Error = io::Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        // Write something if possible
        {
            let mut handle = self.handle.borrow_mut();
            if handle.task.is_none() {
                handle.task = Some(task::current());
            }
            if handle.buf.len() > 0 {
                let res = {
                    let data = &handle.buf[0];
                    if let Some(ref addr) = data.0 {
                        // Packets sent by `send_to`
                        if self.conn_states_reverse_map.contains_key(&addr) {
                            let id = self.conn_states_reverse_map[&addr];
                            if self.conn_states.contains_key(&id) {
                                self.conn_states[&id].borrow_mut().last_activity = Instant::now();
                            }
                        }
                        self.socket.poll_send_to(&data.1, &addr)
                    } else {
                        // Packets sent by `send`
                        if self.conn_states.contains_key(&0) {
                            self.conn_states[&0].borrow_mut().last_activity = Instant::now();
                        }
                        self.socket.poll_send(&data.1)
                    }
                };

                if let Ok(Async::Ready(_)) = res {
                    handle.buf.remove(0);
                }
            }
        }

        // Clear all timeout'ed connections
        let to_remove: Vec<_> = self.conn_states.iter()
            .filter(|(_, v)| Instant::now().duration_since(v.borrow().last_activity) >= self.timeout)
            .map(|(k, _)| *k)
            .collect();
        for r in to_remove {
            self.remove_conn(r);
        }

        if self.close_if_no_conn && self.conn_states.len() == 0 {
            return Ok(Async::Ready(None));
        }

        // Read something if possible
        let (size, addr) = try_ready!(self.socket.poll_recv_from(&mut self.read_buf));
        let buf = self.read_buf[0..size].to_vec();
        if self.conn_states_reverse_map.contains_key(&addr) {
            let mut state = self.conn_states[&self.conn_states_reverse_map[&addr]].borrow_mut();
            if !self.handle.borrow().paused {
                state.write(buf);
                state.notify();
            }
            task::current().notify();
            Ok(Async::NotReady)
        } else {
            let mut state = UdpState::new(&addr, self.timeout);
            state.write(buf);
            let state_rc = Rc::new(RefCell::new(state));
            self.add_conn(state_rc.clone());
            Ok(Async::Ready(Some((addr, UdpStream {
                state: state_rc
            }))))
        }
    }
}