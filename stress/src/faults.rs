//! Fault injectors. Each one talks raw TCP so it can misbehave in ways a
//! well-formed HTTP client never would (half-sent requests, lying
//! `Content-Length`, hung sockets, garbage bytes), and runs for a window
//! while a victim load measures what legitimate clients experience.
//!
//! Every injector caps the connections it opens: on Windows each closed
//! client socket parks an ephemeral port in TIME_WAIT for minutes, and
//! exhausting the generator's own port range would show up as "the daemon
//! refused connections" when it did not.

use serde::Serialize;
use std::collections::BTreeMap;
use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Barrier, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// Max new connections any one injector opens in a window.
const CONNECTION_BUDGET: u64 = 6000;

#[derive(Clone)]
pub struct Target {
    pub addr: SocketAddr,
    pub token: Option<String>,
    pub user: Option<String>,
    pub noun: String,
    /// How this daemon spells a task file.
    pub dialect: crate::api::Dialect,
}

impl Target {
    fn auth(&self) -> String {
        let mut h = String::new();
        if let Some(t) = &self.token {
            h.push_str(&format!("Authorization: Bearer {t}\r\n"));
        }
        if let Some(u) = &self.user {
            h.push_str(&format!("X-Ralphus-User: {u}\r\n"));
        }
        h
    }

    fn get(&self, path: &str) -> Vec<u8> {
        format!(
            "GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n{}Connection: close\r\n\r\n",
            self.auth()
        )
        .into_bytes()
    }

    fn post(&self, path: &str, body: &[u8]) -> Vec<u8> {
        let mut v = format!(
            "POST {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n{}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            self.auth(),
            body.len()
        )
        .into_bytes();
        v.extend_from_slice(body);
        v
    }

    fn connect(&self) -> std::io::Result<TcpStream> {
        TcpStream::connect_timeout(&self.addr, Duration::from_secs(5))
    }
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Fault {
    AbruptDisconnect,
    Slowloris,
    IdleConnections,
    TruncatedBodyHold,
    TruncatedBodyClose,
    AbandonResponse,
    Malformed,
    OversizedBody,
    ConnectionFlood,
    BadAuthFlood,
    ThunderingHerd,
}

impl Fault {
    pub const ALL: [Fault; 11] = [
        Fault::AbruptDisconnect,
        Fault::Slowloris,
        Fault::IdleConnections,
        Fault::TruncatedBodyHold,
        Fault::TruncatedBodyClose,
        Fault::AbandonResponse,
        Fault::Malformed,
        Fault::OversizedBody,
        Fault::ConnectionFlood,
        Fault::BadAuthFlood,
        Fault::ThunderingHerd,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Fault::AbruptDisconnect => "abrupt_disconnect",
            Fault::Slowloris => "slowloris",
            Fault::IdleConnections => "idle_connections",
            Fault::TruncatedBodyHold => "truncated_body_hold",
            Fault::TruncatedBodyClose => "truncated_body_close",
            Fault::AbandonResponse => "abandon_response",
            Fault::Malformed => "malformed_requests",
            Fault::OversizedBody => "oversized_body",
            Fault::ConnectionFlood => "connection_flood",
            Fault::BadAuthFlood => "bad_auth_flood",
            Fault::ThunderingHerd => "thundering_herd",
        }
    }

    pub fn describe(self) -> &'static str {
        match self {
            Fault::AbruptDisconnect => {
                "8 clients repeatedly send half a request line + headers, then close the socket"
            }
            Fault::Slowloris => {
                "hundreds of connections each trickle one header line per second and never finish the request"
            }
            Fault::IdleConnections => {
                "a thousand connections opened and held without sending a byte"
            }
            Fault::TruncatedBodyHold => {
                "POSTs that promise a 1 MiB body, send 64 bytes, then hold the socket open (targets the single write worker)"
            }
            Fault::TruncatedBodyClose => "POSTs that promise 5000 body bytes, send 100, then close",
            Fault::AbandonResponse => {
                "clients send a complete board GET or submit POST and close before reading the response"
            }
            Fault::Malformed => {
                "a corpus of ~35 malformed/hostile requests (bad framing, huge headers, bad JSON/TOML/UTF-8, smuggling shapes, traversal, injection)"
            }
            Fault::OversizedBody => "well-framed submits with 16-64 MiB bodies",
            Fault::ConnectionFlood => "8 clients connect and immediately close as fast as possible",
            Fault::BadAuthFlood => "16 clients hammer the board with an invalid bearer token",
            Fault::ThunderingHerd => {
                "2000 brand-new connections released at the same instant, one board GET each"
            }
        }
    }
}

#[derive(Serialize, Default, Debug, Clone)]
pub struct InjectorReport {
    pub attempts: u64,
    pub outcomes: BTreeMap<String, u64>,
    /// Malformed corpus only: payload name -> outcome -> count.
    pub per_payload: BTreeMap<String, BTreeMap<String, u64>>,
    pub notes: Vec<String>,
    pub latency_p99_ms: Option<f64>,
}

impl InjectorReport {
    fn bump(&mut self, outcome: &str) {
        *self.outcomes.entry(outcome.to_string()).or_default() += 1;
    }
}

fn merge(into: &Mutex<InjectorReport>, from: InjectorReport) {
    if let Ok(mut r) = into.lock() {
        r.attempts += from.attempts;
        for (k, v) in from.outcomes {
            *r.outcomes.entry(k).or_default() += v;
        }
        for (p, m) in from.per_payload {
            let e = r.per_payload.entry(p).or_default();
            for (k, v) in m {
                *e.entry(k).or_default() += v;
            }
        }
        r.notes.extend(from.notes);
    }
}

/// Read until the status line is in, return a label for how it went.
fn read_status(s: &mut TcpStream, timeout: Duration) -> String {
    let _ = s.set_read_timeout(Some(timeout));
    let mut buf = [0u8; 512];
    let mut got = Vec::new();
    loop {
        match s.read(&mut buf) {
            Ok(0) => {
                return if got.is_empty() {
                    "closed_no_response".into()
                } else {
                    status_label(&got)
                };
            }
            Ok(n) => {
                got.extend_from_slice(&buf[..n]);
                if got.windows(2).any(|w| w == b"\r\n") {
                    return status_label(&got);
                }
            }
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                return if got.is_empty() {
                    "no_response_timeout".into()
                } else {
                    status_label(&got)
                };
            }
            Err(e)
                if matches!(
                    e.kind(),
                    ErrorKind::ConnectionReset | ErrorKind::ConnectionAborted
                ) =>
            {
                return "reset".into();
            }
            Err(_) => return "io_error".into(),
        }
    }
}

fn status_label(head: &[u8]) -> String {
    let line = String::from_utf8_lossy(head);
    let first = line.lines().next().unwrap_or("");
    match first.split_whitespace().nth(1) {
        Some(code) if first.starts_with("HTTP/") => format!("status_{code}"),
        _ => "non_http_response".into(),
    }
}

/// Write `bytes`, then read the status line.
fn exchange(t: &Target, bytes: &[u8], timeout: Duration) -> String {
    let mut s = match t.connect() {
        Ok(s) => s,
        Err(_) => return "connect_failed".into(),
    };
    let _ = s.set_write_timeout(Some(Duration::from_secs(10)));
    if s.write_all(bytes).is_err() {
        // The server may already have answered and closed (e.g. 413).
        return format!(
            "write_failed_then_{}",
            read_status(&mut s, Duration::from_millis(500))
        );
    }
    read_status(&mut s, timeout)
}

pub fn inject(
    fault: Fault,
    t: &Target,
    window: Duration,
    stop: &Arc<AtomicBool>,
) -> InjectorReport {
    match fault {
        Fault::AbruptDisconnect => parallel(8, window, stop, t, |t, budget, r| {
            if !budget.take() {
                return false;
            }
            r.attempts += 1;
            match t.connect() {
                Ok(mut s) => {
                    let _ = s.write_all(
                        format!("GET /api/tasks HTTP/1.1\r\nHost: x\r\n{}", t.auth()).as_bytes(),
                    );
                    r.bump("sent_partial_then_closed");
                }
                Err(_) => r.bump("connect_failed"),
            }
            true
        }),
        Fault::Slowloris => slowloris(t, 400, window, stop, true),
        Fault::IdleConnections => slowloris(t, 1000, window, stop, false),
        Fault::TruncatedBodyHold => truncated_hold(t, 4, window, stop),
        Fault::TruncatedBodyClose => parallel(4, window, stop, t, |t, budget, r| {
            if !budget.take() {
                return false;
            }
            r.attempts += 1;
            match t.connect() {
                Ok(mut s) => {
                    let head = format!(
                        "POST /api/{}/validate HTTP/1.1\r\nHost: x\r\n{}Content-Type: application/json\r\nContent-Length: 5000\r\n\r\n",
                        t.noun,
                        t.auth()
                    );
                    let _ = s.write_all(head.as_bytes());
                    let _ = s.write_all(&[b'{'; 100]);
                    r.bump("sent_truncated_then_closed");
                }
                Err(_) => r.bump("connect_failed"),
            }
            true
        }),
        Fault::AbandonResponse => parallel(4, window, stop, t, |t, budget, r| {
            if !budget.take() {
                return false;
            }
            r.attempts += 1;
            let n = r.attempts;
            let bytes = if n % 2 == 0 {
                t.get(crate::api::BOARD_PATH)
            } else {
                let body = crate::api::submit_body(900_000_000 + n, "C:/abandon", t.dialect);
                t.post(&format!("/api/{}", t.noun), body.as_bytes())
            };
            match t.connect() {
                Ok(mut s) => {
                    if s.write_all(&bytes).is_ok() {
                        r.bump(if n % 2 == 0 {
                            "board_get_abandoned"
                        } else {
                            "submit_abandoned"
                        });
                    } else {
                        r.bump("write_failed");
                    }
                }
                Err(_) => r.bump("connect_failed"),
            }
            true
        }),
        Fault::Malformed => malformed(t, window, stop, &mut |_| None),
        Fault::OversizedBody => oversized(t, window, stop),
        Fault::ConnectionFlood => parallel(8, window, stop, t, |t, budget, r| {
            if !budget.take() {
                return false;
            }
            r.attempts += 1;
            match t.connect() {
                Ok(_) => r.bump("connected_then_closed"),
                Err(_) => r.bump("connect_failed"),
            }
            true
        }),
        Fault::BadAuthFlood => bad_auth(t, window, stop),
        Fault::ThunderingHerd => herd(t, 2000),
    }
}

/// Shared connection budget across an injector's threads.
struct Budget(AtomicU64);

impl Budget {
    fn take(&self) -> bool {
        self.0.fetch_add(1, Ordering::Relaxed) < CONNECTION_BUDGET
    }
}

/// Run `body` in a loop on `threads` threads until the window ends, `stop`
/// is raised, or `body` returns false (budget spent).
fn parallel(
    threads: usize,
    window: Duration,
    stop: &Arc<AtomicBool>,
    t: &Target,
    body: fn(&Target, &Budget, &mut InjectorReport) -> bool,
) -> InjectorReport {
    let report = Arc::new(Mutex::new(InjectorReport::default()));
    let budget = Arc::new(Budget(AtomicU64::new(0)));
    let deadline = Instant::now() + window;
    let mut hs = Vec::new();
    for _ in 0..threads {
        let (t, report, budget, stop) = (
            t.clone(),
            Arc::clone(&report),
            Arc::clone(&budget),
            Arc::clone(stop),
        );
        hs.push(thread::spawn(move || {
            let mut local = InjectorReport::default();
            while Instant::now() < deadline && !stop.load(Ordering::Relaxed) {
                if !body(&t, &budget, &mut local) {
                    // Budget spent: idle out the window so the victim load
                    // still measures the aftermath.
                    thread::sleep(Duration::from_millis(50));
                }
            }
            merge(&report, local);
        }));
    }
    for h in hs {
        let _ = h.join();
    }
    let mut r = report.lock().map(|r| r.clone()).unwrap_or_default();
    if budget.0.load(Ordering::Relaxed) >= CONNECTION_BUDGET {
        r.notes.push(format!(
            "connection budget of {CONNECTION_BUDGET} reached before the window ended"
        ));
    }
    r
}

/// Open `conns` connections; if `trickle`, send a partial request and then
/// one header line per second on each; otherwise send nothing. Hold for the
/// window and report how many the server closed on its own.
fn slowloris(
    t: &Target,
    conns: usize,
    window: Duration,
    stop: &Arc<AtomicBool>,
    trickle: bool,
) -> InjectorReport {
    let mut r = InjectorReport::default();
    let deadline = Instant::now() + window;
    let mut socks: Vec<Option<TcpStream>> = Vec::with_capacity(conns);
    for _ in 0..conns {
        r.attempts += 1;
        match t.connect() {
            Ok(mut s) => {
                let _ = s.set_write_timeout(Some(Duration::from_millis(200)));
                if trickle {
                    let _ = s.write_all(
                        format!("GET /api/tasks HTTP/1.1\r\nHost: x\r\n{}", t.auth()).as_bytes(),
                    );
                }
                socks.push(Some(s));
            }
            Err(_) => r.bump("connect_failed"),
        }
    }
    r.notes
        .push(format!("{} of {conns} connections opened", socks.len()));
    let mut server_closed = 0u64;
    let mut first_close_s: Option<f64> = None;
    let t0 = Instant::now();
    while Instant::now() < deadline && !stop.load(Ordering::Relaxed) {
        thread::sleep(Duration::from_secs(1));
        for slot in &mut socks {
            let closed = match slot {
                Some(s) => {
                    if trickle {
                        s.write_all(b"X-Slow: 1\r\n").is_err() || peer_closed(s)
                    } else {
                        peer_closed(s)
                    }
                }
                None => false,
            };
            if closed {
                *slot = None;
                server_closed += 1;
                first_close_s.get_or_insert(t0.elapsed().as_secs_f64());
            }
        }
    }
    r.outcomes.insert("closed_by_server".into(), server_closed);
    r.outcomes.insert(
        "still_open_at_end".into(),
        socks.iter().filter(|s| s.is_some()).count() as u64,
    );
    if let Some(s) = first_close_s {
        r.notes.push(format!(
            "server began closing held connections after ~{s:.1}s"
        ));
    }
    r
}

/// True when the peer has closed (read returns 0) or reset the socket.
fn peer_closed(s: &mut TcpStream) -> bool {
    let _ = s.set_nonblocking(true);
    let mut b = [0u8; 256];
    let closed = match s.read(&mut b) {
        Ok(0) => true,
        Ok(_) => false,
        Err(e) if e.kind() == ErrorKind::WouldBlock => false,
        Err(_) => true,
    };
    let _ = s.set_nonblocking(false);
    closed
}

fn truncated_hold(
    t: &Target,
    conns: usize,
    window: Duration,
    stop: &Arc<AtomicBool>,
) -> InjectorReport {
    let mut r = InjectorReport::default();
    let deadline = Instant::now() + window;
    let mut socks = Vec::new();
    for _ in 0..conns {
        r.attempts += 1;
        if let Ok(mut s) = t.connect() {
            let head = format!(
                "POST /api/{} HTTP/1.1\r\nHost: x\r\n{}Content-Type: application/json\r\nContent-Length: 1048576\r\n\r\n",
                t.noun,
                t.auth()
            );
            let _ = s.write_all(head.as_bytes());
            let _ = s.write_all(&[b' '; 64]);
            socks.push(Some(s));
        } else {
            r.bump("connect_failed");
        }
    }
    let t0 = Instant::now();
    while Instant::now() < deadline && !stop.load(Ordering::Relaxed) {
        thread::sleep(Duration::from_millis(500));
        for slot in &mut socks {
            if slot.as_mut().is_some_and(peer_closed) {
                *slot = None;
                r.bump("closed_by_server");
                r.notes.push(format!(
                    "server closed a truncated-body connection after {:.1}s",
                    t0.elapsed().as_secs_f64()
                ));
            }
        }
    }
    r.outcomes.insert(
        "held_until_end".into(),
        socks.iter().filter(|s| s.is_some()).count() as u64,
    );
    r
}

fn bad_auth(t: &Target, window: Duration, stop: &Arc<AtomicBool>) -> InjectorReport {
    let mut r = InjectorReport::default();
    if t.token.is_none() {
        r.notes
            .push("daemon has no bearer-token auth; nothing to flood".into());
        return r;
    }
    let report = Arc::new(Mutex::new(InjectorReport::default()));
    let deadline = Instant::now() + window;
    let mut hs = Vec::new();
    for _ in 0..16 {
        let (report, stop, base) = (
            Arc::clone(&report),
            Arc::clone(stop),
            format!("http://{}", t.addr),
        );
        hs.push(thread::spawn(move || {
            let ag = crate::api::agent(Duration::from_secs(10));
            let mut local = InjectorReport::default();
            while Instant::now() < deadline && !stop.load(Ordering::Relaxed) {
                local.attempts += 1;
                let rep = crate::api::raw_call(
                    &ag,
                    &base,
                    Some("not-the-token"),
                    None,
                    "GET",
                    "/api/tasks",
                    None,
                    false,
                );
                local.bump(&rep.outcome.class());
            }
            merge(&report, local);
        }));
    }
    for h in hs {
        let _ = h.join();
    }
    report.lock().map(|r| r.clone()).unwrap_or_default()
}

fn herd(t: &Target, n: usize) -> InjectorReport {
    let barrier = Arc::new(Barrier::new(n));
    let results = Arc::new(Mutex::new(Vec::with_capacity(n)));
    let mut hs = Vec::new();
    let mut spawn_failed = 0;
    for _ in 0..n {
        let (t, barrier, results) = (t.clone(), Arc::clone(&barrier), Arc::clone(&results));
        let h = thread::Builder::new()
            .stack_size(128 * 1024)
            .spawn(move || {
                barrier.wait();
                let t0 = Instant::now();
                let label = exchange(&t, &t.get(crate::api::BOARD_PATH), Duration::from_secs(30));
                if let Ok(mut v) = results.lock() {
                    v.push((label, t0.elapsed()));
                }
            });
        match h {
            Ok(h) => hs.push(h),
            Err(_) => spawn_failed += 1,
        }
    }
    if spawn_failed > 0 {
        // The barrier needs `n` parties; release it from here so the
        // spawned threads are not stuck forever.
        for _ in 0..spawn_failed {
            let b = Arc::clone(&barrier);
            thread::spawn(move || {
                b.wait();
            });
        }
    }
    for h in hs {
        let _ = h.join();
    }
    let mut r = InjectorReport::default();
    let v = results.lock().map(|v| v.clone()).unwrap_or_default();
    let mut lats: Vec<u64> = Vec::new();
    for (label, lat) in &v {
        r.attempts += 1;
        r.bump(label);
        lats.push(lat.as_micros() as u64);
    }
    lats.sort_unstable();
    r.latency_p99_ms = Some(crate::stats::percentile(&lats, 99.0));
    if spawn_failed > 0 {
        r.notes
            .push(format!("{spawn_failed} herd threads failed to spawn"));
    }
    r
}

fn oversized(t: &Target, window: Duration, stop: &Arc<AtomicBool>) -> InjectorReport {
    let mut r = InjectorReport::default();
    let deadline = Instant::now() + window;
    let sizes_mb = [16usize, 32, 64];
    let mut i = 0;
    while Instant::now() < deadline && !stop.load(Ordering::Relaxed) && i < 6 {
        let mb = sizes_mb[i % sizes_mb.len()];
        i += 1;
        // A syntactically valid submit whose TOML is padded with comment
        // lines, so the daemon has to read and parse all of it.
        let line = format!("# {}\n", "x".repeat(1022));
        let mut toml = String::with_capacity(mb << 20);
        while toml.len() < (mb << 20) {
            toml.push_str(&line);
        }
        toml.push_str(&crate::api::submit_toml(
            800_000_000 + i as u64,
            "C:/oversized",
            t.dialect,
        ));
        let body = format!(
            "{{\"toml\":{},\"hold\":true}}",
            serde_json::Value::String(toml)
        );
        let t0 = Instant::now();
        let label = exchange(
            t,
            &t.post(&format!("/api/{}", t.noun), body.as_bytes()),
            Duration::from_secs(120),
        );
        r.attempts += 1;
        r.bump(&format!("{mb}MiB_{label}"));
        r.notes.push(format!(
            "{mb} MiB submit -> {label} in {:.1}s",
            t0.elapsed().as_secs_f64()
        ));
    }
    r
}

/// The malformed-request corpus: (name, bytes, complete). `complete` is
/// false for payloads that deliberately never finish a request, where no
/// response is an acceptable outcome.
fn corpus(t: &Target) -> Vec<(&'static str, Vec<u8>, bool)> {
    let auth = t.auth();
    let noun = &t.noun;
    let cell = t.dialect.cell_key;
    let preamble = t.dialect.preamble();
    let submit = format!("/api/{noun}");
    let raw = |s: String| s.into_bytes();
    let mut garbage = Vec::with_capacity(1024);
    let mut x: u32 = 0x1234_5678;
    for _ in 0..1024 {
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        garbage.push((x & 0xff) as u8);
    }
    let many_headers: String = (0..2000).map(|i| format!("X-H{i}: v\r\n")).collect();
    let deep_json = format!("{}{}", "[".repeat(100_000), "]".repeat(100_000));
    let big_squad: String = std::iter::once(preamble.to_string())
        .chain((0..500).map(|i| {
            format!(
                "[[task]]\nname = \"big-{i}\"\n[[task.{cell}]]\ncwd = \"C:/big\"\nprompt = \"p{i}\"\n"
            )
        }))
        .collect();
    let big_squad_body = format!(
        "{{\"toml\":{},\"hold\":true}}",
        serde_json::Value::String(big_squad)
    );
    let chunked_ok = {
        let body = crate::api::submit_body(700_000_000, "C:/chunked", t.dialect);
        format!(
            "POST {submit} HTTP/1.1\r\nHost: x\r\n{auth}Content-Type: application/json\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:x}\r\n{body}\r\n0\r\n\r\n",
            body.len()
        )
    };
    vec![
        ("garbage_bytes", garbage, false),
        ("empty_lines", b"\r\n\r\n\r\n".to_vec(), false),
        ("unknown_method", raw(format!("FOO /api/tasks HTTP/1.1\r\nHost: x\r\n{auth}Connection: close\r\n\r\n")), true),
        ("bad_http_version", raw(format!("GET /api/tasks HTTP/9.9\r\nHost: x\r\n{auth}Connection: close\r\n\r\n")), true),
        ("http_0_9_style", b"GET /api/tasks\r\n\r\n".to_vec(), false),
        ("header_without_colon", raw(format!("GET /api/tasks HTTP/1.1\r\nHost: x\r\n{auth}BadHeaderLine\r\nConnection: close\r\n\r\n")), true),
        ("huge_header_64k", raw(format!("GET /api/tasks HTTP/1.1\r\nHost: x\r\n{auth}X-Big: {}\r\nConnection: close\r\n\r\n", "a".repeat(65536))), true),
        ("two_thousand_headers", raw(format!("GET /api/tasks HTTP/1.1\r\nHost: x\r\n{auth}{many_headers}Connection: close\r\n\r\n")), true),
        ("long_url_100k", raw(format!("GET /api/tasks?q={} HTTP/1.1\r\nHost: x\r\n{auth}Connection: close\r\n\r\n", "a".repeat(100_000))), true),
        ("nul_in_path", raw(format!("GET /api/ta\0sks HTTP/1.1\r\nHost: x\r\n{auth}Connection: close\r\n\r\n")), true),
        ("path_traversal", raw(format!("GET /api/../../../../etc/passwd HTTP/1.1\r\nHost: x\r\n{auth}Connection: close\r\n\r\n")), true),
        ("encoded_traversal_id", raw(format!("GET /api/{noun}/..%2f..%2f..%2fwindows%2fwin.ini HTTP/1.1\r\nHost: x\r\n{auth}Connection: close\r\n\r\n")), true),
        ("bad_percent_encoding", raw(format!("GET /api/{noun}/%zz%%% HTTP/1.1\r\nHost: x\r\n{auth}Connection: close\r\n\r\n")), true),
        ("unicode_id", raw(format!("GET /api/{noun}/\u{fc}n\u{ef}c\u{f6}d\u{e9}-\u{1f600} HTTP/1.1\r\nHost: x\r\n{auth}Connection: close\r\n\r\n")), true),
        ("sql_injection_id", raw(format!("GET /api/{noun}/'%20OR%201=1;--%20DROP%20TABLE%20squads HTTP/1.1\r\nHost: x\r\n{auth}Connection: close\r\n\r\n")), true),
        ("invalid_json_body", t.post(&submit, b"{not json"), true),
        ("invalid_utf8_body", t.post(&submit, &[0xff, 0xfe, 0xfd, b'{', 0xc3, 0x28]), true),
        ("wrong_json_types", t.post(&submit, br#"{"toml": 5, "hold": "yes"}"#), true),
        ("deeply_nested_json", t.post(&submit, deep_json.as_bytes()), true),
        ("invalid_toml", t.post(&submit, br#"{"toml":"[[task]\nname=","hold":true}"#), true),
        ("empty_toml", t.post(&submit, br#"{"toml":"","hold":true}"#), true),
        ("toml_no_cells", t.post(&submit, format!("{{\"toml\":{}, \"hold\":true}}", crate::api::json_str(&format!("{preamble}[[task]]\nname=\"x\"\n"))).as_bytes()), true),
        ("toml_nul_and_control_chars", t.post(&submit, format!("{{\"toml\":{}, \"hold\":true}}", crate::api::json_str(&format!("{preamble}[[task]]\nname=\"a\u{0}b\u{1b}[31m\"\n[[task.{cell}]]\ncwd=\"C:/x\"\nprompt=\"p\"\n"))).as_bytes()), true),
        ("submit_500_tasks", t.post(&submit, big_squad_body.as_bytes()), true),
        ("chunked_valid_submit", chunked_ok.into_bytes(), true),
        ("chunked_bad_size", raw(format!("POST {submit} HTTP/1.1\r\nHost: x\r\n{auth}Transfer-Encoding: chunked\r\nConnection: close\r\n\r\nzz\r\nabc\r\n0\r\n\r\n")), true),
        ("content_length_and_chunked", raw(format!("POST {submit} HTTP/1.1\r\nHost: x\r\n{auth}Content-Length: 4\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n0\r\n\r\nGET /api/tasks HTTP/1.1\r\nHost: x\r\n\r\n")), true),
        ("negative_content_length", raw(format!("POST {submit} HTTP/1.1\r\nHost: x\r\n{auth}Content-Length: -1\r\nConnection: close\r\n\r\n{{}}")), true),
        ("non_numeric_content_length", raw(format!("POST {submit} HTTP/1.1\r\nHost: x\r\n{auth}Content-Length: abc\r\nConnection: close\r\n\r\n{{}}")), true),
        ("huge_content_length_then_close", raw(format!("POST {submit} HTTP/1.1\r\nHost: x\r\n{auth}Content-Length: 99999999999999\r\n\r\n{{}}")), false),
        ("post_without_content_length", raw(format!("POST {submit} HTTP/1.1\r\nHost: x\r\n{auth}Connection: close\r\n\r\n{{\"toml\":\"x\"}}")), true),
        ("unknown_route", t.get("/api/definitely-not-a-route"), true),
        ("wrong_method_on_route", raw(format!("DELETE /api/tasks HTTP/1.1\r\nHost: x\r\n{auth}Connection: close\r\n\r\n")), true),
        ("expect_100_continue", raw(format!("POST /api/{noun}/validate HTTP/1.1\r\nHost: x\r\n{auth}Expect: 100-continue\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}")), true),
        ("pipelined_50", {
            let mut v = Vec::new();
            for _ in 0..49 {
                v.extend(format!("GET /api/daemon HTTP/1.1\r\nHost: x\r\n{auth}\r\n").into_bytes());
            }
            v.extend(t.get("/api/daemon"));
            v
        }, true),
        ("hostile_user_header", raw(format!("GET /api/tasks HTTP/1.1\r\nHost: x\r\n{}X-Ralphus-User: ../../admin\u{0}\r\nConnection: close\r\n\r\n", t.token.as_ref().map(|t| format!("Authorization: Bearer {t}\r\n")).unwrap_or_default())), true),
        ("oversized_bearer_10k", raw(format!("GET /api/tasks HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {}\r\nConnection: close\r\n\r\n", "z".repeat(10_000))), true),
    ]
}

/// Send every corpus payload, cycling while the window lasts. After each
/// payload `after_payload` runs; when it reports the daemon died (and has
/// restarted it), the payload is recorded as `killed_daemon` so the crash is
/// attributed to exactly one input and the rest of the corpus still runs.
pub fn malformed(
    t: &Target,
    window: Duration,
    stop: &Arc<AtomicBool>,
    after_payload: &mut dyn FnMut(&str) -> Option<String>,
) -> InjectorReport {
    let mut r = InjectorReport::default();
    let deadline = Instant::now() + window;
    let corpus = corpus(t);
    let mut hung = Vec::new();
    let mut rounds = 0;
    // Every payload at least once; then keep cycling while the window lasts
    // (max 5 rounds -- `submit_500_tasks` writes a 500-task squad each time).
    while rounds < 5
        && (rounds == 0 || (Instant::now() < deadline && !stop.load(Ordering::Relaxed)))
    {
        rounds += 1;
        for (name, bytes, complete) in &corpus {
            r.attempts += 1;
            let mut label = exchange(
                t,
                bytes,
                Duration::from_secs(if *complete { 10 } else { 3 }),
            );
            if !*complete {
                // The daemon finishes with a deliberately incomplete request
                // only once the socket is closed (when `exchange` returned);
                // give it time to act on that before checking it survived, so
                // a crash is pinned on this payload rather than the next one.
                thread::sleep(Duration::from_secs(1));
            }
            if let Some(note) = after_payload(name) {
                r.notes.push(note);
                label = format!("killed_daemon_after_{label}");
            }
            if *complete && label == "no_response_timeout" && !hung.contains(name) {
                hung.push(*name);
            }
            *r.per_payload
                .entry((*name).to_string())
                .or_default()
                .entry(label.clone())
                .or_default() += 1;
            r.bump(&label);
        }
    }
    for name in hung {
        r.notes.push(format!(
            "complete request {name:?} got no response within 10s"
        ));
    }
    for (name, outcomes) in &r.per_payload {
        for k in outcomes.keys() {
            if k.starts_with("status_5") {
                r.notes
                    .push(format!("payload {name:?} produced a server error ({k})"));
            }
        }
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_label_parses_status_line() {
        assert_eq!(status_label(b"HTTP/1.1 404 Not Found\r\n"), "status_404");
        assert_eq!(status_label(b"garbage\r\n"), "non_http_response");
    }

    #[test]
    fn corpus_names_are_unique() {
        let t = Target {
            addr: "127.0.0.1:1".parse().unwrap(),
            token: Some("t".into()),
            user: Some("u".into()),
            noun: "squads".into(),
            dialect: crate::api::Dialect::ALL[0],
        };
        let c = corpus(&t);
        let mut names: Vec<_> = c.iter().map(|(n, _, _)| *n).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), c.len());
    }
}
