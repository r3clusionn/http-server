//! Responses and how they are written to the wire.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};

use crate::chunked::ChunkWriter;
use crate::request::{Method, Version};

/// A body producer: it writes the whole body to the writer it is given.
pub type StreamFn = Box<dyn FnOnce(&mut dyn Write) -> io::Result<()> + Send>;

/// What is sent after the headers.
pub enum Body {
    Empty,
    Bytes(Vec<u8>),
    /// `len` bytes of a file starting at `offset`.
    File {
        file: File,
        offset: u64,
        len: u64,
    },
    /// Written by the closure as it goes: sent with chunked transfer coding on HTTP/1.1, and
    /// until the connection closes on HTTP/1.0.
    Stream(StreamFn),
}

pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Body,
}

pub fn reason(status: u16) -> &'static str {
    match status {
        100 => "Continue",
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        206 => "Partial Content",
        301 => "Moved Permanently",
        302 => "Found",
        304 => "Not Modified",
        307 => "Temporary Redirect",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        411 => "Length Required",
        413 => "Content Too Large",
        414 => "URI Too Long",
        416 => "Range Not Satisfiable",
        417 => "Expectation Failed",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        503 => "Service Unavailable",
        505 => "HTTP Version Not Supported",
        _ => "Unknown",
    }
}

impl Response {
    pub fn new(status: u16) -> Response {
        Response { status, headers: Vec::new(), body: Body::Empty }
    }

    pub fn bytes(status: u16, content_type: &str, body: impl Into<Vec<u8>>) -> Response {
        Response { status, headers: vec![("Content-Type".into(), content_type.into())], body: Body::Bytes(body.into()) }
    }

    pub fn text(status: u16, body: impl Into<String>) -> Response {
        Response::bytes(status, "text/plain; charset=utf-8", body.into().into_bytes())
    }

    pub fn html(status: u16, body: impl Into<String>) -> Response {
        Response::bytes(status, "text/html; charset=utf-8", body.into().into_bytes())
    }

    pub fn json(status: u16, body: impl Into<String>) -> Response {
        Response::bytes(status, "application/json", body.into().into_bytes())
    }

    pub fn redirect(status: u16, location: &str) -> Response {
        Response::new(status).header("Location", location)
    }

    /// A plain-text error with the reason as its body.
    pub fn error(status: u16) -> Response {
        Response::text(status, format!("{status} {}\n", reason(status)))
    }

    pub fn header(mut self, name: &str, value: &str) -> Response {
        self.headers.push((name.to_string(), value.to_string()));
        self
    }

    /// Replaces any header of this name.
    pub fn set_header(&mut self, name: &str, value: &str) {
        self.headers.retain(|(n, _)| !n.eq_ignore_ascii_case(name));
        self.headers.push((name.to_string(), value.to_string()));
    }

    pub fn stream(
        status: u16,
        content_type: &str,
        f: impl FnOnce(&mut dyn Write) -> io::Result<()> + Send + 'static,
    ) -> Response {
        Response { status, headers: vec![("Content-Type".into(), content_type.into())], body: Body::Stream(Box::new(f)) }
    }
}

/// Header names that the writer sets itself; a handler's copies are dropped.
fn is_reserved(name: &str) -> bool {
    ["content-length", "transfer-encoding", "connection", "date", "server"].iter().any(|r| name.eq_ignore_ascii_case(r))
}

/// A status that never has a body.
fn bodyless(status: u16) -> bool {
    status < 200 || status == 204 || status == 304
}

/// Header names or values with line breaks would let a handler inject headers or split the
/// response; the writer refuses them.
fn clean(s: &str) -> bool {
    !s.bytes().any(|b| b == b'\r' || b == b'\n' || b == 0)
}

/// Writes a response. Returns whether the connection can be used for another request.
pub fn write_response<W: Write>(
    w: &mut W,
    mut resp: Response,
    method: &Method,
    version: Version,
    keep_alive: bool,
    date: &str,
) -> io::Result<bool> {
    let head_only = *method == Method::Head;
    let mut keep = keep_alive;
    if resp.headers.iter().any(|(n, v)| !clean(n) || !clean(v)) {
        resp = Response::error(500);
    }
    let no_body = bodyless(resp.status);
    let mut chunked = false;
    let mut length: Option<u64> = None;
    if !no_body {
        match &resp.body {
            Body::Empty => length = Some(0),
            Body::Bytes(b) => length = Some(b.len() as u64),
            Body::File { len, .. } => length = Some(*len),
            Body::Stream(_) => {
                if version == Version::Http11 {
                    chunked = true;
                } else {
                    // Without chunking the end of the body is the end of the connection.
                    keep = false;
                }
            }
        }
    }

    let mut head = Vec::with_capacity(256);
    write!(head, "HTTP/1.1 {} {}\r\n", resp.status, reason(resp.status))?;
    for (n, v) in resp.headers.iter().filter(|(n, _)| !is_reserved(n)) {
        write!(head, "{n}: {v}\r\n")?;
    }
    write!(head, "Date: {date}\r\nServer: tinyhttp/{}\r\n", env!("CARGO_PKG_VERSION"))?;
    if let Some(n) = length {
        // A 304 and a HEAD answer repeat the length the GET would have had; other bodyless
        // answers must not carry one.
        write!(head, "Content-Length: {n}\r\n")?;
    }
    if chunked && !head_only {
        head.extend_from_slice(b"Transfer-Encoding: chunked\r\n");
    }
    if !keep {
        head.extend_from_slice(b"Connection: close\r\n");
    } else if version == Version::Http10 {
        head.extend_from_slice(b"Connection: keep-alive\r\n");
    }
    head.extend_from_slice(b"\r\n");

    if no_body || head_only {
        w.write_all(&head)?;
        w.flush()?;
        return Ok(keep);
    }
    match resp.body {
        Body::Empty => w.write_all(&head)?,
        Body::Bytes(b) => {
            // One write for head and body keeps small responses in one packet.
            head.extend_from_slice(&b);
            w.write_all(&head)?;
        }
        Body::File { mut file, offset, len } => {
            w.write_all(&head)?;
            file.seek(SeekFrom::Start(offset))?;
            let copied = io::copy(&mut file.take(len), w)?;
            if copied != len {
                // The file shrank while it was being sent; the promised length cannot be met.
                w.flush()?;
                return Ok(false);
            }
        }
        Body::Stream(f) => {
            w.write_all(&head)?;
            if chunked {
                let mut cw = ChunkWriter::new(w);
                f(&mut cw)?;
                cw.finish()?;
            } else {
                f(w)?;
            }
        }
    }
    w.flush()?;
    Ok(keep)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(resp: Response, method: Method, version: Version, keep: bool) -> (String, bool) {
        let mut out = Vec::new();
        let k = write_response(&mut out, resp, &method, version, keep, "DATE").unwrap();
        (String::from_utf8_lossy(&out).into_owned(), k)
    }

    #[test]
    fn plain_response() {
        let (s, k) = render(Response::text(200, "hi"), Method::Get, Version::Http11, true);
        assert!(k);
        assert!(s.starts_with("HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nDate: DATE\r\nServer: tinyhttp/"));
        assert!(s.contains("\r\nContent-Length: 2\r\n"));
        assert!(s.ends_with("\r\n\r\nhi"));
        assert!(!s.contains("Connection"));
    }

    #[test]
    fn head_has_the_headers_of_get_and_no_body() {
        let (s, _) = render(Response::text(200, "hello"), Method::Head, Version::Http11, true);
        assert!(s.contains("Content-Length: 5\r\n") && s.ends_with("\r\n\r\n"));
        let (s, _) = render(Response::stream(200, "text/plain", |w| w.write_all(b"x")), Method::Head, Version::Http11, true);
        assert!(!s.contains("Content-Length") && !s.contains("chunked") && s.ends_with("\r\n\r\n"));
    }

    #[test]
    fn bodyless_statuses_have_neither_body_nor_length() {
        for status in [204u16, 304, 100] {
            let (s, _) = render(Response::text(status, "ignored"), Method::Get, Version::Http11, true);
            assert!(!s.contains("Content-Length") && s.ends_with("\r\n\r\n"), "{status}: {s}");
        }
    }

    #[test]
    fn streams_are_chunked_on_http_11_and_close_delimited_on_http_10() {
        let make = || Response::stream(200, "text/plain", |w| w.write_all(b"abc"));
        let (s, k) = render(make(), Method::Get, Version::Http11, true);
        assert!(k && s.contains("Transfer-Encoding: chunked\r\n") && s.ends_with("\r\n\r\n3\r\nabc\r\n0\r\n\r\n"));
        let (s, k) = render(make(), Method::Get, Version::Http10, true);
        assert!(!k && s.contains("Connection: close") && !s.contains("chunked") && s.ends_with("\r\n\r\nabc"));
    }

    #[test]
    fn connection_header_follows_keep_alive() {
        let (s, k) = render(Response::text(200, "x"), Method::Get, Version::Http11, false);
        assert!(!k && s.contains("Connection: close\r\n"));
        let (s, k) = render(Response::text(200, "x"), Method::Get, Version::Http10, true);
        assert!(k && s.contains("Connection: keep-alive\r\n"));
    }

    #[test]
    fn handlers_cannot_override_framing_or_inject_headers() {
        let r = Response::text(200, "abc")
            .header("Content-Length", "999")
            .header("Transfer-Encoding", "chunked")
            .header("Connection", "upgrade");
        let (s, _) = render(r, Method::Get, Version::Http11, true);
        assert!(s.contains("Content-Length: 3\r\n") && !s.contains("999") && !s.contains("upgrade") && !s.contains("chunked"));
        let r = Response::text(200, "abc").header("X-Evil", "a\r\nSet-Cookie: pwned");
        let (s, k) = render(r, Method::Get, Version::Http11, true);
        assert!(s.starts_with("HTTP/1.1 500 ") && !s.contains("pwned") && k);
        let r = Response::text(200, "abc").header("X\r\nEvil", "a");
        assert!(render(r, Method::Get, Version::Http11, true).0.starts_with("HTTP/1.1 500 "));
    }

    #[test]
    fn file_bodies_send_only_the_requested_range() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f.bin");
        std::fs::write(&p, b"0123456789").unwrap();
        let body = Body::File { file: File::open(&p).unwrap(), offset: 3, len: 4 };
        let (s, _) = render(Response { status: 206, headers: vec![], body }, Method::Get, Version::Http11, true);
        assert!(s.contains("Content-Length: 4\r\n") && s.ends_with("\r\n\r\n3456"));
    }

    #[test]
    fn a_file_that_shrinks_closes_the_connection() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f.bin");
        std::fs::write(&p, b"01234").unwrap();
        let body = Body::File { file: File::open(&p).unwrap(), offset: 0, len: 10 };
        let (_, k) = render(Response { status: 200, headers: vec![], body }, Method::Get, Version::Http11, true);
        assert!(!k);
    }

    #[test]
    fn redirect_and_set_header() {
        let mut r = Response::redirect(301, "/x/");
        r.set_header("location", "/y/");
        assert_eq!(r.headers.len(), 1);
        let (s, _) = render(r, Method::Get, Version::Http11, true);
        assert!(s.starts_with("HTTP/1.1 301 Moved Permanently\r\nlocation: /y/\r\n"));
    }
}
