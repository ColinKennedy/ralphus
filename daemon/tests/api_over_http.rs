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

const GOOD: &str = "[[task]]\nname=\"build\"\n[[task.session]]\ncwd=\"/repo\"\nprompt=\"go\"\n";

#[test]
fn full_submit_and_read_cycle_over_http() {
    let base = spawn_server();

    let (status, body) = get(&base, "/api/daemon");
    assert_eq!(status, 200, "health body: {body}");
    assert!(body.contains("ralphus-daemon"));

    let submit_body = serde_json::json!({ "toml": GOOD, "label": "wire test" }).to_string();
    let (status, body) = post(&base, "/api/runs", &submit_body);
    assert_eq!(status, 201, "submit body: {body}");
    assert!(body.contains("run-000000000001"));

    let (status, body) = get(&base, "/api/tasks");
    assert_eq!(status, 200);
    assert!(body.contains("wire test"));
    assert!(body.contains("\"name\":\"build\""));

    let (status, body) = get(&base, "/api/runs/run-000000000001");
    assert_eq!(status, 200);
    assert!(body.contains("\"cwd\":\"/repo\""));

    let (status, _) = post(&base, "/api/runs/run-000000000001/cancel", "");
    assert_eq!(status, 200);
}

#[test]
fn invalid_submission_is_rejected_over_http() {
    let base = spawn_server();
    let body = serde_json::json!({ "toml": "garbage = true" }).to_string();
    let (status, resp) = post(&base, "/api/runs", &body);
    assert_eq!(status, 400);
    assert!(resp.contains("validation_failed"));
}
