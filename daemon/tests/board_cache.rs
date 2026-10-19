//! STRESS-003 (`perf/stress/followups/`): `/api/tasks`, `/api/task-index` and
//! `/api/queue` share one build between requests while the store's data is
//! unchanged. The UX risk is a stale board -- a click whose effect the next
//! refresh does not show. These pin that every write is visible on the very
//! next read, with the shared build warm.

use std::thread;
use std::time::Duration;

use ralphus_daemon::server::serve_with;
use ralphus_daemon::store::Store;

fn spawn_server() -> String {
    let server = tiny_http::Server::http("127.0.0.1:0").expect("bind ephemeral port");
    let addr = server.server_addr().to_ip().expect("ip addr");
    let store = Store::open_in_memory().expect("store");
    thread::spawn(move || serve_with(server, store, 4));
    format!("http://{addr}")
}

fn call(base: &str, method: &str, path: &str, body: &str) -> (u16, String) {
    let req = ureq::request(method, &format!("{base}{path}"))
        .set("Content-Type", "application/json")
        .set("X-Ralphus-User", "cache-user")
        .timeout(Duration::from_secs(20));
    let resp = if method == "GET" {
        req.call()
    } else {
        req.send_string(body)
    };
    match resp {
        Ok(r) => (r.status(), r.into_string().unwrap_or_default()),
        Err(ureq::Error::Status(code, r)) => (code, r.into_string().unwrap_or_default()),
        Err(e) => panic!("request error: {e}"),
    }
}

fn squad_ids(board: &str) -> Vec<String> {
    let v: serde_json::Value = serde_json::from_str(board).expect("board json");
    v["squads"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|s| s["id"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn submit(base: &str, name: &str) -> String {
    let body = serde_json::json!({
        "toml": format!("[[task]]\nname=\"{name}\"\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\n"),
        "hold": true,
    })
    .to_string();
    let (status, resp) = call(base, "POST", "/api/squads", &body);
    assert_eq!(status, 201, "{resp}");
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    v["squad_id"].as_str().unwrap().to_string()
}

#[test]
fn every_write_is_visible_on_the_next_board_read() {
    let base = spawn_server();
    let (status, _) = call(&base, "POST", "/api/users", r#"{"name":"cache-user"}"#);
    assert_eq!(status, 200);

    // Warm the shared builds.
    for _ in 0..3 {
        assert_eq!(call(&base, "GET", "/api/tasks", "").0, 200);
        assert_eq!(call(&base, "GET", "/api/task-index", "").0, 200);
        assert_eq!(call(&base, "GET", "/api/queue", "").0, 200);
    }

    for round in 0..5 {
        let id = submit(&base, &format!("t{round}"));
        let (_, board) = call(&base, "GET", "/api/tasks", "");
        assert!(
            squad_ids(&board).contains(&id),
            "round {round}: a submitted squad was missing from the next board read"
        );
        let (_, index) = call(&base, "GET", "/api/task-index", "");
        assert!(
            index.contains(&id),
            "round {round}: missing from the next task-index read"
        );
        let (_, queue) = call(&base, "GET", "/api/queue", "");
        assert!(
            queue.contains(&id),
            "round {round}: missing from the next queue read"
        );

        // An edit that changes nothing but a label is visible at once too.
        let label = format!("relabelled-{round}");
        let (status, resp) = call(
            &base,
            "POST",
            &format!("/api/squads/{id}/edit"),
            &serde_json::json!({ "kind": "squad", "label": label }).to_string(),
        );
        assert!(status < 300, "{status}: {resp}");
        let (_, board) = call(&base, "GET", "/api/tasks", "");
        assert!(
            board.contains(&label),
            "round {round}: the label edit was not visible"
        );
    }
}

/// Filtered board reads are cached separately from the unfiltered one.
#[test]
fn a_filtered_board_read_is_not_answered_from_the_unfiltered_one() {
    let base = spawn_server();
    let (status, _) = call(&base, "POST", "/api/users", r#"{"name":"cache-user"}"#);
    assert_eq!(status, 200);
    let id = submit(&base, "only");
    assert!(squad_ids(&call(&base, "GET", "/api/tasks", "").1).contains(&id));
    let filtered = squad_ids(&call(&base, "GET", "/api/tasks?status=running", "").1);
    assert!(!filtered.contains(&id), "a held squad is not running");
}
