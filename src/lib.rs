//! An HTTP/1.1 server on `std::net` alone.
//!
//! ```
//! use tinyhttp::{Config, Response, Router, Server};
//!
//! let router = Router::new().get("/hello/:name", |_, p| Response::text(200, format!("hello {}\n", p.get("name").unwrap())));
//! let server = Server::start("127.0.0.1:0", router, Config::default()).unwrap();
//! let addr = server.local_addr();
//! // ... talk to `addr` ...
//! server.shutdown();
//! # let _ = addr;
//! ```
//!
//! * [`request`] parses the head of a request, strictly.
//! * [`chunked`] decodes and encodes chunked bodies.
//! * [`response`] writes responses with correct framing.
//! * [`router`] picks a handler by method and path.
//! * [`files`] serves a directory.
//! * [`server`] runs the sockets, threads, timeouts and shutdown.

pub mod chunked;
pub mod date;
pub mod demo;
pub mod files;
pub mod request;
pub mod response;
pub mod router;
pub mod server;

pub use files::FileServer;
pub use request::{Limits, Method, Request, Version};
pub use response::{Body, Response};
pub use router::{Params, Router};
pub use server::{Config, LogEntry, Server, Stats};
