//! Per-request samples and the window summaries (throughput, error rate,
//! latency percentiles) every scenario reports.

use serde::Serialize;
use std::collections::BTreeMap;

/// How one request ended. `Ok` carries the 2xx/3xx status; everything else
/// counts against the error rate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Ok(u16),
    Http(u16),
    Timeout,
    Connect,
    Reset,
    Io,
    Protocol,
}

impl Outcome {
    pub fn is_ok(self) -> bool {
        matches!(self, Outcome::Ok(_))
    }

    pub fn class(self) -> String {
        match self {
            Outcome::Ok(_) => "ok".to_string(),
            Outcome::Http(code) => format!("http_{code}"),
            Outcome::Timeout => "timeout".to_string(),
            Outcome::Connect => "connect_failed".to_string(),
            Outcome::Reset => "connection_reset".to_string(),
            Outcome::Io => "io".to_string(),
            Outcome::Protocol => "protocol".to_string(),
        }
    }
}

/// One finished request. Times are microseconds since the window started.
#[derive(Clone, Copy, Debug)]
pub struct Sample {
    pub op: &'static str,
    pub start_us: u64,
    pub latency_us: u64,
    pub outcome: Outcome,
    pub bytes: u64,
}

#[derive(Serialize, Clone, Debug, Default)]
pub struct Latency {
    pub p50_ms: f64,
    pub p90_ms: f64,
    pub p99_ms: f64,
    pub max_ms: f64,
    pub mean_ms: f64,
}

#[derive(Serialize, Clone, Debug, Default)]
pub struct OpStats {
    pub requests: u64,
    pub errors: u64,
    pub error_rate: f64,
    pub latency_ok: Latency,
}

/// Aggregate over one measurement window.
#[derive(Serialize, Clone, Debug, Default)]
pub struct WindowStats {
    pub duration_s: f64,
    pub requests: u64,
    pub ok: u64,
    pub errors: u64,
    pub error_rate: f64,
    /// Requests that *finished* inside the window, per second (any outcome).
    pub rps: f64,
    /// Successful requests that finished inside the window, per second -- the
    /// headline throughput number (fast-failing errors never inflate it).
    pub ok_rps: f64,
    pub latency_ok: Latency,
    pub errors_by_class: BTreeMap<String, u64>,
    /// First error message seen per class, for diagnosing what went wrong.
    pub error_examples: BTreeMap<String, String>,
    pub per_op: BTreeMap<String, OpStats>,
    pub mb_received: f64,
    /// Longest stretch of the window in which no request completed
    /// successfully -- a stalled server shows up here even when every
    /// request eventually succeeds inside the client timeout.
    pub longest_silence_ms: f64,
}

/// Longest gap between successful completions, counting the window's start
/// and end as boundaries.
fn longest_silence_us(mut finishes: Vec<u64>, window_us: u64) -> u64 {
    finishes.retain(|f| *f <= window_us);
    finishes.sort_unstable();
    let mut prev = 0u64;
    let mut longest = 0u64;
    for f in finishes {
        longest = longest.max(f - prev);
        prev = f;
    }
    longest.max(window_us.saturating_sub(prev))
}

pub fn percentile(sorted_us: &[u64], pct: f64) -> f64 {
    if sorted_us.is_empty() {
        return 0.0;
    }
    let idx = ((pct / 100.0) * (sorted_us.len() - 1) as f64).round() as usize;
    sorted_us[idx.min(sorted_us.len() - 1)] as f64 / 1000.0
}

pub fn latency(mut us: Vec<u64>) -> Latency {
    if us.is_empty() {
        return Latency::default();
    }
    us.sort_unstable();
    let sum: u128 = us.iter().map(|v| u128::from(*v)).sum();
    Latency {
        p50_ms: percentile(&us, 50.0),
        p90_ms: percentile(&us, 90.0),
        p99_ms: percentile(&us, 99.0),
        max_ms: *us.last().unwrap_or(&0) as f64 / 1000.0,
        mean_ms: sum as f64 / us.len() as f64 / 1000.0,
    }
}

fn rate(errors: u64, requests: u64) -> f64 {
    if requests == 0 {
        0.0
    } else {
        errors as f64 / requests as f64
    }
}

/// Summarise `samples` over a window of `window_s` seconds.
pub fn summarize(
    samples: &[Sample],
    window_s: f64,
    examples: &BTreeMap<String, String>,
) -> WindowStats {
    let window_us = (window_s * 1_000_000.0) as u64;
    let mut stats = WindowStats {
        duration_s: window_s,
        error_examples: examples.clone(),
        ..WindowStats::default()
    };
    let mut ok_lat = Vec::with_capacity(samples.len());
    let mut per_op_lat: BTreeMap<&'static str, Vec<u64>> = BTreeMap::new();
    let mut finished_in_window = 0u64;
    let mut ok_in_window = 0u64;
    let mut bytes = 0u64;
    let mut ok_finishes = Vec::with_capacity(samples.len());
    for s in samples {
        stats.requests += 1;
        bytes += s.bytes;
        let op = stats.per_op.entry(s.op.to_string()).or_default();
        op.requests += 1;
        let in_window = s.start_us + s.latency_us <= window_us;
        if in_window {
            finished_in_window += 1;
        }
        if s.outcome.is_ok() {
            stats.ok += 1;
            ok_lat.push(s.latency_us);
            per_op_lat.entry(s.op).or_default().push(s.latency_us);
            ok_finishes.push(s.start_us + s.latency_us);
            if in_window {
                ok_in_window += 1;
            }
        } else {
            stats.errors += 1;
            op.errors += 1;
            *stats.errors_by_class.entry(s.outcome.class()).or_default() += 1;
        }
    }
    for (name, op) in &mut stats.per_op {
        op.error_rate = rate(op.errors, op.requests);
        if let Some(lat) = per_op_lat.remove(name.as_str()) {
            op.latency_ok = latency(lat);
        }
    }
    stats.error_rate = rate(stats.errors, stats.requests);
    if window_s > 0.0 {
        stats.rps = finished_in_window as f64 / window_s;
        stats.ok_rps = ok_in_window as f64 / window_s;
    }
    stats.latency_ok = latency(ok_lat);
    stats.mb_received = bytes as f64 / (1024.0 * 1024.0);
    stats.longest_silence_ms = longest_silence_us(ok_finishes, window_us) as f64 / 1000.0;
    stats
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(start_us: u64, latency_us: u64, outcome: Outcome) -> Sample {
        Sample {
            op: "board",
            start_us,
            latency_us,
            outcome,
            bytes: 10,
        }
    }

    #[test]
    fn longest_silence_counts_window_edges() {
        assert_eq!(longest_silence_us(vec![], 1000), 1000);
        assert_eq!(longest_silence_us(vec![100, 200, 900], 1000), 700);
        assert_eq!(longest_silence_us(vec![600, 650], 1000), 600);
        // Completions after the window are ignored.
        assert_eq!(longest_silence_us(vec![100, 5000], 1000), 900);
    }

    #[test]
    fn percentile_picks_nearest_rank() {
        let v: Vec<u64> = (1..=100).map(|ms| ms * 1000).collect();
        assert_eq!(percentile(&v, 50.0), 51.0);
        assert_eq!(percentile(&v, 99.0), 99.0);
        assert_eq!(percentile(&v, 100.0), 100.0);
        assert_eq!(percentile(&[], 99.0), 0.0);
    }

    #[test]
    fn summarize_counts_errors_and_window_throughput() {
        let samples = vec![
            sample(0, 1_000, Outcome::Ok(200)),
            sample(10, 2_000, Outcome::Ok(200)),
            sample(20, 500, Outcome::Http(500)),
            // Finishes after the 1 s window: counted, but not in rps.
            sample(900_000, 200_000, Outcome::Ok(200)),
        ];
        let s = summarize(&samples, 1.0, &BTreeMap::new());
        assert_eq!(s.requests, 4);
        assert_eq!(s.errors, 1);
        assert!((s.error_rate - 0.25).abs() < 1e-9);
        assert!((s.rps - 3.0).abs() < 1e-9);
        assert!((s.ok_rps - 2.0).abs() < 1e-9);
        assert_eq!(s.errors_by_class.get("http_500"), Some(&1));
        assert_eq!(s.per_op["board"].errors, 1);
    }
}
