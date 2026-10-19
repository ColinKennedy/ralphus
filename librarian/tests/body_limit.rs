//! STRESS-001 (`perf/stress/followups/`): the librarian reads every proxied
//! request body before forwarding it, so it enforces the same body limit as
//! the daemon -- and, through the vendored tiny_http's bounded drain, a
//! declared-but-never-sent body can no longer abort it.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::thread;
use std::time::Duration;

use ralphus_librarian::server::{MAX_REQUEST_BODY_BYTES, serve_with};

/// The proxy target is never reached in these tests (the body check runs
/// first); point it at a port nothing listens on.
const UNREACHABLE_DAEMON: &str = "http://127.0.0.1:1";

fn spawn_server() -> SocketAddr {
    let server = tiny_http::Server::http("127.0.0.1:0").expect("bind ephemeral port");
    let addr = server.server_addr().to_ip().expect("ip addr");
    thread::spawn(move || serve_with(server, UNREACHABLE_DAEMON));
    addr
}

fn status_of(addr: SocketAddr, bytes: &[u8]) -> Option<u16> {
    let mut s = TcpStream::connect(addr).expect("connect");
    s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    let _ = s.write_all(bytes);
    let mut got = Vec::new();
    let mut buf = [0u8; 512];
    while !got.windows(2).any(|w| w == b"\r\n") {
        match s.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => got.extend_from_slice(&buf[..n]),
        }
    }
    String::from_utf8_lossy(&got)
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

#[test]
fn oversized_declared_body_is_413_before_proxying() {
    let addr = spawn_server();
    let req = format!(
        "POST /api/squads HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\n\r\n{{",
        MAX_REQUEST_BODY_BYTES + 1
    );
    assert_eq!(status_of(addr, req.as_bytes()), Some(413));
}

#[test]
fn huge_content_length_then_hang_up_does_not_abort_the_librarian() {
    let addr = spawn_server();
    for _ in 0..3 {
        let mut s = TcpStream::connect(addr).expect("connect");
        s.write_all(
            b"POST /api/squads HTTP/1.1\r\nHost: x\r\nContent-Length: 99999999999999\r\n\r\n{}",
        )
        .unwrap();
        drop(s);
    }
    thread::sleep(Duration::from_millis(500));
    // Still serving: the board shell is answered without touching the daemon.
    assert_eq!(
        status_of(
            addr,
            b"GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n"
        ),
        Some(200)
    );
}
