//! HTTP-boundary hardening found by `ralphus-stress` (`perf/stress/followups/`):
//! hostile or malformed requests over a raw socket must get a prompt error
//! status and must never take the daemon down.
//!
//! These guard both the daemon's own body limit (`MAX_REQUEST_BODY_BYTES` in
//! `run_http_loop`) and the patches in the vendored tiny_http
//! (`vendor/tiny_http/RALPHUS_PATCHES.md`), which is excluded from the
//! workspace and so has no CI-run tests of its own. Each test drives a real
//! listener in this process: a regression of STRESS-001 aborts the whole test
//! binary rather than failing one assertion.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::thread;
use std::time::Duration;

use ralphus_daemon::server::{MAX_REQUEST_BODY_BYTES, serve_with, serve_with_token};
use ralphus_daemon::store::Store;

const TOKEN: &str = "hardening-test-token";

fn spawn_server(token: Option<&str>) -> SocketAddr {
    spawn_server_with_timeout(token, None)
}

/// A listener with the vendored tiny_http's per-connection read timeout set
/// (production uses `REQUEST_READ_TIMEOUT`; tests use a short one).
fn spawn_server_with_timeout(token: Option<&str>, read_timeout: Option<Duration>) -> SocketAddr {
    let server = tiny_http::Server::http("127.0.0.1:0").expect("bind ephemeral port");
    server.set_read_timeout(read_timeout);
    let addr = server.server_addr().to_ip().expect("ip addr");
    let store = Store::open_in_memory().expect("store");
    let token = token.map(str::to_string);
    thread::spawn(move || match token {
        Some(t) => serve_with_token(server, store, 4, t),
        None => serve_with(server, store, 4),
    });
    addr
}

/// Write `bytes` and return the response's status code, or `None` when the
/// server closed without answering. Leaves the socket for the caller to drop.
fn exchange(addr: SocketAddr, bytes: &[u8]) -> (Option<u16>, TcpStream) {
    let mut s = TcpStream::connect(addr).expect("connect");
    s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    s.set_write_timeout(Some(Duration::from_secs(20))).unwrap();
    // The server may answer and close before every byte is written (a 413
    // decided from the declared length, a 414 mid-line); a failed write is
    // fine as long as the status line arrives.
    let _ = s.write_all(bytes);
    (read_status(&mut s), s)
}

fn read_status(s: &mut TcpStream) -> Option<u16> {
    let mut got = Vec::new();
    let mut buf = [0u8; 512];
    while !got.windows(2).any(|w| w == b"\r\n") {
        match s.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => got.extend_from_slice(&buf[..n]),
        }
    }
    let line = String::from_utf8_lossy(&got);
    line.split_whitespace().nth(1)?.parse().ok()
}

/// `GET /api/daemon` answers at all (any status -- `401` without a token
/// still proves the process and its accept loop are alive).
fn daemon_answers(addr: SocketAddr, token: Option<&str>) -> Option<u16> {
    let auth = token
        .map(|t| format!("Authorization: Bearer {t}\r\n"))
        .unwrap_or_default();
    let req = format!("GET /api/daemon HTTP/1.1\r\nHost: x\r\n{auth}Connection: close\r\n\r\n");
    exchange(addr, req.as_bytes()).0
}

/// STRESS-001: a declared `Content-Length` far beyond any real body used to
/// abort the process (tiny_http's `EqualReader::drop` allocated the whole
/// remainder). Without a token, before auth runs.
#[test]
fn huge_declared_content_length_is_refused_and_the_daemon_survives() {
    let addr = spawn_server(Some(TOKEN));
    let req = b"POST /api/squads HTTP/1.1\r\nHost: x\r\nContent-Length: 99999999999999\r\n\r\n{}";
    let (status, sock) = exchange(addr, req);
    assert_eq!(status, Some(413));
    drop(sock);
    // Give a would-be abort on the dropped connection time to happen.
    thread::sleep(Duration::from_millis(300));
    assert_eq!(daemon_answers(addr, Some(TOKEN)), Some(200));
}

/// The original crash shape exactly: the client closes before reading any
/// response, so the request (and its unread body) is dropped mid-flight.
#[test]
fn huge_content_length_then_hang_up_does_not_abort() {
    let addr = spawn_server(None);
    for _ in 0..3 {
        let mut s = TcpStream::connect(addr).expect("connect");
        s.write_all(
            b"POST /api/squads HTTP/1.1\r\nHost: x\r\nContent-Length: 99999999999999\r\n\r\n{}",
        )
        .unwrap();
        drop(s);
    }
    thread::sleep(Duration::from_millis(500));
    assert_eq!(daemon_answers(addr, None), Some(200));
}

/// A body declared one byte over the limit is refused from the header alone.
#[test]
fn body_over_the_limit_is_413_without_being_read() {
    let addr = spawn_server(None);
    let req = format!(
        "POST /api/squads/validate HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{{",
        MAX_REQUEST_BODY_BYTES + 1
    );
    let (status, _sock) = exchange(addr, req.as_bytes());
    assert_eq!(status, Some(413));
    assert_eq!(daemon_answers(addr, None), Some(200));
}

/// A chunked body has no declared length; the limit applies to what arrives.
///
/// The status is read on its own thread while the body is still being sent:
/// the daemon answers and closes with the rest of the body unread, and on
/// Windows that close resets the connection, discarding a response the
/// client has not read yet.
#[test]
fn chunked_body_over_the_limit_is_413() {
    let addr = spawn_server(None);
    let mut s = TcpStream::connect(addr).expect("connect");
    s.set_read_timeout(Some(Duration::from_secs(60))).unwrap();
    let mut reader = s.try_clone().expect("clone socket");
    let status = thread::spawn(move || read_status(&mut reader));
    s.write_all(
        b"POST /api/squads/validate HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n",
    )
    .unwrap();
    let chunk = vec![b'a'; 1024 * 1024];
    let mut sent = 0;
    while sent <= MAX_REQUEST_BODY_BYTES {
        if s.write_all(format!("{:x}\r\n", chunk.len()).as_bytes())
            .is_err()
            || s.write_all(&chunk).is_err()
            || s.write_all(b"\r\n").is_err()
        {
            break;
        }
        sent += chunk.len();
    }
    let _ = s.write_all(b"0\r\n\r\n");
    assert_eq!(status.join().expect("reader thread"), Some(413));
    drop(s);
    assert_eq!(daemon_answers(addr, None), Some(200));
}

/// A large but legitimate body is still read and handled (the limit is not
/// so tight that real task files trip it).
#[test]
fn body_under_the_limit_reaches_the_handler() {
    let addr = spawn_server(None);
    let toml = format!("# {}\n", "x".repeat(2 * 1024 * 1024));
    let body = serde_json::json!({ "toml": toml }).to_string();
    let req = format!(
        "POST /api/squads/validate HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let (status, _sock) = exchange(addr, req.as_bytes());
    let status = status.expect("a response");
    assert_ne!(status, 413, "a 2 MiB body is under the limit");
    assert!(status < 500, "validate answered {status}");
}

/// STRESS-009: an over-long request line is `414`, not an unbounded buffer.
#[test]
fn overlong_request_line_is_414() {
    let addr = spawn_server(None);
    let req = format!(
        "GET /api/tasks?q={} HTTP/1.1\r\nHost: x\r\n\r\n",
        "a".repeat(100 * 1024)
    );
    assert_eq!(exchange(addr, req.as_bytes()).0, Some(414));
    assert_eq!(daemon_answers(addr, None), Some(200));
}

/// STRESS-009: an over-long header line, or too many headers, is `431`.
#[test]
fn oversized_or_too_many_headers_are_431() {
    let addr = spawn_server(None);
    let big = format!(
        "GET /api/daemon HTTP/1.1\r\nHost: x\r\nX-Big: {}\r\n\r\n",
        "a".repeat(100 * 1024)
    );
    assert_eq!(exchange(addr, big.as_bytes()).0, Some(431));

    let many: String = (0..2000).map(|i| format!("X-H{i}: v\r\n")).collect();
    let req = format!("GET /api/daemon HTTP/1.1\r\nHost: x\r\n{many}\r\n");
    assert_eq!(exchange(addr, req.as_bytes()).0, Some(431));
    assert_eq!(daemon_answers(addr, None), Some(200));
}

/// Ordinary large-but-sane headers (a long bearer token, a few KiB of
/// cookies) stay well inside the limits.
#[test]
fn realistic_large_headers_are_accepted() {
    let addr = spawn_server(None);
    let req = format!(
        "GET /api/daemon HTTP/1.1\r\nHost: x\r\nCookie: {}\r\nX-Long: {}\r\nConnection: close\r\n\r\n",
        "c".repeat(16 * 1024),
        "v".repeat(8 * 1024)
    );
    assert_eq!(exchange(addr, req.as_bytes()).0, Some(200));
}

/// STRESS-002: one client that declared a body and stopped sending froze
/// every request, because the body was read on the only thread taking
/// requests. Now it waits on its own thread. No read timeout here: the
/// decoupling alone must keep the daemon answering.
#[test]
fn a_held_request_body_does_not_freeze_other_requests() {
    let addr = spawn_server(Some(TOKEN));
    let mut holder = TcpStream::connect(addr).expect("connect");
    holder
        .write_all(
            format!(
                "POST /api/squads HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {TOKEN}\r\nContent-Type: application/json\r\nContent-Length: 1048576\r\n\r\n"
            )
            .as_bytes(),
        )
        .unwrap();
    holder.write_all(&[b' '; 64]).unwrap();
    thread::sleep(Duration::from_millis(300));
    let started = std::time::Instant::now();
    assert_eq!(daemon_answers(addr, Some(TOKEN)), Some(200));
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "the health check waited {:?} behind a held body",
        started.elapsed()
    );
    // Writes too: they go to the write worker, which the held body is not on.
    let body = r#"{"toml":"x"}"#;
    let req = format!(
        "POST /api/squads/validate HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {TOKEN}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    assert!(exchange(addr, req.as_bytes()).0.is_some_and(|s| s < 500));
    drop(holder);
}

/// STRESS-002: with the read timeout set, a held body is answered `408`
/// instead of holding its thread (and socket) forever.
#[test]
fn a_held_request_body_times_out_with_408() {
    let addr = spawn_server_with_timeout(None, Some(Duration::from_secs(1)));
    let mut s = TcpStream::connect(addr).expect("connect");
    s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    s.write_all(
        b"POST /api/squads HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: 1048576\r\n\r\n",
    )
    .unwrap();
    s.write_all(&[b' '; 64]).unwrap();
    let started = std::time::Instant::now();
    assert_eq!(read_status(&mut s), Some(408));
    assert!(started.elapsed() < Duration::from_secs(10));
}

/// STRESS-002: a large body from a caller without the token is refused from
/// its headers -- the daemon never waits for it.
#[test]
fn an_unauthenticated_large_body_is_401_without_being_read() {
    let addr = spawn_server(Some(TOKEN));
    let req = b"POST /api/squads HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: 1048576\r\n\r\n";
    let started = std::time::Instant::now();
    assert_eq!(exchange(addr, req).0, Some(401));
    assert!(started.elapsed() < Duration::from_secs(3));
}

/// STRESS-002: an idle keep-alive connection is closed after the read
/// timeout, without a response (it never started a request).
#[test]
fn an_idle_connection_is_closed_silently_after_the_read_timeout() {
    let addr = spawn_server_with_timeout(None, Some(Duration::from_secs(1)));
    let mut s = TcpStream::connect(addr).expect("connect");
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let mut buf = [0u8; 64];
    let n = s.read(&mut buf).unwrap_or(0);
    assert_eq!(n, 0, "expected a silent close, got {:?}", &buf[..n]);
}

/// STRESS-002/009: headers trickled slowly enough that no single read times
/// out still have to finish within the read timeout of the first byte.
#[test]
fn trickled_headers_are_answered_408() {
    let addr = spawn_server_with_timeout(None, Some(Duration::from_secs(1)));
    let mut s = TcpStream::connect(addr).expect("connect");
    s.set_read_timeout(Some(Duration::from_millis(100)))
        .unwrap();
    s.write_all(b"GET /api/daemon HTTP/1.1\r\n").unwrap();
    let started = std::time::Instant::now();
    let mut got = Vec::new();
    while started.elapsed() < Duration::from_secs(10) {
        let _ = s.write_all(b"X");
        let mut buf = [0u8; 256];
        match s.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                got.extend_from_slice(&buf[..n]);
                break;
            }
            Err(_) => thread::sleep(Duration::from_millis(200)),
        }
    }
    let line = String::from_utf8_lossy(&got);
    assert!(line.starts_with("HTTP/1.1 408"), "got {line:?}");
}

/// STRESS-002 UX guard: the read timeout must never cut the board's live
/// update stream. `/api/events` writes for as long as the client stays; it
/// must still deliver an event well after the read timeout has passed.
#[test]
fn the_event_stream_outlives_the_read_timeout() {
    let addr = spawn_server_with_timeout(Some(TOKEN), Some(Duration::from_secs(1)));
    let base = format!("http://{addr}");
    let ticket: serde_json::Value = serde_json::from_str(
        &ureq::post(&format!("{base}/api/events/ticket"))
            .set("Authorization", &format!("Bearer {TOKEN}"))
            .call()
            .expect("mint ticket")
            .into_string()
            .expect("ticket body"),
    )
    .expect("ticket json");
    let ticket = ticket["ticket"]
        .as_str()
        .expect("ticket string")
        .to_string();

    let mut sse = TcpStream::connect(addr).expect("connect");
    sse.write_all(
        format!("GET /api/events?ticket={ticket} HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes(),
    )
    .unwrap();
    sse.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let mut head = [0u8; 512];
    let n = sse.read(&mut head).expect("stream headers");
    assert!(String::from_utf8_lossy(&head[..n]).starts_with("HTTP/1.1 200"));

    // Three read timeouts with nothing sent by the client.
    thread::sleep(Duration::from_secs(3));

    // A submit writes Cartographer rows, each of which is pushed as an event.
    let user = ureq::post(&format!("{base}/api/users"))
        .set("Authorization", &format!("Bearer {TOKEN}"))
        .send_string(r#"{"name":"sse-user"}"#);
    assert!(user.is_ok());
    let submit = ureq::post(&format!("{base}/api/squads"))
        .set("Authorization", &format!("Bearer {TOKEN}"))
        .set("X-Ralphus-User", "sse-user")
        .send_string(
            &serde_json::json!({
                "toml": "[[task]]\nname=\"t\"\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\n",
                "hold": true
            })
            .to_string(),
        );
    assert!(submit.is_ok(), "submit failed: {submit:?}");

    let mut got = String::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while std::time::Instant::now() < deadline && !got.contains("event:") {
        let mut buf = [0u8; 1024];
        match sse.read(&mut buf) {
            Ok(0) => panic!("the event stream was closed: {got:?}"),
            Ok(n) => got.push_str(&String::from_utf8_lossy(&buf[..n])),
            Err(e) => panic!("no event within 10s ({e}): {got:?}"),
        }
    }
    assert!(got.contains("event:"), "{got:?}");
}

/// STRESS-009: a non-ASCII request line is answered `400` instead of the
/// connection being dropped without a response.
#[test]
fn non_ascii_request_line_is_400() {
    let addr = spawn_server(None);
    let req = "GET /api/squads/\u{fc}n\u{ef}c\u{f6}d\u{e9} HTTP/1.1\r\nHost: x\r\n\r\n";
    assert_eq!(exchange(addr, req.as_bytes()).0, Some(400));
    assert_eq!(daemon_answers(addr, None), Some(200));
}
