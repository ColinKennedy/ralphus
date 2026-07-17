//! The "durable minimum" repeated in-process invocation algorithm (RAL-94).
//!
//! Mirrors `cli/src/ralphus/bench/durable_min.py`'s stopping rule exactly.
//! The two implementations are independent on purpose — Rust and Python
//! bench data live in separate directories and are never compared to each
//! other numerically, so there is no requirement that they interpolate
//! samples the same way, only that the stopping rule itself matches.

use std::time::Instant;

pub const DEFAULT_PATIENCE: u32 = 10;

#[derive(Debug, Clone)]
pub struct DurableMinResult {
    pub durable_min: f64,
    pub samples: Vec<f64>,
}

/// Repeatedly calls `f` in-process, tracking the durable-minimum wall-clock
/// duration: each call that beats the current best resets `patience`; each
/// non-improving call decrements it; the loop stops once `patience` reaches
/// zero. `patience` must be >= 1, matching the harness's own `BenchMeta`
/// contract (the macro never emits a patience of 0).
pub fn run_durable_min(mut f: impl FnMut(), patience: u32) -> DurableMinResult {
    let start = Instant::now();
    run_durable_min_with_clock(&mut f, patience, || start.elapsed().as_secs_f64())
}

/// Same algorithm, but with an injectable clock (seconds since some fixed
/// origin) so the stopping logic can be tested without real wall-clock time.
fn run_durable_min_with_clock(
    f: &mut impl FnMut(),
    patience: u32,
    mut clock: impl FnMut() -> f64,
) -> DurableMinResult {
    assert!(patience >= 1, "patience must be >= 1, got {patience}");

    let mut best: Option<f64> = None;
    let mut remaining = patience;
    let mut samples = Vec::new();

    loop {
        let t0 = clock();
        f();
        let duration = clock() - t0;
        samples.push(duration);

        match best {
            Some(current_best) if duration >= current_best => remaining -= 1,
            _ => {
                best = Some(duration);
                remaining = patience;
            }
        }

        if remaining == 0 {
            break;
        }
    }

    DurableMinResult {
        durable_min: best.expect("loop always records at least one sample"),
        samples,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    fn fake_clock(timestamps: &[f64]) -> impl FnMut() -> f64 + '_ {
        let mut queue: VecDeque<f64> = timestamps.iter().copied().collect();
        move || queue.pop_front().expect("fake clock exhausted")
    }

    #[test]
    fn stops_after_patience_non_improving_runs() {
        // durations: 5.0, 4.0, 3.0 (new best), 3.5, 3.2 -> patience=2 means
        // stop once two non-improving runs (3.5 then 3.2) follow the best.
        let timestamps = [
            0.0, 5.0, // 5.0
            5.0, 9.0, // 4.0
            9.0, 12.0, // 3.0 (new best)
            12.0, 15.5, // 3.5 (non-improving, remaining -> 1)
            15.5, 18.7, // 3.2 (non-improving, remaining -> 0, stop)
        ];
        let result = run_durable_min_with_clock(&mut || {}, 2, fake_clock(&timestamps));
        let expected = [5.0, 4.0, 3.0, 3.5, 3.2];
        assert_eq!(result.samples.len(), expected.len());
        for (got, want) in result.samples.iter().zip(expected) {
            assert!((got - want).abs() < 1e-9, "got {got}, want {want}");
        }
        assert_eq!(result.durable_min, 3.0);
    }

    #[test]
    fn patience_resets_on_each_new_best() {
        // 5.0, 4.0 (better), 3.0 (better), 3.0 (tie -> non-improving) with
        // patience=1 stops right after the tie.
        let timestamps = [0.0, 5.0, 5.0, 9.0, 9.0, 12.0, 12.0, 15.0];
        let result = run_durable_min_with_clock(&mut || {}, 1, fake_clock(&timestamps));
        assert_eq!(result.samples, vec![5.0, 4.0, 3.0, 3.0]);
        assert_eq!(result.durable_min, 3.0);
    }

    #[test]
    fn single_patience_stops_after_first_non_improvement() {
        // Uses the fake clock (not real wall-clock timing) since a trivial
        // closure's real duration is dominated by measurement noise, which
        // would make "does call 2 improve on call 1" nondeterministic.
        let timestamps = [0.0, 3.0, 3.0, 8.0]; // durations: 3.0 (best), 5.0 (non-improving)
        let mut call_count = 0;
        let result =
            run_durable_min_with_clock(&mut || call_count += 1, 1, fake_clock(&timestamps));
        assert_eq!(
            result.samples.len(),
            2,
            "patience=1: one run, then one non-improving run, then stop"
        );
        assert_eq!(call_count, 2);
    }

    #[test]
    #[should_panic(expected = "patience must be >= 1")]
    fn rejects_zero_patience() {
        run_durable_min(|| {}, 0);
    }

    #[test]
    fn durable_min_is_the_minimum_sample() {
        let result = run_durable_min(|| {}, 3);
        let true_min = result.samples.iter().copied().fold(f64::INFINITY, f64::min);
        assert_eq!(result.durable_min, true_min);
    }
}
