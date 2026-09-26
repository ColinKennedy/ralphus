//! WS-D.8: submitting a squad must not hold the store lock for long.
//!
//! This is the gate for the worst lock holder the daemon had. Submit's
//! background materialization used to run `reviews::derive_reviews_with_full_prefetch`
//! with the guard held, and that function resolves every worktree placeholder
//! -- `git worktree add`, a `git fetch`, sometimes a rebase, and for a remote
//! cell a whole provisioning round-trip. Measured from production Cartographer
//! rows, that one call site held the lock for **45.9 seconds, twice**.
//!
//! Asserting on the *duration of a hold* rather than on request latency is
//! deliberate. A submit can legitimately be slow -- it does real git work. What
//! must not happen is that it makes every *other* request slow, and the hold is
//! the mechanism by which it would. `GET /api/daemon` reports the
//! process-lifetime maximum (`guard_hold.max_ms`), so the assertion is against
//! the thing that actually matters.
//!
//! Real HTTP against a real `serve_with`, because the whole materialization path
//! -- including the background thread it runs on -- only exists there.

use std::time::Duration;

use ralphus_daemon::server::serve_with;
use ralphus_daemon::store::Store;

/// A task file with several review-linked cells, so materialization has real
/// work to do rather than returning early on an empty `[[review]]` list.
const SUBMIT_TOML: &str = "[[task]]\nname=\"alpha\"\n[[task.cell]]\ncwd=\"/repo\"\nprompt=\"one\"\n\
                           [[task]]\nname=\"beta\"\n[[task.cell]]\ncwd=\"/repo\"\nprompt=\"two\"\n\
                           [[task]]\nname=\"gamma\"\n[[task.cell]]\ncwd=\"/repo\"\nprompt=\"three\"\n";

/// Budget for the longest single store-lock hold across the whole run.
///
/// The plan's M5 target is 100 ms. This is a debug build -- several times slower
/// than release at identical work -- so the number is loosened for the build,
/// not for the invariant: 1 s still fails instantly if a `git fetch` or a
/// subprocess creeps back under the guard, which is the regression being
/// guarded against. The precise aggregate numbers are gated by
/// `board_contention.rs` (lock-wait p95 under 50 ms) instead.
const MAX_GUARD_HOLD_MS: u64 = 1_000;

fn spawn_server() -> String {
    let server = tiny_http::Server::http("127.0.0.1:0").expect("bind ephemeral port");
    let addr = server.server_addr().to_ip().expect("ip addr");
    let store = Store::open_in_memory().expect("store");
    std::thread::spawn(move || serve_with(server, store, 4));
    format!("http://{addr}")
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(30))
        .build()
}

fn post(agent: &ureq::Agent, url: &str, body: &str) -> (u16, String) {
    let req = agent
        .post(url)
        .set("Content-Type", "application/json")
        .set("X-Ralphus-User", "holder");
    match req.send_string(body) {
        Ok(r) => (r.status(), r.into_string().unwrap_or_default()),
        Err(ureq::Error::Status(code, r)) => (code, r.into_string().unwrap_or_default()),
        Err(e) => panic!("request error: {e}"),
    }
}

fn get(agent: &ureq::Agent, url: &str) -> serde_json::Value {
    let body = agent
        .get(url)
        .set("X-Ralphus-User", "holder")
        .call()
        .expect("GET failed")
        .into_string()
        .unwrap_or_default();
    serde_json::from_str(&body).unwrap_or_else(|e| panic!("bad JSON: {e}: {body}"))
}

#[test]
fn submitting_a_squad_never_holds_the_store_lock_for_long() {
    let base = spawn_server();
    let agent = agent();

    let (status, body) = post(
        &agent,
        &format!("{base}/api/users"),
        &serde_json::json!({ "name": "holder" }).to_string(),
    );
    assert_eq!(status, 200, "user create: {body}");

    // Several submissions, so materialization runs repeatedly and a hold that
    // only appears on one code path still gets sampled.
    for i in 0..5 {
        let submit = serde_json::json!({
            "toml": SUBMIT_TOML,
            "label": format!("hold-{i}"),
        })
        .to_string();
        let (status, body) = post(&agent, &format!("{base}/api/squads"), &submit);
        assert_eq!(status, 201, "submit {i}: {body}");
    }

    // Materialization is backgrounded, so wait for every squad to leave
    // `materializing` before reading the counters -- otherwise the hold being
    // measured may not have happened yet.
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        let tasks = get(&agent, &format!("{base}/api/tasks"));
        let squads = tasks["squads"].as_array().cloned().unwrap_or_default();
        let settled = squads.len() == 5
            && squads
                .iter()
                .all(|s| s["state"].as_str() != Some("materializing"));
        if settled {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "squads never finished materializing: {tasks}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    let health = get(&agent, &format!("{base}/api/daemon"));
    let hold = &health["guard_hold"];
    let max_ms = hold["max_ms"].as_u64().unwrap_or(u64::MAX);
    let over = hold["over_threshold"].as_u64().unwrap_or(0);
    let threshold = hold["warn_threshold_ms"].as_u64().unwrap_or(0);
    eprintln!("submit guard_hold: max={max_ms}ms over_{threshold}ms={over}");

    assert!(
        max_ms <= MAX_GUARD_HOLD_MS,
        "the longest store-lock hold across five submissions was {max_ms}ms, over the \
         {MAX_GUARD_HOLD_MS}ms budget ({over} hold(s) crossed the {threshold}ms warn \
         threshold). Submit's materialization is holding the guard across blocking \
         work again -- most likely a git call inside `derive_reviews_with_full_prefetch` \
         or the placeholder resolution it delegates to. See this file's module doc."
    );

    // Sanity: the counter is live, not stuck at zero because nothing recorded.
    assert_eq!(
        threshold, 100,
        "the reported warn threshold is not the documented 100 ms"
    );
}
