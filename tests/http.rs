mod common;

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use common::*;

#[test]
fn a_simple_get_has_the_expected_framing_and_headers() {
    let f = start();
    let r = raw(f.addr, b"GET /_/hello/world HTTP/1.1\r\nHost: t\r\n\r\n");
    assert_eq!(r.status, 200);
    assert_eq!(r.text(), "hello, world\n");
    assert_eq!(r.header("content-length"), Some("13"));
    assert_eq!(r.header("content-type"), Some("text/plain; charset=utf-8"));
    assert!(r.header("date").is_some_and(|d| d.ends_with(" GMT") && d.len() == 29), "{:?}", r.header("date"));
    assert!(r.header("server").is_some_and(|s| s.starts_with("tinyhttp/")));
}

#[test]
fn keep_alive_serves_many_requests_on_one_connection() {
    let f = start();
    let mut c = Client::connect(f.addr);
    for i in 0..10 {
        let r = c.get(&format!("/_/hello/n{i}"));
        assert_eq!(r.text(), format!("hello, n{i}\n"));
        assert!(r.header("connection").is_none());
    }
    assert_eq!(f.server.stats().connections.load(Ordering::Relaxed), 1);
    assert_eq!(f.server.stats().requests.load(Ordering::Relaxed), 10);
}

#[test]
fn connection_close_and_http_10_end_the_connection() {
    let f = start();
    let mut c = Client::connect(f.addr);
    c.send(b"GET /_/hello/a HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n");
    let r = c.read_response(false).unwrap();
    assert_eq!(r.header("connection"), Some("close"));
    assert!(c.closed());

    let mut c = Client::connect(f.addr);
    c.send(b"GET /_/hello/a HTTP/1.0\r\n\r\n");
    let r = c.read_response(false).unwrap();
    assert_eq!((r.status, r.header("connection")), (200, Some("close")));
    assert!(c.closed());

    // HTTP/1.0 with keep-alive stays open.
    let mut c = Client::connect(f.addr);
    c.send(b"GET /_/hello/a HTTP/1.0\r\nConnection: keep-alive\r\n\r\n");
    let r = c.read_response(false).unwrap();
    assert_eq!(r.header("connection"), Some("keep-alive"));
    c.send(b"GET /_/hello/b HTTP/1.0\r\nConnection: keep-alive\r\n\r\n");
    assert_eq!(c.read_response(false).unwrap().text(), "hello, b\n");
}

#[test]
fn pipelined_requests_are_answered_in_order() {
    let f = start();
    let mut c = Client::connect(f.addr);
    let mut all = Vec::new();
    for i in 0..20 {
        all.extend(format!("GET /_/hello/p{i} HTTP/1.1\r\nHost: t\r\n\r\n").bytes());
    }
    // A POST in the middle with a body that contains something that looks like a request.
    all.extend(b"POST /_/echo HTTP/1.1\r\nHost: t\r\nContent-Length: 39\r\n\r\nGET /_/hello/fake HTTP/1.1\r\nHost: t\r\n\r\n");
    all.extend(b"GET /_/hello/last HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n");
    c.send(&all);
    for i in 0..20 {
        assert_eq!(c.read_response(false).unwrap().text(), format!("hello, p{i}\n"));
    }
    assert_eq!(c.read_response(false).unwrap().text(), "GET /_/hello/fake HTTP/1.1\r\nHost: t\r\n\r\n");
    assert_eq!(c.read_response(false).unwrap().text(), "hello, last\n");
    assert!(c.closed());
}

#[test]
fn a_request_arriving_one_byte_at_a_time_is_handled() {
    let f = start();
    let mut c = Client::connect(f.addr);
    let req = b"POST /_/echo HTTP/1.1\r\nHost: t\r\nContent-Length: 5\r\n\r\nhello";
    for b in req.iter() {
        c.send(std::slice::from_ref(b));
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(c.read_response(false).unwrap().text(), "hello");
}

#[test]
fn request_bodies_with_content_length_and_chunked_coding() {
    let f = start();
    let r =
        raw(f.addr, b"POST /_/echo HTTP/1.1\r\nHost: t\r\nContent-Type: text/x-test\r\nContent-Length: 11\r\n\r\nhello world");
    assert_eq!((r.status, r.text(), r.header("content-type")), (200, "hello world".into(), Some("text/x-test")));

    let r = raw(
        f.addr,
        b"POST /_/echo HTTP/1.1\r\nHost: t\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n6;ext=1\r\n world\r\n0\r\nTrailer: x\r\n\r\n",
    );
    assert_eq!((r.status, r.text()), (200, "hello world".into()));

    // A chunked body split into pieces by the client, with pauses.
    let mut c = Client::connect(f.addr);
    c.send(b"POST /_/echo HTTP/1.1\r\nHost: t\r\nTransfer-Encoding: chunked\r\n\r\n");
    for piece in [&b"3\r\nabc\r\n"[..], b"3\r\ndef\r\n", b"0\r\n\r\n"] {
        std::thread::sleep(Duration::from_millis(30));
        c.send(piece);
    }
    assert_eq!(c.read_response(false).unwrap().text(), "abcdef");
    // The connection is still in sync afterwards.
    assert_eq!(c.get("/_/hello/after").text(), "hello, after\n");
}

#[test]
fn expect_100_continue_gets_an_interim_response() {
    let f = start();
    let mut c = Client::connect(f.addr);
    c.send(b"POST /_/echo HTTP/1.1\r\nHost: t\r\nContent-Length: 4\r\nExpect: 100-continue\r\n\r\n");
    let interim = c.read_response(false).unwrap();
    assert_eq!(interim.status, 100);
    c.send(b"data");
    let r = c.read_response(false).unwrap();
    assert_eq!((r.status, r.text()), (200, "data".into()));
    // Too large: refused without asking for the body.
    c.send(b"POST /_/echo HTTP/1.1\r\nHost: t\r\nContent-Length: 99999999\r\nExpect: 100-continue\r\n\r\n");
    assert_eq!(c.read_response(false).unwrap().status, 413);
}

#[test]
fn streamed_responses_are_chunked_and_complete() {
    let f = start();
    let mut c = Client::connect(f.addr);
    let r = c.get("/_/stream?n=5&size=1000");
    assert_eq!(r.header("transfer-encoding"), Some("chunked"));
    assert!(r.header("content-length").is_none());
    assert_eq!(r.body.len(), 5000);
    assert_eq!(&r.body[..3], b"aaa");
    assert_eq!(&r.body[1000..1003], b"bbb");
    // And the connection is reusable afterwards.
    assert_eq!(c.get("/_/hello/x").status, 200);

    // HTTP/1.0 cannot be chunked: the body ends with the connection.
    let mut c = Client::connect(f.addr);
    c.send(b"GET /_/stream?n=3&size=10 HTTP/1.0\r\n\r\n");
    let r = c.read_response(false).unwrap();
    assert!(r.header("transfer-encoding").is_none());
    assert_eq!(r.header("connection"), Some("close"));
    assert_eq!(r.body.len(), 30);
}

#[test]
fn head_has_no_body_and_does_not_break_framing() {
    let f = start();
    let mut c = Client::connect(f.addr);
    c.send(b"HEAD /_/hello/abc HTTP/1.1\r\nHost: t\r\n\r\n");
    let r = c.read_response(true).unwrap();
    assert_eq!((r.status, r.header("content-length"), r.body.len()), (200, Some("11"), 0));
    // If the server had sent the body, this would read garbage.
    assert_eq!(c.get("/_/hello/next").text(), "hello, next\n");
    c.send(b"HEAD /_/stream?n=2 HTTP/1.1\r\nHost: t\r\n\r\n");
    assert_eq!(c.read_response(true).unwrap().status, 200);
    assert_eq!(c.get("/_/hello/again").text(), "hello, again\n");
}

#[test]
fn routing_errors() {
    let f = start();
    assert_eq!(raw(f.addr, b"GET /_/nothing HTTP/1.1\r\nHost: t\r\n\r\n").status, 404);
    let r = raw(f.addr, b"DELETE /_/echo HTTP/1.1\r\nHost: t\r\n\r\n");
    assert_eq!((r.status, r.header("allow")), (405, Some("POST, OPTIONS")));
    let r = raw(f.addr, b"OPTIONS /_/hello/a HTTP/1.1\r\nHost: t\r\n\r\n");
    assert_eq!((r.status, r.header("allow")), (204, Some("GET, HEAD, OPTIONS")));
    assert!(r.header("content-length").is_none());
}

#[test]
fn malformed_requests_get_an_error_and_a_closed_connection() {
    let f = start();
    let cases: &[(&[u8], u16)] = &[
        (b"GARBAGE\r\n\r\n", 400),
        (b"GET / HTTP/1.1\r\n\r\n", 400),
        (b"GET / HTTP/2.0\r\nHost: t\r\n\r\n", 505),
        (b"GET / HTTP/1.1\r\nHost: t\nX: y\r\n\r\n", 400),
        (
            b"POST /_/echo HTTP/1.1\r\nHost: t\r\nContent-Length: 3\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n0\r\n\r\n",
            400,
        ),
        (b"POST /_/echo HTTP/1.1\r\nHost: t\r\nContent-Length: 3\r\nContent-Length: 4\r\n\r\nabc", 400),
        (b"POST /_/echo HTTP/1.1\r\nHost: t\r\nTransfer-Encoding: gzip\r\n\r\n", 501),
        (b"POST /_/echo HTTP/1.1\r\nHost: t\r\nTransfer-Encoding: chunked\r\n\r\nzz\r\n", 400),
        (b"GET /%zz HTTP/1.1\r\nHost: t\r\n\r\n", 400),
        (b"GET /%00 HTTP/1.1\r\nHost: t\r\n\r\n", 400),
    ];
    for (req, want) in cases {
        let mut c = Client::connect(f.addr);
        c.send(req);
        let r = c.read_response(false).unwrap_or_else(|| panic!("no response to {:?}", String::from_utf8_lossy(req)));
        assert_eq!(r.status, *want, "{:?}", String::from_utf8_lossy(req));
        assert_eq!(r.header("connection"), Some("close"));
        assert!(c.closed(), "connection left open after {:?}", String::from_utf8_lossy(req));
    }
    assert!(f.server.stats().bad_requests.load(Ordering::Relaxed) >= cases.len() as u64);
}

#[test]
fn request_smuggling_attempt_does_not_create_a_second_request() {
    let f = start();
    // A front-end that used Content-Length and a back-end that used Transfer-Encoding would disagree
    // on this; here it is refused outright, and the smuggled request never runs.
    let mut c = Client::connect(f.addr);
    c.send(b"POST /_/echo HTTP/1.1\r\nHost: t\r\nContent-Length: 44\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\nGET /_/hello/smuggled HTTP/1.1\r\nHost: t\r\n\r\n");
    let r = c.read_response(false).unwrap();
    assert_eq!(r.status, 400);
    assert!(c.closed());
    assert_eq!(f.server.stats().requests.load(Ordering::Relaxed), 0);
}

#[test]
fn size_limits() {
    let f = start_with(|c| c.limits.max_body = 1000, |_| {});
    // Exactly at the limit works, one over does not.
    let ok = vec![b'x'; 1000];
    let mut req = b"POST /_/echo HTTP/1.1\r\nHost: t\r\nContent-Length: 1000\r\n\r\n".to_vec();
    req.extend(&ok);
    assert_eq!(raw(f.addr, &req).body.len(), 1000);
    let r = raw(f.addr, b"POST /_/echo HTTP/1.1\r\nHost: t\r\nContent-Length: 1001\r\n\r\n");
    assert_eq!(r.status, 413);
    // Chunked bodies are counted too.
    let mut req = b"POST /_/echo HTTP/1.1\r\nHost: t\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec();
    for _ in 0..3 {
        req.extend(b"190\r\n");
        req.extend(vec![b'y'; 0x190]);
        req.extend(b"\r\n");
    }
    req.extend(b"0\r\n\r\n");
    assert_eq!(raw(f.addr, &req).status, 413);

    let r = raw(f.addr, format!("GET /{} HTTP/1.1\r\nHost: t\r\n\r\n", "a".repeat(9000)).as_bytes());
    assert_eq!(r.status, 414);
    let r = raw(f.addr, format!("GET / HTTP/1.1\r\nHost: t\r\nX-Big: {}\r\n\r\n", "a".repeat(20_000)).as_bytes());
    assert_eq!(r.status, 431);
    let many: String = (0..150).map(|i| format!("X-{i}: v\r\n")).collect();
    assert_eq!(raw(f.addr, format!("GET / HTTP/1.1\r\nHost: t\r\n{many}\r\n").as_bytes()).status, 431);
}

#[test]
fn slow_clients_are_timed_out() {
    let f = start_with(
        |c| {
            c.idle_timeout = Duration::from_millis(300);
            c.head_timeout = Duration::from_millis(300);
            c.body_timeout = Duration::from_millis(300);
        },
        |_| {},
    );
    // Idle keep-alive connection: closed quietly.
    let mut c = Client::connect(f.addr);
    assert_eq!(c.get("/_/hello/a").status, 200);
    let t = Instant::now();
    assert!(c.closed());
    assert!(t.elapsed() < Duration::from_secs(2));

    // A head that never finishes: 408.
    let mut c = Client::connect(f.addr);
    c.send(b"GET / HTTP/1.1\r\nHost: t\r\nX-Slow: ");
    let r = c.read_response(false).unwrap();
    assert_eq!(r.status, 408);
    assert!(c.closed());

    // A body that never arrives: 408.
    let mut c = Client::connect(f.addr);
    c.send(b"POST /_/echo HTTP/1.1\r\nHost: t\r\nContent-Length: 100\r\n\r\nonly a little");
    assert_eq!(c.read_response(false).unwrap().status, 408);

    // Dribbling one byte at a time does not extend the head deadline.
    let mut c = Client::connect(f.addr);
    let t = Instant::now();
    let req = b"GET /_/hello/slow HTTP/1.1\r\nHost: t\r\n\r\n";
    let mut got = None;
    for b in req.iter().cycle().take(200) {
        if c.s.write_all_ok(std::slice::from_ref(b)).is_err() {
            break;
        }
        std::thread::sleep(Duration::from_millis(40));
        if t.elapsed() > Duration::from_secs(3) {
            break;
        }
        c.s.set_read_timeout(Some(Duration::from_millis(1))).unwrap();
        let mut tmp = [0u8; 512];
        if let Ok(n) = std::io::Read::read(&mut c.s, &mut tmp) {
            if n > 0 {
                got = Some(String::from_utf8_lossy(&tmp[..n]).into_owned());
                break;
            }
        }
    }
    let got = got.expect("the server should have answered 408 while the client was still dribbling");
    assert!(got.starts_with("HTTP/1.1 408"), "{got}");
}

trait WriteOk {
    fn write_all_ok(&mut self, b: &[u8]) -> std::io::Result<()>;
}

impl WriteOk for std::net::TcpStream {
    fn write_all_ok(&mut self, b: &[u8]) -> std::io::Result<()> {
        std::io::Write::write_all(self, b)
    }
}

#[test]
fn a_full_queue_answers_503_at_once() {
    let f = start_with(
        |c| {
            c.threads = 1;
            c.queue = 1;
        },
        |_| {},
    );
    // The one worker is busy for a second.
    let addr = f.addr;
    let busy = std::thread::spawn(move || raw(addr, b"GET /_/sleep?ms=1000 HTTP/1.1\r\nHost: t\r\n\r\n").status);
    assert!(eventually(5, || f.server.stats().requests.load(Ordering::Relaxed) == 1
        || f.server.stats().connections.load(Ordering::Relaxed) >= 1));
    std::thread::sleep(Duration::from_millis(100));
    // This one waits in the queue.
    let queued = Client::connect(f.addr);
    std::thread::sleep(Duration::from_millis(100));
    // This one finds the queue full.
    let t = Instant::now();
    let mut third = Client::connect(f.addr);
    let r = third.read_response(false).unwrap();
    assert_eq!(r.status, 503);
    assert_eq!(r.header("retry-after"), Some("1"));
    assert!(t.elapsed() < Duration::from_millis(500), "the 503 must not wait for the busy worker");
    assert_eq!(f.server.stats().rejected.load(Ordering::Relaxed), 1);
    drop(queued);
    assert_eq!(busy.join().unwrap(), 200);
}

#[test]
fn clients_are_served_in_parallel() {
    let f = start_with(|c| c.threads = 8, |_| {});
    let t = Instant::now();
    let handles: Vec<_> = (0..8)
        .map(|_| {
            let addr = f.addr;
            std::thread::spawn(move || raw(addr, b"GET /_/sleep?ms=400 HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n").status)
        })
        .collect();
    for h in handles {
        assert_eq!(h.join().unwrap(), 200);
    }
    // Serial service would take 3.2 s.
    assert!(t.elapsed() < Duration::from_millis(1500), "{:?}", t.elapsed());
}

#[test]
fn many_clients_each_get_their_own_answers() {
    let f = start_with(
        |c| {
            c.threads = 16;
            c.queue = 256;
        },
        |_| {},
    );
    let handles: Vec<_> = (0..48)
        .map(|id| {
            let addr = f.addr;
            std::thread::spawn(move || {
                let mut c = Client::connect(addr);
                for i in 0..25 {
                    let body = format!("client {id} request {i} {}", "z".repeat(id * 7 + i));
                    c.send(
                        format!("POST /_/echo HTTP/1.1\r\nHost: t\r\nContent-Length: {}\r\n\r\n{body}", body.len()).as_bytes(),
                    );
                    assert_eq!(c.read_response(false).unwrap().text(), body);
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    assert_eq!(f.server.stats().requests.load(Ordering::Relaxed), 48 * 25);
}

#[test]
fn a_panicking_handler_gives_500_and_the_server_lives() {
    let f = start();
    let mut c = Client::connect(f.addr);
    assert_eq!(c.get("/_/panic").status, 500);
    assert_eq!(c.get("/_/hello/still-here").text(), "hello, still-here\n");
    assert_eq!(f.server.stats().handler_panics.load(Ordering::Relaxed), 1);
}

#[test]
fn max_requests_per_connection_closes_the_connection() {
    let f = start_with(|c| c.max_requests = 3, |_| {});
    let mut c = Client::connect(f.addr);
    assert!(c.get("/_/hello/1").header("connection").is_none());
    assert!(c.get("/_/hello/2").header("connection").is_none());
    assert_eq!(c.get("/_/hello/3").header("connection"), Some("close"));
    assert!(c.closed());
}

#[test]
fn large_responses_arrive_whole() {
    let f = start();
    let mut c = Client::connect(f.addr);
    let r = c.get("/_/big?mib=32");
    assert_eq!(r.body.len(), 32 << 20);
    assert!(r.body.iter().all(|b| *b == b'x'));
    let r = c.get("/_/stream?n=200&size=65536");
    assert_eq!(r.body.len(), 200 * 65536);
}

#[test]
fn header_injection_through_the_url_is_not_possible() {
    let f = start();
    let r = raw(f.addr, b"GET /_/hello/x%0d%0aSet-Cookie:%20evil=1 HTTP/1.1\r\nHost: t\r\n\r\n");
    assert_eq!(r.status, 200);
    assert!(r.header("set-cookie").is_none());
    assert!(r.text().contains("Set-Cookie"), "the text is in the body, not the headers");
}

#[test]
fn graceful_shutdown_finishes_work_and_closes_idle_connections() {
    let f = start();
    let addr = f.addr;
    let working = std::thread::spawn(move || raw(addr, b"GET /_/sleep?ms=500 HTTP/1.1\r\nHost: t\r\n\r\n"));
    let mut idle = Client::connect(f.addr);
    assert_eq!(idle.get("/_/hello/a").status, 200);
    assert!(eventually(5, || f.server.stats().requests.load(Ordering::Relaxed) == 2));
    let t = Instant::now();
    f.server.shutdown();
    // The slow request completed, and shutdown did not wait for the 5 s idle timeout.
    assert_eq!(working.join().unwrap().text(), "slept\n");
    assert!(t.elapsed() < Duration::from_secs(3), "{:?}", t.elapsed());
    assert!(idle.closed());
    assert!(std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(300)).is_err(), "the port must be closed");
}

#[test]
fn responses_use_correct_status_lines_for_every_status_the_server_sends() {
    let f = start();
    for (req, line) in [
        (&b"GET /_/hello/a HTTP/1.1\r\nHost: t\r\n\r\n"[..], "HTTP/1.1 200 OK\r\n"),
        (b"GET /nope HTTP/1.1\r\nHost: t\r\n\r\n", "HTTP/1.1 404 Not Found\r\n"),
        (b"OPTIONS /_/echo HTTP/1.1\r\nHost: t\r\n\r\n", "HTTP/1.1 204 No Content\r\n"),
        (b"GET / HTTP/1.1\r\n\r\n", "HTTP/1.1 400 Bad Request\r\n"),
    ] {
        let r = raw(f.addr, req);
        assert!(r.raw_head.starts_with(&line.replace("\r\n", "\n")), "{}", r.raw_head);
    }
}
