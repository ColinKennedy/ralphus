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
use std::panic::Location;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::store::Store;

/// RAL-<pending>: `file:line` and acquisition time of whoever currently
/// holds (or most recently acquired) the store lock -- guarded by its own
/// tiny, uncontended `parking_lot::Mutex`, distinct from the `Store`'s own
/// lock, so reading/writing it can never itself wait on the thing it is
/// diagnosing. Written on every [`StoreMutex::lock`] acquisition; read only
/// when a wait is suspiciously long, so a stuck holder gets logged even if
/// nobody is watching a debugger at the time.
#[derive(Clone, Copy)]
struct HolderInfo {
    file: &'static str,
    line: u32,
    acquired_at_ms: i64,
}

static HOLDER: LazyLock<parking_lot::Mutex<Option<HolderInfo>>> =
    LazyLock::new(|| parking_lot::Mutex::new(None));

/// How long a single [`StoreMutex::lock`] wait must be before it's worth
/// logging who was holding the lock while this call waited -- short waits are
/// normal contention noise (see the module doc comment), this is only meant
/// to catch the pathological case.
const SLOW_WAIT_LOG_THRESHOLD: Duration = Duration::from_secs(2);

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
    ///
    /// RAL-<pending>: also updates the [`HOLDER`] breadcrumb and, when this
    /// call had to wait unusually long, logs the *previous* holder's call
    /// site and how long it had held the lock -- diagnostic aid for tracking
    /// down a stuck holder without needing to catch a live hang under a
    /// debugger.
    #[track_caller]
    pub fn lock(&self) -> StoreGuard<'_> {
        let start = Instant::now();
        let guard = self.0.lock();
        let waited = start.elapsed();
        record_wait(waited);
        let loc = Location::caller();
        let now = crate::store::now_ms();
        {
            let mut holder = HOLDER.lock();
            if waited >= SLOW_WAIT_LOG_THRESHOLD {
                if let Some(prev) = *holder {
                    let held_ms = now.saturating_sub(prev.acquired_at_ms);
                    crate::rlog!(
                        WARNING,
                        "ralphus [store_lock] waited {}ms for the store lock; previously acquired at {}:{} ({held_ms}ms ago)",
                        waited.as_millis(),
                        prev.file,
                        prev.line,
                    );
                    crate::cartographer::Note::new("store_lock")
                        .level(crate::logging::LogLevel::WARNING)
                        .emit(
                            &guard,
                            "slow store lock acquisition",
                            serde_json::json!({
                                "waited_ms": waited.as_millis() as u64,
                                "previous_holder": format!("{}:{}", prev.file, prev.line),
                                "previous_holder_held_ms": held_ms,
                            }),
                        );
                }
            }
            *holder = Some(HolderInfo {
                file: loc.file(),
                line: loc.line(),
                acquired_at_ms: now,
            });
        }
        StoreGuard::new(guard, loc)
    }

    /// Try to acquire the store lock, giving up after `timeout`.
    ///
    /// WS-G.2: this is how the watchdog asks "is the store reachable?" without
    /// becoming the next thread stuck behind whatever is holding it. A plain
    /// `lock()` in a liveness checker would itself park forever on exactly the
    /// deadlock it exists to report.
    ///
    /// Deliberately does not record a wait sample: the watchdog polls on a
    /// timer rather than because it has work to do, so folding its waits into
    /// the histogram would report contention that no request experienced.
    #[track_caller]
    pub fn try_lock_for(&self, timeout: Duration) -> Option<StoreGuard<'_>> {
        let guard = self.0.try_lock_for(timeout)?;
        Some(StoreGuard::new(guard, Location::caller()))
    }

    /// The store's non-database state (WS-E.1), reached **without** acquiring
    /// the store lock.
    ///
    /// `StoreMemory` has its own small locks, so nothing here needs the global
    /// one -- worktree leases, tmux liveness, stall debounce and the secret-name
    /// cache are not database state and never were. Several of the callers are
    /// hot (`note_live_activity` fires on every observed pane growth for every
    /// running cell, `check_stall_escalation` on every stall poll), and making
    /// them queue behind the scheduler and the guardian-merge workers was pure
    /// cost.
    ///
    /// Briefly takes `self.0` to clone the `Arc` out, so it is not literally
    /// lock-free at the instant of the call; the name is about what the
    /// *returned* handle costs to use. Hold the result rather than calling this
    /// repeatedly in a loop.
    #[must_use]
    pub fn lock_free_memory(&self) -> std::sync::Arc<crate::store_memory::StoreMemory> {
        self.0.lock().memory()
    }
}

/// Who holds (or last acquired) the store lock, and for how long: a
/// `("file:line", held_ms)` pair, or `None` if the lock has never been taken.
///
/// Read through the [`HOLDER`] breadcrumb's own tiny mutex, never through the
/// store lock, so this stays answerable precisely when the store lock is not
/// -- which is the only time anyone asks. This is the single most useful fact
/// about a wedged daemon, and the reason the captured deadlock took static
/// analysis to diagnose is that nothing surfaced it at the time.
#[must_use]
pub fn store_lock_holder() -> Option<(String, i64)> {
    let holder = HOLDER.lock();
    holder.map(|info| {
        (
            format!("{}:{}", info.file, info.line),
            crate::store::now_ms().saturating_sub(info.acquired_at_ms),
        )
    })
}

/// A held store lock, instrumented by the WS-B.3 guard watchdog: on drop,
/// how long the guard was held is checked against the watchdog threshold --
/// a hold that long means blocking work ran under the daemon's one global
/// lock (an I/O call reached through layers of delegation is exactly what a
/// source-level lint cannot see). Over the threshold this panics in dev/test
/// builds and logs a WARNING plus a Cartographer row in release.
/// Derefs to `&Store`/`&mut Store` exactly like the raw `MutexGuard` it wraps.
pub struct StoreGuard<'a> {
    inner: parking_lot::MutexGuard<'a, Store>,
    acquired_at: Instant,
    site: &'static Location<'static>,
}

impl<'a> StoreGuard<'a> {
    fn new(inner: parking_lot::MutexGuard<'a, Store>, site: &'static Location<'static>) -> Self {
        Self {
            inner,
            acquired_at: Instant::now(),
            site,
        }
    }
}

impl std::ops::Deref for StoreGuard<'_> {
    type Target = Store;
    fn deref(&self) -> &Store {
        &self.inner
    }
}

impl std::ops::DerefMut for StoreGuard<'_> {
    fn deref_mut(&mut self) -> &mut Store {
        &mut self.inner
    }
}

/// Guard holds at or above this many milliseconds are watchdog-worthy
/// (log + Cartographer row, and a panic in dev/test builds).
const GUARD_HOLD_WARN_MS: u128 = 100;

/// Panic threshold for the guard watchdog, in milliseconds; `0` disables
/// panicking (release builds log instead). Overridable via
/// `RALPHUS_GUARD_HOLD_PANIC_MS` (set it very large to disable panicking).
///
/// The thresholds are deliberately far above [`GUARD_HOLD_WARN_MS`], which is
/// where *reporting* starts. Two reasons.
///
/// A debug build is several times slower than a release one at the same work,
/// so a 100 ms hold in a debug build is not evidence of the thing this watchdog
/// exists to catch. What it is evidence of is a query being slow, which the
/// warning already reports and which the aggregate gates in
/// `daemon/tests/board_contention.rs` and `workload_replay.rs` measure properly
/// (store-lock wait p95 under 50 ms, over a real workload).
///
/// And `cfg!(test)` is true only for this crate's own unit tests. An
/// *integration* test binary links the library compiled normally, so it takes
/// the non-test branch -- which is how a 100 ms default came to fail
/// `daemon/tests/guardian_merge.rs` wholesale on a single 133 ms
/// `get_guardian`, in a debug build, against real git fixtures. Worse, the same
/// branch applies to `scripts/build-debug.sh`'s daemon: a developer's daemon
/// would panic on any 101 ms hold.
///
/// What is being caught is blocking I/O under the lock -- a subprocess, a
/// network round-trip, a sleep -- and those are seconds, not milliseconds. The
/// thresholds below are sized to that, so the panic means what it says.
fn guard_hold_panic_ms() -> u128 {
    let default = if cfg!(test) {
        5_000
    } else if cfg!(debug_assertions) {
        2_000
    } else {
        0
    };
    std::env::var("RALPHUS_GUARD_HOLD_PANIC_MS")
        .ok()
        .and_then(|v| v.parse::<u128>().ok())
        .unwrap_or(default)
}

/// Longest guard hold, in ms, over the process lifetime.
static GUARD_HOLD_MAX_MS: AtomicU64 = AtomicU64::new(0);
/// Holds that reached [`GUARD_HOLD_WARN_MS`], over the process lifetime.
static GUARD_HOLD_WARN_COUNT: AtomicU64 = AtomicU64::new(0);

/// Guard-hold statistics, reported by `GET /api/daemon`.
///
/// The plan's M5 target is a maximum hold under 100 ms. Before this the only
/// record of a long hold was a log line and a Cartographer row, which makes the
/// target something you grep for rather than something a test can assert. These
/// counters make it a number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct GuardHoldSnapshot {
    /// Longest hold observed, in ms.
    pub max_ms: u64,
    /// How many holds reached `warn_threshold_ms`.
    pub over_threshold: u64,
    /// The threshold the count is against.
    pub warn_threshold_ms: u64,
}

/// Snapshot of the process-lifetime guard-hold counters.
#[must_use]
pub fn guard_hold_snapshot() -> GuardHoldSnapshot {
    GuardHoldSnapshot {
        max_ms: GUARD_HOLD_MAX_MS.load(Ordering::Relaxed),
        over_threshold: GUARD_HOLD_WARN_COUNT.load(Ordering::Relaxed),
        warn_threshold_ms: GUARD_HOLD_WARN_MS as u64,
    }
}

impl Drop for StoreGuard<'_> {
    fn drop(&mut self) {
        let held_ms = self.acquired_at.elapsed().as_millis();
        // Recorded for every hold, not just the long ones: the maximum is only
        // meaningful if nothing is excluded from it.
        GUARD_HOLD_MAX_MS.fetch_max(
            u64::try_from(held_ms).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
        if held_ms < GUARD_HOLD_WARN_MS {
            return;
        }
        GUARD_HOLD_WARN_COUNT.fetch_add(1, Ordering::Relaxed);
        let detail = format!(
            "store guard held for {held_ms}ms (acquired at {}:{})",
            self.site.file(),
            self.site.line()
        );
        let panic_ms = guard_hold_panic_ms();
        if panic_ms > 0 && held_ms >= panic_ms {
            panic!(
                "{detail} -- the daemon's one global store lock must never be \
                 held this long; something under the guard is blocking (I/O, a \
                 subprocess, or a sleep). Drop the guard before the blocking \
                 work, or justify it with an `allow-lock-io:` comment for the \
                 source lint in daemon/tests/store_lock_reentrancy.rs"
            );
        }
        crate::rlog!(
            WARNING,
            "ralphus [store_lock] {detail} -- blocking work ran under the store lock"
        );
        crate::cartographer::Note::new("store_lock")
            .level(crate::logging::LogLevel::WARNING)
            .emit(
                &self.inner,
                "long store guard hold",
                serde_json::json!({
                    "held_ms": held_ms as u64,
                    "acquired_at": format!("{}:{}", self.site.file(), self.site.line()),
                }),
            );
    }
}

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
