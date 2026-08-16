//! Integration test for the SSE push endpoint (RAL-167): binds the real HTTP
//! server on an ephemeral port, opens a raw SSE connection to `/api/events`,
//! then submits a run over a second connection and asserts the stream
//! delivers a matching push event -- end to end, over the wire, exactly as
//! the librarian's `proxy_events_stream` and `board.html`'s `EventSource`
//! would see it.

use std::io::{BufRead, BufReader};
use std::sync::mpsc;
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

const GOOD: &str = "[[task]]\nname=\"build\"\n[[task.session]]\ncwd=\"/repo\"\nprompt=\"go\"\n";

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
fn events_stream_pushes_a_run_event_on_submit() {
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
    let submitted = ureq::post(&format!("{base}/api/runs"))
        .set("Content-Type", "application/json")
        .send_string(&submit_body);
    assert!(submitted.is_ok(), "submit failed: {:?}", submitted.err());

    let mut found = false;
    for _ in 0..20 {
        match rx.recv_timeout(Duration::from_secs(2)) {
            Ok((event, data)) => {
                if event == "run"
                    && data.contains("run-000000000001")
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
        "expected an SSE 'run' event referencing the submitted run"
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
    ureq::post(&format!("{base}/api/runs"))
        .set("Content-Type", "application/json")
        .send_string(&submit_body)
        .expect("submit ok");

    for rx in [&rx_a, &rx_b] {
        let mut found = false;
        for _ in 0..20 {
            match rx.recv_timeout(Duration::from_secs(2)) {
                Ok((event, data)) => {
                    if event == "run" && data.contains("run-000000000001") {
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
