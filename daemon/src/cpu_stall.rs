//! RAL-308: a conservative, additive periodic sweep that recognizes a
//! still-running cell/proof subprocess whose cumulative CPU time hasn't
//! moved across several repeated samples -- a real hang, as opposed to the
//! existing RAL-241 pane-quiet check (`SubprocessRunner::check_stall_escalation`),
//! which only knows a tmux pane hasn't printed anything and can be
//! (correctly) silent for a long time during genuinely CPU-bound work.
//!
//! This sweep never kills anything -- it only enqueues a `high`-priority
//! mailbox message, exactly like the pane-quiet check, so an operator (or a
//! watching agent) can decide whether a real hang needs manual intervention.
//! It complements rather than replaces the pane-quiet check: a session can
//! trip either, both, or neither.
//!
//! Driven from [`crate::scheduler::run_loop`] at a coarse interval, this
//! reads every currently-registered PID from [`crate::procreg::ProcRegistry`]
//! (which already covers both real cells and proof steps -- see
//! [`crate::procreg::ProcRegistry`]'s module doc) and diffs each one's
//! cumulative CPU time (via [`crate::resources::cpu_seconds`], the same
//! per-OS abstraction the Resources tab uses) against the previous sweep's
//! reading. Platform-specific accounting stays entirely behind that
//! abstraction; this module only ever sees `Option<f64>` seconds.

use std::collections::HashMap;
use std::sync::Mutex;

use crate::procreg::ProcRegistry;
use crate::store_lock::StoreHandle;

/// A flat CPU reading must repeat at least this many consecutive sweeps...
const CPU_STALL_MIN_SAMPLES: u32 = 3;
/// ...and span at least this long, before escalating. Both conditions guard
/// against a single quantization artifact (e.g. two samples landing on the
/// same whole-second CPU-time tick) reading as a false stall; requiring a
/// real span in *minutes* is what makes this "several minutes" per RAL-308,
/// not just "N sweeps" at whatever cadence the caller happens to use.
const CPU_STALL_MIN_SPAN_MS: i64 = 5 * 60 * 1000;

/// Per-session CPU-flat tracking state, keyed the same way
/// [`ProcRegistry`] is: `(run_id, session_id)` -- `run_id` is a squad id,
/// `session_id` is a real cell id or a proof step's synthetic
/// `proof-<scope>-<idx>` id.
struct Entry {
    /// The PID this reading belongs to. A changed PID for the same session
    /// key (e.g. a tmux auto-reattach spawning a fresh subprocess) means a
    /// fresh CPU-time clock, so any existing streak is discarded rather than
    /// compared across processes.
    pid: u32,
    /// Cumulative CPU seconds observed on the most recent sweep.
    last_cpu_secs: f64,
    /// When the current flat streak began (first sweep at `last_cpu_secs`).
    flat_since_ms: i64,
    /// Consecutive sweeps (inclusive of the first) with no measured progress.
    flat_samples: u32,
    /// The `flat_since_ms` value already escalated for, if any -- so a
    /// still-ongoing stall isn't re-messaged every sweep, but a *new* onset
    /// (after CPU activity resumes and then stalls again) can escalate
    /// again. Mirrors `Store::is_stall_escalated`'s debounce shape for the
    /// pane-quiet check, kept locally here since this tracker (like the
    /// pane-quiet check's own in-memory maps) is a runtime-only cache with
    /// no need to survive a daemon restart.
    escalated_onset_ms: Option<i64>,
}

/// Owns the CPU-flat tracking state across sweeps. One instance lives for
/// the lifetime of [`crate::scheduler::run_loop`]'s thread.
#[derive(Default)]
pub struct CpuStallTracker {
    state: Mutex<HashMap<(String, String), Entry>>,
}

impl CpuStallTracker {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Run one sweep: sample every live PID's cumulative CPU time and
    /// escalate any session whose CPU time has been flat for
    /// [`CPU_STALL_MIN_SAMPLES`] consecutive sweeps spanning at least
    /// [`CPU_STALL_MIN_SPAN_MS`]. Never kills or cancels anything --
    /// escalation is the only effect (RAL-308 explicitly keeps this
    /// detector advisory, same as the existing pane-quiet check).
    pub fn sweep(&self, store: &StoreHandle, procs: &ProcRegistry) {
        let live = procs.snapshot();
        let pids: Vec<u32> = live.iter().map(|(_, _, pid)| *pid).collect();
        let cpu = crate::resources::cpu_seconds(&pids);
        let now = crate::store::now_ms();

        let mut state = self.state.lock().expect("cpu_stall mutex poisoned");
        // Drop tracking for any session no longer live -- it either finished
        // normally or was already killed by some other mechanism, and a
        // stale entry must not leak forever or resurface against an
        // unrelated future PID reuse.
        let live_keys: std::collections::HashSet<(String, String)> = live
            .iter()
            .map(|(run_id, session_id, _)| (run_id.clone(), session_id.clone()))
            .collect();
        state.retain(|key, _| live_keys.contains(key));

        for (run_id, session_id, pid) in live {
            let key = (run_id.clone(), session_id.clone());
            let Some(Some(secs)) = cpu.get(&pid).copied() else {
                // No CPU reading this sweep (unsupported platform, or a
                // transient read failure) -- conservative: drop any existing
                // streak rather than guessing at progress.
                state.remove(&key);
                continue;
            };
            let fresh = Entry {
                pid,
                last_cpu_secs: secs,
                flat_since_ms: now,
                flat_samples: 1,
                escalated_onset_ms: None,
            };
            let entry = state.entry(key).or_insert_with(|| Entry {
                pid,
                last_cpu_secs: secs,
                flat_since_ms: now,
                flat_samples: 1,
                escalated_onset_ms: None,
            });
            if entry.pid != pid || secs > entry.last_cpu_secs {
                // A fresh process for this session, or real CPU progress:
                // either way the streak restarts clean.
                *entry = fresh;
                continue;
            }
            entry.last_cpu_secs = secs;
            entry.flat_samples += 1;
            let span_ms = now - entry.flat_since_ms;
            if entry.flat_samples < CPU_STALL_MIN_SAMPLES || span_ms < CPU_STALL_MIN_SPAN_MS {
                continue;
            }
            if entry.escalated_onset_ms == Some(entry.flat_since_ms) {
                continue;
            }
            let guard = store.lock();
            let text = format!(
                "session '{session_id}' in squad {run_id} (pid {pid}) has shown no CPU progress for over {}s across {} samples despite still running -- possible hang. Not auto-killed; investigate manually.",
                span_ms / 1000,
                entry.flat_samples,
            );
            if let Ok(message_id) = guard.enqueue_mailbox_message(
                crate::mailbox::MailboxPriority::High,
                &text,
                Some(&run_id),
                None,
                Some(&session_id),
                None,
            ) {
                crate::cartographer::Note::new("cpu_stall")
                    .level(crate::logging::LogLevel::WARNING)
                    .squad(&run_id)
                    .cell(&session_id)
                    .scope("mailbox")
                    .emit(
                        &guard,
                        format!("mailbox message enqueued for CPU-flat session ({session_id})"),
                        serde_json::json!({
                            "message_id": message_id,
                            "priority": "high",
                            "pid": pid,
                            "flat_span_secs": span_ms / 1000,
                            "flat_samples": entry.flat_samples,
                        }),
                    );
            }
            entry.escalated_onset_ms = Some(entry.flat_since_ms);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;
    use std::sync::Arc;

    fn insert_squad(store: &Store, id: &str) {
        store
            .conn
            .execute(
                "INSERT INTO squads(id, state, created_at_ms, updated_at_ms) VALUES(?1, 'running', 0, 0)",
                rusqlite::params![id],
            )
            .unwrap();
    }

    /// A PID this platform can't read (here, simulated by one no live
    /// process holds) must never escalate -- a missing reading drops
    /// tracking instead of being treated as "definitely flat".
    #[test]
    fn unreadable_pid_never_escalates() {
        let store: StoreHandle = Arc::new(crate::store_lock::StoreMutex::new(
            Store::open_in_memory().unwrap(),
        ));
        insert_squad(&store.lock(), "squad-1");
        let client_id = store.lock().register_mailbox_client().unwrap();
        let procs = ProcRegistry::new();
        // A PID astronomically unlikely to be a real live process on any
        // supported platform.
        procs.register("squad-1", "cell-a", 4_000_000_000);
        let tracker = CpuStallTracker::new();
        for _ in 0..(CPU_STALL_MIN_SAMPLES + 2) {
            tracker.sweep(&store, &procs);
        }
        let messages = store
            .lock()
            .mailbox_messages_for_client(&client_id, true, None)
            .unwrap();
        assert!(
            messages.is_empty(),
            "an unreadable PID must never escalate, got: {messages:?}"
        );
    }

    /// End-to-end: a real (idle) child process's PID, registered like a
    /// live cell/proof would be, escalates once its CPU time has read flat
    /// for enough sweeps spanning enough backdated time, and does not
    /// re-escalate on a subsequent still-flat sweep.
    #[test]
    fn escalates_via_real_registry_and_store() {
        let store: StoreHandle = Arc::new(crate::store_lock::StoreMutex::new(
            Store::open_in_memory().unwrap(),
        ));
        insert_squad(&store.lock(), "squad-1");
        let client_id = store.lock().register_mailbox_client().unwrap();
        let procs = ProcRegistry::new();
        // A real, idle process this test controls -- portable across
        // Windows/Linux/macOS `read_procs` implementations, unlike guessing
        // at a PID.
        // `timeout.exe` refuses to run with redirected stdin ("Input
        // redirection is not supported"), so `ping` is the portable
        // idle-wait substitute in a non-interactive Windows process.
        let mut child = std::process::Command::new(if cfg!(windows) { "ping" } else { "sleep" })
            .args(if cfg!(windows) {
                vec!["-n", "31", "127.0.0.1"]
            } else {
                vec!["30"]
            })
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn idle child");
        procs.register("squad-1", "cell-a", child.id());

        let tracker = CpuStallTracker::new();
        // First sweep establishes the baseline reading.
        tracker.sweep(&store, &procs);
        // Force the tracked entry's clock backward so the *next* sweep (an
        // idle process's CPU time genuinely won't have moved in the couple
        // of milliseconds since) already satisfies the span requirement,
        // without this test actually sleeping for minutes.
        {
            let mut state = tracker.state.lock().unwrap();
            let entry = state
                .get_mut(&("squad-1".to_string(), "cell-a".to_string()))
                .expect("baseline entry recorded");
            entry.flat_since_ms -= CPU_STALL_MIN_SPAN_MS + 1000;
            entry.flat_samples = CPU_STALL_MIN_SAMPLES - 1;
        }
        tracker.sweep(&store, &procs);

        let messages = store
            .lock()
            .mailbox_messages_for_client(&client_id, true, None)
            .unwrap();
        assert_eq!(messages.len(), 1, "exactly one escalation expected");
        assert!(messages[0].message.contains("no CPU progress"));

        // A further still-flat sweep must not re-escalate the same onset.
        tracker.sweep(&store, &procs);
        let messages = store
            .lock()
            .mailbox_messages_for_client(&client_id, true, None)
            .unwrap();
        assert_eq!(messages.len(), 1, "must not re-escalate the same streak");

        let _ = child.kill();
        let _ = child.wait();
    }
}
