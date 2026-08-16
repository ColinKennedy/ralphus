//! The librarian web server.
//!
//! Serves a single static HTML page and proxies `/api/*` requests (GET/POST/
//! DELETE) to the daemon, so the browser only ever talks to one origin. The
//! librarian holds no state and
//! never starts the daemon; if the daemon is down, proxied calls return a 502
//! and the page degrades gracefully.

use std::io::{Cursor, Read, Write};

use opentelemetry::Context;
use opentelemetry::trace::{SpanKind, Status};

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
/// `path` may carry a `?query` string — it is stripped for the static-route
/// match but forwarded to the daemon verbatim so proxied endpoints that read
/// query params (e.g. `/api/cartographer?run_id=...`) keep working.
#[must_use]
pub fn handle(daemon_url: &str, method: &str, path: &str, body: &str) -> Reply {
    let path_only = path.split('?').next().unwrap_or(path);
    match (method, path_only) {
        ("GET", "/" | "/index.html") => Reply::html(INDEX_HTML),
        (_, p) if p.starts_with("/api/") => proxy(daemon_url, method, path, body, None),
        _ => Reply::not_found(),
    }
}

/// Like [`handle`], but wraps the request in an OpenTelemetry HTTP server span
/// (RAL-96) built from the browser's incoming `traceparent` header, and
/// forwards that trace onward to the daemon on any proxied `/api/*` call —
/// continuing the same browser-click → librarian → daemon trace. `handle`
/// itself (and its existing tests) are untouched; this is the entry point the
/// live server loop uses.
#[must_use]
pub fn handle_with_trace(
    daemon_url: &str,
    method: &str,
    path: &str,
    body: &str,
    traceparent: Option<&str>,
) -> Reply {
    let path_only = path.split('?').next().unwrap_or(path);
    let cx = crate::otel::context_from_traceparent(traceparent);
    let span = crate::otel::start_span("librarian.request", &cx, SpanKind::Server);
    span.set_attribute("http.method", method.to_string());
    span.set_attribute("http.target", path.to_string());

    let reply = match (method, path_only) {
        ("GET", "/" | "/index.html") => Reply::html(INDEX_HTML),
        (_, p) if p.starts_with("/api/") => proxy(daemon_url, method, path, body, Some(&span.cx)),
        _ => Reply::not_found(),
    };

    span.set_attribute("http.status_code", i64::from(reply.status));
    if reply.status >= 400 {
        span.set_status(Status::error(format!("http {}", reply.status)));
    } else {
        span.set_status(Status::Ok);
    }
    reply
}

/// Forward a request to the daemon and relay its status and body. A daemon that
/// is down becomes a 502 with an error envelope the page knows how to show.
/// When `parent` carries a valid span, its `traceparent` is forwarded to the
/// daemon as a request header (RAL-96) so the daemon's own span continues the
/// same trace instead of starting a disconnected one.
fn proxy(
    daemon_url: &str,
    method: &str,
    path: &str,
    body: &str,
    parent: Option<&Context>,
) -> Reply {
    let url = format!("{}{}", daemon_url.trim_end_matches('/'), path);
    let traceparent = parent.and_then(crate::otel::traceparent_from_context);
    let with_trace = |req: ureq::Request| match &traceparent {
        Some(tp) => req.set("traceparent", tp),
        None => req,
    };
    let result = match method {
        "GET" => with_trace(ureq::get(&url)).call(),
        "DELETE" => with_trace(ureq::delete(&url)).call(),
        "POST" => {
            with_trace(ureq::post(&url).set("Content-Type", "application/json")).send_string(body)
        }
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

/// The SSE push endpoint (RAL-167), proxied straight through rather than via
/// the buffered `proxy()` path above.
const EVENTS_PATH: &str = "/api/events";

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

        if method == "GET" && url.split('?').next().unwrap_or(&url) == EVENTS_PATH {
            // Like the daemon's own accept loop, this one is otherwise
            // synchronous/single-request -- a long-lived SSE connection held
            // here would starve every other client (RAL-167).
            let daemon_url = daemon_url.to_string();
            std::thread::spawn(move || proxy_events_stream(&daemon_url, request));
            continue;
        }

        let traceparent = request
            .headers()
            .iter()
            .find(|h| h.field.equiv("traceparent"))
            .map(|h| h.value.as_str().to_string());

        let mut body = String::new();
        let _ = request.as_reader().read_to_string(&mut body);

        let reply = handle_with_trace(daemon_url, &method, &url, &body, traceparent.as_deref());
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

/// Stream-proxy the daemon's `/api/events` SSE endpoint straight through to
/// the browser, byte for byte, on its own thread (RAL-167). Unlike `proxy()`,
/// this never buffers a full response: `ureq`'s `.call()` returns as soon as
/// the daemon's response headers arrive (the body is a live, unbounded
/// stream), and every chunk read from it is written straight to the browser
/// connection and flushed immediately.
fn proxy_events_stream(daemon_url: &str, request: tiny_http::Request) {
    let url = format!("{}{EVENTS_PATH}", daemon_url.trim_end_matches('/'));
    let mut writer = request.into_writer();
    let resp = match ureq::get(&url).call() {
        Ok(r) => r,
        Err(_) => {
            let _ = writer.write_all(
                b"HTTP/1.1 502 Bad Gateway\r\nContent-Type: text/plain\r\n\r\ndaemon unreachable",
            );
            return;
        }
    };
    let preamble = b"HTTP/1.1 200 OK\r\n\
Content-Type: text/event-stream\r\n\
Cache-Control: no-cache\r\n\
Connection: keep-alive\r\n\
X-Accel-Buffering: no\r\n\
\r\n";
    if writer.write_all(preamble).is_err() || writer.flush().is_err() {
        return;
    }
    let mut reader = resp.into_reader();
    let mut buf = [0u8; 4096];
    loop {
        let n = match reader.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        if writer.write_all(&buf[..n]).is_err() || writer.flush().is_err() {
            break;
        }
    }
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

    // ── RAL-96: OpenTelemetry trace propagation ─────────────────────────────

    #[test]
    fn handle_with_trace_behaves_identically_to_handle_for_static_routes() {
        let traced = handle_with_trace("http://127.0.0.1:9", "GET", "/", "", None);
        let plain = handle("http://127.0.0.1:9", "GET", "/", "");
        assert_eq!(traced.status, plain.status);
        assert_eq!(traced.body, plain.body);
    }

    #[test]
    fn handle_with_trace_forwards_the_incoming_traceparent_to_the_daemon() {
        let server = tiny_http::Server::http("127.0.0.1:0").expect("bind ephemeral port");
        let port = server.server_addr().to_ip().expect("ip addr").port();
        let received = std::sync::Arc::new(std::sync::Mutex::new(None::<String>));
        let received_clone = std::sync::Arc::clone(&received);
        let handle_thread = std::thread::spawn(move || {
            if let Ok(req) = server.recv() {
                let tp = req
                    .headers()
                    .iter()
                    .find(|h| h.field.equiv("traceparent"))
                    .map(|h| h.value.as_str().to_string());
                *received_clone.lock().unwrap() = tp;
                let _ = req.respond(tiny_http::Response::from_string("{}"));
            }
        });
        let daemon_url = format!("http://127.0.0.1:{port}");
        let incoming = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
        let reply = handle_with_trace(&daemon_url, "GET", "/api/tasks", "", Some(incoming));
        handle_thread.join().unwrap();
        assert_eq!(reply.status, 200);
        let forwarded = received.lock().unwrap().clone().expect("traceparent sent");
        // Same trace id as the browser's header — a new span id (the
        // librarian's own span), not a bare copy.
        assert_eq!(forwarded.split('-').nth(1), incoming.split('-').nth(1));
    }

    /// RAL-98 regression: the proxy must forward `?query` strings to the
    /// daemon verbatim (e.g. `/api/cartographer?run_id=...&limit=...`) rather
    /// than stripping them, or every filtered/paginated endpoint silently
    /// loses its parameters when reached through the librarian.
    #[test]
    fn api_get_forwards_the_full_query_string_to_the_daemon() {
        let server = tiny_http::Server::http("127.0.0.1:0").expect("bind ephemeral port");
        let port = server.server_addr().to_ip().expect("ip addr").port();
        let received = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let received_clone = std::sync::Arc::clone(&received);
        let handle_thread = std::thread::spawn(move || {
            if let Ok(req) = server.recv() {
                *received_clone.lock().unwrap() = req.url().to_string();
                let _ = req.respond(tiny_http::Response::from_string("{}"));
            }
        });
        let daemon_url = format!("http://127.0.0.1:{port}");
        let reply = handle(
            &daemon_url,
            "GET",
            "/api/cartographer?run_id=run-1&limit=5&sort=asc",
            "",
        );
        handle_thread.join().unwrap();
        assert_eq!(reply.status, 200);
        assert_eq!(
            *received.lock().unwrap(),
            "/api/cartographer?run_id=run-1&limit=5&sort=asc"
        );
    }

    #[test]
    fn static_route_match_ignores_a_query_string() {
        // "/" with a stray query string (shouldn't normally happen, but the
        // static-route match must not require an exact string match against
        // the raw path) still serves the page rather than 404ing.
        let reply = handle("http://127.0.0.1:9", "GET", "/?foo=bar", "");
        assert_eq!(reply.status, 200);
        assert!(reply.body.contains("ralphus"));
    }
}
