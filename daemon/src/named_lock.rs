//! Generic string-keyed read/write locks for cross-subsystem worktree
//! ownership (RAL-387).
//!
//! [`Cancellations`](crate::cancel::Cancellations) is a *signal*: it tells a
//! worker "please stop soon," but nothing stops a second worker from starting
//! against the same guardian's worktrees while the first is still cleaning up
//! after being asked to stop. [`NamedLocks`] is a real lock: acquiring it
//! blocks until no conflicting holder remains, so "at most one worker owns
//! this key's worktrees at a time" is structural rather than best-effort.
//!
//! Two keyings are used by `guardian_merge`:
//! - `{guardian_id}` -- a merge/rebase worker holds this for **write** for its
//!   entire run (it may touch every branch's worktree); a feedback worker
//!   holds it for **read** (multiple feedback runs, one per project, may
//!   proceed concurrently, but none may proceed while a merge holds the
//!   writer side, and a merge cannot start doing real work until every
//!   in-flight feedback read-guard has been released).
//! - `{guardian_id}:{project}` -- a feedback worker holds this for **write**
//!   for the duration of one `run_feedback` call, serializing successive
//!   feedback within the same project (a feedback pass restacks every
//!   downstream branch in its project, so two feedback runs in the same
//!   project can never safely proceed concurrently even if they target
//!   different branches).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Instant;

/// A store lock wait at or above which acquisition logs a `WARNING` instead
/// of a `DEBUG` line -- mirrors `guardian_merge::KICKOFF_SLOW_LOCK_MS`.
const SLOW_ACQUIRE_MS: u128 = 250;

/// Registry of named `RwLock`s, created lazily per key and never removed
/// (bounded by the number of guardians/projects ever seen -- negligible).
#[derive(Clone, Default)]
pub struct NamedLocks(Arc<Mutex<HashMap<String, Arc<RwLock<()>>>>>);

impl NamedLocks {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn entry(&self, key: &str) -> Arc<RwLock<()>> {
        let mut map = self.0.lock().expect("named lock registry poisoned");
        Arc::clone(
            map.entry(key.to_string())
                .or_insert_with(|| Arc::new(RwLock::new(()))),
        )
    }

    /// Run `f` while holding `key`'s writer lock (exclusive of every reader
    /// and every other writer). Blocks until acquired.
    pub fn with_write<R>(&self, key: &str, what: &str, f: impl FnOnce() -> R) -> R {
        let lock = self.entry(key);
        let started = Instant::now();
        let _guard = lock.write().expect("named lock poisoned");
        log_slow_acquire(key, what, "write", started);
        f()
    }

    /// Run `f` while holding `key`'s reader lock (exclusive of any writer,
    /// concurrent with every other reader). Blocks until acquired.
    pub fn with_read<R>(&self, key: &str, what: &str, f: impl FnOnce() -> R) -> R {
        let lock = self.entry(key);
        let started = Instant::now();
        let _guard = lock.read().expect("named lock poisoned");
        log_slow_acquire(key, what, "read", started);
        f()
    }
}

fn log_slow_acquire(key: &str, what: &str, mode: &str, started: Instant) {
    let waited_ms = started.elapsed().as_millis();
    if waited_ms >= SLOW_ACQUIRE_MS {
        crate::rlog!(
            WARNING,
            "ralphus [guardian] {what} waited {waited_ms}ms for the {mode} lock on {key} -- \
             another worker owned this review's worktrees for a while; if this recurs check \
             for overlapping merge/feedback ownership"
        );
    } else {
        crate::rlog!(
            DEBUG,
            "ralphus [guardian] {what} acquired the {mode} lock on {key} after {waited_ms}ms"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    #[test]
    fn write_excludes_write_on_the_same_key() {
        let locks = NamedLocks::new();
        let entered = Arc::new(AtomicBool::new(false));
        std::thread::scope(|s| {
            locks.with_write("g1", "test", || {
                let entered_bg = Arc::clone(&entered);
                let locks_bg = locks.clone();
                s.spawn(move || {
                    locks_bg.with_write("g1", "test", || {
                        entered_bg.store(true, Ordering::SeqCst);
                    });
                });
                std::thread::sleep(Duration::from_millis(30));
                assert!(!entered.load(Ordering::SeqCst));
            });
        });
        assert!(entered.load(Ordering::SeqCst));
    }

    #[test]
    fn read_excludes_write_and_write_excludes_read() {
        let locks = NamedLocks::new();
        let writer_entered = Arc::new(AtomicBool::new(false));
        std::thread::scope(|s| {
            locks.with_read("g1", "test", || {
                let writer_entered_bg = Arc::clone(&writer_entered);
                let locks_bg = locks.clone();
                s.spawn(move || {
                    locks_bg.with_write("g1", "test", || {
                        writer_entered_bg.store(true, Ordering::SeqCst);
                    });
                });
                std::thread::sleep(Duration::from_millis(30));
                assert!(!writer_entered.load(Ordering::SeqCst));
            });
        });
        assert!(writer_entered.load(Ordering::SeqCst));
    }

    #[test]
    fn read_does_not_exclude_read_on_the_same_key() {
        let locks = NamedLocks::new();
        let both_entered = Arc::new(AtomicBool::new(false));
        std::thread::scope(|s| {
            locks.with_read("g1", "test", || {
                let both_entered = Arc::clone(&both_entered);
                let locks = locks.clone();
                let handle = s.spawn(move || {
                    locks.with_read("g1", "test", || {
                        both_entered.store(true, Ordering::SeqCst);
                    });
                });
                // A second reader must not block behind the first.
                handle.join().unwrap();
            });
        });
        assert!(both_entered.load(Ordering::SeqCst));
    }

    #[test]
    fn different_keys_do_not_exclude_each_other() {
        let locks = NamedLocks::new();
        let entered = Arc::new(AtomicBool::new(false));
        std::thread::scope(|s| {
            locks.with_write("g1", "test", || {
                let entered = Arc::clone(&entered);
                let locks = locks.clone();
                let handle = s.spawn(move || {
                    locks.with_write("g2", "test", || {
                        entered.store(true, Ordering::SeqCst);
                    });
                });
                handle.join().unwrap();
            });
        });
        assert!(entered.load(Ordering::SeqCst));
    }
}
