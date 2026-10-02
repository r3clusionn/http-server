//! A keep-alive load generator: `loadgen ADDR PATH [--clients N] [--seconds S]`.
//!
//! Every client keeps one connection open and sends the same GET request again as soon as the
//! previous response is complete, so it measures the server, not connection set-up. A response is
//! read by its Content-Length. If the server closes the connection (a per-connection request
//! limit, for example) the client connects again and the request is repeated; the count of
//! reconnects is printed.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn read_response(s: &mut TcpStream, buf: &mut Vec<u8>) -> std::io::Result<(u16, usize)> {
    let mut tmp = [0u8; 65536];
    loop {
        if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = std::str::from_utf8(&buf[..end]).unwrap_or("");
            let status: u16 = head.get(9..12).and_then(|s| s.parse().ok()).unwrap_or(0);
            let lower = head.to_ascii_lowercase();
            if lower.contains("transfer-encoding: chunked") {
                panic!("loadgen reads responses by Content-Length; the server answered with chunked coding");
            }
            let len = lower
                .lines()
                .find_map(|l| l.strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap_or(0)))
                .unwrap_or(0);
            let total = end + 4 + len;
            while buf.len() < total {
                let n = s.read(&mut tmp)?;
                if n == 0 {
                    return Err(std::io::ErrorKind::UnexpectedEof.into());
                }
                buf.extend_from_slice(&tmp[..n]);
            }
            buf.drain(..total);
            return Ok((status, len));
        }
        let n = s.read(&mut tmp)?;
        if n == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        buf.extend_from_slice(&tmp[..n]);
    }
}

struct Outcome {
    latencies_ns: Vec<u32>,
    body_bytes: usize,
    reconnects: u64,
    errors: u64,
}

fn client(addr: &str, request: &[u8], stop: &AtomicBool) -> Outcome {
    let connect = || {
        let s = TcpStream::connect(addr).expect("connect");
        s.set_nodelay(true).unwrap();
        s
    };
    let mut s = connect();
    let mut buf = Vec::new();
    let mut out = Outcome { latencies_ns: Vec::with_capacity(1 << 20), body_bytes: 0, reconnects: 0, errors: 0 };
    while !stop.load(Ordering::Relaxed) {
        let t = Instant::now();
        if s.write_all(request).is_ok() {
            match read_response(&mut s, &mut buf) {
                Ok((200, len)) => {
                    out.latencies_ns.push(t.elapsed().as_nanos() as u32);
                    out.body_bytes += len;
                    continue;
                }
                Ok(_) => {
                    // A status other than 200 is an error, not something to retry.
                    out.errors += 1;
                    break;
                }
                Err(_) => {}
            }
        }
        out.reconnects += 1;
        s = connect();
        buf.clear();
    }
    out
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 2 {
        eprintln!("usage: loadgen ADDR PATH [--clients N] [--seconds S]");
        std::process::exit(2);
    }
    let (addr, path) = (args[0].clone(), args[1].clone());
    let opt = |name: &str, default: u64| {
        args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).and_then(|v| v.parse().ok()).unwrap_or(default)
    };
    let (clients, seconds) = (opt("--clients", 32) as usize, opt("--seconds", 5));

    let stop = Arc::new(AtomicBool::new(false));
    let request = format!("GET {path} HTTP/1.1\r\nHost: bench\r\n\r\n").into_bytes();
    let start = Instant::now();
    let handles: Vec<_> = (0..clients)
        .map(|_| {
            let (addr, stop, request) = (addr.clone(), stop.clone(), request.clone());
            std::thread::spawn(move || client(&addr, &request, &stop))
        })
        .collect();
    std::thread::sleep(Duration::from_secs(seconds));
    stop.store(true, Ordering::Relaxed);
    let mut all: Vec<u32> = Vec::new();
    let (mut bytes, mut reconnects, mut errors) = (0usize, 0u64, 0u64);
    for h in handles {
        let o = h.join().unwrap();
        all.extend(o.latencies_ns);
        bytes += o.body_bytes;
        reconnects += o.reconnects;
        errors += o.errors;
    }
    let secs = start.elapsed().as_secs_f64();
    all.sort_unstable();
    let pct = |p: f64| all.get(((all.len() as f64 - 1.0) * p) as usize).copied().unwrap_or(0) as f64 / 1000.0;
    println!(
        "{} requests in {:.2} s with {} clients: {:.0} req/s, {:.1} MB/s body, latency p50 {:.0} us, p99 {:.0} us, max {:.0} us, {} reconnects, {} errors",
        all.len(),
        secs,
        clients,
        all.len() as f64 / secs,
        bytes as f64 / secs / 1e6,
        pct(0.5),
        pct(0.99),
        pct(1.0),
        reconnects,
        errors
    );
}
