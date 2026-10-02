mod common;

use std::fs;

use common::*;

fn setup(root: &std::path::Path) {
    fs::write(root.join("hello.txt"), "0123456789").unwrap();
    fs::write(root.join("page.html"), "<h1>hi</h1>").unwrap();
    fs::write(root.join("data.bin"), (0..=255u8).collect::<Vec<u8>>()).unwrap();
    fs::write(root.join(".secret"), "dotfile").unwrap();
    fs::create_dir_all(root.join("sub/deeper")).unwrap();
    fs::write(root.join("sub/index.html"), "sub index").unwrap();
    fs::write(root.join("sub/deeper/note.txt"), "deep").unwrap();
    fs::create_dir_all(root.join("noindex")).unwrap();
    fs::write(root.join("noindex/a b.txt"), "x").unwrap();
    fs::write(root.join("noindex/a&b'.txt"), "x").unwrap();
    fs::create_dir_all(root.join("evil.com")).unwrap();
    fs::create_dir_all(root.join(".git")).unwrap();
    fs::write(root.join(".git/config"), "gitconfig").unwrap();
    // A file outside the served directory that no request may reach.
    fs::write(root.parent().unwrap().join(format!("outside-{}.txt", std::process::id())), "TOP SECRET").unwrap();
}

fn outside_name() -> String {
    format!("outside-{}.txt", std::process::id())
}

fn fixture() -> Fixture {
    start_with(|_| {}, setup)
}

fn cleanup(f: &Fixture) {
    let _ = fs::remove_file(f.dir.path().parent().unwrap().join(outside_name()));
}

#[test]
fn serves_a_file_with_type_length_and_validators() {
    let f = fixture();
    let r = raw(f.addr, b"GET /hello.txt HTTP/1.1\r\nHost: t\r\n\r\n");
    assert_eq!(r.status, 200);
    assert_eq!(r.text(), "0123456789");
    assert_eq!(r.header("content-type"), Some("text/plain; charset=utf-8"));
    assert_eq!(r.header("content-length"), Some("10"));
    assert_eq!(r.header("accept-ranges"), Some("bytes"));
    assert!(r.header("etag").is_some_and(|e| e.starts_with("W/\"")));
    assert!(r.header("last-modified").is_some_and(|d| d.ends_with("GMT")));
    assert_eq!(
        raw(f.addr, b"GET /page.html HTTP/1.1\r\nHost: t\r\n\r\n").header("content-type"),
        Some("text/html; charset=utf-8")
    );
    let r = raw(f.addr, b"GET /data.bin HTTP/1.1\r\nHost: t\r\n\r\n");
    assert_eq!((r.body.len(), r.header("content-type")), (256, Some("application/octet-stream")));
    cleanup(&f);
}

#[test]
fn head_matches_get_without_a_body() {
    let f = fixture();
    let mut c = Client::connect(f.addr);
    c.send(b"HEAD /hello.txt HTTP/1.1\r\nHost: t\r\n\r\n");
    let r = c.read_response(true).unwrap();
    assert_eq!((r.status, r.header("content-length"), r.body.len()), (200, Some("10"), 0));
    assert_eq!(c.get("/hello.txt").text(), "0123456789");
    cleanup(&f);
}

#[test]
fn conditional_requests() {
    let f = fixture();
    let first = raw(f.addr, b"GET /hello.txt HTTP/1.1\r\nHost: t\r\n\r\n");
    let (etag, modified) = (first.header("etag").unwrap().to_string(), first.header("last-modified").unwrap().to_string());

    let r = raw(f.addr, format!("GET /hello.txt HTTP/1.1\r\nHost: t\r\nIf-None-Match: {etag}\r\n\r\n").as_bytes());
    assert_eq!((r.status, r.body.len()), (304, 0));
    assert_eq!(r.header("etag"), Some(etag.as_str()));
    assert!(r.header("content-length").is_none());
    // Weak comparison: the strong form of the same tag matches too.
    let r = raw(
        f.addr,
        format!("GET /hello.txt HTTP/1.1\r\nHost: t\r\nIf-None-Match: {}\r\n\r\n", etag.trim_start_matches("W/")).as_bytes(),
    );
    assert_eq!(r.status, 304);
    let r = raw(f.addr, b"GET /hello.txt HTTP/1.1\r\nHost: t\r\nIf-None-Match: \"something-else\"\r\n\r\n");
    assert_eq!(r.status, 200);
    let r = raw(f.addr, b"GET /hello.txt HTTP/1.1\r\nHost: t\r\nIf-None-Match: *\r\n\r\n");
    assert_eq!(r.status, 304);

    let r = raw(f.addr, format!("GET /hello.txt HTTP/1.1\r\nHost: t\r\nIf-Modified-Since: {modified}\r\n\r\n").as_bytes());
    assert_eq!(r.status, 304);
    let r = raw(f.addr, b"GET /hello.txt HTTP/1.1\r\nHost: t\r\nIf-Modified-Since: Thu, 01 Jan 1970 00:00:01 GMT\r\n\r\n");
    assert_eq!(r.status, 200);
    // An unparseable date is ignored.
    let r = raw(f.addr, b"GET /hello.txt HTTP/1.1\r\nHost: t\r\nIf-Modified-Since: yesterday\r\n\r\n");
    assert_eq!(r.status, 200);
    // If-None-Match takes precedence over If-Modified-Since.
    let r = raw(
        f.addr,
        format!("GET /hello.txt HTTP/1.1\r\nHost: t\r\nIf-None-Match: \"nope\"\r\nIf-Modified-Since: {modified}\r\n\r\n")
            .as_bytes(),
    );
    assert_eq!(r.status, 200);

    // A change to the file changes the validator.
    fs::write(f.dir.path().join("hello.txt"), "changed content").unwrap();
    let r = raw(f.addr, format!("GET /hello.txt HTTP/1.1\r\nHost: t\r\nIf-None-Match: {etag}\r\n\r\n").as_bytes());
    assert_eq!((r.status, r.text()), (200, "changed content".into()));
    cleanup(&f);
}

fn range(addr: std::net::SocketAddr, path: &str, value: &str) -> Resp {
    raw(addr, format!("GET {path} HTTP/1.1\r\nHost: t\r\nRange: {value}\r\n\r\n").as_bytes())
}

#[test]
fn byte_ranges() {
    let f = fixture();
    let r = range(f.addr, "/hello.txt", "bytes=2-5");
    assert_eq!((r.status, r.text()), (206, "2345".into()));
    assert_eq!(r.header("content-range"), Some("bytes 2-5/10"));
    assert_eq!(r.header("content-length"), Some("4"));
    assert_eq!(range(f.addr, "/hello.txt", "bytes=7-").text(), "789");
    assert_eq!(range(f.addr, "/hello.txt", "bytes=-3").text(), "789");
    assert_eq!(range(f.addr, "/hello.txt", "bytes=8-100").header("content-range"), Some("bytes 8-9/10"));
    assert_eq!(range(f.addr, "/hello.txt", "bytes=0-0").text(), "0");
    let r = range(f.addr, "/hello.txt", "bytes=10-");
    assert_eq!((r.status, r.header("content-range")), (416, Some("bytes */10")));
    // Ignored forms give the whole file.
    for ignored in ["bytes=0-1,4-5", "items=1-2", "bytes=abc", "bytes=5-2"] {
        let r = range(f.addr, "/hello.txt", ignored);
        assert_eq!((r.status, r.text()), (200, "0123456789".into()), "{ignored}");
    }
    // Every byte of a binary file is where it should be, for many ranges.
    for (a, b) in [(0usize, 0usize), (0, 255), (100, 200), (255, 255), (17, 17)] {
        let r = range(f.addr, "/data.bin", &format!("bytes={a}-{b}"));
        assert_eq!(r.body, (a..=b).map(|x| x as u8).collect::<Vec<u8>>());
    }
    // If-Range with a date that no longer matches: the full file.
    let r =
        raw(f.addr, b"GET /hello.txt HTTP/1.1\r\nHost: t\r\nRange: bytes=0-1\r\nIf-Range: Thu, 01 Jan 1970 00:00:01 GMT\r\n\r\n");
    assert_eq!((r.status, r.body.len()), (200, 10));
    let m = raw(f.addr, b"GET /hello.txt HTTP/1.1\r\nHost: t\r\n\r\n").header("last-modified").unwrap().to_string();
    let r = raw(f.addr, format!("GET /hello.txt HTTP/1.1\r\nHost: t\r\nRange: bytes=0-1\r\nIf-Range: {m}\r\n\r\n").as_bytes());
    assert_eq!((r.status, r.text()), (206, "01".into()));
    cleanup(&f);
}

#[test]
fn directories() {
    let f = fixture();
    // No trailing slash: redirect. The index file is served at the slash.
    let r = raw(f.addr, b"GET /sub HTTP/1.1\r\nHost: t\r\n\r\n");
    assert_eq!((r.status, r.header("location")), (301, Some("/sub/")));
    let r = raw(f.addr, b"GET /sub?x=1 HTTP/1.1\r\nHost: t\r\n\r\n");
    assert_eq!(r.header("location"), Some("/sub/?x=1"));
    assert_eq!(raw(f.addr, b"GET /sub/ HTTP/1.1\r\nHost: t\r\n\r\n").text(), "sub index");
    // A directory without an index is listed (listing is on in this fixture), escaped.
    let r = raw(f.addr, b"GET /noindex/ HTTP/1.1\r\nHost: t\r\n\r\n");
    assert_eq!(r.status, 200);
    let t = r.text();
    assert!(t.contains("href=\"a%20b.txt\"") && t.contains("href=\"a%26b%27.txt\"") && t.contains(">a&amp;b'.txt<"), "{t}");
    assert!(t.contains("href=\"../\""));
    // The root has no index.html here, so it is listed too, without dotfiles.
    let t = raw(f.addr, b"GET / HTTP/1.1\r\nHost: t\r\n\r\n").text();
    assert!(t.contains("hello.txt") && !t.contains(".secret") && !t.contains(".git"));
    cleanup(&f);
}

#[test]
fn a_redirect_can_never_point_at_another_site() {
    let f = fixture();
    // `//evil.com/` in a Location header is a link to another host. The directory exists on disk.
    for target in ["//evil.com", "///evil.com", "//evil.com?x=1"] {
        let r = raw(f.addr, format!("GET {target} HTTP/1.1\r\nHost: t\r\n\r\n").as_bytes());
        assert_eq!(r.status, 301, "{target}");
        let loc = r.header("location").unwrap();
        assert!(loc.starts_with("/evil.com/") && !loc.starts_with("//"), "{target} -> {loc}");
    }
    cleanup(&f);
}

#[test]
fn listing_can_be_off() {
    let f = start_with(|_| {}, setup);
    drop(f);
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("d")).unwrap();
    fs::write(dir.path().join("d/f.txt"), "x").unwrap();
    let files = std::sync::Arc::new(tinyhttp::FileServer::new(dir.path()).unwrap()); // listing off by default
    let router = tinyhttp::Router::new().fallback(move |req, _| files.handle(req));
    let server = tinyhttp::Server::start("127.0.0.1:0", router, tinyhttp::Config::default()).unwrap();
    assert_eq!(raw(server.local_addr(), b"GET /d/ HTTP/1.1\r\nHost: t\r\n\r\n").status, 404);
    assert_eq!(raw(server.local_addr(), b"GET /d/f.txt HTTP/1.1\r\nHost: t\r\n\r\n").status, 200);
}

#[test]
fn nothing_outside_the_root_can_be_reached() {
    let f = fixture();
    let out = outside_name();
    let attempts = [
        format!("/../{out}"),
        format!("/%2e%2e/{out}"),
        format!("/%2E%2E/{out}"),
        format!("/..%2f{out}"),
        format!("/..%5c{out}"),
        format!("/sub/../../{out}"),
        format!("/sub/%2e%2e/%2e%2e/{out}"),
        format!("/sub/..%2f..%2f{out}"),
        format!("/%2e%2e%2f{out}"),
        format!("/.%2e/{out}"),
        format!("/..;/{out}"),
        format!("/....//{out}"),
        format!("/sub\\..\\..\\{out}"),
        format!("/%5c..%5c{out}"),
        format!("/%252e%252e/{out}"),
        "/C:/Windows/win.ini".to_string(),
        "/c%3a/windows/win.ini".to_string(),
        "/\\\\localhost\\c$\\windows\\win.ini".to_string(),
        "/%5c%5clocalhost%5cc$%5cwindows%5cwin.ini".to_string(),
        "/hello.txt::$DATA".to_string(),
        "/hello.txt:stream".to_string(),
        "/hello.txt.".to_string(),
        "/hello.txt ".to_string().replace(' ', "%20"),
        "/CON".to_string(),
        "/nul".to_string(),
        "/aux.txt".to_string(),
        "/COM1".to_string(),
        "/lpt1.log".to_string(),
        "/.git/config".to_string(),
        "/.secret".to_string(),
        "/%2egit/config".to_string(),
        "/sub/%00".to_string(),
    ];
    for target in attempts {
        let r = raw(f.addr, format!("GET {target} HTTP/1.1\r\nHost: t\r\n\r\n").as_bytes());
        assert!(matches!(r.status, 400 | 404), "{target} gave {}", r.status);
        assert!(
            !r.text().contains("TOP SECRET") && !r.text().contains("gitconfig") && !r.text().contains("dotfile"),
            "{target} leaked a file"
        );
    }
    // The control: the legitimate path works.
    assert_eq!(raw(f.addr, b"GET /sub/deeper/note.txt HTTP/1.1\r\nHost: t\r\n\r\n").text(), "deep");
    assert_eq!(
        raw(f.addr, b"GET /sub/deeper/../../hello.txt HTTP/1.1\r\nHost: t\r\n\r\n").status,
        404,
        "dot-dot is refused even when it stays inside"
    );
    cleanup(&f);
}

#[test]
fn a_symbolic_link_out_of_the_root_is_not_followed() {
    let f = fixture();
    let link = f.dir.path().join("escape.txt");
    let target = f.dir.path().parent().unwrap().join(outside_name());
    #[cfg(windows)]
    let made = std::os::windows::fs::symlink_file(&target, &link).is_ok();
    #[cfg(unix)]
    let made = std::os::unix::fs::symlink(&target, &link).is_ok();
    if !made {
        eprintln!("SKIPPED: cannot create symbolic links here");
        cleanup(&f);
        return;
    }
    let r = raw(f.addr, b"GET /escape.txt HTTP/1.1\r\nHost: t\r\n\r\n");
    assert_eq!(r.status, 404);
    assert!(!r.text().contains("TOP SECRET"));
    // A link that stays inside is fine.
    #[cfg(windows)]
    let ok = std::os::windows::fs::symlink_file(f.dir.path().join("hello.txt"), f.dir.path().join("inside.txt")).is_ok();
    #[cfg(unix)]
    let ok = std::os::unix::fs::symlink(f.dir.path().join("hello.txt"), f.dir.path().join("inside.txt")).is_ok();
    if ok {
        assert_eq!(raw(f.addr, b"GET /inside.txt HTTP/1.1\r\nHost: t\r\n\r\n").text(), "0123456789");
    }
    // The listing does not reveal the escaping link either.
    let t = raw(f.addr, b"GET / HTTP/1.1\r\nHost: t\r\n\r\n").text();
    assert!(!t.contains("escape.txt"));
    cleanup(&f);
}

#[test]
fn other_methods_on_files_are_refused() {
    let f = fixture();
    let r = raw(f.addr, b"POST /hello.txt HTTP/1.1\r\nHost: t\r\nContent-Length: 0\r\n\r\n");
    assert_eq!((r.status, r.header("allow")), (405, Some("GET, HEAD, OPTIONS")));
    assert_eq!(raw(f.addr, b"DELETE /hello.txt HTTP/1.1\r\nHost: t\r\n\r\n").status, 405);
    assert_eq!(fs::read_to_string(f.dir.path().join("hello.txt")).unwrap(), "0123456789");
    cleanup(&f);
}

#[test]
fn unicode_and_encoded_names() {
    let f = fixture();
    fs::write(f.dir.path().join("café 日本.txt"), "unicode").unwrap();
    let r = raw(f.addr, "GET /caf%C3%A9%20%E6%97%A5%E6%9C%AC.txt HTTP/1.1\r\nHost: t\r\n\r\n".as_bytes());
    assert_eq!((r.status, r.text()), (200, "unicode".into()));
    // Overlong or invalid UTF-8 in the path is a client error, not a lookup.
    assert_eq!(raw(f.addr, b"GET /%C0%AE%C0%AE/x HTTP/1.1\r\nHost: t\r\n\r\n").status, 400);
    cleanup(&f);
}
