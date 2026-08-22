//! Over-the-wire integration tests for RAL-219's bearer-token auth gate:
//! every route requires `Authorization: Bearer <token>` once one is
//! configured, using the exact same real-HTTP idiom as `api_over_http.rs`.

use std::thread;

use ralphus_daemon::server::serve_with_token;
use ralphus_daemon::store::Store;

const TOKEN: &str = "test-token-value";

fn spawn_authenticated_server() -> String {
    let server = tiny_http::Server::http("127.0.0.1:0").expect("bind ephemeral port");
    let addr = server.server_addr().to_ip().expect("ip addr");
    let store = Store::open_in_memory().expect("store");
    thread::spawn(move || serve_with_token(server, store, 4, TOKEN.to_string()));
    format!("http://{addr}")
}

fn get(base: &str, path: &str, token: Option<&str>) -> (u16, String) {
    let mut req = ureq::get(&format!("{base}{path}"));
    if let Some(t) = token {
        req = req.set("Authorization", &format!("Bearer {t}"));
    }
    match req.call() {
        Ok(r) => (r.status(), r.into_string().unwrap_or_default()),
        Err(ureq::Error::Status(code, r)) => (code, r.into_string().unwrap_or_default()),
        Err(e) => panic!("request error: {e}"),
    }
}

fn post(base: &str, path: &str, body: &str, token: Option<&str>) -> (u16, String) {
    let mut req = ureq::post(&format!("{base}{path}")).set("Content-Type", "application/json");
    if let Some(t) = token {
        req = req.set("Authorization", &format!("Bearer {t}"));
    }
    match req.send_string(body) {
        Ok(r) => (r.status(), r.into_string().unwrap_or_default()),
        Err(ureq::Error::Status(code, r)) => (code, r.into_string().unwrap_or_default()),
        Err(e) => panic!("request error: {e}"),
    }
}

#[test]
fn a_get_route_without_a_token_is_rejected() {
    let base = spawn_authenticated_server();
    let (status, body) = get(&base, "/api/daemon", None);
    assert_eq!(status, 401, "body: {body}");
    assert!(body.contains("unauthorized"));
}

#[test]
fn a_get_route_with_the_wrong_token_is_rejected() {
    let base = spawn_authenticated_server();
    let (status, body) = get(&base, "/api/daemon", Some("not-the-real-token"));
    assert_eq!(status, 401, "body: {body}");
}

#[test]
fn a_get_route_with_the_correct_token_succeeds() {
    let base = spawn_authenticated_server();
    let (status, body) = get(&base, "/api/daemon", Some(TOKEN));
    assert_eq!(status, 200, "body: {body}");
    assert!(body.contains("ralphus-daemon"));
}

#[test]
fn a_post_route_without_a_token_is_rejected_before_reaching_the_store() {
    let base = spawn_authenticated_server();
    const GOOD: &str = "[[task]]\nname=\"build\"\n[[task.cell]]\ncwd=\"/repo\"\nprompt=\"go\"\n";
    let submit_body = serde_json::json!({ "toml": GOOD }).to_string();
    let (status, _) = post(&base, "/api/squads", &submit_body, None);
    assert_eq!(status, 401);
    // Confirms the squad was never persisted -- listing tasks (with the right
    // token this time) shows nothing landed.
    let (status, body) = get(&base, "/api/tasks", Some(TOKEN));
    assert_eq!(status, 200);
    assert!(!body.contains("\"name\":\"build\""));
}

#[test]
fn a_post_route_with_the_correct_token_succeeds() {
    let base = spawn_authenticated_server();
    const GOOD: &str = "[[task]]\nname=\"build\"\n[[task.cell]]\ncwd=\"/repo\"\nprompt=\"go\"\n";
    let submit_body = serde_json::json!({ "toml": GOOD }).to_string();
    let (status, body) = post(&base, "/api/squads", &submit_body, Some(TOKEN));
    assert_eq!(status, 201, "body: {body}");
}

/// The stated reason for RAL-219 is enabling a remote/non-browser caller --
/// a plain HTTP client (here, `ureq`, standing in for curl/a script) with the
/// right token must be able to drive the full API, not just one endpoint.
#[test]
fn a_non_browser_client_can_complete_a_full_submit_and_read_cycle_with_a_valid_token() {
    let base = spawn_authenticated_server();
    const GOOD: &str = "[[task]]\nname=\"build\"\n[[task.cell]]\ncwd=\"/repo\"\nprompt=\"go\"\n";

    let (status, _) = get(&base, "/api/daemon", Some(TOKEN));
    assert_eq!(status, 200);

    let submit_body = serde_json::json!({ "toml": GOOD, "label": "auth test" }).to_string();
    let (status, body) = post(&base, "/api/squads", &submit_body, Some(TOKEN));
    assert_eq!(status, 201, "submit body: {body}");
    assert!(body.contains("squad-000000000001"));

    let (status, body) = get(&base, "/api/squads/squad-000000000001", Some(TOKEN));
    assert_eq!(status, 200);
    assert!(body.contains("\"cwd\":\"/repo\""));

    let (status, _) = post(
        &base,
        "/api/squads/squad-000000000001/cancel",
        "",
        Some(TOKEN),
    );
    assert_eq!(status, 200);
}

/// A daemon with no token configured (e.g. `serve_with` in `api_over_http.rs`)
/// stays open -- so those existing over-the-wire tests keep working without a
/// header. This test pins that intentional default rather than relying on
/// the other file's tests to notice a regression.
#[test]
fn no_token_configured_means_the_api_stays_open() {
    let server = tiny_http::Server::http("127.0.0.1:0").expect("bind ephemeral port");
    let addr = server.server_addr().to_ip().expect("ip addr");
    let store = Store::open_in_memory().expect("store");
    thread::spawn(move || ralphus_daemon::server::serve_with(server, store, 4));
    let base = format!("http://{addr}");
    let (status, _) = get(&base, "/api/daemon", None);
    assert_eq!(status, 200);
}
