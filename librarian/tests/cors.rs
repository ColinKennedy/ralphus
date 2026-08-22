//! Over-the-wire integration test for RAL-220's CORS gate: bind the real
//! librarian HTTP server on an ephemeral port and drive it with an HTTP
//! client carrying an `Origin` header, exercising the full `tiny_http`
//! request loop rather than just the pure, header-free `handle`/
//! `handle_with_trace` functions (which never see `Origin` at all -- the
//! gate lives in `serve_with`'s loop).

use std::thread;

use ralphus_librarian::server::serve_with;

fn spawn_server(daemon_url: &'static str) -> String {
    let server = tiny_http::Server::http("127.0.0.1:0").expect("bind ephemeral port");
    let addr = server.server_addr().to_ip().expect("ip addr");
    thread::spawn(move || serve_with(server, daemon_url));
    format!("http://{addr}")
}

/// A cross-origin browser request must be rejected outright at the
/// librarian's own boundary -- it must never even reach the proxy step, or a
/// browser page could bypass the daemon's own CORS gate by going through the
/// librarian instead (see AGENTS.md's RAL-220 risk list).
#[test]
fn cross_origin_request_to_librarian_is_blocked() {
    let base = spawn_server("http://127.0.0.1:9"); // port 9 (discard): daemon irrelevant, request must never reach it
    let resp = ureq::post(&format!("{base}/api/runs"))
        .set("Content-Type", "application/json")
        .set("Origin", "https://evil.example")
        .send_string("{}");
    let status = match resp {
        Ok(r) => r.status(),
        Err(ureq::Error::Status(code, _)) => code,
        Err(e) => panic!("request error: {e}"),
    };
    assert_eq!(status, 403);
}

/// A same-origin request (the board's own relative-URL fetches, whose
/// `Origin` matches the librarian's own address) is unaffected and gets back
/// an explicit `Access-Control-Allow-Origin` header echoing that origin.
#[test]
fn same_origin_request_is_allowed_and_echoes_the_origin_header() {
    let base = spawn_server("http://127.0.0.1:9");
    let resp = ureq::get(&format!("{base}/api/tasks"))
        .set("Origin", &base)
        .call();
    // Daemon is unreachable (port 9), so the proxy degrades to 502 -- the
    // point here is that the request executes at all (unlike the blocked
    // case above) and carries the CORS header back.
    let resp = match resp {
        Ok(r) => r,
        Err(ureq::Error::Status(_, r)) => r,
        Err(e) => panic!("request error: {e}"),
    };
    assert_eq!(resp.status(), 502);
    assert_eq!(
        resp.header("Access-Control-Allow-Origin"),
        Some(base.as_str())
    );
}

/// A request with no `Origin` header (a non-browser caller, or a top-level
/// page navigation) is untouched by CORS.
#[test]
fn request_without_origin_header_serves_the_board() {
    let base = spawn_server("http://127.0.0.1:9");
    let resp = ureq::get(&base).call().expect("request succeeds");
    assert_eq!(resp.status(), 200);
}
