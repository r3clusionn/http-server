//! Requests and the parser for their head (request line and headers).
//!
//! The parser is strict on purpose. Anything that could make two HTTP implementations disagree
//! about where a request ends (bare line feeds, spaces before a colon, folded headers, two
//! different `Content-Length` values, `Content-Length` together with `Transfer-Encoding`) is an
//! error rather than something to guess about.

use std::fmt;

/// Limits on what a client may send.
#[derive(Clone, Debug)]
pub struct Limits {
    /// Request line plus all headers, in bytes.
    pub max_head: usize,
    pub max_request_line: usize,
    pub max_headers: usize,
    /// Request body, in bytes.
    pub max_body: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits { max_head: 16 * 1024, max_request_line: 8 * 1024, max_headers: 100, max_body: 1024 * 1024 }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Method {
    Get,
    Head,
    Post,
    Put,
    Delete,
    Options,
    Patch,
    Other(String),
}

impl Method {
    pub fn parse(s: &str) -> Method {
        match s {
            "GET" => Method::Get,
            "HEAD" => Method::Head,
            "POST" => Method::Post,
            "PUT" => Method::Put,
            "DELETE" => Method::Delete,
            "OPTIONS" => Method::Options,
            "PATCH" => Method::Patch,
            other => Method::Other(other.to_string()),
        }
    }

    pub fn as_str(&self) -> &str {
        match self {
            Method::Get => "GET",
            Method::Head => "HEAD",
            Method::Post => "POST",
            Method::Put => "PUT",
            Method::Delete => "DELETE",
            Method::Options => "OPTIONS",
            Method::Patch => "PATCH",
            Method::Other(s) => s,
        }
    }
}

impl fmt::Display for Method {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Version {
    Http10,
    Http11,
}

/// How the request body is framed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BodyKind {
    None,
    Length(u64),
    Chunked,
}

/// A parse failure, with the status the server answers with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParseError {
    pub status: u16,
    pub reason: &'static str,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.status, self.reason)
    }
}

impl std::error::Error for ParseError {}

fn err(status: u16, reason: &'static str) -> ParseError {
    ParseError { status, reason }
}

/// A parsed request.
#[derive(Clone, Debug)]
pub struct Request {
    pub method: Method,
    /// The request target exactly as sent (`/a/b?x=1`).
    pub target: String,
    /// The path part of the target, percent-decoded.
    pub path: String,
    /// The raw query string, without the `?`.
    pub query: Option<String>,
    pub version: Version,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub peer: Option<std::net::SocketAddr>,
}

impl Request {
    /// First header with this name (case-insensitive).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    /// Whether the connection stays open after the response.
    pub fn keep_alive(&self) -> bool {
        let tokens = |v: &str| v.split(',').map(|t| t.trim().to_ascii_lowercase()).collect::<Vec<_>>();
        let conn: Vec<String> =
            self.headers.iter().filter(|(n, _)| n.eq_ignore_ascii_case("connection")).flat_map(|(_, v)| tokens(v)).collect();
        match self.version {
            Version::Http11 => !conn.iter().any(|t| t == "close"),
            Version::Http10 => conn.iter().any(|t| t == "keep-alive"),
        }
    }

    /// Value of one query parameter, percent-decoded (`+` is a space).
    pub fn query_param(&self, name: &str) -> Option<String> {
        self.query.as_deref()?.split('&').filter_map(|kv| kv.split_once('=').or(Some((kv, "")))).find_map(|(k, v)| {
            let k = percent_decode(&k.replace('+', " "), false).ok()?;
            (k == name).then(|| percent_decode(&v.replace('+', " "), false).unwrap_or_default())
        })
    }
}

/// The head of a request, before its body is read.
#[derive(Debug)]
pub struct Head {
    pub request: Request,
    pub body: BodyKind,
    pub expect_continue: bool,
}

/// Result of looking at the bytes received so far.
#[derive(Debug)]
pub enum Parsed {
    /// More bytes are needed.
    Incomplete,
    /// A complete head; `consumed` bytes of the buffer belong to it.
    Complete { head: Box<Head>, consumed: usize },
}

fn is_token_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)
}

fn is_field_value_byte(b: u8) -> bool {
    b == b'\t' || (b >= 0x20 && b != 0x7f)
}

/// Decodes `%XX` sequences. Invalid sequences and (unless `allow_nul`) NUL bytes are errors. The
/// result must be valid UTF-8.
pub fn percent_decode(s: &str, allow_nul: bool) -> Result<String, ParseError> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let hex = |c: u8| (c as char).to_digit(16);
            let (Some(h), Some(l)) = (b.get(i + 1).and_then(|c| hex(*c)), b.get(i + 2).and_then(|c| hex(*c))) else {
                return Err(err(400, "bad percent-encoding"));
            };
            out.push((h * 16 + l) as u8);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    if !allow_nul && out.contains(&0) {
        return Err(err(400, "NUL in path"));
    }
    String::from_utf8(out).map_err(|_| err(400, "path is not UTF-8"))
}

fn find_head_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4)
}

/// Parses the head at the start of `buf`.
pub fn parse_head(buf: &[u8], limits: &Limits) -> Result<Parsed, ParseError> {
    // A line feed that is not part of a CRLF is an error as soon as it is seen, even before the
    // end of the head arrives.
    let end = find_head_end(buf);
    let scanned = &buf[..end.unwrap_or(buf.len()).min(limits.max_head + 4)];
    for (i, b) in scanned.iter().enumerate() {
        if *b == b'\n' && (i == 0 || scanned[i - 1] != b'\r') {
            return Err(err(400, "bare line feed"));
        }
    }
    // A request line that is too long is reported as such even before its end arrives, and before
    // the size of the whole head is judged.
    let first_line = buf.iter().position(|b| *b == b'\r').unwrap_or(buf.len());
    if first_line > limits.max_request_line {
        return Err(err(414, "request line too long"));
    }
    let Some(end) = end else {
        if buf.len() > limits.max_head {
            return Err(err(431, "request head too large"));
        }
        return Ok(Parsed::Incomplete);
    };
    if end > limits.max_head {
        return Err(err(431, "request head too large"));
    }
    let head = &buf[..end - 4];
    let mut lines = head.split(|b| *b == b'\n').map(|l| l.strip_suffix(b"\r").unwrap_or(l));
    let line = lines.next().unwrap_or(b"");
    if line.is_empty() {
        // RFC 9112 allows ignoring empty lines before the request line; refusing is simpler and safe.
        return Err(err(400, "empty request line"));
    }
    if line.len() > limits.max_request_line {
        return Err(err(414, "request line too long"));
    }
    let line = std::str::from_utf8(line).map_err(|_| err(400, "request line is not UTF-8"))?;
    let mut parts = line.split(' ');
    let (Some(method), Some(target), Some(version), None) = (parts.next(), parts.next(), parts.next(), parts.next()) else {
        return Err(err(400, "malformed request line"));
    };
    if method.is_empty() || !method.bytes().all(is_token_char) {
        return Err(err(400, "bad method"));
    }
    if target.is_empty() || target.bytes().any(|b| b < 0x21 || b == 0x7f) {
        return Err(err(400, "bad request target"));
    }
    let version = match version {
        "HTTP/1.1" => Version::Http11,
        "HTTP/1.0" => Version::Http10,
        v if v.starts_with("HTTP/")
            && v.len() == 8
            && v.as_bytes()[5].is_ascii_digit()
            && v.as_bytes()[6] == b'.'
            && v.as_bytes()[7].is_ascii_digit() =>
        {
            return Err(err(505, "HTTP version not supported"))
        }
        _ => return Err(err(400, "bad HTTP version")),
    };

    let mut headers: Vec<(String, String)> = Vec::new();
    for raw in lines {
        if headers.len() >= limits.max_headers {
            return Err(err(431, "too many headers"));
        }
        if raw.first().is_some_and(|b| *b == b' ' || *b == b'\t') {
            return Err(err(400, "folded header line"));
        }
        let Some(colon) = raw.iter().position(|b| *b == b':') else {
            return Err(err(400, "header without a colon"));
        };
        let (name, value) = (&raw[..colon], &raw[colon + 1..]);
        if name.is_empty() || !name.iter().copied().all(is_token_char) {
            return Err(err(400, "bad header name"));
        }
        if !value.iter().copied().all(is_field_value_byte) {
            return Err(err(400, "bad header value"));
        }
        let value = std::str::from_utf8(value)
            .map_err(|_| err(400, "header value is not UTF-8"))?
            .trim_matches(|c| c == ' ' || c == '\t');
        headers.push((String::from_utf8_lossy(name).into_owned(), value.to_string()));
    }

    let named = |n: &'static str| headers.iter().filter(move |(h, _)| h.eq_ignore_ascii_case(n)).map(|(_, v)| v.as_str());
    if version == Version::Http11 {
        let mut hosts = named("host");
        match (hosts.next(), hosts.next()) {
            (Some(h), None) if !h.is_empty() => {}
            _ => return Err(err(400, "HTTP/1.1 needs exactly one Host header")),
        }
    }

    let te: Vec<&str> = named("transfer-encoding").collect();
    let cl: Vec<&str> = named("content-length").collect();
    let body = if !te.is_empty() {
        if !cl.is_empty() {
            return Err(err(400, "both Content-Length and Transfer-Encoding"));
        }
        if version == Version::Http10 {
            return Err(err(400, "Transfer-Encoding in an HTTP/1.0 request"));
        }
        if te.len() != 1 || !te[0].eq_ignore_ascii_case("chunked") {
            return Err(err(501, "only chunked transfer encoding is supported"));
        }
        BodyKind::Chunked
    } else if !cl.is_empty() {
        // Several Content-Length values, or a comma-separated list, must all be the same number.
        let mut values = cl.iter().flat_map(|v| v.split(',')).map(str::trim);
        let first = values.next().unwrap_or("");
        if first.is_empty() || !first.bytes().all(|b| b.is_ascii_digit()) || first.len() > 18 {
            return Err(err(400, "bad Content-Length"));
        }
        if values.any(|v| v != first) {
            return Err(err(400, "conflicting Content-Length values"));
        }
        let n: u64 = first.parse().map_err(|_| err(400, "bad Content-Length"))?;
        if n == 0 {
            BodyKind::None
        } else {
            BodyKind::Length(n)
        }
    } else {
        BodyKind::None
    };
    let expect_continue = named("expect").any(|v| v.eq_ignore_ascii_case("100-continue")) && version == Version::Http11;

    let (path_raw, query) = match target.split_once('?') {
        Some((p, q)) => (p, Some(q.to_string())),
        None => (target, None),
    };
    // Absolute-form targets (`http://host/path`) are for proxies; origin-form and `*` only.
    if !path_raw.starts_with('/') && target != "*" {
        return Err(err(400, "only origin-form targets are supported"));
    }
    let path = percent_decode(path_raw, false)?;
    let request = Request {
        method: Method::parse(method),
        target: target.to_string(),
        path,
        query,
        version,
        headers,
        body: Vec::new(),
        peer: None,
    };
    Ok(Parsed::Complete { head: Box::new(Head { request, body, expect_continue }), consumed: end })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Result<Parsed, ParseError> {
        parse_head(s.as_bytes(), &Limits::default())
    }

    fn ok(s: &str) -> (Head, usize) {
        match parse(s).unwrap() {
            Parsed::Complete { head, consumed } => (*head, consumed),
            Parsed::Incomplete => panic!("incomplete"),
        }
    }

    fn status(s: &str) -> u16 {
        parse(s).unwrap_err().status
    }

    #[test]
    fn simple_get() {
        let (h, n) = ok("GET /a/b?x=1&y=two%20words HTTP/1.1\r\nHost: example.com\r\nAccept: */*\r\n\r\nrest");
        assert_eq!(n, 71);
        let r = h.request;
        assert_eq!((r.method.clone(), r.path.as_str(), r.query.as_deref()), (Method::Get, "/a/b", Some("x=1&y=two%20words")));
        assert_eq!(r.header("HOST"), Some("example.com"));
        assert_eq!(r.query_param("y").as_deref(), Some("two words"));
        assert_eq!(r.query_param("x").as_deref(), Some("1"));
        assert_eq!(r.query_param("z"), None);
        assert_eq!(h.body, BodyKind::None);
        assert!(r.keep_alive());
    }

    #[test]
    fn partial_input_is_incomplete_not_an_error() {
        let full = "GET / HTTP/1.1\r\nHost: a\r\n\r\n";
        for i in 0..full.len() {
            assert!(matches!(parse(&full[..i]), Ok(Parsed::Incomplete)), "prefix of {i} bytes");
        }
        assert!(matches!(parse(full), Ok(Parsed::Complete { .. })));
    }

    #[test]
    fn keep_alive_rules() {
        let k = |s: &str| ok(s).0.request.keep_alive();
        assert!(k("GET / HTTP/1.1\r\nHost: a\r\n\r\n"));
        assert!(!k("GET / HTTP/1.1\r\nHost: a\r\nConnection: close\r\n\r\n"));
        assert!(!k("GET / HTTP/1.1\r\nHost: a\r\nConnection: keep-alive, Close\r\n\r\n"));
        assert!(!k("GET / HTTP/1.0\r\n\r\n"));
        assert!(k("GET / HTTP/1.0\r\nConnection: Keep-Alive\r\n\r\n"));
    }

    #[test]
    fn framing_of_bodies() {
        let b = |s: &str| ok(s).0.body;
        assert_eq!(b("POST / HTTP/1.1\r\nHost: a\r\nContent-Length: 5\r\n\r\n"), BodyKind::Length(5));
        assert_eq!(b("POST / HTTP/1.1\r\nHost: a\r\nContent-Length: 0\r\n\r\n"), BodyKind::None);
        assert_eq!(b("POST / HTTP/1.1\r\nHost: a\r\nTransfer-Encoding: Chunked\r\n\r\n"), BodyKind::Chunked);
        // The same value twice is harmless.
        assert_eq!(b("POST / HTTP/1.1\r\nHost: a\r\nContent-Length: 7\r\nContent-Length: 7\r\n\r\n"), BodyKind::Length(7));
        assert_eq!(b("POST / HTTP/1.1\r\nHost: a\r\nContent-Length: 7, 7\r\n\r\n"), BodyKind::Length(7));
    }

    #[test]
    fn request_smuggling_shapes_are_rejected() {
        let h = "Host: a\r\n";
        assert_eq!(status(&format!("POST / HTTP/1.1\r\n{h}Content-Length: 4\r\nTransfer-Encoding: chunked\r\n\r\n")), 400);
        assert_eq!(status(&format!("POST / HTTP/1.1\r\n{h}Content-Length: 4\r\nContent-Length: 5\r\n\r\n")), 400);
        assert_eq!(status(&format!("POST / HTTP/1.1\r\n{h}Content-Length: 4, 5\r\n\r\n")), 400);
        assert_eq!(status(&format!("POST / HTTP/1.1\r\n{h}Content-Length: +4\r\n\r\n")), 400);
        assert_eq!(status(&format!("POST / HTTP/1.1\r\n{h}Content-Length: -4\r\n\r\n")), 400);
        assert_eq!(status(&format!("POST / HTTP/1.1\r\n{h}Content-Length: 0x10\r\n\r\n")), 400);
        assert_eq!(status(&format!("POST / HTTP/1.1\r\n{h}Content-Length: \r\n\r\n")), 400);
        assert_eq!(status(&format!("POST / HTTP/1.1\r\n{h}Content-Length: 99999999999999999999\r\n\r\n")), 400);
        assert_eq!(status(&format!("POST / HTTP/1.1\r\n{h}Transfer-Encoding: gzip, chunked\r\n\r\n")), 501);
        assert_eq!(
            status(&format!("POST / HTTP/1.1\r\n{h}Transfer-Encoding: chunked\r\nTransfer-Encoding: chunked\r\n\r\n")),
            501
        );
        assert_eq!(status("POST / HTTP/1.0\r\nTransfer-Encoding: chunked\r\n\r\n"), 400);
        // Whitespace before the colon, folded lines and bare line feeds.
        assert_eq!(status(&format!("GET / HTTP/1.1\r\n{h}Transfer-Encoding : chunked\r\n\r\n")), 400);
        assert_eq!(status(&format!("GET / HTTP/1.1\r\n{h}X: a\r\n b\r\n\r\n")), 400);
        assert_eq!(status(&format!("GET / HTTP/1.1\r\n{h}X: a\nY: b\r\n\r\n")), 400);
        assert_eq!(status("GET / HTTP/1.1\nHost: a\n\n"), 400);
    }

    #[test]
    fn malformed_heads() {
        assert_eq!(status("GET / HTTP/1.1\r\n\r\n"), 400); // no Host
        assert_eq!(status("GET / HTTP/1.1\r\nHost: a\r\nHost: b\r\n\r\n"), 400);
        assert_eq!(status("GET /  HTTP/1.1\r\nHost: a\r\n\r\n"), 400); // two spaces
        assert_eq!(status("GET / HTTP/1.1 x\r\nHost: a\r\n\r\n"), 400);
        assert_eq!(status("GET\r\nHost: a\r\n\r\n"), 400);
        assert_eq!(status("\r\nGET / HTTP/1.1\r\nHost: a\r\n\r\n"), 400);
        assert_eq!(status("G@T / HTTP/1.1\r\nHost: a\r\n\r\n"), 400);
        assert_eq!(status("GET / HTTP/2.0\r\nHost: a\r\n\r\n"), 505);
        assert_eq!(status("GET / HTTP/1.2\r\nHost: a\r\n\r\n"), 505);
        assert_eq!(status("GET / FTP/1.1\r\nHost: a\r\n\r\n"), 400);
        assert_eq!(status("GET / HTTP/1.1\r\nHost: a\r\nBad Name: x\r\n\r\n"), 400);
        assert_eq!(status("GET / HTTP/1.1\r\nHost: a\r\nNoColon\r\n\r\n"), 400);
        assert_eq!(status("GET / HTTP/1.1\r\nHost: a\r\nX: \u{1}\r\n\r\n"), 400);
        assert_eq!(status("GET http://a/ HTTP/1.1\r\nHost: a\r\n\r\n"), 400);
        assert_eq!(status("GET /%zz HTTP/1.1\r\nHost: a\r\n\r\n"), 400);
        assert_eq!(status("GET /%00 HTTP/1.1\r\nHost: a\r\n\r\n"), 400);
        assert_eq!(status("GET /%ff HTTP/1.1\r\nHost: a\r\n\r\n"), 400);
        // HTTP/1.0 needs no Host.
        assert!(parse("GET / HTTP/1.0\r\n\r\n").is_ok());
    }

    #[test]
    fn size_limits() {
        let l = Limits { max_head: 64, max_request_line: 32, max_headers: 3, max_body: 10 };
        let p = |s: &str| parse_head(s.as_bytes(), &l).unwrap_err().status;
        assert_eq!(p(&format!("GET /{} HTTP/1.1\r\nHost: a\r\n\r\n", "a".repeat(40))), 414);
        assert_eq!(p(&format!("GET /{}", "a".repeat(40))), 414); // detected before the line ends
        assert_eq!(p(&format!("GET / HTTP/1.1\r\nHost: a\r\nX: {}\r\n\r\n", "b".repeat(80))), 431);
        assert_eq!(p(&format!("GET / HTTP/1.1\r\nHost: a\r\nX: {}", "b".repeat(80))), 431); // no end yet
        assert_eq!(p("GET / HTTP/1.1\r\nHost: a\r\nA: 1\r\nB: 2\r\nC: 3\r\n\r\n"), 431);
    }

    #[test]
    fn target_forms() {
        assert_eq!(ok("OPTIONS * HTTP/1.1\r\nHost: a\r\n\r\n").0.request.path, "*");
        assert_eq!(ok("GET /a%2Fb HTTP/1.1\r\nHost: a\r\n\r\n").0.request.path, "/a/b");
        assert_eq!(ok("GET /caf%C3%A9 HTTP/1.1\r\nHost: a\r\n\r\n").0.request.path, "/café");
        assert_eq!(ok("GET /?q HTTP/1.1\r\nHost: a\r\n\r\n").0.request.query.as_deref(), Some("q"));
        let m = ok("PURGE / HTTP/1.1\r\nHost: a\r\n\r\n").0.request.method;
        assert_eq!(m, Method::Other("PURGE".into()));
    }

    #[test]
    fn expect_continue_is_noticed() {
        assert!(ok("POST / HTTP/1.1\r\nHost: a\r\nContent-Length: 3\r\nExpect: 100-Continue\r\n\r\n").0.expect_continue);
        assert!(!ok("POST / HTTP/1.1\r\nHost: a\r\nContent-Length: 3\r\n\r\n").0.expect_continue);
    }

    #[test]
    fn header_values_are_trimmed_but_not_otherwise_changed() {
        let (h, _) = ok("GET / HTTP/1.1\r\nHost:   a  \r\nX-Tab:\tv\t\r\nEmpty:\r\n\r\n");
        assert_eq!(h.request.header("host"), Some("a"));
        assert_eq!(h.request.header("x-tab"), Some("v"));
        assert_eq!(h.request.header("empty"), Some(""));
    }

    /// Random garbage never panics, and anything that parses is internally consistent.
    #[test]
    fn random_bytes_never_panic() {
        let mut seed = 0x9e3779b97f4a7c15u64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let base = b"POST /a/b?c=d HTTP/1.1\r\nHost: x\r\nContent-Length: 4\r\nX: y\r\n\r\nbody";
        for _ in 0..20_000 {
            let mut v = base.to_vec();
            for _ in 0..(next() % 6) {
                let i = (next() as usize) % v.len();
                match next() % 3 {
                    0 => v[i] = next() as u8,
                    1 => {
                        v.remove(i);
                    }
                    _ => v.insert(i, next() as u8),
                }
            }
            if let Ok(Parsed::Complete { consumed, .. }) = parse_head(&v, &Limits::default()) {
                assert!(consumed <= v.len() && v[..consumed].ends_with(b"\r\n\r\n"));
            }
        }
        for _ in 0..2_000 {
            let n = (next() % 200) as usize;
            let v: Vec<u8> = (0..n).map(|_| next() as u8).collect();
            let _ = parse_head(&v, &Limits::default());
        }
    }
}
