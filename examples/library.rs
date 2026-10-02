//! The example from the README: routes, a streaming handler and a file server fallback.
//! Run it with `cargo run --release --example library` and open http://127.0.0.1:8080/api/hello/you

use std::sync::Arc;

use tinyhttp::{Config, FileServer, Response, Router, Server};

fn main() {
    let files = Arc::new(FileServer::new(".").unwrap());
    let router = Router::new()
        .get("/api/hello/:name", |_, p| Response::text(200, format!("hello, {}\n", p.get("name").unwrap())))
        .post("/api/echo", |req, _| Response::bytes(200, "application/octet-stream", req.body.clone()))
        .get("/api/count", |_, _| {
            Response::stream(200, "text/plain", |w| {
                for i in 0..5 {
                    writeln!(w, "{i}")?; // sent as a chunk each time
                }
                Ok(())
            })
        })
        .fallback(move |req, _| files.handle(req));

    let server = Server::start("127.0.0.1:8080", router, Config::default()).unwrap();
    println!("listening on http://{} (press Enter to stop)", server.local_addr());
    let _ = std::io::stdin().read_line(&mut String::new());
    server.shutdown();
}
