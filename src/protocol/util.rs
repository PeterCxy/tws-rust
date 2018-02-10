use errors::*;
use futures::{Async, Stream, Sink};
use futures::future::Future;
use futures::task::{self, Task};
use rand::{self, Rng};
use std::cell::{Cell, RefCell};
use std::error;
use std::fmt::Debug;
use std::marker::PhantomData;
use std::net::SocketAddr;
use std::rc::Rc;
use std::str;
use std::time::Duration;
use time;
use tokio_timer::{Timer};
use websocket::OwnedMessage;

/*
 * Abstrasct loggers
 * We'd like to deal with logging outside
 * of this module
 */
#[derive(Debug)]
pub enum LogLevel {
    ERROR,
    WARNING,
    INFO,
    DEBUG
}
pub type Logger = Rc<Fn(LogLevel, &str)>;
pub fn default_logger(_level: LogLevel, _msg: &str) {
    // We simply do nothing by default
}

macro_rules! do_log {
    ($s: expr, $x: ident, $y: expr) => {
        ($s)(util::LogLevel::$x, $y)
    };
    ($s: expr, $x: ident, $y: expr, $($z: expr),*) => {
        ($s)(util::LogLevel::$x, &format!($y, $($z),*))
    };
}

macro_rules! clone {
    /*
     * Clone some members from a struct
     * to the corresponding local variables.
     */
    ($s:ident, $($n:ident),+) => (
        $( let $n = $s.$n.clone(); )+
    );

    /*
     * Simulate a closure that clones
     * some environment variables and
     * take ownership of them by default.
     */
    ($($n:ident),+; || $body:block) => (
        {
            $( let $n = $n.clone(); )+
            move || { $body }
        }
    );
    ($($n:ident),+; |$($p:pat),+| $body:block) => (
        {
            $( let $n = $n.clone(); )+
            move |$($p),+| { $body }
        }
    );
}

macro_rules! unwrap {
    ($t:pat, $r:ident, $d:expr) => {
        let _result = {
            match $r {
                $t => Some($r),
                x => {
                    println!("Type mismatch: expected {}, found {:?}", stringify!($t), x);
                    None
                }
            }
        };
        if let None = _result {
            return $d;
        }
        #[allow(unused_mut)]
        let mut $r = _result.unwrap();
};
}

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


// Glue code to make error-chain work with futures
// Source: <https://github.com/alexcrichton/sccache/blob/master/src/errors.rs>
// Modified to avoid static lifetimes
pub type BoxFuture<'a, T> = Box<'a + Future<Item = T, Error = Error>>;

pub trait FutureChainErr<'a, T> {
    fn chain_err<F, E>(self, callback: F) -> BoxFuture<'a, T>
        where F: FnOnce() -> E + 'a,
              E: Into<ErrorKind>;
}

impl<'a, F> FutureChainErr<'a, F::Item> for F
    where F: Future + 'a,
          F::Error: error::Error + Send + 'static,
{
    fn chain_err<C, E>(self, callback: C) -> BoxFuture<'a, F::Item>
        where C: FnOnce() -> E + 'a,
              E: Into<ErrorKind>,
    {
        self.then(|r| r.chain_err(callback))._box()
    }
}

/*
 * Convenience method to box a trait object
 * normally a Future
 */
pub trait Boxable: Sized {
    fn _box(self) -> Box<Self> {
        Box::new(self)
    }
}

impl<'a, F> Boxable for F
    where F: Future + 'a {}

macro_rules! empty_future {
    () => {
        Ok(()).into_future()._box()
    }
}

/*
 * Shared state between BufferedWriter and BufferedStream
 */
struct BufferedStreamState<I, E> {
    marker: PhantomData<E>, // Placeholder
    queue: Vec<I>, // Items to be written
    finished: bool, // Whether this stream should finish on next poll()
    task: Option<Task> // The Task driving the stream
}

/*
 * A stream that emits items from a
 * shared buffer in a RefCell
 * i.e. interior-mutable stream
 * 
 * if `finished` flag is set, the stream will end
 * on the next poll()
 * 
 * This stream is not thread-safe. Only to be used
 * in single-threaded asynchronous code.
 */
struct BufferedStream<I: Debug, E> {
    state: Rc<RefCell<BufferedStreamState<I, E>>>
}

impl<I: Debug, E> BufferedStream<I, E> {
    fn new(state: Rc<RefCell<BufferedStreamState<I, E>>>) -> BufferedStream<I, E> {
        BufferedStream {
            state
        }
    }
}

impl<I: Debug, E> Stream for BufferedStream<I, E> {
    type Item = I;
    type Error = E;

    fn poll(&mut self) -> ::std::result::Result<Async<Option<I>>, E> {
        let ref mut state = self.state.borrow_mut();

        // If the task has not been known yet
        // fetch the current task and put into the current state
        if state.task.is_none() {
            state.task = Some(task::current());
        }

        if state.finished {
            // Finished. Return None to signal the end of stream.
            return Ok(Async::Ready(None));
        } else if state.queue.len() > 0 {
            // There is item to be sent.
            // Send the first element in the queue (buffer)
            let item = state.queue.remove(0);
            return Ok(Async::Ready(Some(item)));
        } else {
            // Nothing to be sent, but the stream
            // is not marked as finished yet.
            return Ok(Async::NotReady);
        }
    }
}

/*
 * A writer that writes to a Sink
 * but does not flush() or wait for finishing
 * using BufferedStream.
 * 
 * This is a workaround because Sink::send()
 * will consume ownership and will do flush()
 * each time we try to send an item.
 * 
 * The writer is cheaply clonable, allowing to
 * be shared between multiple owners.
 */
pub struct BufferedWriter<S: 'static + Sink> where S::SinkItem: Debug {
    state: Rc<RefCell<BufferedStreamState<S::SinkItem, S::SinkError>>>
}

impl<S: 'static + Sink> BufferedWriter<S> where S::SinkItem: Debug {
    pub fn new() -> BufferedWriter<S> {
        BufferedWriter {
            state: Rc::new(RefCell::new(BufferedStreamState {
                marker: PhantomData,
                queue: Vec::new(),
                finished: false,
                task: None
            }))
        }
    }

    /*
     * Start writing to a sink. This will consume the sink.
     * In order to write to the sink, use the `feed` method
     * on this writer subsequent to this call.
     * 
     * Returns a Future that finishes when closing.
     * Please consume the Future by joining with the
     * reading part of the sink.
     */
    pub fn run(&self, sink: S) -> Box<Future<Item=(), Error=S::SinkError>> {
        let state = self.state.clone();
        let stream = BufferedStream::new(state);
        Box::new(stream.forward(sink)
            .map(|_| ()))
    }

    /*
     * Feed a new item into the sink
     */
    pub fn feed(&self, item: S::SinkItem) {
        let mut state = self.state.borrow_mut();
        if !state.finished {
            state.queue.push(item);
            Self::notify_task(&state.task);
        }
    }

    /*
     * Mark as finished.
     */
    pub fn close(&self) {
        let mut state = self.state.borrow_mut();
        if !state.finished {
            state.finished = true;
            Self::notify_task(&state.task);
        }
    }

    /*
     * Convenience method to notify a task in Option<Task>
     * The task must be notified if a stream has something
     * new to send or if the stream is finished.
     * Failing to do so will cause the stream not being polled
     * any more.
     */
    fn notify_task(task: &Option<Task>) {
        if let Some(ref task) = *task {
            task.notify();
        }
    }
}

/*
 * Destructor implementation of BufferedWriter
 * This should be customized because BufferedWriter
 * itself is clonable.
 */
impl<S: 'static + Sink> Drop for BufferedWriter<S> where S::SinkItem: Debug {
    fn drop(&mut self) {
        // The state is shared between at least
        // one BufferedStream and one BufferedWriter
        // Therefore, when the reference count is
        // less than 2, we can be sure that this
        // will be the last BufferedWriter alive,
        // and thus we can safely release the resource.
        if Rc::strong_count(&self.state) <= 2 {
            self.close();
        }
    }
}

/*
 * BufferedWriter is clonable by just cloning the state
 * allowing multiple ownership without two levels of `Rc`s
 */
impl<S: 'static + Sink> Clone for BufferedWriter<S> where S::SinkItem: Debug {
    fn clone(&self) -> BufferedWriter<S> {
        BufferedWriter {
            state: self.state.clone()
        }
    }
}

/*
 * An EventEmitter model similar to that
 * in node.js. It is an object that can
 * be subscribed to and can accept
 * emitted events from outside.
 */
pub struct EventEmitter<T, U> where T: Eq {
    subscribers: Vec<(T, Box<Fn(&U)>)>
}

impl<T, U> EventEmitter<T, U> where T: Eq {
    pub fn new() -> EventEmitter<T, U> {
        EventEmitter {
            subscribers: Vec::new()
        }
    }

    pub fn subscribe<F>(&mut self, ev_type: T, f: F) where F: 'static + Fn(&U) {
        self.subscribers.push((ev_type, Box::new(f)));
    }

    pub fn emit(&self, ev_type: T, ev: U) {
        for &(ref t, ref f) in &self.subscribers {
            if *t == ev_type {
                f(&ev)
            }
        }
    }
}

pub type RcEventEmitter<T, U> = Rc<RefCell<EventEmitter<T, U>>>;

pub fn new_emitter<T: Eq, U>() -> RcEventEmitter<T, U> {
    Rc::new(RefCell::new(EventEmitter::new()))
}

/*
 * Model of objects that generate events
 * and emit them using an EventEmitter.
 * This needs Rc because the events might
 * be emitted from multiple spots.
 */
pub trait EventSource<T, U> where T: Eq {
    fn get_event_emitter(&self) -> RcEventEmitter<T, U>;

    fn subscribe<F>(&self, ev_type: T, f: F) where F: 'static + Fn(&U) {
        let emitter = self.get_event_emitter();
        emitter.borrow_mut().subscribe(ev_type, f);
    }
}

/*
 * Shared heartbeat logic for WebSocket sessions.
 * Takes the BufferedWriter for the session,
 * runs a Futures loop that can be joint to the main
 * work using `select` combinator.
 */
pub struct HeartbeatAgent<S> where S: 'static + Sink<SinkItem=OwnedMessage> {
    timeout: u64, // Milliseconds
    writer: BufferedWriter<S>,
    heartbeat_received: Rc<Cell<bool>>
}

impl<S> HeartbeatAgent<S> where S: 'static + Sink<SinkItem=OwnedMessage> {
    pub fn new(timeout: u64, writer: BufferedWriter<S>) -> HeartbeatAgent<S> {
        HeartbeatAgent {
            timeout,
            writer,
            heartbeat_received: Rc::new(Cell::new(true))
        }
    }

    /*
     * Set the flag that we have now received the heartbeat message
     */
    pub fn set_heartbeat_received(&self) {
        self.heartbeat_received.set(true);
    }

    /*
     * Returns a future that sends a heartbeat
     * for every interval of `timeout`.
     * 
     * If the next interval has passed but the 
     * heartbeat_received flag is still `false`,
     * then it considers the session to be timed
     * out and throws an error, which will then
     * propagate to the parent Futrue, causing
     * the connection to be teared down.
     * 
     * Please always use a Select combinator
     * to run this, instead of a Join combinator.
     */
    pub fn run<'a>(&self) -> BoxFuture<'a, ()> {
        let writer = self.writer.clone();
        let heartbeat_received = self.heartbeat_received.clone();
        Timer::default().interval(Duration::from_millis(self.timeout))
            .map_err(|_| "Unknown error".into())
            .for_each(move |_| {
                if !heartbeat_received.get() {
                    // Close if no Pong is received within
                    // a timeout period.
                    writer.close();
                    return Err("Timed out".into());
                }

                // Send Ping message
                writer.feed(OwnedMessage::Ping(vec![]));
                heartbeat_received.set(false);
                Ok(())
            })
            ._box()
    }
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