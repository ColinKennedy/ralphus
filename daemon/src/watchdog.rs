//! WS-G.1/G.2/G.3: a liveness watchdog for the daemon itself.
//!
//! # Why this exists
//!
//! Nothing watched the daemon. `cpu_stall.rs` recognizes a hung *cell or proof
//! subprocess*; `health_sweep.rs` probes agent binaries on PATH. Neither
//! notices the daemon deadlocking, and when it did, the failure was silent:
//! 58 threads all in `Wait`, zero CPU over repeated samples, no outbound
//! sockets, 18 client connections abandoned in `CloseWait`, and no log line
//! saying anything was wrong. Diagnosing it took static analysis after the
//! fact, because the one thing worth knowing -- who was holding the store lock
//! -- was never reported while it was happening.
//!
//! # What it checks
//!
//! Specifically that **the store lock is obtainable**, not that the process is
//! alive. A heartbeat thread that merely ticks a counter would have kept
//! ticking happily through the captured deadlock: the process was running, it
//! just could not do anything. So each round calls
//! [`StoreMutex::try_lock_for`], which gives up rather than joining the queue
//! of threads already stuck behind the holder.
//!
//! # What it reports
//!
//! When a round fails, the holder breadcrumb
//! ([`crate::store_lock::store_lock_holder`]) names the `file:line` that took
//! the lock and how long ago -- which for the captured hang would have pointed
//! straight at the re-entrant `guardian_cancel`/`guardian_approve` pair instead
//! of leaving it to be inferred.
//!
//! Reporting is **stderr-only** (`rlog!`) while a stall is in progress. Writing
//! a Cartographer row needs the store lock, so a watchdog that tried would
//! block on the very thing it is reporting and go silent exactly when it
//! matters. Once the store becomes reachable again, the recovery *is* recorded
//! as a Cartographer row, since by then there is a working lock to write it
//! with.
//!
//! Full thread backtraces are not dumped: Rust has no safe portable way to walk
//! another thread's stack, and `unsafe_code = "forbid"` at the workspace level
//! rules out the alternatives. The holder's call site plus its hold duration is
//! what the plan's G.3 asks for as the minimum, and it is the part that
//! identifies the bug.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::time::Duration;

use crate::store_lock::StoreHandle;

/// How often the watchdog checks that the store lock is reachable.
///
/// Frequent enough to satisfy the plan's M18 target (a hang detected in under
/// 60 s), rare enough that the check itself is not a contender for the lock it
/// is sampling.
pub const CHECK_INTERVAL: Duration = Duration::from_secs(10);

/// How long a single check waits for the lock before calling the store stalled.
///
/// This is not "how long a hold may last" -- the WS-B.3 guard watchdog owns
/// that, at 100 ms. This is deliberately far longer, because a *legitimately*
/// busy daemon can make a 10-second-old acquisition wait a while, and a
/// liveness alarm that cries wolf gets ignored. Five seconds of not being able
/// to reach the store at all is not busy; it is wedged.
pub const STALL_TIMEOUT: Duration = Duration::from_secs(5);

/// Shared watchdog state, readable by `GET /api/daemon` (WS-G.4).
///
/// Atomics rather than a mutex: this is read by HTTP handlers and written by
/// the watchdog thread, and putting a lock around the liveness signal of a
/// daemon whose failure mode is lock contention would be a poor choice.
#[derive(Debug, Default)]
pub struct WatchdogState {
    /// Unix-epoch ms of the last round that reached the store. `0` before the
    /// first round completes.
    last_ok_at_ms: AtomicI64,
    /// Longest wait, in ms, that any round has needed to reach the store.
    worst_wait_ms: AtomicU64,
    /// Rounds that timed out since the last successful one.
    consecutive_stalls: AtomicU64,
    /// Whether the most recent round failed to reach the store.
    stalled: AtomicBool,
    /// Total rounds that have ever timed out, so a resolved stall still leaves
    /// a trace in the health response rather than vanishing on recovery.
    total_stalls: AtomicU64,
}

/// A point-in-time view of [`WatchdogState`], for serializing into the health
/// response.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct WatchdogSnapshot {
    /// How long ago the store was last reachable, in ms. `None` before the
    /// first round has run.
    pub last_ok_age_ms: Option<i64>,
    /// Whether the most recent check could not reach the store.
    pub stalled: bool,
    /// Consecutive failed checks; `0` whenever `stalled` is false.
    pub consecutive_stalls: u64,
    /// Failed checks over the whole process lifetime.
    pub total_stalls: u64,
    /// Longest observed wait to reach the store, in ms.
    pub worst_wait_ms: u64,
}

impl WatchdogState {
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Records a round that reached the store after waiting `waited`.
    fn record_ok(&self, waited: Duration) {
        self.last_ok_at_ms
            .store(crate::store::now_ms(), Ordering::Relaxed);
        self.stalled.store(false, Ordering::Relaxed);
        self.consecutive_stalls.store(0, Ordering::Relaxed);
        let waited_ms = u64::try_from(waited.as_millis()).unwrap_or(u64::MAX);
        self.worst_wait_ms.fetch_max(waited_ms, Ordering::Relaxed);
    }

    /// Records a round that gave up. Returns the new consecutive-stall count.
    fn record_stall(&self) -> u64 {
        self.stalled.store(true, Ordering::Relaxed);
        self.total_stalls.fetch_add(1, Ordering::Relaxed);
        let waited_ms = u64::try_from(STALL_TIMEOUT.as_millis()).unwrap_or(u64::MAX);
        self.worst_wait_ms.fetch_max(waited_ms, Ordering::Relaxed);
        self.consecutive_stalls.fetch_add(1, Ordering::Relaxed) + 1
    }

    #[must_use]
    pub fn snapshot(&self) -> WatchdogSnapshot {
        let last_ok = self.last_ok_at_ms.load(Ordering::Relaxed);
        WatchdogSnapshot {
            last_ok_age_ms: (last_ok > 0).then(|| crate::store::now_ms().saturating_sub(last_ok)),
            stalled: self.stalled.load(Ordering::Relaxed),
            consecutive_stalls: self.consecutive_stalls.load(Ordering::Relaxed),
            total_stalls: self.total_stalls.load(Ordering::Relaxed),
            worst_wait_ms: self.worst_wait_ms.load(Ordering::Relaxed),
        }
    }
}

/// One watchdog round: try to reach the store, report either way.
///
/// Split out from the loop so it is testable directly against a `StoreHandle`
/// a test can deliberately wedge, with no thread and no waiting for a timer.
pub fn check_once(store: &StoreHandle, state: &WatchdogState, timeout: Duration) -> bool {
    let started = std::time::Instant::now();
    match store.try_lock_for(timeout) {
        Some(guard) => {
            let waited = started.elapsed();
            let recovering = state.stalled.load(Ordering::Relaxed);
            let stalls = state.consecutive_stalls.load(Ordering::Relaxed);
            if recovering {
                crate::rlog!(
                    WARNING,
                    "ralphus [watchdog] store lock reachable again after {stalls} failed check(s) (waited {}ms)",
                    waited.as_millis()
                );
                // Safe to write a row now: the lock is in hand, which is
                // precisely what was untrue while the stall was in progress.
                crate::cartographer::Note::new("watchdog")
                    .level(crate::logging::LogLevel::WARNING)
                    .emit(
                        &guard,
                        "store lock reachable again after a stall",
                        serde_json::json!({
                            "failed_checks": stalls,
                            "waited_ms": waited.as_millis() as u64,
                        }),
                    );
            }
            drop(guard);
            state.record_ok(waited);
            true
        }
        None => {
            let stalls = state.record_stall();
            // stderr only -- see the module doc comment. A Cartographer write
            // needs the lock this round just failed to get.
            match crate::store_lock::store_lock_holder() {
                Some((site, held_ms)) => crate::rlog!(
                    ERROR,
                    "ralphus [watchdog] store lock UNREACHABLE for {}ms ({stalls} consecutive failed check(s)); \
                     lock last acquired at {site}, held for {held_ms}ms",
                    timeout.as_millis()
                ),
                None => crate::rlog!(
                    ERROR,
                    "ralphus [watchdog] store lock UNREACHABLE for {}ms ({stalls} consecutive failed check(s)); \
                     no holder recorded",
                    timeout.as_millis()
                ),
            }
            false
        }
    }
}

/// Starts the watchdog thread. Returns the state the health endpoint reads.
///
/// The thread is intentionally not joined or shut down: it holds only an
/// `Arc<StoreMutex>` and an `Arc<WatchdogState>`, does no work between checks,
/// and the process exiting is the only thing that should stop liveness
/// monitoring.
pub fn spawn(store: StoreHandle, state: Arc<WatchdogState>) {
    std::thread::spawn(move || {
        // One immediate check, so a daemon that is already wedged at startup
        // says so now rather than after the first interval.
        check_once(&store, &state, STALL_TIMEOUT);
        loop {
            std::thread::sleep(CHECK_INTERVAL);
            check_once(&store, &state, STALL_TIMEOUT);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;
    use crate::store_lock::StoreMutex;

    fn handle() -> StoreHandle {
        Arc::new(StoreMutex::new(Store::open_in_memory().expect("store")))
    }

    #[test]
    fn a_reachable_store_records_a_successful_round() {
        let store = handle();
        let state = WatchdogState::new();
        assert!(check_once(&store, &state, STALL_TIMEOUT));

        let snap = state.snapshot();
        assert!(!snap.stalled);
        assert_eq!(snap.consecutive_stalls, 0);
        assert_eq!(snap.total_stalls, 0);
        assert!(
            snap.last_ok_age_ms.is_some(),
            "a successful round must stamp last_ok"
        );
    }

    #[test]
    fn snapshot_reports_no_age_before_the_first_round() {
        let state = WatchdogState::new();
        assert_eq!(state.snapshot().last_ok_age_ms, None);
    }

    /// The case this module exists for: the lock is held and not coming back.
    ///
    /// Uses a short timeout so the test is fast; what is being asserted is that
    /// the watchdog gives up and reports rather than parking behind the holder,
    /// which is the difference between a watchdog and a 59th stuck thread.
    #[test]
    fn a_wedged_store_is_reported_and_does_not_block_the_watchdog() {
        let store = handle();
        let state = WatchdogState::new();
        let held = store.lock();

        let started = std::time::Instant::now();
        let ok = check_once(&store, &state, Duration::from_millis(50));
        let elapsed = started.elapsed();

        assert!(!ok, "the watchdog claimed a wedged store was reachable");
        assert!(
            elapsed < Duration::from_secs(2),
            "the watchdog blocked for {elapsed:?} instead of giving up on its timeout"
        );

        let snap = state.snapshot();
        assert!(snap.stalled);
        assert_eq!(snap.consecutive_stalls, 1);
        assert_eq!(snap.total_stalls, 1);

        // The breadcrumb must name a holder, or a real stall report would say
        // "no holder recorded" and be useless.
        let holder = crate::store_lock::store_lock_holder();
        assert!(holder.is_some(), "no holder breadcrumb for a held lock");

        drop(held);
    }

    #[test]
    fn consecutive_stalls_accumulate_and_reset_on_recovery() {
        let store = handle();
        let state = WatchdogState::new();
        {
            let _held = store.lock();
            for expected in 1..=3 {
                assert!(!check_once(&store, &state, Duration::from_millis(20)));
                assert_eq!(state.snapshot().consecutive_stalls, expected);
            }
        }

        assert!(check_once(&store, &state, STALL_TIMEOUT));
        let snap = state.snapshot();
        assert!(!snap.stalled, "recovery did not clear the stalled flag");
        assert_eq!(snap.consecutive_stalls, 0);
        // The total is kept: a resolved stall still happened, and a health
        // response that forgets it hides the incident entirely.
        assert_eq!(snap.total_stalls, 3);
    }
}
