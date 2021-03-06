use errors::*;
use bytes::{Bytes, BytesMut};
use futures::{Async, Stream, Sink, Poll};
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
use std::time::{Duration, Instant};
use time;
use tokio_timer;
use websocket::OwnedMessage;

/*
 * Default value functions for use with serde
 */
pub fn default_timeout() -> u64 { 5000 }
pub fn default_retry_timeout() -> u64 { 1000 }
pub fn default_connections() -> usize { 2 }
pub fn default_no_udp() -> bool { false }
pub fn default_udp_timeout() -> u64 { 60000 }

/*
 * Abstract loggers
 * We'd like to deal with logging outside
 * of this module
 */
#[derive(Debug)]
#[allow(dead_code)]
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
    for _ in 0..len {
        ret.push(DICTIONARY[rng.gen_range(0, DICTIONARY.len())]);
    }
    str::from_utf8(ret.as_slice()).unwrap().to_string()
}


// Glue code to make error-chain work with futures
// Source: <https://github.com/alexcrichton/sccache/blob/master/src/errors.rs>
// Modified to avoid static lifetimes and heap allocation
pub type BoxFuture<'a, T> = Box<dyn 'a + Future<Item = T, Error = Error>>;

pub trait FutureChainErr<'a, F: Future, T>
    where F: Future + 'a,
          F::Error: error::Error + Send + 'static {
    fn chain_err<CB, E>(self, callback: CB) -> ChainErr<F, CB, E>
        where CB: FnOnce() -> E + Clone + 'a,
              E: Into<ErrorKind>;
}

impl<'a, F> FutureChainErr<'a, F, F::Item> for F
    where F: Future + 'a,
          F::Error: error::Error + Send + 'static,
{
    fn chain_err<C, E>(self, callback: C) -> ChainErr<F, C, E>
        where C: FnOnce() -> E + Clone + 'a,
              E: Into<ErrorKind>,
    {
        ChainErr::new(self, callback)
    }
}

pub struct ChainErr<F: Future, CB, EK: Into<ErrorKind>>
    where F::Error: error::Error + Send + 'static,
          CB: FnOnce() -> EK + Clone {
    future: F,
    callback: CB
}

impl<F: Future, CB, EK: Into<ErrorKind>> ChainErr<F, CB, EK>
    where F::Error: error::Error + Send + 'static,
          CB: FnOnce() -> EK + Clone {

    fn new(future: F, callback: CB) -> ChainErr<F, CB, EK> {
        ChainErr {
            future,
            callback: callback
        }
    }

}

impl<F: Future, CB, EK: Into<ErrorKind>> Future for ChainErr<F, CB, EK>
    where F::Error: error::Error + Send + 'static,
          CB: FnOnce() -> EK + Clone {
    type Error = Error;
    type Item = F::Item;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let p = self.future.poll();
        p.chain_err(self.callback.clone())
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

/*
 * Facilities to throttle a stream
 */

/*
 * A meter of speed
 * used for throttling logical connections
 */
pub struct Speedometer {
    begin_instant: Instant,
    counter: u64
}

impl Speedometer {
    pub fn new() -> Speedometer {
        Speedometer {
            begin_instant: Instant::now(),
            counter: 0
        }
    }

    pub fn feed_counter(&mut self, count: u64) {
        let cur_instant = Instant::now();
        if cur_instant.duration_since(self.begin_instant) >= Duration::from_secs(5) {
            self.begin_instant = cur_instant;
            self.counter = 0;
        }
        self.counter += count;
    }

    pub fn speed(&self) -> u64 {
        let duration = Instant::now().duration_since(self.begin_instant).as_secs();
        if duration != 0 {
            self.counter / duration
        } else {
            0
        }
    }
}

pub type SharedSpeedometer = Rc<RefCell<Speedometer>>;

/*
 * Generic handler for throttling a stream
 */
pub trait ThrottlingHandler {
    /*
     * Pause the stream
     * do not poll until it is resumed
     * max_speed: the maximum speed of the stream
     *   used for partial pausing of the underlying connections
     *   when the WebSocket link is full
     */
    fn pause(&mut self, max_speed: u64);

    /*
     * Resume the stream
     */
    fn resume(&mut self);

    /*
     * Has the stream been paused?
     */
    fn is_paused(&self) -> bool;

    /*
     * Override this to true if this handler
     * can accept `pause` events even after pausing
     * This is desirable if the `pause` handler does not
     * try to pause all streams but only pause parts of them
     * to see if it will work
     */
    fn allow_pause_multiple_times(&self) -> bool {
        false
    }
}

/*
 * State object shared between StreamThrottler
 * and ThrottledStream
 */
struct ThrottledStreamState {
    task: Option<Task>,
    paused: bool
}

/*
 * An interface for pausing and resuming a stream
 */
pub struct StreamThrottler {
    state: Rc<RefCell<ThrottledStreamState>>
}

impl StreamThrottler {
    pub fn new() -> StreamThrottler {
        StreamThrottler {
            state: Rc::new(RefCell::new(ThrottledStreamState {
                task: None,
                paused: false
            }))
        }
    }

    /*
     * Throttle a stream using this throttler
     * it is recommended to use one throttler only with one stream
     */
    pub fn wrap_stream<S: Stream>(&self, stream: S) -> ThrottledStream<S> {
        ThrottledStream {
            stream,
            state: self.state.clone()
        }
    }
}

impl ThrottlingHandler for StreamThrottler {
    fn pause(&mut self, _max_speed: u64) {
        self.state.borrow_mut().paused = true;
    }

    fn resume(&mut self) {
        let mut state = self.state.borrow_mut();
        state.paused = false;
        notify_task(&state.task);
    }

    fn is_paused(&self) -> bool {
        self.state.borrow_mut().paused
    }
}

impl Clone for StreamThrottler {
    fn clone(&self) -> StreamThrottler {
        StreamThrottler {
            state: self.state.clone()
        }
    }
}

/*
 * A wrapper around a stream
 * throttled by StreamThrottler
 */
pub struct ThrottledStream<S: Stream> {
    stream: S,
    state: Rc<RefCell<ThrottledStreamState>>
}

impl<S: Stream> Stream for ThrottledStream<S> {
    type Item = S::Item;
    type Error = S::Error;

    fn poll(&mut self) -> ::std::result::Result<Async<Option<S::Item>>, S::Error> {
        let mut state = self.state.borrow_mut();

        if state.task.is_none() {
            state.task = Some(task::current()); // TODO: Abstract out this logic
        }

        if !state.paused {
            // Only poll the original stream if the `paused` flag is not set
            self.stream.poll()
        } else {
            Ok(Async::NotReady)
        }
    }
}

/*
 * A wrapper of Stream that makes the
 * original stream alternate
 * i.e. if the stream returns Ready,
 *      then the next time of `poll()`
 *      will always return NotReady.
 * This simulates a round-robin
 * pattern between multiple streams
 * that will be present in this program,
 * which avoids making all the
 * streams stalled because of one super
 * fast stream, and also (generally)
 * fairly distributes the bandwidth.
 */
pub struct AlternatingStream<S: Stream> {
    stream: S,
    flag: bool
}

impl<S: Stream> AlternatingStream<S> {
    pub fn new(stream: S) -> AlternatingStream<S> {
        AlternatingStream {
            stream,
            flag: false
        }
    }
}

impl<S: Stream> Stream for AlternatingStream<S> {
    type Item = S::Item;
    type Error = S::Error;

    fn poll(&mut self) -> ::std::result::Result<Async<Option<S::Item>>, S::Error> {
        if self.flag {
            // Do not poll the original stream if the last poll was successful.
            self.flag = false;
            // Notify the task to poll this stream on the next tick.
            task::current().notify();
            return Ok(Async::NotReady);
        } else {
            let ret = self.stream.poll();
            if let Ok(Async::Ready(_)) = ret {
                // Do not poll after a successful poll.
                self.flag = true;
            }
            return ret;
        }
    }
}

/*
 * Maximum length of the queue in SharedWriter before calling pause()
 */
const QUEUE_MAX_LENGTH: usize = 4;

/*
 * Trait for all supported types of SharedStream
 * all must have a size
 */
pub trait SizedBuf {
    fn get_size(&self) -> u64;
}

impl SizedBuf for Bytes {
    fn get_size(&self) -> u64 {
        self.len() as u64
    }
}

impl SizedBuf for BytesMut {
    fn get_size(&self) -> u64 {
        self.len() as u64
    }
}

impl SizedBuf for OwnedMessage {
    fn get_size(&self) -> u64 {
        match *self {
            OwnedMessage::Text(ref s) => s.len() as u64,
            OwnedMessage::Binary(ref vec) => vec.len() as u64,
            _ => 0
        }
    }
}

/*
 * Shared state between SharedWriter and SharedStream
 */
struct SharedStreamState<I, E> {
    marker: PhantomData<E>, // Placeholder
    queue: Vec<I>, // Items to be written
    finished: bool, // Whether this stream should finish on next poll()
    throttling_handler: Option<Box<dyn ThrottlingHandler>>,
    speedometer: Speedometer,
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
struct SharedStream<I: Debug + SizedBuf, E> {
    state: Rc<RefCell<SharedStreamState<I, E>>>
}

impl<I: Debug + SizedBuf, E> SharedStream<I, E> {
    fn new(state: Rc<RefCell<SharedStreamState<I, E>>>) -> SharedStream<I, E> {
        SharedStream {
            state
        }
    }
}

impl<I: Debug + SizedBuf, E> Stream for SharedStream<I, E> {
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

            // Calculate the speed of this stream (write part)
            state.speedometer.feed_counter(item.get_size());

            if state.queue.len() < QUEUE_MAX_LENGTH && state.throttling_handler.is_some() {
                let h = state.throttling_handler.as_mut().unwrap();
                if h.is_paused() {
                    h.resume();
                }
            }

            return Ok(Async::Ready(Some(item)));
        } else {
            // Nothing to be sent, but the stream
            // is not marked as finished yet.
            return Ok(Async::NotReady);
        }
    }
}

/*
 * A sharable writer that writes to a Sink
 * but does not flush() or wait for finishing
 * using SharedStream.
 * 
 * This is a workaround because Sink::send()
 * will consume ownership and will do flush()
 * each time we try to send an item.
 * 
 * The writer is cheaply clonable, allowing to
 * be shared between multiple owners.
 */
pub struct SharedWriter<S: 'static + Sink> where S::SinkItem: Debug + SizedBuf {
    state: Rc<RefCell<SharedStreamState<S::SinkItem, S::SinkError>>>
}

impl<S: 'static + Sink> SharedWriter<S> where S::SinkItem: Debug + SizedBuf {
    pub fn new() -> SharedWriter<S> {
        SharedWriter {
            state: Rc::new(RefCell::new(SharedStreamState {
                marker: PhantomData,
                queue: Vec::with_capacity(QUEUE_MAX_LENGTH * 2),
                finished: false,
                task: None,
                throttling_handler: None,
                speedometer: Speedometer::new()
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
    pub fn run(&self, sink: S) -> impl Future<Item=(), Error=S::SinkError> {
        let state = self.state.clone();
        let stream = SharedStream::new(state);
        stream.forward(sink)
            .map(|_| ())
    }

    /*
     * Feed a new item into the sink
     */
    pub fn feed(&self, item: S::SinkItem) {
        let mut state = self.state.borrow_mut();
        if !state.finished {
            state.queue.push(item);
            notify_task(&state.task);
            //println!("new len = {}", state.queue.len());

            if state.queue.len() >= QUEUE_MAX_LENGTH && state.throttling_handler.is_some() {
                let speed = state.speedometer.speed();
                let h = state.throttling_handler.as_mut().unwrap();
                if !h.is_paused() || h.allow_pause_multiple_times() {
                    h.pause(speed);
                }
            }
        }
    }

    /*
     * Mark as finished.
     */
    pub fn close(&self) {
        let mut state = self.state.borrow_mut();
        if !state.finished {
            state.finished = true;
            state.queue.clear();
            notify_task(&state.task);
        }
    }

    pub fn set_throttling_handler<H: 'static + ThrottlingHandler>(&self, handler: H) {
        self.state.borrow_mut().throttling_handler = Some(Box::new(handler));
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

/*
 * Destructor implementation of SharedWriter
 * This should be customized because SharedWriter
 * itself is clonable.
 */
impl<S: 'static + Sink> Drop for SharedWriter<S> where S::SinkItem: Debug + SizedBuf {
    fn drop(&mut self) {
        // The state is shared between at least
        // one SharedStream and one SharedWriter
        // Therefore, when the reference count is
        // less than 2, we can be sure that this
        // will be the last SharedWriter alive,
        // and thus we can safely release the resource.
        if Rc::strong_count(&self.state) <= 2 {
            self.close();
        }
    }
}

/*
 * SharedWriter is clonable by just cloning the state
 * allowing multiple ownership without two levels of `Rc`s
 */
impl<S: 'static + Sink> Clone for SharedWriter<S> where S::SinkItem: Debug + SizedBuf {
    fn clone(&self) -> SharedWriter<S> {
        SharedWriter {
            state: self.state.clone()
        }
    }
}

/*
 * Shared heartbeat logic for WebSocket sessions.
 * Takes the SharedWriter for the session,
 * runs a Futures loop that can be joint to the main
 * work using `select` combinator.
 */
pub struct HeartbeatAgent<S> where S: 'static + Sink<SinkItem=OwnedMessage> {
    timeout: u64, // Milliseconds
    writer: SharedWriter<S>,
    heartbeat_received: Rc<Cell<bool>>
}

impl<S> HeartbeatAgent<S> where S: 'static + Sink<SinkItem=OwnedMessage> {
    pub fn new(timeout: u64, writer: SharedWriter<S>) -> HeartbeatAgent<S> {
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
    pub fn run<'a>(&self) -> impl Future<Error=Error, Item=()> {
        let writer = self.writer.clone();
        let heartbeat_received = self.heartbeat_received.clone();
        tokio_timer::Interval::new(Instant::now(), Duration::from_millis(self.timeout))
            .map_err(|e| {
                println!("{:?}", e);
                "Unknown error".into()
            })
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