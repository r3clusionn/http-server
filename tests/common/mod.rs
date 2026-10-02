#![allow(dead_code)]

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, Instant};

use tinyhttp::{demo, Config, FileServer, Server};

/// A deliberately separate, minimal HTTP client: it knows nothing about the server's code.
pub struct Client {
    pub s: TcpStream,
    pub buf: Vec<u8>,
}

#[derive(Debug)]
pub struct Resp {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub raw_head: String,
}

impl Resp {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

impl Client {
    pub fn connect(addr: SocketAddr) -> Client {
        let s = TcpStream::connect(addr).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        s.set_nodelay(true).unwrap();
        Client { s, buf: Vec::new() }
    }

    pub fn send(&mut self, bytes: &[u8]) {
        self.s.write_all(bytes).unwrap();
    }

    fn fill(&mut self) -> usize {
        let mut tmp = [0u8; 65536];
        let n = self.s.read(&mut tmp).unwrap_or(0);
        self.buf.extend_from_slice(&tmp[..n]);
        n
    }

    fn take_line(&mut self) -> Option<String> {
        loop {
            if let Some(p) = self.buf.windows(2).position(|w| w == b"\r\n") {
                let line = String::from_utf8_lossy(&self.buf[..p]).into_owned();
                self.buf.drain(..p + 2);
                return Some(line);
            }
            if self.fill() == 0 {
                return None;
            }
        }
    }

    fn take_exact(&mut self, n: usize) -> Vec<u8> {
        while self.buf.len() < n {
            if self.fill() == 0 {
                panic!("connection closed with {} of {n} body bytes", self.buf.len());
            }
        }
        self.buf.drain(..n).collect()
    }

    /// Reads one response. `head_only` is for answers to HEAD, which have no body whatever the headers say.
    pub fn read_response(&mut self, head_only: bool) -> Option<Resp> {
        let status_line = self.take_line()?;
        let mut parts = status_line.splitn(3, ' ');
        assert!(parts.next()?.starts_with("HTTP/1."), "{status_line}");
        let status: u16 = parts.next()?.parse().expect("status code");
        let mut headers = Vec::new();
        let mut raw_head = status_line.clone() + "\n";
        loop {
            let l = self.take_line()?;
            if l.is_empty() {
                break;
            }
            raw_head += &l;
            raw_head += "\n";
            let (n, v) = l.split_once(':').expect("header colon");
            headers.push((n.to_string(), v.trim().to_string()));
        }
        let get = |name: &str| headers.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v.clone());
        let mut body = Vec::new();
        if status == 100 {
            return Some(Resp { status, headers, body, raw_head });
        }
        if !head_only && status != 204 && status != 304 {
            if get("transfer-encoding").is_some_and(|v| v.eq_ignore_ascii_case("chunked")) {
                loop {
                    let size_line = self.take_line()?;
                    let size = usize::from_str_radix(size_line.split(';').next().unwrap().trim(), 16).expect("chunk size");
                    if size == 0 {
                        // Trailers, then the empty line.
                        while !self.take_line()?.is_empty() {}
                        break;
                    }
                    body.extend(self.take_exact(size));
                    assert_eq!(self.take_exact(2), b"\r\n");
                }
            } else if let Some(n) = get("content-length") {
                body = self.take_exact(n.parse().unwrap());
            } else {
                // Close-delimited.
                while self.fill() > 0 {}
                body = std::mem::take(&mut self.buf);
            }
        }
        Some(Resp { status, headers, body, raw_head })
    }

    pub fn get(&mut self, path: &str) -> Resp {
        self.send(format!("GET {path} HTTP/1.1\r\nHost: t\r\n\r\n").as_bytes());
        self.read_response(false).expect("a response")
    }

    /// True if the server closed the connection (nothing more to read).
    pub fn closed(&mut self) -> bool {
        self.s.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        self.buf.is_empty() && self.fill() == 0
    }
}

pub fn raw(addr: SocketAddr, request: &[u8]) -> Resp {
    let mut c = Client::connect(addr);
    c.send(request);
    c.read_response(false).expect("a response")
}

/// A server with the demo routes and a small directory tree.
pub struct Fixture {
    pub server: Server,
    pub addr: SocketAddr,
    pub dir: tempfile::TempDir,
}

pub fn start_with(tweak: impl FnOnce(&mut Config), setup: impl FnOnce(&std::path::Path)) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    setup(dir.path());
    let mut config = Config { threads: 8, ..Config::default() };
    tweak(&mut config);
    let mut files = FileServer::new(dir.path()).unwrap();
    files.listing = true;
    let files = std::sync::Arc::new(files);
    let router = demo::router(Some(config.stats.clone())).fallback(move |req, _| files.handle(req));
    let server = Server::start("127.0.0.1:0", router, config).unwrap();
    let addr = server.local_addr();
    Fixture { server, addr, dir }
}

pub fn start() -> Fixture {
    start_with(|_| {}, |_| {})
}

/// Waits for a condition for up to `secs` seconds.
pub fn eventually(secs: u64, mut f: impl FnMut() -> bool) -> bool {
    let end = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < end {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    f()
}
