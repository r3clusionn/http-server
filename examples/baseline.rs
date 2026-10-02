//! A comparison server built on the `tiny_http` crate: every request gets the same body as
//! `/_/hello/world` (or, with `--big`, a 1 MiB body), so `loadgen` can compare like with like.
//! `baseline [PORT] [--big] [--threads N]`

use std::io::Cursor;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let port: u16 = args.first().and_then(|a| a.parse().ok()).unwrap_or(18090);
    let big = args.iter().any(|a| a == "--big");
    let threads: usize =
        args.iter().position(|a| a == "--threads").and_then(|i| args.get(i + 1)).and_then(|v| v.parse().ok()).unwrap_or(16);
    let server = std::sync::Arc::new(tiny_http::Server::http(format!("127.0.0.1:{port}")).unwrap());
    println!("baseline on 127.0.0.1:{port} with {threads} threads");
    let body: std::sync::Arc<Vec<u8>> = std::sync::Arc::new(if big { vec![b'x'; 1 << 20] } else { b"hello, world\n".to_vec() });
    let handles: Vec<_> = (0..threads)
        .map(|_| {
            let (server, body) = (server.clone(), body.clone());
            std::thread::spawn(move || {
                for request in server.incoming_requests() {
                    // tiny_http switches to chunked coding above 32 KiB; keep Content-Length like tinyhttp does.
                    let response = tiny_http::Response::new(
                        tiny_http::StatusCode(200),
                        vec![tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"text/plain"[..]).unwrap()],
                        Cursor::new(body.as_slice().to_vec()),
                        Some(body.len()),
                        None,
                    )
                    .with_chunked_threshold(usize::MAX);
                    let _ = request.respond(response);
                }
            })
        })
        .collect();
    for h in handles {
        let _ = h.join();
    }
}
