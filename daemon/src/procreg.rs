//! Live registry of runner subprocess PIDs (RAL-11).
//!
//! The scheduler spawns one `ralphus-runner` subprocess per session. To attribute
//! OS resource usage (CPU/RAM/GPU) back to the exact running task, we need each
//! session's process id while it is alive. The [`SubprocessRunner`](crate::runner::SubprocessRunner)
//! registers its child's PID here on spawn and removes it on exit; the
//! `/api/resources` endpoint reads the live set to sample per-task metrics.
//!
//! Keyed by `(run_id, session_id)` — the pair a [`RunnerSpec`](crate::runner::RunnerSpec)
//! already carries, and unique across the store. Verify-step subprocesses register
//! too (their synthetic `verify-<scope>-<idx>` id never matches a real session row,
//! so they simply don't show up in the resource view).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// A shareable map of live session subprocess PIDs. Cheap to clone (`Arc` inside).
#[derive(Clone, Default)]
pub struct ProcRegistry {
    inner: Arc<Mutex<HashMap<(String, String), u32>>>,
}

impl ProcRegistry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record the subprocess `pid` for a running session.
    pub fn register(&self, run_id: &str, session_id: &str, pid: u32) {
        self.lock()
            .insert((run_id.to_string(), session_id.to_string()), pid);
    }

    /// Forget a session's subprocess (called when it exits, however it exits).
    pub fn unregister(&self, run_id: &str, session_id: &str) {
        self.lock()
            .remove(&(run_id.to_string(), session_id.to_string()));
    }

    /// The live PID of a session, if one is currently registered.
    #[must_use]
    pub fn pid_of(&self, run_id: &str, session_id: &str) -> Option<u32> {
        self.lock()
            .get(&(run_id.to_string(), session_id.to_string()))
            .copied()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<(String, String), u32>> {
        self.inner.lock().expect("procreg mutex poisoned")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_then_lookup_then_unregister() {
        let reg = ProcRegistry::new();
        assert_eq!(reg.pid_of("run-1", "s0"), None);
        reg.register("run-1", "s0", 4242);
        assert_eq!(reg.pid_of("run-1", "s0"), Some(4242));
        // A different session/run does not collide.
        assert_eq!(reg.pid_of("run-1", "s1"), None);
        assert_eq!(reg.pid_of("run-2", "s0"), None);
        reg.unregister("run-1", "s0");
        assert_eq!(reg.pid_of("run-1", "s0"), None);
    }

    #[test]
    fn clone_shares_the_same_map() {
        let reg = ProcRegistry::new();
        let clone = reg.clone();
        reg.register("run-1", "s0", 7);
        assert_eq!(clone.pid_of("run-1", "s0"), Some(7));
    }
}
