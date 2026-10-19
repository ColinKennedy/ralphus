//! STRESS-006 (`perf/stress/followups/`): the read path sheds load instead
//! of queueing work no client will read. A full read queue, or a read that
//! waited past the point any client is still listening, is answered `503`
//! with `Retry-After`; the health check has its own lane and writes are
//! never shed. The limits are shrunk here (`serve_with_limits`) so the
//! shedding paths run deterministically without real overload.

use std::thread;
use std::time::Duration;

use ralphus_daemon::server::{HttpLimits, serve_with_limits};
use ralphus_daemon::store::Store;

fn spawn_server(limits: HttpLimits) -> String {
    let server = tiny_http::Server::http("127.0.0.1:0").expect("bind ephemeral port");
    let addr = server.server_addr().to_ip().expect("ip addr");
    let store = Store::open_in_memory().expect("store");
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

#[test]
fn a_full_read_queue_sheds_reads_but_not_the_health_check_or_writes() {
    let base = spawn_server(HttpLimits {
        read_queue_limit: 0,
        read_stale_after: Duration::from_secs(10),
    });

    let (status, retry_after, body) = call(&base, "GET", "/api/tasks", "");
    assert_eq!(status, 503, "{body}");
    assert_eq!(retry_after.as_deref(), Some("1"));
    assert!(body.contains("\"busy\""), "{body}");

    // "Busy" must stay distinguishable from "down".
    let (status, _, _) = call(&base, "GET", "/api/daemon", "");
    assert_eq!(status, 200);

    // Writes are never shed: a client must not be told a mutation was
    // refused after it may already have happened.
    let (status, _, body) = call(&base, "POST", "/api/squads/validate", r#"{"toml":"x"}"#);
    assert_ne!(status, 503, "{body}");
    assert!(status < 500, "{status}: {body}");
}

#[test]
fn a_read_that_waited_too_long_is_shed_instead_of_built() {
    let base = spawn_server(HttpLimits {
        read_queue_limit: 256,
        read_stale_after: Duration::ZERO,
    });
    let (status, retry_after, body) = call(&base, "GET", "/api/tasks", "");
    assert_eq!(status, 503, "{body}");
    assert_eq!(retry_after.as_deref(), Some("1"));
    assert!(body.contains("waited too long"), "{body}");
    let (status, _, _) = call(&base, "GET", "/api/daemon", "");
    assert_eq!(status, 200);
}

#[test]
fn ordinary_reads_are_untouched_by_the_default_limits() {
    let base = spawn_server(HttpLimits::default());
    for path in ["/api/tasks", "/api/queue", "/api/projects", "/api/daemon"] {
        let (status, _, body) = call(&base, "GET", path, "");
        assert_eq!(status, 200, "{path}: {body}");
    }
}
