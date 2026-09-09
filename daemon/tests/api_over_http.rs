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

fn request_as_user(base: &str, method: &str, path: &str, user: &str, body: &str) -> (u16, String) {
    let request = ureq::request(method, &format!("{base}{path}"))
        .set("Content-Type", "application/json")
        .set("X-Ralphus-User", user);
    let response = if body.is_empty() {
        request.call()
    } else {
        request.send_string(body)
    };
    match response {
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

#[test]
fn hidden_squads_are_scoped_by_the_user_header() {
    let base = spawn_server();
    for name in ["alice", "bob"] {
        let body = serde_json::json!({ "name": name }).to_string();
        let (status, response) = post(&base, "/api/users", &body);
        assert_eq!(status, 200, "create user body: {response}");
    }
    let submit_body = serde_json::json!({ "toml": GOOD }).to_string();
    let (status, response) = post(&base, "/api/squads", &submit_body);
    assert_eq!(status, 201, "submit body: {response}");
    let review_body = serde_json::json!({
        "name": "wire review",
        "base_branch": "main",
        "git_root": "/repo"
    })
    .to_string();
    let (status, response) = post(&base, "/api/guardians", &review_body);
    assert_eq!(status, 201, "create review body: {response}");

    let path = "/api/hidden/squads/squad-000000000001";
    let (status, body) = request_as_user(&base, "POST", path, "alice", "");
    assert_eq!(status, 200, "hide body: {body}");
    assert_eq!(body, r#"{"hidden":true}"#);
    let review_path = "/api/hidden/reviews/guardian-000000000001";
    let (status, body) = request_as_user(&base, "POST", review_path, "alice", "");
    assert_eq!(status, 200, "hide review body: {body}");

    let (status, alice) = request_as_user(&base, "GET", "/api/hidden", "alice", "");
    assert_eq!(status, 200, "alice list body: {alice}");
    assert!(alice.contains("squad-000000000001"));
    assert!(alice.contains("guardian-000000000001"));
    assert!(alice.contains("hidden_at_ms"));
    let (status, bob) = request_as_user(&base, "GET", "/api/hidden", "bob", "");
    assert_eq!(status, 200, "bob list body: {bob}");
    assert_eq!(bob, r#"{"hidden":[]}"#);

    // Identity in the URL is deliberately no longer accepted.
    let (status, _) = get(&base, "/api/hidden?user=alice");
    assert_eq!(status, 400);

    let (status, body) = request_as_user(&base, "DELETE", path, "alice", "");
    assert_eq!(status, 200, "unhide body: {body}");
    assert_eq!(body, r#"{"hidden":false}"#);
    let (status, body) = request_as_user(&base, "DELETE", review_path, "alice", "");
    assert_eq!(status, 200, "unhide review body: {body}");
    let (_, alice) = request_as_user(&base, "GET", "/api/hidden", "alice", "");
    assert_eq!(alice, r#"{"hidden":[]}"#);

    // RAL-332: hide/unhide rows are admin-only Cartographer rows -- a
    // resolved non-admin (or no-identity) viewer of the Logs tab must not
    // see them, but an admin must.
    let (status, _) = post(
        &base,
        "/api/users/alice/admin",
        &serde_json::json!({ "is_admin": true }).to_string(),
    );
    assert_eq!(status, 200);
    let (_, non_admin_events) =
        request_as_user(&base, "GET", "/api/cartographer?source=hidden", "bob", "");
    assert!(
        !non_admin_events.contains("squad hidden"),
        "events body: {non_admin_events}"
    );
    let (_, admin_events) =
        request_as_user(&base, "GET", "/api/cartographer?source=hidden", "alice", "");
    assert!(
        admin_events.contains("squad hidden"),
        "events body: {admin_events}"
    );
    assert!(
        admin_events.contains("squad unhidden"),
        "events body: {admin_events}"
    );
    assert!(
        admin_events.contains("review hidden"),
        "events body: {admin_events}"
    );
    assert!(
        admin_events.contains("review unhidden"),
        "events body: {admin_events}"
    );
}

#[test]
fn hidden_squads_batch_applies_in_one_request() {
    let base = spawn_server();
    let body = serde_json::json!({ "name": "alice" }).to_string();
    let (status, response) = post(&base, "/api/users", &body);
    assert_eq!(status, 200, "create user body: {response}");

    for _ in 0..2 {
        let submit_body = serde_json::json!({ "toml": GOOD }).to_string();
        let (status, response) = post(&base, "/api/squads", &submit_body);
        assert_eq!(status, 201, "submit body: {response}");
    }

    // One request hides both squads plus an id that does not exist -- the
    // valid ids still succeed and the bad one comes back in `failed`
    // instead of failing the whole batch.
    let batch_body = serde_json::json!({
        "ids": ["squad-000000000001", "squad-000000000002", "squad-missing"],
        "hidden": true
    })
    .to_string();
    let (status, body) = request_as_user(
        &base,
        "POST",
        "/api/hidden/squads/batch",
        "alice",
        &batch_body,
    );
    assert_eq!(status, 200, "batch hide body: {body}");
    let parsed: serde_json::Value = serde_json::from_str(&body).expect("json");
    assert_eq!(parsed["hidden"], true);
    let failed = parsed["failed"].as_array().expect("failed array");
    assert_eq!(failed.len(), 1, "batch hide body: {body}");
    assert_eq!(failed[0]["id"], "squad-missing");

    let (_, alice) = request_as_user(&base, "GET", "/api/hidden", "alice", "");
    assert!(alice.contains("squad-000000000001"));
    assert!(alice.contains("squad-000000000002"));

    let unbatch_body = serde_json::json!({
        "ids": ["squad-000000000001", "squad-000000000002"],
        "hidden": false
    })
    .to_string();
    let (status, body) = request_as_user(
        &base,
        "POST",
        "/api/hidden/squads/batch",
        "alice",
        &unbatch_body,
    );
    assert_eq!(status, 200, "batch unhide body: {body}");
    let (_, alice) = request_as_user(&base, "GET", "/api/hidden", "alice", "");
    assert_eq!(alice, r#"{"hidden":[]}"#);

    let (status, body) = request_as_user(
        &base,
        "POST",
        "/api/hidden/squads/batch",
        "alice",
        &serde_json::json!({ "ids": [], "hidden": true }).to_string(),
    );
    assert_eq!(status, 400, "empty batch body: {body}");
}

// ── RAL-332: admin flag, admin-only endpoints, Cartographer visibility ─────

/// Promotes `name` to admin via `POST /api/users/{name}/admin`, as `caller`
/// (empty caller means "no X-Ralphus-User header", exercising the bootstrap
/// exception when no admin exists yet).
fn promote(base: &str, caller: &str, name: &str, is_admin: bool) -> (u16, String) {
    let body = serde_json::json!({ "is_admin": is_admin }).to_string();
    let path = format!("/api/users/{name}/admin");
    if caller.is_empty() {
        post(base, &path, &body)
    } else {
        request_as_user(base, "POST", &path, caller, &body)
    }
}

#[test]
fn admin_flag_bootstraps_and_then_gates_admin_only_endpoints() {
    let base = spawn_server();
    for name in ["alice", "bob"] {
        let body = serde_json::json!({ "name": name }).to_string();
        let (status, response) = post(&base, "/api/users", &body);
        assert_eq!(status, 200, "create user body: {response}");
    }

    // Bootstrap: no admin exists yet, so a non-admin caller (bob, or nobody
    // at all) can still promote the first admin.
    let (status, body) = promote(&base, "bob", "alice", true);
    assert_eq!(status, 200, "bootstrap promote body: {body}");

    // Now that an admin exists, the bootstrap window is closed: bob (still
    // non-admin) cannot promote anyone, including himself.
    let (status, _) = promote(&base, "bob", "bob", true);
    assert_eq!(status, 403);

    // GET /api/whoami reports identity + admin flag without requiring the
    // caller to already prove admin-ness.
    let (status, body) = request_as_user(&base, "GET", "/api/whoami", "alice", "");
    assert_eq!(status, 200, "whoami body: {body}");
    assert!(body.contains("\"is_admin\":true"), "whoami body: {body}");
    let (status, body) = request_as_user(&base, "GET", "/api/whoami", "bob", "");
    assert_eq!(status, 200, "whoami body: {body}");
    assert!(body.contains("\"is_admin\":false"), "whoami body: {body}");

    // Admin-only endpoints (Users/Machines/Secrets/Triage): reject a
    // resolved non-admin caller, accept the admin.
    let (status, _) = request_as_user(&base, "GET", "/api/users", "bob", "");
    assert_eq!(status, 403);
    let (status, _) = request_as_user(&base, "GET", "/api/users", "alice", "");
    assert_eq!(status, 200);
    let (status, _) = request_as_user(&base, "GET", "/api/machines", "bob", "");
    assert_eq!(status, 403);
    let (status, _) = request_as_user(&base, "GET", "/api/machines", "alice", "");
    assert_eq!(status, 200);
    let (status, _) = request_as_user(&base, "GET", "/api/secret-env-names", "bob", "");
    assert_eq!(status, 403);
    let (status, _) = request_as_user(&base, "GET", "/api/triage/types", "bob", "");
    assert_eq!(status, 403);

    // Projects: registering one is admin-gated (a non-admin never even
    // reaches path validation -- the 403 comes back before a real repo path
    // would be needed), but reads stay open to every caller (including no
    // resolved identity at all) since the Simple task form's project/branch
    // pickers depend on them for every user. The registration success path
    // itself already has dedicated coverage in `server.rs`'s own unit tests
    // (`register_project_route_success` et al.) with a real temp git repo.
    let register_body = serde_json::json!({ "name": "demo", "path": "/repo" }).to_string();
    let (status, _) = request_as_user(&base, "POST", "/api/projects", "bob", &register_body);
    assert_eq!(status, 403);
    let (status, body) = get(&base, "/api/projects");
    assert_eq!(status, 200, "list projects body: {body}");

    // "Edit Profile"/visit-as is itself admin-gated.
    let (status, _) = request_as_user(&base, "POST", "/api/users/bob/visit", "bob", "");
    assert_eq!(status, 403);
    let (status, body) = request_as_user(&base, "POST", "/api/users/bob/visit", "alice", "");
    assert_eq!(status, 200, "visit body: {body}");

    // Demote alice back down: with no admin left, the bootstrap window
    // reopens for anyone.
    let (status, _) = promote(&base, "alice", "alice", false);
    assert_eq!(status, 200);
    let (status, _) = promote(&base, "bob", "bob", true);
    assert_eq!(
        status, 200,
        "bootstrap should have reopened with zero admins"
    );
}

#[test]
fn cartographer_admin_only_rows_are_hidden_from_non_admin_viewers() {
    let base = spawn_server();
    for name in ["alice", "bob"] {
        let body = serde_json::json!({ "name": name }).to_string();
        post(&base, "/api/users", &body);
    }
    promote(&base, "", "alice", true);

    let submit_body = serde_json::json!({ "toml": GOOD }).to_string();
    let (status, response) = post(&base, "/api/squads", &submit_body);
    assert_eq!(status, 201, "submit body: {response}");

    // RAL-328's hide/unhide rows are marked admin-only (RAL-332 closes that
    // loop) -- hidden from a resolved non-admin, visible to the admin.
    let (status, _) = request_as_user(
        &base,
        "POST",
        "/api/hidden/squads/squad-000000000001",
        "bob",
        "",
    );
    assert_eq!(status, 200);

    let (_, bob_log) = request_as_user(&base, "GET", "/api/cartographer", "bob", "");
    assert!(
        !bob_log.contains("squad hidden"),
        "non-admin must not see the hide event: {bob_log}"
    );
    let (_, alice_log) = request_as_user(&base, "GET", "/api/cartographer", "alice", "");
    assert!(
        alice_log.contains("squad hidden"),
        "admin must see the hide event: {alice_log}"
    );
    // An unresolved/no-identity caller defaults to the same non-admin view.
    let (_, anon_log) = get(&base, "/api/cartographer");
    assert!(
        !anon_log.contains("squad hidden"),
        "no-identity caller must default to non-admin: {anon_log}"
    );

    // Fetching the admin-only row by id: 404 (not 403 -- indistinguishable
    // from missing) for the non-admin, 200 for the admin.
    let parsed: serde_json::Value = serde_json::from_str(&alice_log).expect("valid json");
    let row_id = parsed["rows"]
        .as_array()
        .expect("rows array")
        .iter()
        .find(|r| r["message"] == "squad hidden")
        .expect("the hide event is present in the admin's view")["id"]
        .as_i64()
        .expect("row id");
    let (status, _) = request_as_user(
        &base,
        "GET",
        &format!("/api/cartographer/{row_id}"),
        "bob",
        "",
    );
    assert_eq!(status, 404);
    let (status, _) = request_as_user(
        &base,
        "GET",
        &format!("/api/cartographer/{row_id}"),
        "alice",
        "",
    );
    assert_eq!(status, 200);
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
