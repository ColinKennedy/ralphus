//! STRESS-006 (`perf/stress/followups/`): the daemon sheds load instead of
//! queueing work no client will read. A full queue, or a read that waited
//! past the point any client is still listening, is answered `503` with
//! `Retry-After`; the health check has its own lane. A write is only ever
//! refused before it runs, and a queued request whose client has hung up is
//! skipped. The limits are shrunk here (`serve_with_limits`) so the shedding
//! paths run deterministically without real overload.

use std::io::Write;
use std::net::TcpStream;
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

use ralphus_daemon::server::{HttpLimits, serve_with_limits};
use ralphus_daemon::store::Store;

fn spawn_server(limits: HttpLimits) -> String {
    let store = Store::open_in_memory().expect("store");
    spawn_server_on(store, limits)
}

fn spawn_server_on(store: Store, limits: HttpLimits) -> String {
    let server = tiny_http::Server::http("127.0.0.1:0").expect("bind ephemeral port");
    let addr = server.server_addr().to_ip().expect("ip addr");
    thread::spawn(move || serve_with_limits(server, store, 4, limits));
    format!("http://{addr}")
}

/// Status, `Retry-After` header, body.
fn call(base: &str, method: &str, path: &str, body: &str) -> (u16, Option<String>, String) {
    let req = ureq::request(method, &format!("{base}{path}"))
        .set("Content-Type", "application/json")
        .timeout(Duration::from_secs(20));
    let resp = if method == "GET" {
        req.call()
    } else {
        req.send_string(body)
    };
    let (code, resp) = match resp {
        Ok(r) => (r.status(), r),
        Err(ureq::Error::Status(code, r)) => (code, r),
        Err(e) => panic!("request error: {e}"),
    };
    let retry_after = resp.header("Retry-After").map(str::to_string);
    (code, retry_after, resp.into_string().unwrap_or_default())
}

fn users(base: &str) -> String {
    let (status, _, body) = call(base, "GET", "/api/users", "");
    assert_eq!(status, 200, "{body}");
    body
}

fn temp_db(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "ralphus-load-shedding-{name}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("tasks.db")
}

#[test]
fn a_full_read_queue_sheds_reads_but_not_the_health_check_or_writes() {
    let base = spawn_server(HttpLimits {
        read_queue_limit: 0,
        ..HttpLimits::default()
    });

    let (status, retry_after, body) = call(&base, "GET", "/api/tasks", "");
    assert_eq!(status, 503, "{body}");
    assert_eq!(retry_after.as_deref(), Some("1"));
    assert!(body.contains("\"busy\""), "{body}");

    // "Busy" must stay distinguishable from "down".
    let (status, _, _) = call(&base, "GET", "/api/daemon", "");
    assert_eq!(status, 200);

    // Writes have their own queue.
    let (status, _, body) = call(&base, "POST", "/api/users", r#"{"name":"writer"}"#);
    assert_eq!(status, 200, "{body}");
}

#[test]
fn a_read_that_waited_too_long_is_shed_instead_of_built() {
    let base = spawn_server(HttpLimits {
        read_stale_after: Duration::ZERO,
        ..HttpLimits::default()
    });
    let (status, retry_after, body) = call(&base, "GET", "/api/tasks", "");
    assert_eq!(status, 503, "{body}");
    assert_eq!(retry_after.as_deref(), Some("1"));
    assert!(body.contains("waited too long"), "{body}");
    let (status, _, _) = call(&base, "GET", "/api/daemon", "");
    assert_eq!(status, 200);
}

#[test]
fn a_full_write_queue_refuses_a_write_before_running_it() {
    let base = spawn_server(HttpLimits {
        write_queue_limit: 0,
        ..HttpLimits::default()
    });
    let (status, retry_after, body) = call(&base, "POST", "/api/users", r#"{"name":"refused"}"#);
    assert_eq!(status, 503, "{body}");
    assert_eq!(retry_after.as_deref(), Some("1"));
    // Refused means not run: the client may retry safely.
    assert!(!users(&base).contains("refused"));
}

#[test]
fn validating_a_task_file_does_not_queue_behind_writes() {
    // STRESS-007: validation only reads, so it is answered even when no
    // write may queue.
    let base = spawn_server(HttpLimits {
        write_queue_limit: 0,
        ..HttpLimits::default()
    });
    let (status, _, body) = call(&base, "POST", "/api/squads/validate", r#"{"toml":"x"}"#);
    assert_ne!(status, 503, "{body}");
    assert!(status < 500, "{status}: {body}");
}

#[test]
fn a_queued_write_whose_client_hung_up_is_skipped() {
    let db = temp_db("abandoned");
    let base = spawn_server_on(Store::open(&db).expect("store"), HttpLimits::default());
    let addr = base.trim_start_matches("http://").to_string();
    users(&base); // the schema exists before the test takes the write lock

    // Hold SQLite's write lock so the write worker stalls on the first write.
    let blocker = rusqlite::Connection::open(&db).expect("open db");
    blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
    let first = {
        let base = base.clone();
        thread::spawn(move || call(&base, "POST", "/api/users", r#"{"name":"waited"}"#))
    };
    thread::sleep(Duration::from_millis(300));

    // Queued behind it: a write whose client sends it and hangs up.
    let body = r#"{"name":"abandoned"}"#;
    let mut s = TcpStream::connect(&addr).unwrap();
    write!(
        s,
        "POST /api/users HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    drop(s);
    thread::sleep(Duration::from_millis(300));

    blocker.execute_batch("COMMIT").unwrap();
    let (status, _, body) = first.join().unwrap();
    assert_eq!(status, 200, "{body}");
    // The worker is past the abandoned request once a later write answers.
    let (status, _, body) = call(&base, "POST", "/api/users", r#"{"name":"after"}"#);
    assert_eq!(status, 200, "{body}");

    let listed = users(&base);
    assert!(
        listed.contains("waited") && listed.contains("after"),
        "{listed}"
    );
    assert!(
        !listed.contains("abandoned"),
        "an abandoned write ran: {listed}"
    );
    let _ = std::fs::remove_dir_all(db.parent().unwrap());
}

#[test]
fn ordinary_reads_are_untouched_by_the_default_limits() {
    let base = spawn_server(HttpLimits::default());
    for path in ["/api/tasks", "/api/queue", "/api/projects", "/api/daemon"] {
        let (status, _, body) = call(&base, "GET", path, "");
        assert_eq!(status, 200, "{path}: {body}");
    }
}
