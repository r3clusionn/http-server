//! A static file server: safe path resolution, MIME types, conditional requests and ranges.

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::date;
use crate::request::{Method, Request};
use crate::response::{Body, Response};

/// Names Windows treats as devices wherever they appear, with any extension.
const DEVICE_NAMES: &[&str] = &[
    "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8", "com9", "lpt1", "lpt2", "lpt3",
    "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
];

#[derive(Clone, Debug)]
pub struct FileServer {
    root: PathBuf,
    /// List directories that have no index file.
    pub listing: bool,
    /// Serve files and directories whose name starts with a dot.
    pub hidden: bool,
    pub index: Vec<String>,
    pub cache_control: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Denied {
    /// The path is not acceptable (traversal, reserved names, hidden files): 404 so as not to say what exists.
    NotFound,
}

/// Why a path was refused, for tests and logs.
fn segment_ok(seg: &str, hidden: bool) -> bool {
    if seg.is_empty() || seg == "." {
        return true;
    }
    if seg == ".."
        || seg.bytes().any(|b| b < 0x20 || b == 0x7f || matches!(b, b'\\' | b':' | b'*' | b'?' | b'"' | b'<' | b'>' | b'|'))
    {
        return false;
    }
    // Windows drops trailing dots and spaces when opening a file, so `x.` is `x`.
    if seg.ends_with('.') || seg.ends_with(' ') {
        return false;
    }
    let stem = seg.split('.').next().unwrap_or("").to_ascii_lowercase();
    if DEVICE_NAMES.contains(&stem.trim_end()) {
        return false;
    }
    if !hidden && seg.starts_with('.') {
        return false;
    }
    true
}

/// Joins a decoded URL path onto `root` without ever leaving it.
pub fn safe_join(root: &Path, url_path: &str, hidden: bool) -> Result<PathBuf, Denied> {
    let mut out = root.to_path_buf();
    for seg in url_path.split('/') {
        if !segment_ok(seg, hidden) {
            return Err(Denied::NotFound);
        }
        if !seg.is_empty() && seg != "." {
            out.push(seg);
        }
    }
    Ok(out)
}

pub fn mime_type(path: &Path) -> &'static str {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "json" => "application/json",
        "txt" | "md" | "log" | "toml" | "rs" | "c" | "h" | "py" => "text/plain; charset=utf-8",
        "csv" => "text/csv; charset=utf-8",
        "xml" => "application/xml",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "pdf" => "application/pdf",
        "zip" => "application/zip",
        "wasm" => "application/wasm",
        "woff2" => "font/woff2",
        _ => "application/octet-stream",
    }
}

/// A range request, resolved against the file length.
#[derive(Debug, PartialEq, Eq)]
pub enum Range {
    /// No usable Range header: send everything.
    Full,
    /// Inclusive byte positions.
    Part(u64, u64),
    Unsatisfiable,
}

/// Parses a `Range` header value for a file of `len` bytes. Only a single `bytes` range is
/// served; several ranges, other units and malformed values are ignored, as RFC 9110 allows.
pub fn parse_range(value: &str, len: u64) -> Range {
    let Some(spec) = value.trim().strip_prefix("bytes=") else { return Range::Full };
    if spec.contains(',') {
        return Range::Full;
    }
    let Some((a, b)) = spec.trim().split_once('-') else { return Range::Full };
    let (a, b) = (a.trim(), b.trim());
    let digits = |s: &str| !s.is_empty() && s.len() <= 18 && s.bytes().all(|c| c.is_ascii_digit());
    match (digits(a), digits(b)) {
        (true, false) if b.is_empty() => {
            let start: u64 = a.parse().unwrap();
            if start >= len {
                Range::Unsatisfiable
            } else {
                Range::Part(start, len - 1)
            }
        }
        (true, true) => {
            let (start, end): (u64, u64) = (a.parse().unwrap(), b.parse().unwrap());
            if start > end {
                Range::Full
            } else if start >= len {
                Range::Unsatisfiable
            } else {
                Range::Part(start, end.min(len - 1))
            }
        }
        // A suffix: the last n bytes.
        (false, true) if a.is_empty() => {
            let n: u64 = b.parse().unwrap();
            if n == 0 || len == 0 {
                Range::Unsatisfiable
            } else {
                Range::Part(len.saturating_sub(n), len - 1)
            }
        }
        _ => Range::Full,
    }
}

fn mtime_nanos(t: SystemTime) -> u128 {
    t.duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0)
}

fn etag(len: u64, modified: SystemTime) -> String {
    format!("W/\"{len:x}-{:x}\"", mtime_nanos(modified))
}

/// Whether an `If-None-Match` header matches an ETag (weak comparison).
fn none_match(header: &str, etag: &str) -> bool {
    let strip = |s: &str| s.trim().trim_start_matches("W/").to_string();
    let ours = strip(etag);
    header.split(',').any(|t| t.trim() == "*" || strip(t) == ours)
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn url_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

impl FileServer {
    /// Serves files below `root`. The root must exist.
    pub fn new(root: impl AsRef<Path>) -> std::io::Result<FileServer> {
        let root = fs::canonicalize(root)?;
        Ok(FileServer { root, listing: false, hidden: false, index: vec!["index.html".into()], cache_control: None })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn handle(&self, req: &Request) -> Response {
        if req.method != Method::Get && req.method != Method::Head {
            return Response::error(405).header("Allow", "GET, HEAD, OPTIONS");
        }
        let Ok(path) = safe_join(&self.root, &req.path, self.hidden) else { return Response::error(404) };
        // Resolve symbolic links and junctions, and make sure the real location is still inside.
        let Ok(real) = fs::canonicalize(&path) else { return Response::error(404) };
        if !real.starts_with(&self.root) {
            return Response::error(404);
        }
        let Ok(meta) = fs::metadata(&real) else { return Response::error(404) };
        if meta.is_dir() {
            if !req.path.ends_with('/') {
                // `//evil.example/` would be read as a network-path reference, so never emit two leading slashes.
                let mut loc = format!("/{}/", req.path.trim_matches('/'));
                if let Some(q) = &req.query {
                    loc.push('?');
                    loc.push_str(q);
                }
                return Response::redirect(301, &loc);
            }
            for name in &self.index {
                let candidate = real.join(name);
                if candidate.is_file() {
                    return self.serve_file(req, &candidate);
                }
            }
            return if self.listing { self.listing_page(req, &real) } else { Response::error(404) };
        }
        if !meta.is_file() {
            return Response::error(404);
        }
        self.serve_file(req, &real)
    }

    fn serve_file(&self, req: &Request, path: &Path) -> Response {
        let Ok(file) = File::open(path) else { return Response::error(404) };
        let Ok(meta) = file.metadata() else { return Response::error(500) };
        let len = meta.len();
        // HTTP dates have a resolution of a second.
        let modified = meta
            .modified()
            .ok()
            .map(|t| UNIX_EPOCH + Duration::from_secs(t.duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)));
        let tag = modified.map(|m| etag(len, m));
        let mut resp;
        let validators = |r: &mut Response| {
            if let Some(t) = &tag {
                r.set_header("ETag", t);
            }
            if let Some(m) = modified {
                r.set_header("Last-Modified", &date::format(m));
            }
            if let Some(c) = &self.cache_control {
                r.set_header("Cache-Control", c);
            }
        };

        // Conditional requests: If-None-Match wins over If-Modified-Since.
        let not_modified = match (req.header("if-none-match"), &tag) {
            (Some(h), Some(t)) => none_match(h, t),
            (Some(_), None) => false,
            (None, _) => match (req.header("if-modified-since").and_then(date::parse), modified) {
                (Some(since), Some(m)) => m <= since,
                _ => false,
            },
        };
        if not_modified {
            resp = Response::new(304);
            validators(&mut resp);
            return resp;
        }

        let mut range = req.header("range").map(|v| parse_range(v, len)).unwrap_or(Range::Full);
        // If-Range: the range only applies if the validator still matches.
        if let (Some(ir), Range::Part(..)) = (req.header("if-range"), &range) {
            let fresh = if ir.trim_start().starts_with('"') || ir.trim_start().starts_with("W/") {
                // Strong comparison is required, and ours are weak tags.
                false
            } else {
                date::parse(ir).zip(modified).is_some_and(|(d, m)| d == m)
            };
            if !fresh {
                range = Range::Full;
            }
        }

        let ctype = mime_type(path);
        match range {
            Range::Unsatisfiable => {
                resp = Response::error(416).header("Content-Range", &format!("bytes */{len}"));
            }
            Range::Part(start, end) => {
                resp = Response { status: 206, headers: vec![], body: Body::File { file, offset: start, len: end - start + 1 } };
                resp.set_header("Content-Type", ctype);
                resp.set_header("Content-Range", &format!("bytes {start}-{end}/{len}"));
                resp.set_header("Accept-Ranges", "bytes");
                validators(&mut resp);
            }
            Range::Full => {
                resp = Response { status: 200, headers: vec![], body: Body::File { file, offset: 0, len } };
                resp.set_header("Content-Type", ctype);
                resp.set_header("Accept-Ranges", "bytes");
                validators(&mut resp);
            }
        }
        resp
    }

    fn listing_page(&self, req: &Request, dir: &Path) -> Response {
        let Ok(rd) = fs::read_dir(dir) else { return Response::error(403) };
        let mut entries: Vec<(String, bool, u64)> = rd
            .flatten()
            .filter_map(|e| {
                let name = e.file_name().into_string().ok()?;
                if !self.hidden && name.starts_with('.') {
                    return None;
                }
                // Follow links only if they stay inside the root, like a direct request would.
                let real = fs::canonicalize(e.path()).ok().filter(|p| p.starts_with(&self.root))?;
                let m = fs::metadata(real).ok()?;
                Some((name, m.is_dir(), m.len()))
            })
            .collect();
        entries.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.to_lowercase().cmp(&b.0.to_lowercase())));
        let title = html_escape(&req.path);
        let mut html = format!("<!doctype html><meta charset=utf-8><title>Index of {title}</title><h1>Index of {title}</h1><ul>");
        if req.path != "/" {
            html.push_str("<li><a href=\"../\">../</a></li>");
        }
        for (name, is_dir, size) in entries {
            let slash = if is_dir { "/" } else { "" };
            let info = if is_dir { String::new() } else { format!(" ({size} bytes)") };
            html.push_str(&format!("<li><a href=\"{}{slash}\">{}{slash}</a>{info}</li>", url_encode(&name), html_escape(&name)));
        }
        html.push_str("</ul>");
        Response::html(200, html)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_join_refuses_everything_that_could_leave_the_root() {
        let root = Path::new("/srv/www");
        for bad in [
            "/../etc/passwd",
            "/a/../../b",
            "/..",
            "/a/..",
            "/a\\b",
            "/..\\x",
            "/c:/windows",
            "/a/b:stream",
            "/a/CON",
            "/a/con.txt",
            "/NUL",
            "/com1.x",
            "/LPT9",
            "/file.",
            "/file ",
            "/dir./x",
            "/a\u{1}b",
            "/.git/config",
            "/a/.hidden",
            "/a?b",
            "/a*b",
        ] {
            assert_eq!(safe_join(root, bad, false), Err(Denied::NotFound), "{bad}");
        }
    }

    #[test]
    fn safe_join_accepts_ordinary_paths() {
        let root = Path::new("/srv/www");
        assert_eq!(safe_join(root, "/", false).unwrap(), root);
        assert_eq!(safe_join(root, "/a//b/./c.txt", false).unwrap(), root.join("a").join("b").join("c.txt"));
        assert_eq!(safe_join(root, "/connect.txt", false).unwrap(), root.join("connect.txt"));
        assert_eq!(safe_join(root, "/console/x", false).unwrap(), root.join("console").join("x"));
        assert_eq!(safe_join(root, "/.well-known/x", true).unwrap(), root.join(".well-known").join("x"));
        assert!(safe_join(root, "/.well-known/x", false).is_err());
        assert!(safe_join(root, "/caf\u{e9}/\u{4e2d}\u{6587}.txt", false).is_ok());
    }

    #[test]
    fn range_parsing() {
        use Range::*;
        assert_eq!(parse_range("bytes=0-4", 10), Part(0, 4));
        assert_eq!(parse_range("bytes=5-", 10), Part(5, 9));
        assert_eq!(parse_range("bytes=-3", 10), Part(7, 9));
        assert_eq!(parse_range("bytes=-30", 10), Part(0, 9), "a suffix longer than the file is the whole file");
        assert_eq!(parse_range("bytes=8-100", 10), Part(8, 9), "an end past the file is clamped");
        assert_eq!(parse_range("bytes=10-", 10), Unsatisfiable);
        assert_eq!(parse_range("bytes=10-12", 10), Unsatisfiable);
        assert_eq!(parse_range("bytes=-0", 10), Unsatisfiable);
        assert_eq!(parse_range("bytes=0-0", 0), Unsatisfiable);
        assert_eq!(parse_range("bytes=-5", 0), Unsatisfiable);
        assert_eq!(parse_range("bytes=0-9", 10), Part(0, 9));
        // Ignored: the full file is sent.
        for ignored in
            ["bytes=0-1,3-4", "items=0-1", "bytes=", "bytes=-", "bytes=a-b", "bytes=5-2", "0-1", "bytes=0-1-2", "bytes=+1-2", ""]
        {
            assert_eq!(parse_range(ignored, 10), Full, "{ignored:?}");
        }
        assert_eq!(parse_range("bytes=99999999999999999999-", 10), Full, "numbers too long to be a size are ignored");
    }

    #[test]
    fn etag_matching() {
        let t = "W/\"a-1\"";
        assert!(none_match("W/\"a-1\"", t));
        assert!(none_match("\"a-1\"", t), "weak comparison ignores the W/ prefix");
        assert!(none_match("\"x\", W/\"a-1\"", t));
        assert!(none_match("*", t));
        assert!(!none_match("\"b-2\"", t));
        assert!(!none_match("", t));
    }

    #[test]
    fn mime_types() {
        assert_eq!(mime_type(Path::new("a.HTML")), "text/html; charset=utf-8");
        assert_eq!(mime_type(Path::new("a.png")), "image/png");
        assert_eq!(mime_type(Path::new("a")), "application/octet-stream");
        assert_eq!(mime_type(Path::new("a.unknownext")), "application/octet-stream");
    }

    #[test]
    fn escaping() {
        assert_eq!(html_escape("<a href=\"x\">&"), "&lt;a href=&quot;x&quot;&gt;&amp;");
        assert_eq!(url_encode("a b/c?d#é"), "a%20b%2Fc%3Fd%23%C3%A9");
    }
}
