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
        (_, p) if p.starts_with("/api/") => proxy(
            daemon_url,
            method,
            path,
            body,
            None,
            daemon_token().as_deref(),
        ),
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
        (_, p) if p.starts_with("/api/") => proxy(
            daemon_url,
            method,
            path,
            body,
            Some(&span.cx),
            daemon_token().as_deref(),
        ),
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

/// Read the daemon's bearer token from `state_dir()/daemon.token` (RAL-219),
/// the same file the daemon itself generates/persists at startup. The
/// librarian holds no other state, so this is read fresh on every proxied
/// request rather than cached — a cheap local file read, and it means a
/// token that didn't exist yet at librarian startup (daemon started later)
/// or was regenerated is always picked up on the very next request.
fn daemon_token() -> Option<String> {
    std::fs::read_to_string(ralphus_core::daemon_token_path())
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Forward a request to the daemon and relay its status and body. A daemon that
/// is down becomes a 502 with an error envelope the page knows how to show.
/// When `parent` carries a valid span, its `traceparent` is forwarded to the
/// daemon as a request header (RAL-96) so the daemon's own span continues the
/// same trace instead of starting a disconnected one. `token` (RAL-219, read
/// by the caller via [`daemon_token`] — kept as a plain parameter here, like
/// `parent`, so tests can exercise the forwarding without touching the real
/// token file) is attached the same way, so the board UI keeps working
/// without the operator configuring anything extra for local use.
fn proxy(
    daemon_url: &str,
    method: &str,
    path: &str,
    body: &str,
    parent: Option<&Context>,
    token: Option<&str>,
) -> Reply {
    let url = format!("{}{}", daemon_url.trim_end_matches('/'), path);
    let traceparent = parent.and_then(crate::otel::traceparent_from_context);
    let with_trace = |req: ureq::Request| {
        let req = match &traceparent {
            Some(tp) => req.set("traceparent", tp),
            None => req,
        };
        match token {
            Some(t) => req.set("Authorization", &format!("Bearer {t}")),
            None => req,
        }
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

/// Case-insensitive header lookup (`tiny_http::Header::field` compares
/// case-insensitively via `.equiv`).
fn header_value(request: &tiny_http::Request, name: &'static str) -> Option<String> {
    request
        .headers()
        .iter()
        .find(|h| h.field.equiv(name))
        .map(|h| h.value.as_str().to_string())
}

fn cors_header(name: &'static [u8], value: &str) -> tiny_http::Header {
    tiny_http::Header::from_bytes(name, value.as_bytes()).expect("valid header")
}

/// `Access-Control-Allow-Origin` (echoing the exact allowed origin, never a
/// wildcard, per RAL-220) plus `Vary: Origin`.
fn cors_response_headers(origin: &str) -> Vec<tiny_http::Header> {
    vec![
        cors_header(b"Access-Control-Allow-Origin", origin),
        cors_header(b"Vary", "Origin"),
    ]
}

/// The additional headers a preflight `OPTIONS` response needs beyond
/// [`cors_response_headers`].
fn cors_preflight_headers(origin: &str) -> Vec<tiny_http::Header> {
    let mut headers = cors_response_headers(origin);
    headers.push(cors_header(
        b"Access-Control-Allow-Methods",
        "GET, POST, DELETE, OPTIONS",
    ));
    headers.push(cors_header(
        b"Access-Control-Allow-Headers",
        "Content-Type, traceparent",
    ));
    headers
}

/// Resolve the CORS decision for one incoming request against the effective
/// (global + per-project `.ralphus.toml`) `[cors]` allow-list. Applied at the
/// librarian's own HTTP boundary, independent of the daemon's own gate --
/// see this module's `config` sibling for why (the daemon never sees a
/// browser's `Origin` header when it arrives via the librarian's proxy).
fn resolve_cors(request: &tiny_http::Request) -> ralphus_core::cors::CorsDecision {
    let origin = header_value(request, "Origin");
    let host = header_value(request, "Host");
    let allowed = crate::config::load_cors_config().allowed_origins;
    ralphus_core::cors::decide(origin.as_deref(), host.as_deref(), &allowed)
}

/// Serve the librarian on `port`, proxying the API to `daemon_url`. Binds
/// `127.0.0.1` by default, overridable via `RALPHUS_BIND_ADDR` (see
/// `crate::resolve_bind_host`) -- the container execution mode (RAL-225)
/// sets it to `0.0.0.0` so the published port is reachable from the host.
///
/// # Errors
/// Returns an error if the listener cannot bind.
pub fn serve(port: u16, daemon_url: &str) -> std::io::Result<()> {
    let bind_host = crate::resolve_bind_host(std::env::var(crate::BIND_ADDR_ENV).ok().as_deref());
    let mut server = tiny_http::Server::http((bind_host.as_str(), port))
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    // tiny_http 0.12 treats any accept() error as fatal: its accept thread
    // reports the error via the `log` crate (no logger is installed here, so
    // the message vanishes) and exits, after which [`serve_with`]'s loop ends.
    // A transient WSAENOBUFS/WSAEMFILE under machine-wide socket pressure
    // used to end the librarian silently — and scripts/build-debug's cleanup
    // then killed the daemon too, since the librarian is its foreground
    // process. Rebind and keep serving instead; `serve_with` only returns
    // when the accept thread has died.
    loop {
        serve_with(server, daemon_url);
        eprintln!(
            "ralphus-librarian: HTTP listener stopped accepting connections \
             (tiny_http accept thread died); rebinding"
        );
        server = rebind_listener(&bind_host, port)?;
    }
}

/// Re-create the HTTP listener after tiny_http's accept thread died (see the
/// rebind loop in [`serve`]). Retries briefly: the port is freed when the
/// previous `Server` is dropped, but the same resource pressure that killed
/// the accept thread can make the first bind attempts fail too.
fn rebind_listener(bind_host: &str, port: u16) -> std::io::Result<tiny_http::Server> {
    const ATTEMPTS: u32 = 20;
    let mut last_err = String::new();
    for attempt in 1..=ATTEMPTS {
        match tiny_http::Server::http((bind_host, port)) {
            Ok(server) => {
                eprintln!(
                    "ralphus-librarian: listener rebound on {bind_host}:{port} (attempt {attempt})"
                );
                return Ok(server);
            }
            Err(e) => {
                eprintln!(
                    "ralphus-librarian: rebind attempt {attempt}/{ATTEMPTS} on \
                     {bind_host}:{port} failed: {e}"
                );
                last_err = e.to_string();
                std::thread::sleep(std::time::Duration::from_millis(500));
            }
        }
    }
    Err(std::io::Error::other(format!(
        "could not rebind the librarian listener on {bind_host}:{port} after {ATTEMPTS} attempts: {last_err}"
    )))
}

/// Serve requests from an already-bound server, proxying the API to
/// `daemon_url`. Split out from [`serve`] (mirroring
/// `ralphus_daemon::server::serve`/`serve_with`) so tests can bind an
/// ephemeral port and drive the CORS-gated HTTP loop directly, rather than
/// only the pure, header-free [`handle`]/[`handle_with_trace`] functions.
///
/// RAL-239 follow-up: each accepted request is dispatched onto its own
/// thread via [`handle_request`], the same pattern this file already used
/// for the SSE branch alone. Previously every request -- including plain
/// proxied `GET`s -- ran inline in this loop, one at a time; a browser tab
/// left open on the board keeps several connections alive concurrently
/// (polling, live-view peek panes, PR-sync checks), and any one of those
/// taking a while to answer serialized every other request behind it,
/// including an unrelated page load or tab switch. Handing each request its
/// own thread immediately makes the accept loop free to take the next one
/// right away.
pub fn serve_with(server: tiny_http::Server, daemon_url: &str) {
    for request in server.incoming_requests() {
        let daemon_url = daemon_url.to_string();
        std::thread::spawn(move || handle_request(request, &daemon_url));
    }
}

/// Handle one accepted request end to end: CORS gating, `OPTIONS` preflight,
/// the SSE stream, or an ordinary proxied call -- exactly the branches
/// [`serve_with`] used to run inline, now run on that request's own thread.
fn handle_request(mut request: tiny_http::Request, daemon_url: &str) {
    let method = request.method().as_str().to_string();
    let url = request.url().to_string();

    // RAL-220: reject a disallowed cross-origin browser request outright
    // before it is proxied anywhere -- see `ralphus_core::cors`'s doc
    // comment for why omitting `Access-Control-*` headers alone isn't
    // enough. A request with no `Origin` header (same-origin browser
    // traffic, or any non-browser caller) is unaffected.
    let cors = resolve_cors(&request);
    if cors == ralphus_core::cors::CorsDecision::Denied {
        let response = tiny_http::Response::new(
            tiny_http::StatusCode(403),
            vec![cors_header(b"Content-Type", "application/json")],
            Cursor::new(
                br#"{"error":{"code":"origin_not_allowed","message":"cross-origin request denied"}}"#
                    .to_vec(),
            ),
            None,
            None,
        );
        let _ = request.respond(response);
        return;
    }

    if method == "OPTIONS" {
        let headers = match &cors {
            ralphus_core::cors::CorsDecision::Allowed(origin) => cors_preflight_headers(origin),
            _ => vec![],
        };
        let response = tiny_http::Response::new(
            tiny_http::StatusCode(204),
            headers,
            Cursor::new(Vec::new()),
            None,
            None,
        );
        let _ = request.respond(response);
        return;
    }

    if method == "GET" && url.split('?').next().unwrap_or(&url) == EVENTS_PATH {
        let allow_origin = match cors {
            ralphus_core::cors::CorsDecision::Allowed(origin) => Some(origin),
            _ => None,
        };
        proxy_events_stream(daemon_url, &url, request, allow_origin.as_deref());
        return;
    }

    let traceparent = header_value(&request, "traceparent");

    let mut body = String::new();
    let _ = request.as_reader().read_to_string(&mut body);

    let reply = handle_with_trace(daemon_url, &method, &url, &body, traceparent.as_deref());
    let mut headers = vec![
        tiny_http::Header::from_bytes(&b"Content-Type"[..], reply.content_type.as_bytes())
            .expect("valid header"),
    ];
    if let ralphus_core::cors::CorsDecision::Allowed(origin) = &cors {
        headers.extend(cors_response_headers(origin));
    }
    let response = tiny_http::Response::new(
        tiny_http::StatusCode(reply.status),
        headers,
        Cursor::new(reply.body.into_bytes()),
        None,
        None,
    );
    let _ = request.respond(response);
}

/// Stream-proxy the daemon's `/api/events` SSE endpoint straight through to
/// the browser, byte for byte, on its own thread (RAL-167). Unlike `proxy()`,
/// this never buffers a full response: `ureq`'s `.call()` returns as soon as
/// the daemon's response headers arrive (the body is a live, unbounded
/// stream), and every chunk read from it is written straight to the browser
/// connection and flushed immediately.
///
/// `incoming_url` is the browser's full request URL, `?ticket=...` and all
/// (RAL-222) — board.html mints that ticket itself via `POST
/// /api/events/ticket` (forwarded to the daemon like any other `/api/*` call,
/// see `proxy`) and supplies it here because `EventSource` cannot set custom
/// headers, so the ticket has to travel as a query param instead. This
/// function's only job on the auth front is to relay it through verbatim —
/// the daemon is the one that validates and consumes it.
fn proxy_events_stream(
    daemon_url: &str,
    incoming_url: &str,
    request: tiny_http::Request,
    allow_origin: Option<&str>,
) {
    let url = format!("{}{incoming_url}", daemon_url.trim_end_matches('/'));
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
    // RAL-220: echo the resolved-allowed origin (if any) the same way the
    // buffered response path does -- see `resolve_cors` in `serve()`.
    let cors_lines = match allow_origin {
        Some(origin) => format!("Access-Control-Allow-Origin: {origin}\r\nVary: Origin\r\n"),
        None => String::new(),
    };
    let preamble = format!(
        "HTTP/1.1 200 OK\r\n\
Content-Type: text/event-stream\r\n\
Cache-Control: no-cache\r\n\
Connection: keep-alive\r\n\
X-Accel-Buffering: no\r\n\
{cors_lines}\r\n"
    );
    if writer.write_all(preamble.as_bytes()).is_err() || writer.flush().is_err() {
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
        let reply = handle("http://127.0.0.1:9", "POST", "/api/squads", "{}");
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

    // ── RAL-219: bearer-token forwarding ────────────────────────────────────

    /// The proxy must attach `Authorization: Bearer <token>` when one is
    /// available, so the board UI keeps working against a daemon that now
    /// requires it, with no extra operator configuration. Calls `proxy()`
    /// directly with an explicit token (rather than through `handle`, which
    /// reads the real `state_dir()/daemon.token`) so this test never touches
    /// the developer machine's actual token file.
    #[test]
    fn proxy_forwards_the_bearer_token_when_present() {
        let server = tiny_http::Server::http("127.0.0.1:0").expect("bind ephemeral port");
        let port = server.server_addr().to_ip().expect("ip addr").port();
        let received = std::sync::Arc::new(std::sync::Mutex::new(None::<String>));
        let received_clone = std::sync::Arc::clone(&received);
        let handle_thread = std::thread::spawn(move || {
            if let Ok(req) = server.recv() {
                let auth = req
                    .headers()
                    .iter()
                    .find(|h| h.field.equiv("Authorization"))
                    .map(|h| h.value.as_str().to_string());
                *received_clone.lock().unwrap() = auth;
                let _ = req.respond(tiny_http::Response::from_string("{}"));
            }
        });
        let daemon_url = format!("http://127.0.0.1:{port}");
        let reply = proxy(
            &daemon_url,
            "GET",
            "/api/tasks",
            "",
            None,
            Some("secret-token-value"),
        );
        handle_thread.join().unwrap();
        assert_eq!(reply.status, 200);
        assert_eq!(
            received.lock().unwrap().clone(),
            Some("Bearer secret-token-value".to_string())
        );
    }

    /// No token available (e.g. the daemon hasn't written one yet) must not
    /// crash or send a bogus header — just proxy without `Authorization`.
    #[test]
    fn proxy_sends_no_authorization_header_when_no_token_is_available() {
        let server = tiny_http::Server::http("127.0.0.1:0").expect("bind ephemeral port");
        let port = server.server_addr().to_ip().expect("ip addr").port();
        let received = std::sync::Arc::new(std::sync::Mutex::new(None::<String>));
        let received_clone = std::sync::Arc::clone(&received);
        let handle_thread = std::thread::spawn(move || {
            if let Ok(req) = server.recv() {
                let auth = req
                    .headers()
                    .iter()
                    .find(|h| h.field.equiv("Authorization"))
                    .map(|h| h.value.as_str().to_string());
                *received_clone.lock().unwrap() = auth;
                let _ = req.respond(tiny_http::Response::from_string("{}"));
            }
        });
        let daemon_url = format!("http://127.0.0.1:{port}");
        let reply = proxy(&daemon_url, "GET", "/api/tasks", "", None, None);
        handle_thread.join().unwrap();
        assert_eq!(reply.status, 200);
        assert_eq!(received.lock().unwrap().clone(), None);
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
            "/api/cartographer?squad_id=squad-1&limit=5&sort=asc",
            "",
        );
        handle_thread.join().unwrap();
        assert_eq!(reply.status, 200);
        assert_eq!(
            *received.lock().unwrap(),
            "/api/cartographer?squad_id=squad-1&limit=5&sort=asc"
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

    // ── RAL-239: per-request threading, not a shared serial queue ──────────

    /// `serve_with` used to process every request -- including plain proxied
    /// `GET`s -- one at a time on its single accept-loop thread. In practice
    /// a browser tab left open on the board keeps several connections alive
    /// concurrently, and any one request that's slow to answer (a forge
    /// PR-sync check, a tmux pane capture, ...) serialized every other
    /// request behind it, including an unrelated page's own load. Spins up a
    /// real `serve_with` loop against a fake daemon that deliberately sleeps
    /// on one path, fires a slow and a fast request concurrently, and asserts
    /// the fast one finishes well within the slow one's sleep window rather
    /// than queuing behind it.
    #[test]
    fn serve_with_does_not_serialize_requests_behind_a_slow_one() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::time::{Duration, Instant};

        // The fake daemon hands each accepted connection its own thread too
        // (so it never becomes the bottleneck under test here), sleeping
        // only for the `/api/slow` path.
        let daemon_server = tiny_http::Server::http("127.0.0.1:0").expect("bind daemon");
        let daemon_port = daemon_server.server_addr().to_ip().expect("ip addr").port();
        let served = Arc::new(AtomicUsize::new(0));
        let served_clone = Arc::clone(&served);
        std::thread::spawn(move || {
            for req in daemon_server.incoming_requests() {
                served_clone.fetch_add(1, Ordering::SeqCst);
                std::thread::spawn(move || {
                    if req.url().starts_with("/api/slow") {
                        std::thread::sleep(Duration::from_millis(500));
                    }
                    let _ = req.respond(tiny_http::Response::from_string("{}"));
                });
            }
        });
        let daemon_url = format!("http://127.0.0.1:{daemon_port}");

        let lib_server = tiny_http::Server::http("127.0.0.1:0").expect("bind librarian");
        let lib_port = lib_server.server_addr().to_ip().expect("ip addr").port();
        std::thread::spawn(move || serve_with(lib_server, &daemon_url));
        // Give both accept loops a moment to actually be listening before
        // any client connects.
        std::thread::sleep(Duration::from_millis(100));

        let lib_url = format!("http://127.0.0.1:{lib_port}");
        let slow_url = format!("{lib_url}/api/slow");
        std::thread::spawn(move || {
            let _ = ureq::get(&slow_url).call();
        });
        // Give the slow request a head start so it's already in flight
        // through the librarian when the fast one arrives.
        std::thread::sleep(Duration::from_millis(100));

        let fast_url = format!("{lib_url}/api/fast");
        let t0 = Instant::now();
        let resp = ureq::get(&fast_url)
            .call()
            .expect("fast request should succeed");
        let elapsed = t0.elapsed();

        assert_eq!(resp.status(), 200);
        assert!(
            elapsed < Duration::from_millis(400),
            "fast request took {elapsed:?}, expected it to complete well under the slow \
             request's 500ms sleep -- it must not be serialized behind it"
        );
        assert_eq!(
            served.load(Ordering::SeqCst),
            2,
            "both requests should have reached the fake daemon"
        );
    }
}
