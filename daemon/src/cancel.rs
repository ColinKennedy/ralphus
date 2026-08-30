//! Cooperative cancellation for in-flight runs.
//!
//! When a user cancels a run, two things must happen: the run's board state must
//! flip to `cancelled` (handled in [`crate::store::Store::cancel`]), and the
//! worker thread executing it — including any subprocess it has spawned — must
//! actually stop. This module provides the shared signal for the latter.
//!
//! The API thread ([`crate::server`]) and the scheduler's worker threads share a
//! [`Cancellations`] registry. A worker [`register`](Cancellations::register)s a
//! token when it starts a run and [`remove`](Cancellations::remove)s it when the
//! run finishes; the API [`cancel`](Cancellations::cancel)s by run id. The
//! [`Runner`](crate::runner::Runner) polls the token and kills its child when it
//! trips.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// A per-run cancellation flag, cheap to clone (shared via `Arc`).
#[derive(Clone, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    /// A fresh, un-cancelled token.
    #[must_use]
    pub fn new() -> Self {
        Self(Arc::new(AtomicBool::new(false)))
    }

    /// A token that can never be cancelled — for callers with no run to cancel
    /// (tests, guardian merges).
    #[must_use]
    pub fn never() -> Self {
        Self::new()
    }

    /// Request cancellation.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    /// Whether cancellation has been requested.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// Registry of the cancel tokens of runs that are currently executing.
#[derive(Clone, Default)]
pub struct Cancellations(Arc<Mutex<HashMap<String, CancelToken>>>);

impl Cancellations {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a fresh token for a run that is about to execute, returning it
    /// for the worker to poll. Replaces any stale token for the same id.
    #[must_use]
    pub fn register(&self, run_id: &str) -> CancelToken {
        let token = CancelToken::new();
        self.lock().insert(run_id.to_string(), token.clone());
        token
    }

    /// Signal cancellation for a run if it is currently executing. A no-op when
    /// the run is not running (nothing to stop).
    pub fn cancel(&self, run_id: &str) {
        if let Some(token) = self.lock().get(run_id) {
            token.cancel();
        }
    }

    /// Drop a run's token once it has finished executing.
    pub fn remove(&self, run_id: &str) {
        self.lock().remove(run_id);
    }

    /// Signal cancellation for every currently-registered run, regardless of
    /// id. Used by daemon-wide shutdown (`ralphus-daemon stop`) to stop every
    /// live worker's subprocess without having to enumerate run ids itself.
    pub fn cancel_all(&self) {
        for token in self.lock().values() {
            token.cancel();
        }
    }

    /// Whether `run_id` currently has a registered token — i.e. a worker
    /// thread is actively executing it (registered at the start of
    /// `execute_run_inner`, removed only once that call returns). Used to
    /// wait out a still-in-flight worker before a restart re-claims the run;
    /// see `server::restart_run`'s doc comment.
    #[must_use]
    pub fn is_active(&self, run_id: &str) -> bool {
        self.lock().contains_key(run_id)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, CancelToken>> {
        self.0.lock().expect("cancellation registry poisoned")
    }
}

/// A per-cell "please detach cleanly" signal (RAL-288 Stage 6). Structurally
/// identical to [`Cancellations`]/[`CancelToken`] -- register a token when a
/// cell's subprocess starts, poll it in the same loop that already polls the
/// cancel token, trip it externally via the registry -- but a *separate*
/// registry, keyed per-cell rather than per-squad, and checked for a
/// different reason: a detach must stop exactly one running cell so a real
/// interactive `claude --resume`/`codex resume`/`pi --session` can safely
/// take over its session, without touching the rest of that cell's squad,
/// and the runner must report a `"detached"` outcome rather than
/// `"cancelled"` when it trips.
pub type Detachments = Cancellations;
/// See [`Detachments`].
pub type DetachToken = CancelToken;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_starts_uncancelled_and_trips_once_cancelled() {
        let t = CancelToken::new();
        assert!(!t.is_cancelled());
        t.cancel();
        assert!(t.is_cancelled());
    }

    #[test]
    fn registry_cancel_trips_the_registered_token() {
        let reg = Cancellations::new();
        let token = reg.register("run-1");
        assert!(!token.is_cancelled());
        reg.cancel("run-1");
        assert!(token.is_cancelled());
    }

    #[test]
    fn cancel_unknown_run_is_a_noop() {
        let reg = Cancellations::new();
        reg.cancel("run-nope"); // must not panic
    }

    #[test]
    fn removed_token_is_no_longer_cancellable_via_registry() {
        let reg = Cancellations::new();
        let token = reg.register("run-1");
        reg.remove("run-1");
        reg.cancel("run-1"); // the registry no longer knows it
        assert!(!token.is_cancelled());
    }

    #[test]
    fn is_active_reflects_registration_and_removal() {
        let reg = Cancellations::new();
        assert!(!reg.is_active("run-1"));
        let _ = reg.register("run-1");
        assert!(reg.is_active("run-1"));
        reg.remove("run-1");
        assert!(!reg.is_active("run-1"));
    }

    #[test]
    fn cancel_all_trips_every_registered_token() {
        let reg = Cancellations::new();
        let a = reg.register("run-1");
        let b = reg.register("run-2");
        assert!(!a.is_cancelled());
        assert!(!b.is_cancelled());
        reg.cancel_all();
        assert!(a.is_cancelled());
        assert!(b.is_cancelled());
    }

    #[test]
    fn cancel_all_on_empty_registry_is_a_noop() {
        let reg = Cancellations::new();
        reg.cancel_all(); // must not panic
    }
}
