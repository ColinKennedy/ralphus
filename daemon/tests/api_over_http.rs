//! Over-the-wire integration test: bind the real HTTP server on an ephemeral
//! port and drive it with an HTTP client, exercising the full submit → list →
//! get path through `tiny_http` and JSON serialization.

use std::thread;

use ralphus_daemon::server::serve_with;
use ralphus_daemon::store::Store;

fn spawn_server() -> String {
    let server = tiny_http::Server::http("127.0.0.1:0").expect("bind ephemeral port");
    let addr = server.server_addr().to_ip().expect("ip addr");
    let store = Store::open_in_memory().expect("store");
    thread::spawn(move || serve_with(server, store, 4));
    format!("http://{addr}")
}

fn post(base: &str, path: &str, body: &str) -> (u16, String) {
    let resp = ureq::post(&format!("{base}{path}"))
        .set("Content-Type", "application/json")
        .send_string(body);
    match resp {
        Ok(r) => (r.status(), r.into_string().unwrap_or_default()),
        Err(ureq::Error::Status(code, r)) => (code, r.into_string().unwrap_or_default()),
        Err(e) => panic!("request error: {e}"),
    }
}

fn get(base: &str, path: &str) -> (u16, String) {
    let resp = ureq::get(&format!("{base}{path}")).call();
    match resp {
        Ok(r) => (r.status(), r.into_string().unwrap_or_default()),
        Err(ureq::Error::Status(code, r)) => (code, r.into_string().unwrap_or_default()),
        Err(e) => panic!("request error: {e}"),
    }
}

const GOOD: &str = "[[task]]\nname=\"build\"\n[[task.cell]]\ncwd=\"/repo\"\nprompt=\"go\"\n";

#[test]
fn full_submit_and_read_cycle_over_http() {
    let base = spawn_server();

    let (status, body) = get(&base, "/api/daemon");
    assert_eq!(status, 200, "health body: {body}");
    assert!(body.contains("ralphus-daemon"));

    let submit_body = serde_json::json!({ "toml": GOOD, "label": "wire test" }).to_string();
    let (status, body) = post(&base, "/api/squads", &submit_body);
    assert_eq!(status, 201, "submit body: {body}");
    assert!(body.contains("squad-000000000001"));

    let (status, body) = get(&base, "/api/tasks");
    assert_eq!(status, 200);
    assert!(body.contains("wire test"));
    assert!(body.contains("\"name\":\"build\""));

    let (status, body) = get(&base, "/api/squads/squad-000000000001");
    assert_eq!(status, 200);
    assert!(body.contains("\"cwd\":\"/repo\""));

    let (status, _) = post(&base, "/api/squads/squad-000000000001/cancel", "");
    assert_eq!(status, 200);
}

#[test]
fn invalid_submission_is_rejected_over_http() {
    let base = spawn_server();
    let body = serde_json::json!({ "toml": "garbage = true" }).to_string();
    let (status, resp) = post(&base, "/api/squads", &body);
    assert_eq!(status, 400);
    assert!(resp.contains("validation_failed"));
}

// ── RAL-220: explicit CORS policy ───────────────────────────────────────────

/// A cross-origin browser `POST` to a state-changing endpoint must be
/// rejected outright -- not merely served without `Access-Control-*`
/// headers, which would stop the browser from *reading* the response but not
/// from the squad actually being created server-side (the "simple request"
/// CSRF-style gap this ticket closes).
#[test]
fn cross_origin_post_to_state_changing_endpoint_is_blocked() {
    let base = spawn_server();
    let submit_body = serde_json::json!({ "toml": GOOD, "label": "cors probe" }).to_string();

    let resp = ureq::post(&format!("{base}/api/squads"))
        .set("Content-Type", "application/json")
        .set("Origin", "https://evil.example")
        .send_string(&submit_body);
    let status = match resp {
        Ok(r) => r.status(),
        Err(ureq::Error::Status(code, _)) => code,
        Err(e) => panic!("request error: {e}"),
    };
    assert_eq!(status, 403);

    // The request must never have reached the handler at all -- confirm no
    // squad was created, not just that the 403 response was returned.
    let (_, body) = get(&base, "/api/tasks");
    assert!(!body.contains("cors probe"));
}

/// A cross-origin preflight `OPTIONS` for a disallowed origin is rejected the
/// same way -- the browser never learns it may send the real request.
#[test]
fn cross_origin_preflight_for_disallowed_origin_is_blocked() {
    let base = spawn_server();
    let resp = ureq::request("OPTIONS", &format!("{base}/api/squads"))
        .set("Origin", "https://evil.example")
        .set("Access-Control-Request-Method", "POST")
        .call();
    let status = match resp {
        Ok(r) => r.status(),
        Err(ureq::Error::Status(code, _)) => code,
        Err(e) => panic!("request error: {e}"),
    };
    assert_eq!(status, 403);
}

/// Same-origin usage (the board's own relative-URL fetches, which browsers
/// send with `Origin` set to the page's own origin) must keep working
/// unaffected, and get back an explicit `Access-Control-Allow-Origin` header
/// echoing that origin.
#[test]
fn same_origin_post_is_allowed_and_echoes_the_origin_header() {
    let base = spawn_server();
    let submit_body = serde_json::json!({ "toml": GOOD, "label": "same origin" }).to_string();

    let resp = ureq::post(&format!("{base}/api/squads"))
        .set("Content-Type", "application/json")
        .set("Origin", &base)
        .send_string(&submit_body);
    let resp = match resp {
        Ok(r) => r,
        Err(ureq::Error::Status(code, r)) => {
            panic!("unexpected {code}: {}", r.into_string().unwrap_or_default())
        }
        Err(e) => panic!("request error: {e}"),
    };
    assert_eq!(resp.status(), 201);
    assert_eq!(
        resp.header("Access-Control-Allow-Origin"),
        Some(base.as_str())
    );
}

/// A request with no `Origin` header at all (a non-browser caller: the CLI,
/// `curl`, a remote-machine caller per RAL-185) is untouched by CORS -- it is
/// not a browser cross-origin request, so there is nothing to gate.
#[test]
fn request_without_origin_header_is_unaffected() {
    let base = spawn_server();
    let (status, body) = get(&base, "/api/daemon");
    assert_eq!(status, 200, "health body: {body}");
}
