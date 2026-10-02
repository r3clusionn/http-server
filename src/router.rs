//! Routing by method and path pattern.
//!
//! Patterns are `/` separated segments: a literal (`users`), a parameter (`:id`, one segment) or a
//! trailing wildcard (`*rest`, everything that is left, possibly nothing). Matching is exact:
//! `/a` and `/a/` are different paths. A path that matches a pattern with another method gets a
//! 405 with an `Allow` header; `HEAD` uses the `GET` handler; `OPTIONS` is answered automatically.

use crate::request::{Method, Request};
use crate::response::Response;

pub type Handler = Box<dyn Fn(&Request, &Params) -> Response + Send + Sync>;

/// Values captured by `:name` and `*name` segments.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Params(Vec<(String, String)>);

impl Params {
    pub fn get(&self, name: &str) -> Option<&str> {
        self.0.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str())
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Segment {
    Literal(String),
    Param(String),
    Wildcard(String),
}

fn compile(pattern: &str) -> Vec<Segment> {
    assert!(pattern.starts_with('/'), "route patterns start with /: {pattern}");
    let segs: Vec<&str> = pattern[1..].split('/').collect();
    let last = segs.len() - 1;
    segs.iter()
        .enumerate()
        .map(|(i, s)| {
            if let Some(name) = s.strip_prefix(':') {
                assert!(!name.is_empty(), "empty parameter name in {pattern}");
                Segment::Param(name.to_string())
            } else if let Some(name) = s.strip_prefix('*') {
                assert!(i == last, "a wildcard must be the last segment: {pattern}");
                Segment::Wildcard(name.to_string())
            } else {
                Segment::Literal(s.to_string())
            }
        })
        .collect()
}

fn matches(pattern: &[Segment], path: &str) -> Option<Params> {
    let parts: Vec<&str> = path.strip_prefix('/')?.split('/').collect();
    let mut params = Vec::new();
    for (i, seg) in pattern.iter().enumerate() {
        match seg {
            Segment::Literal(l) => {
                if parts.get(i) != Some(&l.as_str()) {
                    return None;
                }
            }
            Segment::Param(name) => {
                let v = parts.get(i).filter(|v| !v.is_empty())?;
                params.push((name.clone(), v.to_string()));
            }
            Segment::Wildcard(name) => {
                if parts.len() <= i {
                    return None;
                }
                params.push((name.clone(), parts.get(i..).unwrap_or(&[]).join("/")));
                return Some(Params(params));
            }
        }
    }
    (parts.len() == pattern.len()).then_some(Params(params))
}

struct Route {
    method: Method,
    pattern: Vec<Segment>,
    handler: Handler,
}

#[derive(Default)]
pub struct Router {
    routes: Vec<Route>,
    fallback: Option<Handler>,
}

impl Router {
    pub fn new() -> Router {
        Router::default()
    }

    pub fn route(
        mut self,
        method: Method,
        pattern: &str,
        handler: impl Fn(&Request, &Params) -> Response + Send + Sync + 'static,
    ) -> Router {
        self.routes.push(Route { method, pattern: compile(pattern), handler: Box::new(handler) });
        self
    }

    pub fn get(self, pattern: &str, h: impl Fn(&Request, &Params) -> Response + Send + Sync + 'static) -> Router {
        self.route(Method::Get, pattern, h)
    }

    pub fn post(self, pattern: &str, h: impl Fn(&Request, &Params) -> Response + Send + Sync + 'static) -> Router {
        self.route(Method::Post, pattern, h)
    }

    pub fn put(self, pattern: &str, h: impl Fn(&Request, &Params) -> Response + Send + Sync + 'static) -> Router {
        self.route(Method::Put, pattern, h)
    }

    pub fn delete(self, pattern: &str, h: impl Fn(&Request, &Params) -> Response + Send + Sync + 'static) -> Router {
        self.route(Method::Delete, pattern, h)
    }

    /// Handles every request no route matched (instead of a 404). It sees the method too, so a
    /// file server can answer 405 itself.
    pub fn fallback(mut self, h: impl Fn(&Request, &Params) -> Response + Send + Sync + 'static) -> Router {
        self.fallback = Some(Box::new(h));
        self
    }

    pub fn dispatch(&self, req: &Request) -> Response {
        let mut allowed: Vec<&str> = Vec::new();
        for r in &self.routes {
            let Some(params) = matches(&r.pattern, &req.path) else { continue };
            let wanted = if req.method == Method::Head { &Method::Get } else { &req.method };
            if r.method == *wanted {
                return (r.handler)(req, &params);
            }
            if !allowed.contains(&r.method.as_str()) {
                allowed.push(r.method.as_str());
            }
        }
        if !allowed.is_empty() {
            if allowed.contains(&"GET") && !allowed.contains(&"HEAD") {
                allowed.push("HEAD");
            }
            if !allowed.contains(&"OPTIONS") {
                allowed.push("OPTIONS");
            }
            let allow = allowed.join(", ");
            return if req.method == Method::Options {
                Response::new(204).header("Allow", &allow)
            } else {
                Response::error(405).header("Allow", &allow)
            };
        }
        match &self.fallback {
            Some(f) => f(req, &Params::default()),
            None => Response::error(404),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::request::Version;

    fn req(method: Method, path: &str) -> Request {
        Request {
            method,
            target: path.into(),
            path: path.into(),
            query: None,
            version: Version::Http11,
            headers: vec![],
            body: vec![],
            peer: None,
        }
    }

    fn body(r: Response) -> (u16, String) {
        let b = match r.body {
            crate::response::Body::Bytes(b) => String::from_utf8(b).unwrap(),
            _ => String::new(),
        };
        (r.status, b)
    }

    fn router() -> Router {
        Router::new()
            .get("/", |_, _| Response::text(200, "root"))
            .get("/users/:id", |_, p| Response::text(200, format!("user {}", p.get("id").unwrap())))
            .post("/users/:id", |_, p| Response::text(201, format!("create {}", p.get("id").unwrap())))
            .get("/users/:id/posts/:post", |_, p| {
                Response::text(200, format!("{}/{}", p.get("id").unwrap(), p.get("post").unwrap()))
            })
            .get("/files/*path", |_, p| Response::text(200, format!("file [{}]", p.get("path").unwrap())))
            .get("/static/logo", |_, _| Response::text(200, "logo"))
    }

    #[test]
    fn literals_params_and_wildcards() {
        let r = router();
        assert_eq!(body(r.dispatch(&req(Method::Get, "/"))), (200, "root".into()));
        assert_eq!(body(r.dispatch(&req(Method::Get, "/users/42"))), (200, "user 42".into()));
        assert_eq!(body(r.dispatch(&req(Method::Get, "/users/7/posts/abc"))), (200, "7/abc".into()));
        assert_eq!(body(r.dispatch(&req(Method::Get, "/files/a/b/c.txt"))), (200, "file [a/b/c.txt]".into()));
        assert_eq!(body(r.dispatch(&req(Method::Get, "/files/"))), (200, "file []".into()));
        assert_eq!(body(r.dispatch(&req(Method::Get, "/files"))).0, 404, "the wildcard needs its slash");
        assert_eq!(body(r.dispatch(&req(Method::Get, "/static/logo"))).0, 200);
    }

    #[test]
    fn matching_is_exact() {
        let r = router();
        for path in ["/users", "/users/", "/users/42/", "/users/42/posts", "/static/logo/", "/nope", "/users//posts/x"] {
            assert_eq!(body(r.dispatch(&req(Method::Get, path))).0, 404, "{path}");
        }
    }

    #[test]
    fn wrong_method_is_405_with_allow() {
        let r = router();
        let resp = r.dispatch(&req(Method::Delete, "/users/42"));
        assert_eq!(resp.status, 405);
        let allow = resp.headers.iter().find(|(n, _)| n == "Allow").unwrap().1.clone();
        assert_eq!(allow, "GET, POST, HEAD, OPTIONS");
    }

    #[test]
    fn head_uses_get_and_options_is_automatic() {
        let r = router();
        assert_eq!(body(r.dispatch(&req(Method::Head, "/users/1"))).0, 200);
        let o = r.dispatch(&req(Method::Options, "/users/1"));
        assert_eq!(o.status, 204);
        assert!(o.headers.iter().any(|(n, v)| n == "Allow" && v.contains("POST")));
        assert_eq!(r.dispatch(&req(Method::Options, "/nope")).status, 404);
    }

    #[test]
    fn first_matching_route_wins_and_fallback_catches_the_rest() {
        let r = Router::new()
            .get("/a/:x", |_, _| Response::text(200, "param"))
            .get("/a/b", |_, _| Response::text(200, "literal"))
            .fallback(|req, _| Response::text(200, format!("fallback {}", req.path)));
        assert_eq!(body(r.dispatch(&req(Method::Get, "/a/b"))).1, "param");
        assert_eq!(body(r.dispatch(&req(Method::Get, "/zzz"))).1, "fallback /zzz");
        // A known path with the wrong method is still a 405, not a fallback.
        let r = Router::new().get("/a", |_, _| Response::text(200, "x")).fallback(|_, _| Response::text(200, "fb"));
        assert_eq!(r.dispatch(&req(Method::Post, "/a")).status, 405);
    }

    #[test]
    #[should_panic(expected = "wildcard must be the last")]
    fn a_wildcard_in_the_middle_is_a_programming_error() {
        let _ = Router::new().get("/a/*x/b", |_, _| Response::new(200));
    }
}
