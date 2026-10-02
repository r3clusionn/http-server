//! The server: an accept thread, a pool of worker threads and one connection loop per client.
//!
//! A worker serves one connection at a time, request after request (keep-alive), so the number of
//! clients served at once is the number of workers. Connections that arrive while every worker is
//! busy wait in a bounded queue; when that is full the client gets `503 Service Unavailable`
//! straight away instead of hanging.

use std::io::{self, BufWriter, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::chunked::Decoder;
use crate::date;
use crate::request::{parse_head, BodyKind, Limits, Method, Parsed, Request};
use crate::response::{write_response, Response};
use crate::router::Router;

/// One finished request, for the access log.
#[derive(Debug)]
pub struct LogEntry {
    pub peer: Option<SocketAddr>,
    pub method: String,
    pub target: String,
    pub status: u16,
    pub bytes: u64,
    pub elapsed: Duration,
}

pub type Logger = Arc<dyn Fn(&LogEntry) + Send + Sync>;

#[derive(Clone)]
pub struct Config {
    /// Worker threads, which is also the number of clients served at the same moment.
    pub threads: usize,
    /// Connections that may wait for a worker.
    pub queue: usize,
    /// How long a kept-alive connection may sit idle before the server closes it.
    pub idle_timeout: Duration,
    /// Time allowed from the first byte of a request to the end of its head.
    pub head_timeout: Duration,
    /// Time allowed to receive a request body.
    pub body_timeout: Duration,
    pub write_timeout: Duration,
    /// Requests served on one connection before it is closed (0 for no limit).
    pub max_requests: usize,
    pub limits: Limits,
    pub logger: Option<Logger>,
    /// Counters; share this `Arc` with handlers that want to report them.
    pub stats: Arc<Stats>,
}

impl Default for Config {
    fn default() -> Self {
        let cpus = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
        Config {
            threads: (cpus * 2).max(4),
            queue: 256,
            idle_timeout: Duration::from_secs(5),
            head_timeout: Duration::from_secs(10),
            body_timeout: Duration::from_secs(30),
            write_timeout: Duration::from_secs(30),
            max_requests: 1000,
            limits: Limits::default(),
            logger: None,
            stats: Arc::new(Stats::default()),
        }
    }
}

/// Counters, readable while the server runs.
#[derive(Default, Debug)]
pub struct Stats {
    pub connections: AtomicU64,
    pub requests: AtomicU64,
    /// Connections turned away because the queue was full.
    pub rejected: AtomicU64,
    /// Requests that could not be parsed or were too large.
    pub bad_requests: AtomicU64,
    pub timeouts: AtomicU64,
    pub handler_panics: AtomicU64,
}

/// A running server.
pub struct Server {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    stats: Arc<Stats>,
    acceptor: Option<JoinHandle<()>>,
    workers: Vec<JoinHandle<()>>,
}

impl Server {
    pub fn start(addr: impl ToSocketAddrs, router: Router, config: Config) -> io::Result<Server> {
        let listener = TcpListener::bind(addr)?;
        let addr = listener.local_addr()?;
        let stop = Arc::new(AtomicBool::new(false));
        let stats = config.stats.clone();
        let router = Arc::new(router);
        let config = Arc::new(config);
        let (tx, rx) = sync_channel::<TcpStream>(config.queue.max(1));
        let rx = Arc::new(Mutex::new(rx));

        let mut workers = Vec::new();
        for i in 0..config.threads.max(1) {
            let (rx, router, config, stop, stats) = (rx.clone(), router.clone(), config.clone(), stop.clone(), stats.clone());
            workers.push(std::thread::Builder::new().name(format!("tinyhttp-worker-{i}")).spawn(move || loop {
                let next = rx.lock().unwrap_or_else(|e| e.into_inner()).recv();
                match next {
                    Ok(stream) => serve_connection(stream, &router, &config, &stop, &stats),
                    Err(_) => break,
                }
            })?);
        }

        let acceptor = {
            let (stop, stats) = (stop.clone(), stats.clone());
            std::thread::Builder::new().name("tinyhttp-accept".into()).spawn(move || {
                for incoming in listener.incoming() {
                    if stop.load(Ordering::SeqCst) {
                        break;
                    }
                    match incoming {
                        Ok(stream) => {
                            stats.connections.fetch_add(1, Ordering::Relaxed);
                            match tx.try_send(stream) {
                                Ok(()) => {}
                                Err(TrySendError::Full(s)) => {
                                    stats.rejected.fetch_add(1, Ordering::Relaxed);
                                    reject_busy(s);
                                }
                                Err(TrySendError::Disconnected(_)) => break,
                            }
                        }
                        // Out of file descriptors or a connection reset before accept: keep going.
                        Err(_) => std::thread::sleep(Duration::from_millis(10)),
                    }
                }
                // Dropping the sender ends the workers once the queue is empty.
            })?
        };
        Ok(Server { addr, stop, stats, acceptor: Some(acceptor), workers })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn stats(&self) -> &Stats {
        &self.stats
    }

    /// Stops accepting, lets requests in progress finish, closes idle connections and waits for
    /// every thread.
    pub fn shutdown(mut self) {
        self.stop_and_join();
    }

    fn stop_and_join(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(a) = self.acceptor.take() {
            // Wake the accept call with a connection of our own.
            let wake = if self.addr.ip().is_unspecified() {
                SocketAddr::new(std::net::Ipv4Addr::LOCALHOST.into(), self.addr.port())
            } else {
                self.addr
            };
            let _ = TcpStream::connect_timeout(&wake, Duration::from_secs(1));
            let _ = a.join();
        }
        for w in self.workers.drain(..) {
            let _ = w.join();
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop_and_join();
    }
}

fn reject_busy(mut s: TcpStream) {
    let _ = s.set_write_timeout(Some(Duration::from_millis(200)));
    let body = "503 Service Unavailable\n";
    let _ = write!(
        s,
        "HTTP/1.1 503 Service Unavailable\r\nRetry-After: 1\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = s.shutdown(Shutdown::Both);
}

/// Counts the bytes written through it.
struct Counting<W> {
    inner: W,
    count: u64,
}

impl<W: Write> Write for Counting<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.count += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

enum Fill {
    Data,
    Eof,
    Timeout,
    Stopped,
}

struct Conn<'a> {
    stream: TcpStream,
    buf: Vec<u8>,
    stop: &'a AtomicBool,
}

impl Conn<'_> {
    /// Reads more bytes into `buf`, waking every 200 ms to check the deadline and the stop flag.
    fn fill(&mut self, deadline: Instant, stop_when_idle: bool) -> io::Result<Fill> {
        let mut tmp = [0u8; 16 * 1024];
        loop {
            if stop_when_idle && self.stop.load(Ordering::SeqCst) {
                return Ok(Fill::Stopped);
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(Fill::Timeout);
            }
            self.stream.set_read_timeout(Some((deadline - now).min(Duration::from_millis(200))))?;
            match self.stream.read(&mut tmp) {
                Ok(0) => return Ok(Fill::Eof),
                Ok(n) => {
                    self.buf.extend_from_slice(&tmp[..n]);
                    return Ok(Fill::Data);
                }
                Err(e)
                    if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted) =>
                {
                    continue
                }
                Err(e) => return Err(e),
            }
        }
    }
}

/// A request the connection loop could not turn into a handler call.
struct Refusal {
    status: u16,
}

fn serve_connection(stream: TcpStream, router: &Router, config: &Config, stop: &AtomicBool, stats: &Stats) {
    let _ = stream.set_nodelay(true);
    let _ = stream.set_write_timeout(Some(config.write_timeout));
    let peer = stream.peer_addr().ok();
    let Ok(write_half) = stream.try_clone() else { return };
    let mut out = Counting { inner: BufWriter::with_capacity(16 * 1024, write_half), count: 0 };
    let mut conn = Conn { stream, buf: Vec::with_capacity(4096), stop };

    let mut served = 0usize;
    let reply_and_close = |out: &mut Counting<BufWriter<TcpStream>>, status: u16, stats: &Stats| {
        stats.bad_requests.fetch_add(1, Ordering::Relaxed);
        let _ = write_response(
            out,
            Response::error(status),
            &Method::Get,
            crate::request::Version::Http11,
            false,
            &date::format(std::time::SystemTime::now()),
        );
    };

    'requests: loop {
        // --- head ---
        let idle_deadline = Instant::now() + config.idle_timeout;
        let mut head_deadline: Option<Instant> =
            if conn.buf.is_empty() { None } else { Some(Instant::now() + config.head_timeout) };
        let (head, consumed) = loop {
            match parse_head(&conn.buf, &config.limits) {
                Ok(Parsed::Complete { head, consumed }) => break (head, consumed),
                Ok(Parsed::Incomplete) => {
                    let deadline = head_deadline.unwrap_or(idle_deadline);
                    match conn.fill(deadline, conn.buf.is_empty()) {
                        Ok(Fill::Data) => {
                            head_deadline.get_or_insert_with(|| Instant::now() + config.head_timeout);
                        }
                        Ok(Fill::Timeout) => {
                            if !conn.buf.is_empty() {
                                stats.timeouts.fetch_add(1, Ordering::Relaxed);
                                reply_and_close(&mut out, 408, stats);
                                break 'requests;
                            }
                            // An idle connection is closed without a word.
                            break 'requests;
                        }
                        Ok(Fill::Eof | Fill::Stopped) | Err(_) => break 'requests,
                    }
                }
                Err(e) => {
                    reply_and_close(&mut out, e.status, stats);
                    break 'requests;
                }
            }
        };
        let started = Instant::now();
        conn.buf.drain(..consumed);
        let mut request: Request = head.request;
        request.peer = peer;

        // --- body ---
        let body_deadline = Instant::now() + config.body_timeout;
        let refusal = (|| -> Result<Vec<u8>, Refusal> {
            match head.body {
                BodyKind::None => Ok(Vec::new()),
                BodyKind::Length(n) => {
                    if n > config.limits.max_body as u64 {
                        return Err(Refusal { status: 413 });
                    }
                    if head.expect_continue && conn.buf.is_empty() {
                        let _ = out.inner.write_all(b"HTTP/1.1 100 Continue\r\n\r\n");
                        let _ = out.inner.flush();
                    }
                    let n = n as usize;
                    while conn.buf.len() < n {
                        match conn.fill(body_deadline, false) {
                            Ok(Fill::Data) => {}
                            Ok(Fill::Timeout) => {
                                stats.timeouts.fetch_add(1, Ordering::Relaxed);
                                return Err(Refusal { status: 408 });
                            }
                            _ => return Err(Refusal { status: 400 }),
                        }
                    }
                    Ok(conn.buf.drain(..n).collect())
                }
                BodyKind::Chunked => {
                    if head.expect_continue && conn.buf.is_empty() {
                        let _ = out.inner.write_all(b"HTTP/1.1 100 Continue\r\n\r\n");
                        let _ = out.inner.flush();
                    }
                    let mut dec = Decoder::new(config.limits.max_body);
                    let mut body = Vec::new();
                    loop {
                        let used = dec.feed(&conn.buf, &mut body).map_err(|e| Refusal { status: e.status })?;
                        conn.buf.drain(..used);
                        if dec.is_done() {
                            return Ok(body);
                        }
                        match conn.fill(body_deadline, false) {
                            Ok(Fill::Data) => {}
                            Ok(Fill::Timeout) => {
                                stats.timeouts.fetch_add(1, Ordering::Relaxed);
                                return Err(Refusal { status: 408 });
                            }
                            _ => return Err(Refusal { status: 400 }),
                        }
                    }
                }
            }
        })();
        match refusal {
            Ok(body) => request.body = body,
            Err(r) => {
                reply_and_close(&mut out, r.status, stats);
                break 'requests;
            }
        }

        // --- handler ---
        served += 1;
        stats.requests.fetch_add(1, Ordering::Relaxed);
        let keep_alive =
            request.keep_alive() && (config.max_requests == 0 || served < config.max_requests) && !stop.load(Ordering::SeqCst);
        let resp = match catch_unwind(AssertUnwindSafe(|| router.dispatch(&request))) {
            Ok(r) => r,
            Err(_) => {
                stats.handler_panics.fetch_add(1, Ordering::Relaxed);
                Response::error(500)
            }
        };
        let status = resp.status;
        let before = out.count;
        let kept = write_response(
            &mut out,
            resp,
            &request.method,
            request.version,
            keep_alive,
            &date::format(std::time::SystemTime::now()),
        )
        .unwrap_or(false);
        if let Some(log) = &config.logger {
            log(&LogEntry {
                peer,
                method: request.method.to_string(),
                target: request.target.clone(),
                status,
                bytes: out.count - before,
                elapsed: started.elapsed(),
            });
        }
        if !kept {
            break;
        }
    }
    close_politely(&mut out, &mut conn);
}

/// Closes a connection so the client can still read the last response: shut the sending side, then
/// read and discard whatever the client is still sending for a moment. Closing a socket that has
/// unread data makes TCP reset the connection, which can destroy a response not yet read.
fn close_politely(out: &mut Counting<BufWriter<TcpStream>>, conn: &mut Conn) {
    let _ = out.flush();
    let _ = conn.stream.shutdown(Shutdown::Write);
    let _ = conn.stream.set_read_timeout(Some(Duration::from_millis(100)));
    let end = Instant::now() + Duration::from_millis(500);
    let mut sink = [0u8; 4096];
    let mut total = 0usize;
    while Instant::now() < end && total < 256 * 1024 {
        match conn.stream.read(&mut sink) {
            Ok(0) | Err(_) => break,
            Ok(n) => total += n,
        }
    }
}
