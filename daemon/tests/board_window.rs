//! STRESS-004 (`perf/stress/followups/`): `GET /api/tasks?window=N` (and
//! `/api/task-index?window=N`) returns every unfinished squad plus the `N`
//! newest finished ones, with a cursor for older pages and an id/label
//! search across all history -- while the unwindowed default stays exactly
//! as it was, because the CLI's selector resolution relies on it.

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

fn call(base: &str, method: &str, path: &str, body: &str) -> (u16, serde_json::Value) {
    let req = ureq::request(method, &format!("{base}{path}"))
        .set("Content-Type", "application/json")
        .set("X-Ralphus-User", "window-user")
        .timeout(Duration::from_secs(20));
    let resp = if method == "GET" {
        req.call()
    } else {
        req.send_string(body)
    };
    let (status, text) = match resp {
        Ok(r) => (r.status(), r.into_string().unwrap_or_default()),
        Err(ureq::Error::Status(code, r)) => (code, r.into_string().unwrap_or_default()),
        Err(e) => panic!("request error: {e}"),
    };
    (
        status,
        serde_json::from_str(&text).unwrap_or(serde_json::Value::Null),
    )
}

fn submit(base: &str, label: &str) -> String {
    let body = serde_json::json!({
        "toml": "[[task]]\nname=\"t\"\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\n",
        "hold": true,
        "label": label,
    })
    .to_string();
    let (status, v) = call(base, "POST", "/api/squads", &body);
    assert_eq!(status, 201, "{v}");
    v["squad_id"].as_str().unwrap().to_string()
}

fn finish(base: &str, id: &str) {
    let (status, v) = call(
        base,
        "POST",
        &format!("/api/squads/{id}/set-status"),
        &serde_json::json!({ "kind": "squad", "state": "done" }).to_string(),
    );
    assert!(status < 300, "{status}: {v}");
}

fn ids(v: &serde_json::Value) -> Vec<String> {
    v["squads"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["id"].as_str().unwrap().to_string())
        .collect()
}

/// 2 active + 5 finished squads.
fn seeded() -> (String, Vec<String>, Vec<String>) {
    let base = spawn_server();
    let (status, _) = call(&base, "POST", "/api/users", r#"{"name":"window-user"}"#);
    assert_eq!(status, 200);
    let mut finished = Vec::new();
    for i in 0..5 {
        let id = submit(&base, &format!("old-{i}"));
        finish(&base, &id);
        finished.push(id);
    }
    let active = vec![submit(&base, "live-a"), submit(&base, "live-b")];
    (base, active, finished)
}

#[test]
fn the_unwindowed_listing_is_unchanged() {
    let (base, active, finished) = seeded();
    let (status, v) = call(&base, "GET", "/api/tasks", "");
    assert_eq!(status, 200);
    assert_eq!(ids(&v).len(), active.len() + finished.len());
    assert!(
        v.get("window").is_none(),
        "no window metadata without ?window"
    );
    let (_, v) = call(&base, "GET", "/api/task-index", "");
    assert_eq!(ids(&v).len(), 7);
    assert!(v.get("window").is_none());
}

#[test]
fn a_window_holds_every_active_squad_plus_the_newest_finished_ones() {
    let (base, active, finished) = seeded();
    for path in ["/api/tasks?window=2", "/api/task-index?window=2"] {
        let (status, v) = call(&base, "GET", path, "");
        assert_eq!(status, 200, "{path}");
        let got = ids(&v);
        for id in &active {
            assert!(got.contains(id), "{path}: active squad {id} missing");
        }
        // The two newest finished squads, and no older ones.
        assert!(
            got.contains(&finished[4]) && got.contains(&finished[3]),
            "{path}"
        );
        assert!(!got.contains(&finished[2]), "{path}");
        assert_eq!(got.len(), 4, "{path}");
        // Newest first, like the unwindowed listing.
        let mut sorted = got.clone();
        sorted.sort_by(|a, b| b.cmp(a));
        assert_eq!(got, sorted, "{path}");
        let w = &v["window"];
        assert_eq!(w["total"], 7, "{path}");
        assert_eq!(w["loaded"], 4, "{path}");
        assert_eq!(w["has_more"], true, "{path}");
        assert!(w["next_before"].is_string(), "{path}");
    }
}

#[test]
fn the_cursor_pages_through_older_finished_squads_only() {
    let (base, active, finished) = seeded();
    let (_, first) = call(&base, "GET", "/api/tasks?window=2", "");
    let cursor = first["window"]["next_before"].as_str().unwrap().to_string();
    let enc = cursor.replace(':', "%3A");
    let (_, second) = call(
        &base,
        "GET",
        &format!("/api/tasks?window=2&before={enc}"),
        "",
    );
    let got = ids(&second);
    assert_eq!(got, vec![finished[2].clone(), finished[1].clone()]);
    for id in &active {
        assert!(
            !got.contains(id),
            "active squads come only with the first page"
        );
    }
    assert_eq!(second["window"]["has_more"], true);
    let cursor = second["window"]["next_before"]
        .as_str()
        .unwrap()
        .replace(':', "%3A");
    let (_, third) = call(
        &base,
        "GET",
        &format!("/api/tasks?window=2&before={cursor}"),
        "",
    );
    assert_eq!(ids(&third), vec![finished[0].clone()]);
    assert_eq!(third["window"]["has_more"], false);
    assert!(third["window"]["next_before"].is_null());
}

#[test]
fn search_reaches_finished_squads_outside_the_window_by_label_or_id() {
    let (base, _, finished) = seeded();
    let (_, v) = call(&base, "GET", "/api/tasks?window=1&q=OLD-0", "");
    assert_eq!(
        ids(&v),
        vec![finished[0].clone()],
        "case-insensitive label match"
    );
    assert_eq!(v["window"]["total"], 1);
    let tail = &finished[1][finished[1].len() - 4..];
    let (_, v) = call(&base, "GET", &format!("/api/tasks?window=1&q={tail}"), "");
    assert!(ids(&v).contains(&finished[1]), "id substring match");
    // `%`/`_` are matched literally, not as wildcards.
    let (_, v) = call(&base, "GET", "/api/tasks?window=5&q=%25", "");
    assert!(ids(&v).is_empty(), "{v}");
}

#[test]
fn a_finished_squad_retried_back_to_life_rejoins_the_first_page() {
    let (base, _, finished) = seeded();
    let oldest = &finished[0];
    let (status, v) = call(&base, "POST", &format!("/api/squads/{oldest}/retry"), "");
    assert!(status < 300, "{status}: {v}");
    let (_, v) = call(&base, "GET", "/api/tasks?window=1", "");
    assert!(
        ids(&v).contains(oldest),
        "an active squad is always in the window"
    );
}
