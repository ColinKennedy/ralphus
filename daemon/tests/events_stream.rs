//! Integration test for the SSE push endpoint (RAL-167): binds the real HTTP
//! server on an ephemeral port, opens a raw SSE connection to `/api/events`,
//! then submits a squad over a second connection and asserts the stream
//! delivers a matching push event -- end to end, over the wire, exactly as
//! the librarian's `proxy_events_stream` and `board.html`'s `EventSource`
//! would see it.

use std::io::{BufRead, BufReader};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use ralphus_daemon::server::{serve_with, serve_with_token};
use ralphus_daemon::store::Store;

fn spawn_server() -> String {
    let server = tiny_http::Server::http("127.0.0.1:0").expect("bind ephemeral port");
    let addr = server.server_addr().to_ip().expect("ip addr");
    let store = Store::open_in_memory().expect("store");
    thread::spawn(move || serve_with(server, store, 4));
    format!("http://{addr}")
}

/// Bearer token for the RAL-222 ticket-gating tests below — a tokened daemon
/// is what actually exercises the gate; `spawn_server` above (no token) is
/// the pre-existing RAL-167 open-API baseline and must keep working
/// unauthenticated (see `no_ticket_required_when_no_token_is_configured`).
const TOKEN: &str = "sse-ticket-test-token";

fn spawn_authenticated_server() -> String {
    let server = tiny_http::Server::http("127.0.0.1:0").expect("bind ephemeral port");
    let addr = server.server_addr().to_ip().expect("ip addr");
    let store = Store::open_in_memory().expect("store");
    thread::spawn(move || serve_with_token(server, store, 4, TOKEN.to_string()));
    format!("http://{addr}")
}

/// Mint a ticket via the real `POST /api/events/ticket` route (bearer-gated
/// like any other route, since it's dispatched through `route()` rather than
/// `run_http_loop`'s SSE special-case).
fn mint_ticket(base: &str) -> String {
    let resp = ureq::post(&format!("{base}/api/events/ticket"))
        .set("Authorization", &format!("Bearer {TOKEN}"))
        .call()
        .expect("mint ticket");
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value =
        serde_json::from_str(&resp.into_string().expect("body")).expect("json body");
    body["ticket"]
        .as_str()
        .expect("ticket field is a string")
        .to_string()
}

const GOOD: &str = "[[task]]\nname=\"build\"\n[[task.cell]]\ncwd=\"/repo\"\nprompt=\"go\"\n";

/// Reads `event: .../data: ...` pairs off an SSE stream on a background
/// thread and forwards each as `(event_name, data)` over `tx`, until the
/// connection closes.
fn spawn_sse_reader(
    reader: impl std::io::Read + Send + 'static,
) -> mpsc::Receiver<(String, String)> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut buffered = BufReader::new(reader);
        let mut event_name = String::new();
        loop {
            let mut line = String::new();
            match buffered.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            let line = line.trim_end_matches(['\r', '\n']);
            if let Some(name) = line.strip_prefix("event: ") {
                event_name = name.to_string();
            } else if let Some(data) = line.strip_prefix("data: ") {
                if tx.send((event_name.clone(), data.to_string())).is_err() {
                    break;
                }
            }
        }
    });
    rx
}

#[test]
fn events_stream_pushes_a_squad_event_on_submit() {
    let base = spawn_server();

    let resp = ureq::get(&format!("{base}/api/events"))
        .call()
        .expect("SSE connection opens");
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.header("Content-Type"), Some("text/event-stream"));
    let rx = spawn_sse_reader(resp.into_reader());

    // Best-effort: give the subscriber a moment to register before the state
    // change fires. The scan loop below tolerates the race regardless (it
    // keeps reading until it finds the matching event or times out).
    thread::sleep(Duration::from_millis(50));

    let submit_body = serde_json::json!({ "toml": GOOD, "label": "sse test" }).to_string();
    let submitted = ureq::post(&format!("{base}/api/squads"))
        .set("Content-Type", "application/json")
        .send_string(&submit_body);
    assert!(submitted.is_ok(), "submit failed: {:?}", submitted.err());

    let mut found = false;
    for _ in 0..20 {
        match rx.recv_timeout(Duration::from_secs(2)) {
            Ok((event, data)) => {
                if event == "squad"
                    && data.contains("squad-000000000001")
                    && data.contains("\"source\":\"submit\"")
                {
                    found = true;
                    break;
                }
            }
            Err(_) => break,
        }
    }
    assert!(
        found,
        "expected an SSE 'squad' event referencing the submitted squad"
    );
}

#[test]
fn events_stream_supports_multiple_concurrent_subscribers() {
    let base = spawn_server();

    let resp_a = ureq::get(&format!("{base}/api/events")).call().unwrap();
    let rx_a = spawn_sse_reader(resp_a.into_reader());
    let resp_b = ureq::get(&format!("{base}/api/events")).call().unwrap();
    let rx_b = spawn_sse_reader(resp_b.into_reader());

    thread::sleep(Duration::from_millis(50));

    let submit_body = serde_json::json!({ "toml": GOOD, "label": "sse test 2" }).to_string();
    ureq::post(&format!("{base}/api/squads"))
        .set("Content-Type", "application/json")
        .send_string(&submit_body)
        .expect("submit ok");

    for rx in [&rx_a, &rx_b] {
        let mut found = false;
        for _ in 0..20 {
            match rx.recv_timeout(Duration::from_secs(2)) {
                Ok((event, data)) => {
                    if event == "squad" && data.contains("squad-000000000001") {
                        found = true;
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        assert!(
            found,
            "each independent subscriber must see its own copy of the event"
        );
    }
}

// ── RAL-222: ticket-gated `/api/events` ─────────────────────────────────────

#[test]
fn no_ticket_required_when_no_token_is_configured() {
    // Pins the RAL-219/RAL-222 symmetric "no token configured" default that
    // `spawn_server` (used by every test above) already relies on implicitly.
    let base = spawn_server();
    let resp = ureq::get(&format!("{base}/api/events")).call();
    assert!(
        resp.is_ok(),
        "unauthenticated connect should succeed: {resp:?}"
    );
    assert_eq!(resp.unwrap().status(), 200);
}

#[test]
fn minting_a_ticket_requires_the_bearer_token() {
    let base = spawn_authenticated_server();
    let resp = ureq::post(&format!("{base}/api/events/ticket")).call();
    match resp {
        Err(ureq::Error::Status(401, _)) => {}
        other => panic!("expected 401, got {other:?}"),
    }
}

/// Acceptance criterion: an unauthenticated/invalid-ticket connection is
/// rejected up front, not just "an authenticated one also happens to work".
#[test]
fn events_stream_rejects_a_connection_with_no_ticket() {
    let base = spawn_authenticated_server();
    let resp = ureq::get(&format!("{base}/api/events")).call();
    match resp {
        Err(ureq::Error::Status(401, r)) => {
            assert!(r.into_string().unwrap_or_default().contains("unauthorized"));
        }
        other => panic!("expected 401, got {other:?}"),
    }
}

#[test]
fn events_stream_rejects_a_connection_with_an_invalid_ticket() {
    let base = spawn_authenticated_server();
    let resp = ureq::get(&format!("{base}/api/events?ticket=not-a-real-ticket")).call();
    match resp {
        Err(ureq::Error::Status(401, _)) => {}
        other => panic!("expected 401, got {other:?}"),
    }
}

#[test]
fn events_stream_accepts_a_freshly_minted_ticket() {
    let base = spawn_authenticated_server();
    let ticket = mint_ticket(&base);
    let resp = ureq::get(&format!("{base}/api/events?ticket={ticket}")).call();
    assert!(
        resp.is_ok(),
        "expected the fresh ticket to be accepted: {resp:?}"
    );
    assert_eq!(resp.unwrap().status(), 200);
}

/// A ticket is single-use — the resolved decision behind this ticket
/// (see RAL-222) is explicit that a leaked ticket "can't be reused".
#[test]
fn events_stream_rejects_replay_of_an_already_used_ticket() {
    let base = spawn_authenticated_server();
    let ticket = mint_ticket(&base);
    let first = ureq::get(&format!("{base}/api/events?ticket={ticket}")).call();
    assert!(first.is_ok(), "first connect should succeed: {first:?}");

    let second = ureq::get(&format!("{base}/api/events?ticket={ticket}")).call();
    match second {
        Err(ureq::Error::Status(401, _)) => {}
        other => panic!("expected replay to be rejected with 401, got {other:?}"),
    }
}
