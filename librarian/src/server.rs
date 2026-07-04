//! The librarian web server.
//!
//! Serves a single static HTML page and proxies `/api/*` requests (GET/POST/
//! DELETE) to the daemon, so the browser only ever talks to one origin. The
//! librarian holds no state and
//! never starts the daemon; if the daemon is down, proxied calls return a 502
//! and the page degrades gracefully.

use std::io::Cursor;

/// The board page (plain HTML + inline JS, so it restarts in seconds).
const INDEX_HTML: &str = include_str!("../assets/board.html");

/// A librarian response: status, content type, and body.
pub struct Reply {
    /// HTTP status code.
    pub status: u16,
    /// Content-Type header value.
    pub content_type: &'static str,
    /// Response body.
    pub body: String,
}

impl Reply {
    fn html(body: impl Into<String>) -> Self {
        Self {
            status: 200,
            content_type: "text/html; charset=utf-8",
            body: body.into(),
        }
    }

    fn json(status: u16, body: impl Into<String>) -> Self {
        Self {
            status,
            content_type: "application/json",
            body: body.into(),
        }
    }

    fn not_found() -> Self {
        Self {
            status: 404,
            content_type: "text/plain; charset=utf-8",
            body: "not found".into(),
        }
    }
}

/// Route a request. Static assets are resolved locally; `/api/*` requests are
/// proxied to the daemon at `daemon_url` (GET/POST/DELETE, body forwarded).
#[must_use]
pub fn handle(daemon_url: &str, method: &str, path: &str, body: &str) -> Reply {
    match (method, path) {
        ("GET", "/" | "/index.html") => Reply::html(INDEX_HTML),
        (_, p) if p.starts_with("/api/") => proxy(daemon_url, method, p, body),
        _ => Reply::not_found(),
    }
}

/// Forward a request to the daemon and relay its status and body. A daemon that
/// is down becomes a 502 with an error envelope the page knows how to show.
fn proxy(daemon_url: &str, method: &str, path: &str, body: &str) -> Reply {
    let url = format!("{}{}", daemon_url.trim_end_matches('/'), path);
    let result = match method {
        "GET" => ureq::get(&url).call(),
        "DELETE" => ureq::delete(&url).call(),
        "POST" => ureq::post(&url)
            .set("Content-Type", "application/json")
            .send_string(body),
        other => {
            return Reply::json(
                405,
                format!(
                    "{{\"error\":{{\"code\":\"method_not_allowed\",\"message\":\"{other} not supported\"}}}}"
                ),
            );
        }
    };
    match result {
        Ok(resp) => {
            let status = resp.status();
            Reply::json(status, resp.into_string().unwrap_or_default())
        }
        Err(ureq::Error::Status(code, resp)) => {
            Reply::json(code, resp.into_string().unwrap_or_default())
        }
        Err(_) => Reply::json(
            502,
            r#"{"error":{"code":"daemon_unreachable","message":"the ralphus daemon is not running"}}"#,
        ),
    }
}

/// Serve the librarian on `127.0.0.1:port`, proxying the API to `daemon_url`.
///
/// # Errors
/// Returns an error if the listener cannot bind.
pub fn serve(port: u16, daemon_url: &str) -> std::io::Result<()> {
    let server = tiny_http::Server::http(("127.0.0.1", port))
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    for mut request in server.incoming_requests() {
        let method = request.method().as_str().to_string();
        let url = request.url().to_string();
        let path = url.split('?').next().unwrap_or(&url).to_string();

        let mut body = String::new();
        let _ = request.as_reader().read_to_string(&mut body);

        let reply = handle(daemon_url, &method, &path, &body);
        let header =
            tiny_http::Header::from_bytes(&b"Content-Type"[..], reply.content_type.as_bytes())
                .expect("valid header");
        let response = tiny_http::Response::new(
            tiny_http::StatusCode(reply.status),
            vec![header],
            Cursor::new(reply.body.into_bytes()),
            None,
            None,
        );
        let _ = request.respond(response);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serves_index_html() {
        let reply = handle("http://127.0.0.1:9", "GET", "/", "");
        assert_eq!(reply.status, 200);
        assert!(reply.body.contains("ralphus"));
        assert!(reply.content_type.starts_with("text/html"));
    }

    #[test]
    fn unknown_path_is_404() {
        assert_eq!(
            handle("http://127.0.0.1:9", "GET", "/whatever", "").status,
            404
        );
    }

    #[test]
    fn api_post_is_proxied_not_rejected() {
        // POST is now forwarded; with the daemon down (port 9) it degrades to 502,
        // NOT a 405 read-only rejection.
        let reply = handle("http://127.0.0.1:9", "POST", "/api/runs", "{}");
        assert_eq!(reply.status, 502);
    }

    #[test]
    fn api_get_with_daemon_down_returns_502() {
        // Port 9 (discard) refuses HTTP; the proxy must degrade to 502.
        let reply = handle("http://127.0.0.1:9", "GET", "/api/tasks", "");
        assert_eq!(reply.status, 502);
        assert!(reply.body.contains("daemon_unreachable"));
    }
}
