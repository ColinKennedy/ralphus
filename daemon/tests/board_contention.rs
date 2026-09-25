//! WS-B.4 generalized contention: the board's hot reads stay fast while
//! writers hammer the daemon, measured over **real HTTP against a real
//! `serve_with` daemon**.
//!
//! This deliberately does not reuse `server.rs`'s in-process `route()`-calling
//! contention test: `route()` never exercises the accept-loop queueing or the
//! `ReadPool` dispatch where real-world stalls live (the captured deadlock's
//! 18–19 `CloseWait` sockets were clients that gave up on connections the
//! handler threads never returned to). Here the requests go through
//! `tiny_http` exactly the way the board does.
//!
//! Writers are the cheap, repeatable, fully-valid mutation
//! `POST /api/squads/{id}/env` -- a store write plus a Cartographer row and
//! an SSE event, i.e. the exact "writes generate reads that contend with
//! writes" pattern the plan calls out (RC-4). Four concurrent writers model
//! the scheduler / guardian-merge / PR-poller background load (RC-1).
//!
//! Asserts, per polled endpoint, a p95 latency budget and that
//! `GET /api/daemon` still reports `db == "ok"` afterwards. The budgets are
//! ratchets: when a fix lands, tighten the number to the new measured value.

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use ralphus_daemon::server::serve_with;
use ralphus_daemon::store::Store;

/// Background writers modelling the daemon's own write-heavy workers.
const WRITERS: usize = 4;
/// How long the contention window runs. Long enough to collect a meaningful
/// sample per endpoint, short enough to keep the suite fast.
const LOAD_MS: u64 = 4_000;
/// Squads seeded before the load starts, so the board endpoints hydrate a
/// realistic amount of data rather than an empty board.
const SQUADS: usize = 10;
/// Per-endpoint p95 budget in milliseconds. This is the plan's M8 target;
/// locally the worst endpoint measures ~32 ms, so the number is a ratchet with
/// room for slower CI hardware, not a description of current behavior.
const P95_BUDGET_MS: u128 = 200;
/// M3: store-lock *wait* p95 over the whole contention window. Baseline before
/// the WS-A fixes was 10,450 ms; locally this now measures ~25 ms.
const LOCK_WAIT_P95_BUDGET_MS: f64 = 200.0;
/// M4: store-lock wait max. Baseline was 45,890 ms; locally ~76 ms.
const LOCK_WAIT_MAX_BUDGET_MS: f64 = 500.0;

fn submit_body(tag: usize) -> String {
    serde_json::json!({
        "toml": format!(
            "[[task]]\nname=\"task-{tag}\"\n[[task.cell]]\ncwd=\"/repo\"\nprompt=\"do work {tag}\"\n"
        ),
        "label": format!("contention-{tag}"),
    })
    .to_string()
}

/// A private on-disk database directory. The store must be **file-backed**
/// here: `Store::open_in_memory` uses `cache=shared`, whose table-level locks
/// surface as `SQLITE_LOCKED` ("database table is locked"), and
/// `busy_timeout` does not apply to those -- so a pooled read concurrent with
/// a write fails outright instead of waiting. WAL on a real file is also what
/// production runs, which is the configuration whose contention this measures.
fn temp_db(tag: &str) -> PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "ralphus-contention-{tag}-{}-{n}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir");
    dir.join("tasks.db")
}

fn spawn_server(db: &Path) -> String {
    let server = tiny_http::Server::http("127.0.0.1:0").expect("bind ephemeral port");
    let addr = server.server_addr().to_ip().expect("ip addr");
    let store = Store::open(db).expect("store");
    thread::spawn(move || serve_with(server, store, 4));
    format!("http://{addr}")
}

/// The submitting identity. Squad submission resolves the caller from the
/// `X-Ralphus-User` header and rejects an unregistered name, so every request
/// carries it and the test registers it first.
const USER: &str = "load";

/// Minimal HTTP client returning `(status, body)` with a hard timeout so a
/// stuck handler fails the test instead of hanging it.
fn request(method: &str, url: &str, body: Option<&str>) -> (u16, String) {
    let timeout = Duration::from_secs(30);
    let agent = ureq::AgentBuilder::new().timeout(timeout).build();
    let req = match method {
        "POST" => agent.post(url).set("Content-Type", "application/json"),
        _ => agent.get(url),
    }
    .set("X-Ralphus-User", USER);
    let resp = match body {
        Some(b) => req.send_string(b),
        None => req.call(),
    };
    match resp {
        Ok(r) => {
            let status = r.status();
            let mut body = String::new();
            let _ = r
                .into_reader()
                .take(10 * 1024 * 1024)
                .read_to_string(&mut body);
            (status, body)
        }
        Err(ureq::Error::Status(code, r)) => {
            let mut body = String::new();
            let _ = r
                .into_reader()
                .take(10 * 1024 * 1024)
                .read_to_string(&mut body);
            (code, body)
        }
        Err(e) => panic!("request error: {e}"),
    }
}

fn percentile(samples: &mut [u128], fraction: f64) -> u128 {
    if samples.is_empty() {
        return 0;
    }
    samples.sort_unstable();
    let idx = (((samples.len() as f64) * fraction).ceil() as usize)
        .saturating_sub(1)
        .min(samples.len() - 1);
    samples[idx]
}

#[test]
// Wall-clock budgets under deliberate multi-thread load, so this runs in the
// dedicated `perf-tests` CI job with `--test-threads 1` rather than alongside
// the rest of the workspace suite -- the same arrangement
// `board_cold_load_perf.rs` uses and for the same reason: sharing cores with
// other tests measures the scheduler, not the store.
#[ignore = "perf budget; run via the perf-tests job (--run-ignored only --test-threads 1)"]
fn board_reads_stay_fast_under_four_writers() {
    let db = temp_db("board");
    let base = spawn_server(&db);

    // Seed: a user to submit as, then enough squads that the board endpoints
    // do real hydration work.
    let (status, body) = request(
        "POST",
        &format!("{base}/api/users"),
        Some(&serde_json::json!({ "name": USER }).to_string()),
    );
    assert_eq!(status, 200, "user create: {body}");
    let mut squad_ids = Vec::with_capacity(SQUADS);
    for i in 0..SQUADS {
        let (status, body) = request("POST", &format!("{base}/api/squads"), Some(&submit_body(i)));
        assert_eq!(status, 201, "seed submit {i}: {body}");
        let resp: serde_json::Value = serde_json::from_str(&body).expect("submit JSON");
        let id = resp["squad_id"]
            .as_str()
            .unwrap_or_else(|| panic!("no squad_id in submit response: {body}"))
            .to_string();
        squad_ids.push(id);
    }

    let endpoints = [
        "/api/tasks",
        "/api/queue",
        "/api/guardian-index",
        "/api/daemon",
    ];
    let deadline = Instant::now() + Duration::from_millis(LOAD_MS);

    // Four writers, rotating env writes across the seeded squads.
    let writers: Vec<_> = (0..WRITERS)
        .map(|w| {
            let base = base.clone();
            let squad_ids = squad_ids.clone();
            thread::spawn(move || {
                let mut writes = 0u64;
                let mut n = 0u64;
                while Instant::now() < deadline {
                    let id = &squad_ids[((w as u64 + n) % squad_ids.len() as u64) as usize];
                    let body =
                        serde_json::json!({"set": {"RALPHUS_CONTENTION_PING": n.to_string()}})
                            .to_string();
                    let (status, resp) =
                        request("POST", &format!("{base}/api/squads/{id}/env"), Some(&body));
                    if status != 200 {
                        panic!("writer {w} env write failed ({status}): {resp}");
                    }
                    writes += 1;
                    n += 1;
                }
                writes
            })
        })
        .collect();

    // One reader per polled endpoint, sampling latency for the whole window.
    let readers: Vec<_> = endpoints
        .iter()
        .map(|endpoint| {
            let url = format!("{base}{endpoint}");
            thread::spawn(move || {
                let mut samples: Vec<u128> = Vec::new();
                let mut statuses: Vec<u16> = Vec::new();
                while Instant::now() < deadline {
                    let started = Instant::now();
                    let (status, body) = request("GET", &url, None);
                    samples.push(started.elapsed().as_millis());
                    statuses.push(status);
                    let _ = body;
                }
                (samples, statuses)
            })
        })
        .collect();

    let write_counts: Vec<u64> = writers
        .into_iter()
        .map(|w| w.join().expect("writer"))
        .collect();
    let total_writes: u64 = write_counts.iter().sum();
    assert!(
        total_writes > 40,
        "writers only managed {total_writes} writes in {LOAD_MS}ms -- the load was not real"
    );

    for (endpoint, reader) in endpoints.iter().zip(readers) {
        let (mut samples, statuses) = reader.join().expect("reader");
        assert!(
            samples.len() >= 20,
            "{endpoint}: only {} samples in the window; the read is starved",
            samples.len()
        );
        assert!(
            statuses.iter().all(|s| *s == 200),
            "{endpoint}: non-200 responses under load: {:?}",
            statuses
        );
        let p50 = percentile(&mut samples, 0.50);
        let p95 = percentile(&mut samples, 0.95);
        let max = *samples.last().expect("non-empty");
        // Printed so a ratchet can be tightened from the observed numbers
        // rather than guessed: `cargo nextest run --test board_contention
        // --no-capture` shows them.
        eprintln!(
            "contention {endpoint}: n={} p50={p50}ms p95={p95}ms max={max}ms",
            samples.len()
        );
        assert!(
            p95 <= P95_BUDGET_MS,
            "{endpoint}: p95 {p95}ms (p50 {p50}ms, max {max}ms) exceeds the \
             {P95_BUDGET_MS}ms budget under {WRITERS} writers and {total_writes} writes"
        );
    }

    // The daemon is still healthy after the storm.
    let (status, body) = request("GET", &format!("{base}/api/daemon"), None);
    assert_eq!(status, 200);
    let health: serde_json::Value = serde_json::from_str(&body).expect("health JSON");
    assert_eq!(
        health["db"].as_str(),
        Some("ok"),
        "store unhealthy after contention: {body}"
    );

    // M3/M4: the store lock's own wait distribution over the window. This is
    // the metric the plan's baseline was measured in, and the daemon reports
    // it for free -- so gate on it here rather than only on HTTP latency,
    // which can hide contention behind the read pool.
    let wait = &health["lock_wait"];
    let samples = wait["samples"].as_u64().unwrap_or(0);
    let p50 = wait["p50_ms"].as_f64().unwrap_or(0.0);
    let p95 = wait["p95_ms"].as_f64().unwrap_or(0.0);
    let max = wait["max_ms"].as_f64().unwrap_or(0.0);
    eprintln!("contention lock_wait: n={samples} p50={p50}ms p95={p95}ms max={max}ms");
    assert!(
        samples > 0,
        "no store-lock wait samples recorded; the instrumentation is not measuring this workload: {body}"
    );
    assert!(
        p95 <= LOCK_WAIT_P95_BUDGET_MS,
        "store-lock wait p95 {p95}ms (p50 {p50}ms, max {max}ms, n={samples}) exceeds the {LOCK_WAIT_P95_BUDGET_MS}ms budget"
    );
    assert!(
        max <= LOCK_WAIT_MAX_BUDGET_MS,
        "store-lock wait max {max}ms (p95 {p95}ms, n={samples}) exceeds the {LOCK_WAIT_MAX_BUDGET_MS}ms budget"
    );

    let _ = std::fs::remove_dir_all(db.parent().expect("db dir"));
}
