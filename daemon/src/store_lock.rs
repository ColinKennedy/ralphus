//! RAL-393: an instrumented, eventually-fair replacement for the daemon's
//! single `Arc<Mutex<Store>>`.
//!
//! `std::sync::Mutex` makes no fairness guarantee: on contention, an unlocked
//! mutex is up for grabs to whichever thread's `lock()` next reaches the OS
//! futex/condvar, which is not necessarily the thread that has been waiting
//! longest. Under this daemon's real load -- a scheduler thread and several
//! guardian-merge workers reacquiring the store lock in a tight loop --
//! already-parked HTTP read-pool workers were repeatedly "barged" past,
//! turning a `SELECT COUNT(*)` (`GET /api/daemon`) into a multi-second stall
//! (see the RAL-393 ticket for measurements). `parking_lot::Mutex` implements
//! "eventual fairness": once a thread has been waiting past a short internal
//! threshold, the next unlock hands the lock directly to it instead of
//! letting a fresh `lock()` call barge in. That alone removes the starvation
//! this ticket is about; [`StoreMutex`] additionally times every acquisition
//! so lock-wait is visible (p50/p95/max) separately from handler execution
//! time, per the ticket's Stage 1.
//!
//! `parking_lot::Mutex::lock()` cannot be poisoned and returns the guard
//! directly (no `Result`) -- see `RAL-393-AUDIT.md` for the audit of every
//! call site that used to `.expect(...)` a poison result.

use std::cell::Cell;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::store::Store;

/// A `Store` behind an eventually-fair, timed mutex. See the module doc
/// comment.
pub struct StoreMutex(parking_lot::Mutex<Store>);

impl StoreMutex {
    #[must_use]
    pub fn new(store: Store) -> Self {
        Self(parking_lot::Mutex::new(store))
    }

    /// Acquire the store lock, recording how long this call waited into both
    /// the process-lifetime histogram ([`store_lock_wait_snapshot`]) and the
    /// current thread's per-request accumulator ([`take_request_lock_wait_ms`]).
    pub fn lock(&self) -> StoreGuard<'_> {
        let start = Instant::now();
        let guard = self.0.lock();
        record_wait(start.elapsed());
        guard
    }
}

/// A held store lock. `parking_lot::MutexGuard` derefs to `&Store`/`&mut
/// Store` exactly like `std::sync::MutexGuard` did, just without the
/// `Result` wrapper `std::sync::Mutex::lock()` returned.
pub type StoreGuard<'a> = parking_lot::MutexGuard<'a, Store>;

/// The daemon's one shared handle to its `Store`, cloned into every
/// background worker and the HTTP layer alike.
pub type StoreHandle = std::sync::Arc<StoreMutex>;

fn micros(d: Duration) -> u64 {
    u64::try_from(d.as_micros()).unwrap_or(u64::MAX)
}

fn record_wait(waited: Duration) {
    STORE_LOCK_HISTOGRAM.record(waited);
    REQUEST_LOCK_WAIT_US.with(|acc| acc.set(acc.get().saturating_add(micros(waited))));
}

thread_local! {
    /// Sum of this thread's store-lock wait time since the last
    /// [`reset_request_lock_wait`] call -- one HTTP request's worth, since
    /// each request runs start-to-finish on a single accept-loop or
    /// `ReadPool` worker thread (see `server.rs::answer_request`).
    static REQUEST_LOCK_WAIT_US: Cell<u64> = const { Cell::new(0) };
}

/// Zero this thread's per-request lock-wait accumulator. Call before running
/// a request's handler.
pub fn reset_request_lock_wait() {
    REQUEST_LOCK_WAIT_US.with(|c| c.set(0));
}

/// This thread's accumulated store-lock wait time (in ms) since the last
/// [`reset_request_lock_wait`], for logging alongside `handler_ms` -- see
/// `server.rs::answer_request`. `handler_ms` measures the whole handler;
/// this is the subset of it spent waiting for the store lock specifically,
/// which is what RAL-393 is about.
#[must_use]
pub fn take_request_lock_wait_ms() -> u128 {
    u128::from(REQUEST_LOCK_WAIT_US.with(Cell::get)) / 1000
}

/// Histogram bucket upper bounds, in microseconds. The last (implicit)
/// bucket is "greater than the last bound here". Chosen to resolve both the
/// sub-millisecond case (an uncontended lock) and the multi-second case the
/// RAL-393 baseline measured (0.38s-4.2s).
const BUCKET_BOUNDS_US: &[u64] = &[
    200, 1_000, 5_000, 10_000, 25_000, 50_000, 100_000, 250_000, 500_000, 1_000_000, 2_000_000,
    5_000_000,
];

struct LockWaitHistogram {
    /// One counter per bound in `BUCKET_BOUNDS_US`, plus one overflow bucket.
    buckets: Vec<AtomicU64>,
    max_us: AtomicU64,
}

impl LockWaitHistogram {
    fn new() -> Self {
        Self {
            buckets: (0..=BUCKET_BOUNDS_US.len())
                .map(|_| AtomicU64::new(0))
                .collect(),
            max_us: AtomicU64::new(0),
        }
    }

    fn record(&self, waited: Duration) {
        let us = micros(waited);
        let idx = BUCKET_BOUNDS_US
            .iter()
            .position(|&bound| us <= bound)
            .unwrap_or(BUCKET_BOUNDS_US.len());
        self.buckets[idx].fetch_add(1, Ordering::Relaxed);
        self.max_us.fetch_max(us, Ordering::Relaxed);
    }

    /// Estimate the wait time (in microseconds) below which `fraction` of
    /// recorded samples fall, by walking the cumulative histogram -- exact
    /// per-bucket boundary resolution, not full precision, which is enough
    /// for an operational p50/p95.
    fn percentile_us(&self, fraction: f64) -> u64 {
        let total: u64 = self.buckets.iter().map(|b| b.load(Ordering::Relaxed)).sum();
        if total == 0 {
            return 0;
        }
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let target = ((total as f64) * fraction).ceil() as u64;
        let mut cumulative = 0u64;
        for (idx, bucket) in self.buckets.iter().enumerate() {
            cumulative += bucket.load(Ordering::Relaxed);
            if cumulative >= target.max(1) {
                return BUCKET_BOUNDS_US
                    .get(idx)
                    .copied()
                    .unwrap_or_else(|| self.max_us.load(Ordering::Relaxed));
            }
        }
        self.max_us.load(Ordering::Relaxed)
    }

    fn snapshot(&self) -> LockWaitSnapshot {
        let samples: u64 = self.buckets.iter().map(|b| b.load(Ordering::Relaxed)).sum();
        LockWaitSnapshot {
            samples,
            p50_ms: us_to_ms(self.percentile_us(0.50)),
            p95_ms: us_to_ms(self.percentile_us(0.95)),
            max_ms: us_to_ms(self.max_us.load(Ordering::Relaxed)),
        }
    }
}

fn us_to_ms(us: u64) -> f64 {
    (us as f64) / 1000.0
}

static STORE_LOCK_HISTOGRAM: LazyLock<LockWaitHistogram> = LazyLock::new(LockWaitHistogram::new);

/// Process-lifetime store-lock acquisition wait stats, surfaced by `GET
/// /api/daemon` -- see `server.rs::health`. Distinct from `handler_ms`
/// (which folds lock-wait and query time together): this is only the time
/// spent blocked on [`StoreMutex::lock`].
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
pub struct LockWaitSnapshot {
    pub samples: u64,
    pub p50_ms: f64,
    pub p95_ms: f64,
    pub max_ms: f64,
}

/// Snapshot of the process-lifetime store-lock wait histogram.
#[must_use]
pub fn store_lock_wait_snapshot() -> LockWaitSnapshot {
    STORE_LOCK_HISTOGRAM.snapshot()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_histogram_reports_zero() {
        let h = LockWaitHistogram::new();
        let s = h.snapshot();
        assert_eq!(s.samples, 0);
        assert_eq!(s.p50_ms, 0.0);
        assert_eq!(s.p95_ms, 0.0);
        assert_eq!(s.max_ms, 0.0);
    }

    #[test]
    fn percentiles_track_recorded_samples() {
        let h = LockWaitHistogram::new();
        for _ in 0..99 {
            h.record(Duration::from_micros(500));
        }
        h.record(Duration::from_millis(3000));
        let s = h.snapshot();
        assert_eq!(s.samples, 100);
        // `percentile_us` reports a bucket's upper bound, not the sample
        // value itself, so a 500us sample landing in the "<=1000us" bucket
        // reads back as exactly 1.0ms -- `<=`, not `<`.
        assert!(
            s.p50_ms <= 1.0,
            "p50 should stay in the sub-ms bucket: {s:?}"
        );
        assert!(
            s.p95_ms <= 1.0,
            "p95 should still be in the sub-ms bucket at 99/100: {s:?}"
        );
        assert!(
            (s.max_ms - 3000.0).abs() < 1.0,
            "max should reflect the one 3s outlier exactly: {s:?}"
        );
    }

    #[test]
    fn request_accumulator_resets_between_requests() {
        reset_request_lock_wait();
        record_wait(Duration::from_millis(5));
        record_wait(Duration::from_millis(7));
        assert_eq!(take_request_lock_wait_ms(), 12);
        // `take_request_lock_wait_ms` reads via `Cell::get` without
        // resetting -- `answer_request` calls `reset_request_lock_wait`
        // itself at the top of the next request instead, so a second read
        // without an intervening reset must still see the same value.
        assert_eq!(take_request_lock_wait_ms(), 12);
        reset_request_lock_wait();
        assert_eq!(take_request_lock_wait_ms(), 0);
    }

    #[test]
    fn lock_records_wait_time() {
        reset_request_lock_wait();
        let mutex = StoreMutex::new(Store::open_in_memory().expect("open store"));
        {
            let _guard = mutex.lock();
        }
        let snapshot = store_lock_wait_snapshot();
        assert!(snapshot.samples >= 1);
    }
}
