//! Scans the daemon's stderr/stdout/log file for panics, slow-request
//! warnings and error-looking lines written during one scenario.
//!
//! `rlog!` lines carry no level token (`ralphus [component] message`), so
//! "error-looking" is a keyword match; panics and the server's own `SLOW:`
//! marker are exact.

use serde::Serialize;
use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;

#[derive(Serialize, Clone, Debug)]
pub struct Panic {
    pub thread: String,
    /// `file:line:col` from the panic message.
    pub location: String,
    pub message: String,
    pub count: u64,
    pub log_file: String,
}

#[derive(Serialize, Clone, Debug)]
pub struct LineCount {
    pub pattern: String,
    pub example: String,
    pub count: u64,
}

/// Server-side timing for one route, from the daemon's own request log
/// (`ralphus [http] GET /api/tasks -> 200 (12ms, queued 3ms, lock_wait 0ms)`):
/// `queued` is time waiting for a read/write worker, `lock_wait` time waiting
/// for the store lock, `handler` the rest of the handler.
#[derive(Serialize, Clone, Debug)]
pub struct RouteTiming {
    pub route: String,
    pub count: u64,
    pub non_2xx: u64,
    pub handler_p50_ms: f64,
    pub handler_p99_ms: f64,
    pub handler_max_ms: f64,
    pub queued_p50_ms: f64,
    pub queued_p99_ms: f64,
    pub lock_wait_p50_ms: f64,
    pub lock_wait_p99_ms: f64,
}

/// Long store-lock holds grouped by the call site that took the lock
/// (`store guard held for 630ms (acquired at daemon\src\server.rs:7510)`).
#[derive(Serialize, Clone, Debug)]
pub struct LockHold {
    pub site: String,
    pub count: u64,
    pub max_ms: u64,
    pub total_ms: u64,
}

#[derive(Serialize, Clone, Debug, Default)]
pub struct LogScan {
    /// Panics, plus process aborts (allocation failure, stack overflow,
    /// fatal runtime errors) recorded with thread `process-abort`.
    pub panics: Vec<Panic>,
    pub slow_requests: u64,
    pub slow_examples: Vec<String>,
    pub error_lines: Vec<LineCount>,
    pub error_lines_total: u64,
    pub routes: Vec<RouteTiming>,
    pub lock_holds: Vec<LockHold>,
}

impl LogScan {
    pub fn is_clean(&self) -> bool {
        self.panics.is_empty() && self.error_lines_total == 0
    }
}

const ERROR_WORDS: [&str; 10] = [
    "error",
    "failed",
    "panic",
    "poison",
    "deadlock",
    "watchdog",
    "stalled",
    "database is locked",
    "busy",
    "fatal",
];

/// Lines that match an error word but are routine for every run (the
/// sandbox has no agent CLIs, forge tokens or remotes configured).
const BENIGN: [&str; 6] = [
    "not installed",
    "not found on path",
    "no forge token",
    "login",
    "could not resolve",
    "health probe",
];

pub struct LogCursor {
    files: Vec<(PathBuf, u64)>,
}

impl LogCursor {
    pub fn at_end(paths: Vec<PathBuf>) -> Self {
        let files = paths
            .into_iter()
            .map(|p| {
                let len = fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
                (p, len)
            })
            .collect();
        LogCursor { files }
    }

    /// Scan everything written since the last scan (or construction).
    pub fn scan(&mut self) -> LogScan {
        let mut scan = LogScan::default();
        let mut panics: BTreeMap<(String, String), Panic> = BTreeMap::new();
        let mut errors: BTreeMap<String, LineCount> = BTreeMap::new();
        let mut routes: BTreeMap<String, RouteAcc> = BTreeMap::new();
        let mut holds: BTreeMap<String, LockHold> = BTreeMap::new();
        for (path, offset) in &mut self.files {
            let Ok(mut f) = fs::File::open(&*path) else {
                continue;
            };
            let len = f.metadata().map(|m| m.len()).unwrap_or(0);
            if len < *offset {
                *offset = 0;
            }
            if f.seek(SeekFrom::Start(*offset)).is_err() {
                continue;
            }
            let mut buf = Vec::new();
            if f.read_to_end(&mut buf).is_err() {
                continue;
            }
            *offset += buf.len() as u64;
            let text = String::from_utf8_lossy(&buf);
            let lines: Vec<&str> = text.lines().collect();
            let file_name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            for (i, line) in lines.iter().enumerate() {
                if let Some((thread, location, inline_msg)) = parse_panic(line) {
                    let message = if inline_msg.is_empty() {
                        lines
                            .get(i + 1)
                            .map(|l| l.trim().to_string())
                            .unwrap_or_default()
                    } else {
                        inline_msg
                    };
                    panics
                        .entry((location.clone(), message.clone()))
                        .and_modify(|p| p.count += 1)
                        .or_insert(Panic {
                            thread,
                            location,
                            message,
                            count: 1,
                            log_file: file_name.clone(),
                        });
                    continue;
                }
                if let Some(msg) = parse_abort(line) {
                    panics
                        .entry(("abort".to_string(), normalize(&msg)))
                        .and_modify(|p| p.count += 1)
                        .or_insert(Panic {
                            thread: "process-abort".to_string(),
                            location: "abort".to_string(),
                            message: msg,
                            count: 1,
                            log_file: file_name.clone(),
                        });
                    continue;
                }
                if let Some((site, ms)) = parse_lock_hold(line) {
                    let h = holds.entry(site.clone()).or_insert(LockHold {
                        site,
                        count: 0,
                        max_ms: 0,
                        total_ms: 0,
                    });
                    h.count += 1;
                    h.max_ms = h.max_ms.max(ms);
                    h.total_ms += ms;
                    continue;
                }
                if let Some(t) = parse_http(line) {
                    let acc = routes.entry(t.route).or_default();
                    acc.handler.push(t.handler_us);
                    acc.queued.push(t.queued_us);
                    acc.lock_wait.push(t.lock_wait_us);
                    if !(200..400).contains(&t.status) {
                        acc.non_2xx += 1;
                    }
                }
                if line.contains(" SLOW: ") {
                    scan.slow_requests += 1;
                    if scan.slow_examples.len() < 10 {
                        scan.slow_examples.push(line.trim().to_string());
                    }
                    continue;
                }
                let lower = line.to_lowercase();
                if ERROR_WORDS.iter().any(|w| lower.contains(w))
                    && !BENIGN.iter().any(|b| lower.contains(b))
                {
                    scan.error_lines_total += 1;
                    let pattern = normalize(line);
                    errors
                        .entry(pattern.clone())
                        .and_modify(|e| e.count += 1)
                        .or_insert(LineCount {
                            pattern,
                            example: line.trim().chars().take(500).collect(),
                            count: 1,
                        });
                }
            }
        }
        scan.panics = panics.into_values().collect();
        let mut errs: Vec<LineCount> = errors.into_values().collect();
        errs.sort_by_key(|e| std::cmp::Reverse(e.count));
        errs.truncate(25);
        scan.error_lines = errs;
        let mut rts: Vec<RouteTiming> = routes
            .into_iter()
            .map(|(route, acc)| acc.finish(route))
            .collect();
        // Biggest share of server time first.
        let weight = |r: &RouteTiming| (r.handler_p50_ms + r.queued_p50_ms) * r.count as f64;
        rts.sort_by(|a, b| weight(b).total_cmp(&weight(a)));
        rts.truncate(30);
        scan.routes = rts;
        let mut hs: Vec<LockHold> = holds.into_values().collect();
        hs.sort_by_key(|h| std::cmp::Reverse(h.total_ms));
        hs.truncate(20);
        scan.lock_holds = hs;
        scan
    }
}

#[derive(Default)]
struct RouteAcc {
    handler: Vec<u64>,
    queued: Vec<u64>,
    lock_wait: Vec<u64>,
    non_2xx: u64,
}

impl RouteAcc {
    fn finish(mut self, route: String) -> RouteTiming {
        for v in [&mut self.handler, &mut self.queued, &mut self.lock_wait] {
            v.sort_unstable();
        }
        let p = crate::stats::percentile;
        RouteTiming {
            route,
            count: self.handler.len() as u64,
            non_2xx: self.non_2xx,
            handler_p50_ms: p(&self.handler, 50.0),
            handler_p99_ms: p(&self.handler, 99.0),
            handler_max_ms: p(&self.handler, 100.0),
            queued_p50_ms: p(&self.queued, 50.0),
            queued_p99_ms: p(&self.queued, 99.0),
            lock_wait_p50_ms: p(&self.lock_wait, 50.0),
            lock_wait_p99_ms: p(&self.lock_wait, 99.0),
        }
    }
}

struct HttpLine {
    route: String,
    status: u16,
    handler_us: u64,
    queued_us: u64,
    lock_wait_us: u64,
}

/// Leading decimal number in `s` (`"12.5ms ..."` -> 12.5).
fn leading_number(s: &str) -> Option<f64> {
    let end = s
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(s.len());
    s[..end].parse().ok()
}

fn number_after(s: &str, key: &str) -> Option<f64> {
    let i = s.find(key)?;
    leading_number(&s[i + key.len()..])
}

/// Replace id-like path segments so `/api/squads/squad-000000000012/env`
/// groups as `/api/squads/{id}/env`.
fn route_key(method: &str, url: &str) -> String {
    let path = url.split('?').next().unwrap_or(url);
    let segs: Vec<&str> = path
        .split('/')
        .map(|s| {
            if s.chars().any(|c| c.is_ascii_digit()) {
                "{id}"
            } else {
                s
            }
        })
        .collect();
    format!("{method} {}", segs.join("/"))
}

/// `ralphus [http] GET /api/tasks -> 200 (12ms, queued 0ms, lock_wait 0ms)`
/// or `... -> 201 SLOW: handler 2435ms (queued 0ms, lock_wait 0ms)`.
fn parse_http(line: &str) -> Option<HttpLine> {
    let rest = &line[line.find("ralphus [http] ")? + "ralphus [http] ".len()..];
    let mut parts = rest.splitn(3, ' ');
    let method = parts.next()?;
    let url = parts.next()?;
    let tail = parts.next()?.strip_prefix("-> ")?;
    let status: u16 = tail.split_whitespace().next()?.parse().ok()?;
    let handler = if tail.contains("SLOW: handler ") {
        number_after(tail, "SLOW: handler ")?
    } else {
        number_after(tail, "(")?
    };
    let us = |ms: f64| (ms * 1000.0) as u64;
    Some(HttpLine {
        route: route_key(method, url),
        status,
        handler_us: us(handler),
        queued_us: us(number_after(tail, "queued ").unwrap_or(0.0)),
        lock_wait_us: us(number_after(tail, "lock_wait ").unwrap_or(0.0)),
    })
}

/// `store guard held for 630ms (acquired at daemon\src\server.rs:7510)`.
fn parse_lock_hold(line: &str) -> Option<(String, u64)> {
    let ms = number_after(line, "store guard held for ")? as u64;
    let i = line.find("(acquired at ")? + "(acquired at ".len();
    let site = line[i..].split(')').next()?.replace('\\', "/");
    Some((site, ms))
}

/// Process-killing conditions Rust reports without a panic message.
fn parse_abort(line: &str) -> Option<String> {
    const MARKERS: [&str; 4] = [
        "memory allocation of",
        "has overflowed its stack",
        "fatal runtime error",
        "STATUS_STACK_BUFFER_OVERRUN",
    ];
    MARKERS
        .iter()
        .any(|m| line.contains(m))
        .then(|| line.trim().to_string())
}

/// `thread 'name' panicked at src/x.rs:1:2:` (message on the next line, Rust
/// 1.73+) or `... panicked at 'msg', src/x.rs:1:2` (older).
fn parse_panic(line: &str) -> Option<(String, String, String)> {
    let idx = line.find("panicked at ")?;
    let thread = line
        .find("thread '")
        .and_then(|s| {
            let rest = &line[s + 8..];
            rest.find('\'').map(|e| rest[..e].to_string())
        })
        .unwrap_or_else(|| "?".to_string());
    let rest = line[idx + "panicked at ".len()..].trim();
    if let Some(stripped) = rest.strip_prefix('\'') {
        // Old format: 'message', location
        let end = stripped.rfind("', ")?;
        let msg = stripped[..end].to_string();
        let loc = stripped[end + 3..].trim().to_string();
        return Some((thread, loc, msg));
    }
    let loc = rest.trim_end_matches(':').to_string();
    Some((thread, loc, String::new()))
}

/// Collapse digits and quoted ids so repeats of the same message group.
fn normalize(line: &str) -> String {
    let trimmed = match line.find("ralphus [") {
        Some(i) => &line[i..],
        None => line.trim(),
    };
    let mut out = String::with_capacity(trimmed.len());
    let mut last_digit = false;
    for ch in trimmed.chars().take(200) {
        if ch.is_ascii_digit() {
            if !last_digit {
                out.push('#');
            }
            last_digit = true;
        } else {
            out.push(ch);
            last_digit = false;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_modern_panic_line() {
        let (t, loc, msg) =
            parse_panic("thread 'http-read-3' panicked at daemon/src/server.rs:120:9:").unwrap();
        assert_eq!(t, "http-read-3");
        assert_eq!(loc, "daemon/src/server.rs:120:9");
        assert!(msg.is_empty());
    }

    #[test]
    fn parses_old_panic_line() {
        let (_, loc, msg) =
            parse_panic("thread 'main' panicked at 'boom', src/main.rs:3:5").unwrap();
        assert_eq!(loc, "src/main.rs:3:5");
        assert_eq!(msg, "boom");
    }

    #[test]
    fn normalize_groups_numbers() {
        assert_eq!(
            normalize("12:00:01 ralphus [http] squad-000000000012 failed after 35ms"),
            "ralphus [http] squad-# failed after #ms"
        );
    }

    #[test]
    fn parses_http_timing_lines() {
        let t = parse_http("01:26:51 ralphus [http] POST /api/squads/squad-000000000214/env -> 201 (653ms, queued 2ms, lock_wait 635.5ms)").unwrap();
        assert_eq!(t.route, "POST /api/squads/{id}/env");
        assert_eq!(t.status, 201);
        assert_eq!(t.handler_us, 653_000);
        assert_eq!(t.queued_us, 2_000);
        assert_eq!(t.lock_wait_us, 635_500);
        let s = parse_http("ralphus [http] GET /api/tasks?x=1 -> 200 SLOW: handler 2435ms (queued 7ms, lock_wait 0ms)").unwrap();
        assert_eq!(s.route, "GET /api/tasks");
        assert_eq!(s.handler_us, 2_435_000);
        assert_eq!(s.queued_us, 7_000);
        assert!(
            parse_http("ralphus [http] POST /api/squads rejected 400 validation_failed").is_none()
        );
    }

    #[test]
    fn parses_lock_hold_and_abort_lines() {
        let (site, ms) = parse_lock_hold(
            "01:26:51 ralphus [store_lock] store guard held for 630ms (acquired at daemon\\src\\server.rs:7510) -- blocking work",
        )
        .unwrap();
        assert_eq!(site, "daemon/src/server.rs:7510");
        assert_eq!(ms, 630);
        assert!(parse_abort("memory allocation of 99999999999997 bytes failed").is_some());
        assert!(parse_abort("ralphus [http] GET / -> 200").is_none());
    }

    #[test]
    fn scan_reads_only_new_bytes() {
        let dir = std::env::temp_dir().join(format!("ralphus-stress-logs-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("d.log");
        std::fs::write(&p, "ralphus [x] old failed\n").unwrap();
        let mut c = LogCursor::at_end(vec![p.clone()]);
        let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
        std::io::Write::write_all(
            &mut f,
            b"thread 'w' panicked at a.rs:1:2:\nboom\nralphus [http] GET / -> 200 SLOW: handler 1200ms\n",
        )
        .unwrap();
        let s = c.scan();
        assert_eq!(s.panics.len(), 1);
        assert_eq!(s.panics[0].message, "boom");
        assert_eq!(s.slow_requests, 1);
        assert_eq!(s.error_lines_total, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
