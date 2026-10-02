//! Demonstration endpoints under `/_/`, used by the tests, the load generator and `--demo`.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use crate::request::Request;
use crate::response::Response;
use crate::router::Router;
use crate::server::Stats;

fn number(req: &Request, name: &str, default: u64, max: u64) -> u64 {
    req.query_param(name).and_then(|v| v.parse::<u64>().ok()).unwrap_or(default).min(max)
}

/// The demo routes. `stats` backs `/_/stats`.
pub fn router(stats: Option<Arc<Stats>>) -> Router {
    let mut r = Router::new()
        .get("/_/hello/:name", |_, p| Response::text(200, format!("hello, {}\n", p.get("name").unwrap_or(""))))
        .post("/_/echo", |req, _| {
            let ct = req.header("content-type").unwrap_or("application/octet-stream").to_string();
            Response::bytes(200, &ct, req.body.clone())
        })
        .get("/_/headers", |req, _| {
            let mut s = format!("{} {}\n", req.method, req.target);
            for (n, v) in &req.headers {
                s.push_str(&format!("{n}: {v}\n"));
            }
            Response::text(200, s)
        })
        .get("/_/stream", |req, _| {
            let (n, size) = (number(req, "n", 5, 10_000), number(req, "size", 1024, 1 << 20) as usize);
            let delay = number(req, "delay_ms", 0, 5_000);
            Response::stream(200, "application/octet-stream", move |w| {
                for i in 0..n {
                    w.write_all(&vec![b'a' + (i % 26) as u8; size])?;
                    w.flush()?;
                    if delay > 0 {
                        std::thread::sleep(Duration::from_millis(delay));
                    }
                }
                Ok(())
            })
        })
        .get("/_/sleep", |req, _| {
            std::thread::sleep(Duration::from_millis(number(req, "ms", 100, 10_000)));
            Response::text(200, "slept\n")
        })
        .get("/_/big", |req, _| {
            let mib = number(req, "mib", 1, 1024) as usize;
            Response::bytes(200, "application/octet-stream", vec![b'x'; mib << 20])
        })
        .get("/_/panic", |_, _| panic!("the panic endpoint panics on purpose"));
    if let Some(stats) = stats {
        r = r.get("/_/stats", move |_, _| {
            Response::json(
                200,
                format!(
                    "{{\"connections\":{},\"requests\":{},\"rejected\":{},\"bad_requests\":{},\"timeouts\":{},\"handler_panics\":{}}}\n",
                    stats.connections.load(Ordering::Relaxed),
                    stats.requests.load(Ordering::Relaxed),
                    stats.rejected.load(Ordering::Relaxed),
                    stats.bad_requests.load(Ordering::Relaxed),
                    stats.timeouts.load(Ordering::Relaxed),
                    stats.handler_panics.load(Ordering::Relaxed)
                ),
            )
        });
    }
    r
}
