//! A new connection is served promptly while many keep-alive connections are
//! open (the vendored tiny_http's `TaskPool`, `vendor/tiny_http/RALPHUS_PATCHES.md`
//! patch 6). Each connection holds a pool thread for as long as it stays
//! open, and a race in deciding whether to start another thread could queue
//! a new connection behind them: the board's static assets then hung until
//! a keep-alive connection timed out (~30 s with a read timeout set).

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::thread;
use std::time::{Duration, Instant};

fn spawn_server() -> SocketAddr {
    let server = tiny_http::Server::http("127.0.0.1:0").expect("bind ephemeral port");
    let addr = server.server_addr().to_ip().expect("ip addr");
    thread::spawn(move || {
        for request in server.incoming_requests() {
            thread::spawn(move || {
                let _ = request.respond(tiny_http::Response::from_string("ok"));
            });
        }
    });
    addr
}

/// Send one keep-alive request and wait (at most 3 s) for its response,
/// leaving the connection open -- and its server thread busy -- for as long
/// as the returned stream lives. `Err` names how long it waited.
fn keep_alive(addr: SocketAddr) -> Result<TcpStream, Duration> {
    let started = Instant::now();
    let mut s = TcpStream::connect(addr).expect("connect");
    s.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    s.write_all(b"GET /a HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
    let mut buf = [0u8; 256];
    match s.read(&mut buf) {
        Ok(n) if n > 0 => Ok(s),
        _ => Err(started.elapsed()),
    }
}

#[test]
fn every_new_connection_is_served_while_keep_alive_connections_hold_threads() {
    let addr = spawn_server();
    let mut held = Vec::new();
    for round in 0..30 {
        // A burst of new keep-alive connections, opened concurrently: each
        // takes a pool thread for as long as it stays open.
        let burst: Vec<_> = (0..16)
            .map(|_| thread::spawn(move || keep_alive(addr)))
            .collect();
        for (i, h) in burst.into_iter().enumerate() {
            match h.join().unwrap() {
                Ok(s) => held.push(s),
                Err(waited) => panic!(
                    "round {round}: connection {i} unanswered after {waited:?} \
                     with {} connections already open",
                    held.len()
                ),
            }
        }
    }
}
