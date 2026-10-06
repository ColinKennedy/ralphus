//! WS-E.1: the `Store`'s in-memory-only state, moved off `Store` and behind its
//! own small locks.
//!
//! # Why this is a prerequisite for the rest of WS-E
//!
//! `Store` owned seven collections that never touch SQLite -- worktree leases,
//! restack bookkeeping, tmux liveness, stall debounce, a secret-name cache.
//! Because they lived on `Store`, every method that read one had to be reached
//! through the global `StoreMutex`, even though none of them is database state
//! at all. That is the `Store`-as-god-object problem the plan calls RC-8, and it
//! blocks the read-path migration directly: a read method cannot move to a
//! pooled connection if it also has to reach a `HashMap` that only the writer
//! lock protects.
//!
//! With the state here instead, those methods take `&self` rather than
//! `&mut self`, so a caller holding nothing at all can use them -- and
//! `Store`'s remaining fields are the connection, the event bus, and the read
//! pool, all of which a pooled reader can be given.
//!
//! # Lock grouping is not arbitrary
//!
//! The collections are grouped by the invariants that actually span them, not
//! one lock each. Two of them matter:
//!
//! - A restack may only be claimed when no branch of that guardian holds a
//!   worktree lease, and no lease may be acquired while a restack is running.
//!   That interlock reads and writes leases, requests and running together, so
//!   all three sit under one lock ([`RestackState`]). Giving them separate locks
//!   would make `try_claim_guardian_restack` and
//!   `try_acquire_guardian_worktree_lease` race, and both could succeed --
//!   rebasing a branch out from under a running feedback pass, which is the
//!   exact thing the lease exists to prevent.
//! - Stall escalation is keyed to a session's last-known-good activity
//!   timestamp, and is cleared alongside that session's liveness entry, so
//!   liveness and escalation share one lock ([`ActivityState`]).
//!
//! The remaining two are genuinely independent and get their own.

use std::collections::{BTreeSet, HashMap, HashSet};

use parking_lot::{Mutex, RwLock};

use crate::cancel::CancelToken;

/// The worktree-lease / restack interlock. See the module doc comment for why
/// these three live under one lock.
#[derive(Default)]
struct RestackState {
    /// Exclusive ownership of one review branch's mutable worktree, keyed by
    /// `(guardian_id, branch_id)` and valued by an owner tag (e.g.
    /// `feedback:{branch_id}`) -- see the `worktree lease` glossary entry.
    /// In-memory only: a daemon restart mid-lease simply drops it, which is safe
    /// since the worktree is re-checked for dirt on the next pass through
    /// `drive_rebase`.
    leases: HashMap<(String, String), String>,
    /// A pending restack request per guardian, coalesced to the lowest
    /// requested `from_position` -- a later request for a *later* position while
    /// an earlier one is still queued would otherwise lose ground already
    /// claimed.
    requests: HashMap<String, i64>,
    /// Guardians with a restack currently claimed (running).
    running: HashSet<String>,
}

/// Per-session tmux liveness and stall-escalation debounce.
#[derive(Default)]
struct ActivityState {
    /// Liveness signal (RAL-170): last time fresh pane output was observed for a
    /// running tmux-wrapped session, keyed by `crate::tmux::session_name`.
    /// Entries are removed once the owning `run_via_tmux` call returns, so this
    /// stays bounded by the number of *currently running* sessions rather than
    /// lifetime history.
    live: HashMap<String, i64>,
    /// RAL-241: which session keys have already had a stall escalation enqueued
    /// for their *current* stall onset, and when that onset's last-known-good
    /// activity timestamp was -- so an ongoing stall does not re-enqueue a
    /// mailbox message on every poll.
    escalated: HashMap<String, i64>,
}

/// RAL-550: what the daemon knows about one running cell's worktree diff,
/// learned purely from the runner's pushed `worktree-diff` events -- the daemon
/// never polls for it.
#[derive(Default, Clone)]
pub struct CellDiffState {
    /// Bumped by every pushed change event.
    pub version: u64,
    /// The latest pushed numstat summary (`files_changed`, `lines_added`, ...).
    pub summary: serde_json::Value,
    /// Last pushed version claimed by the large-diff Arbiter inspection.
    reviewed_version: u64,
    /// The full diff last pulled on demand, and the `version` it was computed at.
    cached: Option<(u64, String)>,
    /// Recency tick, so the map can be bounded by evicting the stalest cell.
    touched: u64,
}

impl CellDiffState {
    /// A pushed change has not been pulled yet (or nothing was ever pulled).
    #[must_use]
    pub fn is_dirty(&self) -> bool {
        self.cached.as_ref().is_none_or(|(v, _)| *v != self.version)
    }
}

#[derive(Default)]
struct CellDiffs {
    cells: HashMap<String, CellDiffState>,
    tick: u64,
}

/// Cells whose diff state is retained before the stalest is evicted.
const MAX_TRACKED_CELL_DIFFS: usize = 256;

/// Per-guardian debounce bookkeeping for the LLM-authored final change summary
/// (RAL-208).
///
/// In-memory only: losing this across a restart just means the next
/// enabled-branch-set change regenerates the summary once more than strictly
/// necessary, not a correctness problem.
#[derive(Debug, Default, Clone)]
struct SummaryDebounce {
    /// The enabled-branch signature the current LLM-authored `change_summary`
    /// was generated from. `None` until the first final summary is produced.
    generated_signature: Option<String>,
    /// A signature awaiting generation, and when it was last (re)requested.
    /// Each new request overwrites both fields -- that is what implements the
    /// trailing debounce: the quiet-period clock restarts on every
    /// enable/disable toggle instead of accumulating separate pending jobs.
    pending_signature: Option<String>,
    pending_requested_at_ms: Option<i64>,
    /// RAL-303: whether this daemon process has already tried to repair a
    /// guardian left with no LLM-authored summary.
    repair_attempted: bool,
}

#[derive(Default)]
struct PreparationState {
    next_generation: u64,
    active: HashMap<String, (u64, CancelToken)>,
    gates: HashMap<String, std::sync::Arc<Mutex<()>>>,
}

/// RAL-400: which waypoint surveys are running right now, and how many times
/// the work each one is judging has been edited since.
///
/// Both halves are needed together: `in_flight` stops a slow classifier call
/// from being launched a second time by the next sweep, and `generations` lets
/// the call that is already running notice that the prompt it read has since
/// been replaced, so its verdict is dropped rather than recorded against text
/// nobody asked it about.
#[derive(Default)]
struct SurveyState {
    /// Bumped each time a candidate's prompt text changes, keyed by
    /// `(kind, entry_id)`. Absent means never edited (generation 0).
    generations: HashMap<(String, String), u64>,
    /// `(waypoint_id, kind, entry_id)` of every survey currently running.
    in_flight: HashSet<(String, String, String)>,
}

/// Exclusive right to survey one candidate against one waypoint. Dropping it
/// -- including by a panic in the survey thread -- frees the slot, so a
/// crashed survey is retried by the next sweep rather than blocking forever.
pub struct SurveyClaim {
    memory: std::sync::Arc<StoreMemory>,
    key: (String, String, String),
}

impl Drop for SurveyClaim {
    fn drop(&mut self) {
        self.memory.survey.lock().in_flight.remove(&self.key);
    }
}

/// One `Store`'s non-database state. Cloning the `Arc` is cheap and touches no
/// lock, so a caller can hold a handle to this without holding the store.
#[derive(Default)]
pub struct StoreMemory {
    restack: Mutex<RestackState>,
    activity: Mutex<ActivityState>,
    summary_debounce: Mutex<HashMap<String, SummaryDebounce>>,
    /// RAL-281: process-lifetime cache of `secret_env_names`, `None` when
    /// invalidated by a mutation. Scoped to one `StoreMemory` (not a global
    /// static) so it cannot leak between the daemon's one real database and the
    /// many independent in-memory stores each test opens. An `RwLock` because
    /// the scheduler's per-cell env-merge choke point reads it on every
    /// dispatch and writes only on invalidation.
    secret_env_names: RwLock<Option<BTreeSet<String>>>,
    /// RAL-536: consecutive automatic restarts a [`crate::thinking_stall`]
    /// trip has triggered for one unit of work, keyed by
    /// `crate::tmux::session_name(run_id, task, session_id)` -- the same
    /// deterministic key [`ActivityState`] already uses, so an automatic
    /// retry of the same cell/proof/review-agent operation (which reuses
    /// that triple) keeps accumulating against the same counter, while a
    /// genuinely different unit of work starts fresh. Independent of every
    /// other group here: nothing else reads or writes it, and it does not
    /// share an invariant with the tmux-liveness/stall-escalation state
    /// despite both being about "is this session stalled" -- this counter
    /// tracks completed automatic restarts, not liveness.
    thinking_stall_strikes: Mutex<HashMap<String, u32>>,
    /// RAL-550: per-cell live-diff staleness, see [`CellDiffState`]. Independent
    /// of every other group: only the runner-event forwarder writes the version
    /// and only the on-demand diff pull reads/clears it.
    cell_diffs: Mutex<CellDiffs>,
    /// Per-review ownership for advisory preparation. Starting a newer
    /// generation cancels the prior token immediately, while the per-review
    /// gate keeps their retained checkout and artifacts from being mutated by
    /// two workers at once.
    preparation: Mutex<PreparationState>,
    /// RAL-400: waypoint survey in-flight set and edit generations, see
    /// [`SurveyState`]. Independent of every other group here.
    survey: Mutex<SurveyState>,
}

impl StoreMemory {
    #[must_use]
    pub fn new() -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self::default())
    }

    // ---- waypoint survey guard ----

    /// Claim the right to survey `entry_id` against `waypoint_id`, or `None`
    /// if a survey of that pair is already running.
    #[must_use]
    pub fn try_claim_survey(
        self: &std::sync::Arc<Self>,
        waypoint_id: &str,
        kind: &str,
        entry_id: &str,
    ) -> Option<SurveyClaim> {
        let key = (
            waypoint_id.to_string(),
            kind.to_string(),
            entry_id.to_string(),
        );
        if !self.survey.lock().in_flight.insert(key.clone()) {
            return None;
        }
        Some(SurveyClaim {
            memory: std::sync::Arc::clone(self),
            key,
        })
    }

    /// How many times the work behind `(kind, entry_id)` has been edited. A
    /// survey reads this before it describes the work and again before it
    /// records a verdict; a difference means the description it judged is stale.
    #[must_use]
    pub fn survey_generation(&self, kind: &str, entry_id: &str) -> u64 {
        self.survey
            .lock()
            .generations
            .get(&(kind.to_string(), entry_id.to_string()))
            .copied()
            .unwrap_or(0)
    }

    /// Record that the work behind `(kind, entry_id)` changed, invalidating any
    /// survey already running against the old text.
    pub fn invalidate_surveys(&self, kind: &str, entry_id: &str) {
        let mut state = self.survey.lock();
        let generation = state
            .generations
            .entry((kind.to_string(), entry_id.to_string()))
            .or_insert(0);
        *generation = generation.saturating_add(1);
    }

    // ---- review preparation ownership ----

    /// Supersede any active preparation and reserve the next generation.
    /// The caller locks the returned gate before touching prepared files.
    pub fn begin_guardian_preparation(
        &self,
        guardian_id: &str,
    ) -> (u64, CancelToken, std::sync::Arc<Mutex<()>>) {
        let mut state = self.preparation.lock();
        if let Some((_, token)) = state.active.get(guardian_id) {
            token.cancel();
        }
        state.next_generation = state.next_generation.saturating_add(1);
        let generation = state.next_generation;
        let token = CancelToken::new();
        state
            .active
            .insert(guardian_id.to_string(), (generation, token.clone()));
        let gate = state
            .gates
            .entry(guardian_id.to_string())
            .or_insert_with(|| std::sync::Arc::new(Mutex::new(())))
            .clone();
        (generation, token, gate)
    }

    /// Cancel preparation as soon as work starts changing the review basis.
    pub fn cancel_guardian_preparation(&self, guardian_id: &str) {
        if let Some((_, token)) = self.preparation.lock().active.get(guardian_id) {
            token.cancel();
        }
    }

    /// Release ownership only if this is still the newest generation.
    pub fn finish_guardian_preparation(&self, guardian_id: &str, generation: u64) {
        let mut state = self.preparation.lock();
        let current = state
            .active
            .get(guardian_id)
            .is_some_and(|(active, _)| *active == generation);
        if current {
            state.active.remove(guardian_id);
        }
    }

    // ---- worktree leases / restack interlock ----

    /// Claim `(guardian_id, branch_id)`'s worktree for `owner`, unless a restack
    /// is running for that guardian or the branch is already leased.
    pub fn try_acquire_worktree_lease(
        &self,
        guardian_id: &str,
        branch_id: &str,
        owner: &str,
    ) -> bool {
        let mut state = self.restack.lock();
        if state.running.contains(guardian_id) {
            return false;
        }
        let key = (guardian_id.to_string(), branch_id.to_string());
        if state.leases.contains_key(&key) {
            return false;
        }
        state.leases.insert(key, owner.to_string());
        true
    }

    /// Release the lease only if `owner` still holds it, so a stale releaser
    /// cannot drop someone else's lease.
    pub fn release_worktree_lease(&self, guardian_id: &str, branch_id: &str, owner: &str) -> bool {
        let mut state = self.restack.lock();
        let key = (guardian_id.to_string(), branch_id.to_string());
        if state.leases.get(&key).map(String::as_str) != Some(owner) {
            return false;
        }
        state.leases.remove(&key);
        true
    }

    #[must_use]
    pub fn worktree_lease_owner(&self, guardian_id: &str, branch_id: &str) -> Option<String> {
        self.restack
            .lock()
            .leases
            .get(&(guardian_id.to_string(), branch_id.to_string()))
            .cloned()
    }

    /// Queue a restack, coalescing to the lowest requested position.
    pub fn request_restack(&self, guardian_id: &str, from_position: i64) {
        self.restack
            .lock()
            .requests
            .entry(guardian_id.to_string())
            .and_modify(|p| *p = (*p).min(from_position))
            .or_insert(from_position);
    }

    /// Claim a queued restack, if one is queued and nothing blocks it.
    ///
    /// The lease check and the claim happen under one lock: that is the
    /// interlock this module's grouping exists to preserve.
    pub fn try_claim_restack(&self, guardian_id: &str) -> Option<i64> {
        let mut state = self.restack.lock();
        if state.running.contains(guardian_id)
            || state.leases.keys().any(|(gid, _)| gid == guardian_id)
        {
            return None;
        }
        let position = state.requests.remove(guardian_id)?;
        state.running.insert(guardian_id.to_string());
        Some(position)
    }

    pub fn finish_restack(&self, guardian_id: &str) {
        self.restack.lock().running.remove(guardian_id);
    }

    // ---- tmux liveness / stall escalation ----

    pub fn note_live_activity(&self, session_name: &str, at_ms: i64) {
        self.activity
            .lock()
            .live
            .insert(session_name.to_string(), at_ms);
    }

    #[must_use]
    pub fn live_activity_ms(&self, session_name: &str) -> Option<i64> {
        self.activity.lock().live.get(session_name).copied()
    }

    pub fn clear_live_activity(&self, session_name: &str) {
        self.activity.lock().live.remove(session_name);
    }

    #[must_use]
    pub fn is_stall_escalated(&self, session_name: &str, last_activity_ms: i64) -> bool {
        self.activity.lock().escalated.get(session_name) == Some(&last_activity_ms)
    }

    pub fn note_stall_escalated(&self, session_name: &str, last_activity_ms: i64) {
        self.activity
            .lock()
            .escalated
            .insert(session_name.to_string(), last_activity_ms);
    }

    pub fn clear_stall_escalated(&self, session_name: &str) {
        self.activity.lock().escalated.remove(session_name);
    }

    // ---- final-summary debounce ----

    /// Request a final-summary regeneration for `id` at `signature`, restarting
    /// the trailing debounce clock.
    ///
    /// A request for the signature already generated is a no-op unless `force`
    /// -- RAL-303: the signature check assumes the stored summary is the LLM one
    /// this branch set last produced, which is false while `change_summary`
    /// still holds the deterministic git-log preliminary. There the branch set
    /// is unchanged but the summary has never been through the LLM at all, so
    /// the caller passes `force` to get the handoff it would otherwise be
    /// denied.
    pub fn request_final_summary(&self, id: &str, signature: &str, now_ms: i64, force: bool) {
        let mut map = self.summary_debounce.lock();
        let d = map.entry(id.to_string()).or_default();
        if !force && d.generated_signature.as_deref() == Some(signature) {
            return;
        }
        d.pending_signature = Some(signature.to_string());
        d.pending_requested_at_ms = Some(now_ms);
    }

    /// Atomically claim every guardian whose pending request has gone
    /// `debounce_ms` without a newer one, clearing their pending state so a
    /// concurrent duplicate sweep finds nothing left to claim. Returns
    /// `(guardian_id, signature)` pairs for the caller to generate well outside
    /// any lock.
    pub fn take_due_final_summary_requests(
        &self,
        now_ms: i64,
        debounce_ms: i64,
    ) -> Vec<(String, String)> {
        let mut map = self.summary_debounce.lock();
        let mut due = Vec::new();
        for (id, d) in map.iter_mut() {
            let Some(requested_at) = d.pending_requested_at_ms else {
                continue;
            };
            if now_ms.saturating_sub(requested_at) >= debounce_ms {
                if let Some(sig) = d.pending_signature.take() {
                    due.push((id.clone(), sig));
                }
                d.pending_requested_at_ms = None;
            }
        }
        due
    }

    /// Record that guardian `id`'s change summary now reflects `signature`, so a
    /// later request for the same signature is recognized as already satisfied.
    pub fn mark_final_summary_generated(&self, id: &str, signature: &str) {
        self.summary_debounce
            .lock()
            .entry(id.to_string())
            .or_default()
            .generated_signature = Some(signature.to_string());
    }

    /// RAL-303: claim the one repair attempt this daemon process gets at a
    /// guardian whose change summary is missing or was never upgraded past the
    /// git-log preliminary. Returns `true` for the first caller only.
    pub fn claim_final_summary_repair(&self, id: &str) -> bool {
        let mut map = self.summary_debounce.lock();
        let d = map.entry(id.to_string()).or_default();
        if d.repair_attempted || d.generated_signature.is_some() {
            return false;
        }
        d.repair_attempted = true;
        true
    }

    // ---- secret env-name cache ----

    #[must_use]
    pub fn secret_env_names_cached(&self) -> Option<BTreeSet<String>> {
        self.secret_env_names.read().clone()
    }

    pub fn cache_secret_env_names(&self, names: BTreeSet<String>) {
        *self.secret_env_names.write() = Some(names);
    }

    pub fn invalidate_secret_env_names(&self) {
        *self.secret_env_names.write() = None;
    }

    // ---- thinking-stall strike counter (RAL-536) ----
    //
    // A [`crate::thinking_stall::ThinkingStallDetector`] trip triggers an
    // automatic restart of the affected cell/proof/review-agent, injecting
    // recovery context so the agent takes a different approach. This counter
    // tracks how many such automatic restarts have happened *in a row* for
    // one unit of work (keyed by `crate::tmux::session_name(run_id, task,
    // session_id)`), independent of the low-diversity streak the detector
    // itself tracks -- that streak lives inside the detector, is scoped to
    // one running attempt, and is gone once the attempt ends. This counter
    // is the opposite: it must survive across attempts, since its entire
    // purpose is noticing a *third* automatic restart of the same work.
    //
    // A third strike stops the automatic-restart cycle: instead of
    // restarting again, the caller terminates the operation and escalates to
    // a human via `Store::enqueue_error_mailbox_message`. Only a human
    // action -- resuming, restarting, or retrying the work by hand --
    // resets the count back to zero; the automatic restarts that raise it do
    // not reset each other's count, or the streak would never reach three.

    /// Record one automatic restart triggered by a thinking-stall trip for
    /// `key`, returning the new strike count.
    pub fn record_thinking_stall_strike(&self, key: &str) -> u32 {
        let mut strikes = self.thinking_stall_strikes.lock();
        let count = strikes.entry(key.to_string()).or_insert(0);
        *count += 1;
        *count
    }

    /// The current consecutive-automatic-restart count for `key`, `0` if
    /// none has ever been recorded (or it was last reset).
    #[must_use]
    pub fn thinking_stall_strikes(&self, key: &str) -> u32 {
        self.thinking_stall_strikes
            .lock()
            .get(key)
            .copied()
            .unwrap_or(0)
    }

    /// Reset `key`'s strike count to zero -- called on a human resume,
    /// restart, or retry of the work, never on an automatic one.
    pub fn reset_thinking_stall_strikes(&self, key: &str) {
        self.thinking_stall_strikes.lock().remove(key);
    }

    /// Reset every strike count whose key belongs to `run_id` -- the
    /// review/guardian-merge counterpart of [`Self::reset_thinking_stall_strikes`].
    /// A review's agent sessions (auto-build, resolver/fix-pass,
    /// proof-synthesis, per-branch feedback, ...) all share `run_id`
    /// (`guardian-{id}`) but use call-site-specific task/session ids that
    /// aren't practical to enumerate the way a cell/task/proof restart's
    /// exact keys are, so a human restart or reopen of the whole review
    /// clears everything under its prefix (`crate::tmux::session_name_run_prefix`)
    /// at once rather than key-by-key.
    pub fn reset_thinking_stall_strikes_for_run(&self, run_id: &str) {
        let prefix = crate::tmux::session_name_run_prefix(run_id);
        self.thinking_stall_strikes
            .lock()
            .retain(|k, _| !k.starts_with(&prefix));
    }

    // ---- live cell diff (RAL-550) ----

    /// The key for one cell's diff state. Cell ids are only unique within a
    /// task (every task in a squad may have a `work` cell), so the task name
    /// is part of the key.
    #[must_use]
    pub fn cell_diff_key(squad_id: &str, task: &str, cell_id: &str) -> String {
        format!("{squad_id}/{task}/{cell_id}")
    }

    fn with_cell_diff<R>(&self, key: &str, f: impl FnOnce(&mut CellDiffState) -> R) -> R {
        let mut guard = self.cell_diffs.lock();
        guard.tick += 1;
        let tick = guard.tick;
        if !guard.cells.contains_key(key) && guard.cells.len() >= MAX_TRACKED_CELL_DIFFS {
            if let Some(stalest) = guard
                .cells
                .iter()
                .min_by_key(|(_, s)| s.touched)
                .map(|(k, _)| k.clone())
            {
                guard.cells.remove(&stalest);
            }
        }
        let state = guard.cells.entry(key.to_string()).or_default();
        state.touched = tick;
        f(state)
    }

    /// Record a pushed change: bump the version (marking the diff dirty) and
    /// keep the latest summary.
    pub fn note_cell_diff_changed(&self, key: &str, summary: serde_json::Value) {
        self.with_cell_diff(key, |s| {
            s.version += 1;
            s.summary = summary;
        });
    }

    /// The tracked state for `key`, if any push (or pull) has happened.
    #[must_use]
    pub fn cell_diff_state(&self, key: &str) -> Option<CellDiffState> {
        self.cell_diffs.lock().cells.get(key).cloned()
    }

    /// The cached full diff when it is still current (not dirty).
    #[must_use]
    pub fn fresh_cell_diff(&self, key: &str) -> Option<(u64, String)> {
        let guard = self.cell_diffs.lock();
        let state = guard.cells.get(key)?;
        match &state.cached {
            Some((v, diff)) if *v == state.version => Some((*v, diff.clone())),
            _ => None,
        }
    }

    /// Cache `diff` as computed at `version`; the dirty flag only clears if no
    /// newer push landed while it was being computed.
    pub fn store_cell_diff(&self, key: &str, version: u64, diff: String) {
        self.with_cell_diff(key, |s| s.cached = Some((version, diff)));
    }

    /// Claim one changed diff version for Arbiter inspection. A second event
    /// for the same version cannot spend another Arbiter call.
    pub fn claim_cell_diff_inspection(&self, key: &str) -> Option<(u64, serde_json::Value)> {
        self.with_cell_diff(key, |s| {
            if s.version == 0 || s.reviewed_version == s.version {
                None
            } else {
                s.reviewed_version = s.version;
                Some((s.version, s.summary.clone()))
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pushed_change_marks_the_cell_diff_dirty_until_it_is_pulled() {
        let m = StoreMemory::default();
        let key = StoreMemory::cell_diff_key("squad-1", "t0", "c0");
        assert!(m.fresh_cell_diff(&key).is_none());
        m.note_cell_diff_changed(&key, serde_json::json!({"files_changed": 1}));
        let state = m.cell_diff_state(&key).unwrap();
        assert!(state.is_dirty());
        assert_eq!(state.version, 1);

        m.store_cell_diff(&key, 1, "diff".into());
        assert!(!m.cell_diff_state(&key).unwrap().is_dirty());
        assert_eq!(m.fresh_cell_diff(&key), Some((1, "diff".to_string())));

        // A push after the pull makes the cache stale again.
        m.note_cell_diff_changed(&key, serde_json::json!({"files_changed": 2}));
        assert!(m.cell_diff_state(&key).unwrap().is_dirty());
        assert!(m.fresh_cell_diff(&key).is_none());
        // A pull that raced a newer push must not clear the dirty flag.
        m.store_cell_diff(&key, 1, "old".into());
        assert!(m.cell_diff_state(&key).unwrap().is_dirty());
    }

    #[test]
    fn cell_diff_state_is_bounded_by_evicting_the_stalest_cell() {
        let m = StoreMemory::default();
        for i in 0..=MAX_TRACKED_CELL_DIFFS {
            m.note_cell_diff_changed(&format!("k{i}"), serde_json::json!({}));
        }
        assert!(m.cell_diff_state("k0").is_none());
        assert!(
            m.cell_diff_state(&format!("k{MAX_TRACKED_CELL_DIFFS}"))
                .is_some()
        );
    }

    #[test]
    fn a_lease_is_exclusive_and_only_its_owner_can_release_it() {
        let mem = StoreMemory::new();
        assert!(mem.try_acquire_worktree_lease("g1", "b1", "feedback:b1"));
        assert!(
            !mem.try_acquire_worktree_lease("g1", "b1", "other"),
            "a second holder got the same lease"
        );
        assert_eq!(
            mem.worktree_lease_owner("g1", "b1").as_deref(),
            Some("feedback:b1")
        );
        assert!(
            !mem.release_worktree_lease("g1", "b1", "other"),
            "a non-owner released someone else's lease"
        );
        assert!(mem.release_worktree_lease("g1", "b1", "feedback:b1"));
        assert_eq!(mem.worktree_lease_owner("g1", "b1"), None);
        // A different branch of the same guardian is a different lease.
        assert!(mem.try_acquire_worktree_lease("g1", "b2", "x"));
    }

    /// The interlock this module's lock grouping exists to preserve: a restack
    /// and a worktree lease must never both be held for one guardian.
    #[test]
    fn a_restack_and_a_lease_are_mutually_exclusive_per_guardian() {
        let mem = StoreMemory::new();
        mem.request_restack("g1", 0);

        // A held lease blocks the claim...
        assert!(mem.try_acquire_worktree_lease("g1", "b1", "feedback:b1"));
        assert_eq!(
            mem.try_claim_restack("g1"),
            None,
            "a restack was claimed while a branch of the same guardian was leased"
        );

        // ...and once released, the claim succeeds and the request is consumed.
        assert!(mem.release_worktree_lease("g1", "b1", "feedback:b1"));
        assert_eq!(mem.try_claim_restack("g1"), Some(0));
        assert_eq!(
            mem.try_claim_restack("g1"),
            None,
            "the same restack request was claimed twice"
        );

        // A running restack blocks a new lease, the other half of the interlock.
        assert!(
            !mem.try_acquire_worktree_lease("g1", "b1", "feedback:b1"),
            "a lease was acquired while a restack was running"
        );
        mem.finish_restack("g1");
        assert!(mem.try_acquire_worktree_lease("g1", "b1", "feedback:b1"));
    }

    #[test]
    fn a_restack_request_coalesces_to_the_lowest_position() {
        let mem = StoreMemory::new();
        mem.request_restack("g1", 5);
        mem.request_restack("g1", 2);
        mem.request_restack("g1", 7);
        assert_eq!(
            mem.try_claim_restack("g1"),
            Some(2),
            "a later request for a later position lost ground already claimed"
        );
    }

    #[test]
    fn restack_state_is_per_guardian() {
        let mem = StoreMemory::new();
        assert!(mem.try_acquire_worktree_lease("g1", "b1", "x"));
        mem.request_restack("g2", 0);
        assert_eq!(
            mem.try_claim_restack("g2"),
            Some(0),
            "another guardian's lease blocked this guardian's restack"
        );
    }

    #[test]
    fn liveness_and_stall_escalation_round_trip() {
        let mem = StoreMemory::new();
        assert_eq!(mem.live_activity_ms("s"), None);
        mem.note_live_activity("s", 100);
        assert_eq!(mem.live_activity_ms("s"), Some(100));
        mem.clear_live_activity("s");
        assert_eq!(mem.live_activity_ms("s"), None);

        // Escalation is keyed to the onset timestamp, so a *new* stall onset is
        // escalatable again even for the same session.
        assert!(!mem.is_stall_escalated("s", 100));
        mem.note_stall_escalated("s", 100);
        assert!(mem.is_stall_escalated("s", 100));
        assert!(
            !mem.is_stall_escalated("s", 200),
            "a new stall onset was treated as already escalated"
        );
        mem.clear_stall_escalated("s");
        assert!(!mem.is_stall_escalated("s", 100));
    }

    #[test]
    fn a_summary_request_is_only_due_after_the_debounce_and_only_once() {
        let mem = StoreMemory::new();
        mem.request_final_summary("g1", "sig-a", 1_000, false);
        assert!(
            mem.take_due_final_summary_requests(1_500, 1_000).is_empty(),
            "a request came due before its debounce elapsed"
        );
        assert_eq!(
            mem.take_due_final_summary_requests(2_000, 1_000),
            vec![("g1".to_string(), "sig-a".to_string())]
        );
        assert!(
            mem.take_due_final_summary_requests(3_000, 1_000).is_empty(),
            "the same request was claimed twice -- a concurrent duplicate sweep              would generate the summary twice"
        );
    }

    #[test]
    fn a_later_request_refreshes_the_debounce_window() {
        let mem = StoreMemory::new();
        mem.request_final_summary("g1", "sig-a", 1_000, false);
        mem.request_final_summary("g1", "sig-b", 1_800, false);
        assert!(
            mem.take_due_final_summary_requests(2_000, 1_000).is_empty(),
            "the quiet-period clock did not restart on the later request"
        );
        assert_eq!(
            mem.take_due_final_summary_requests(2_800, 1_000),
            vec![("g1".to_string(), "sig-b".to_string())],
            "the newer signature must win, not the one it superseded"
        );
    }

    #[test]
    fn a_request_for_the_already_generated_signature_is_a_no_op_unless_forced() {
        let mem = StoreMemory::new();
        mem.mark_final_summary_generated("g1", "sig-a");

        mem.request_final_summary("g1", "sig-a", 1_000, false);
        assert!(
            mem.take_due_final_summary_requests(9_000, 1_000).is_empty(),
            "re-requested a signature the summary already reflects"
        );

        // RAL-303: `force` is how the caller gets the handoff while
        // `change_summary` still holds the git-log preliminary -- the branch set
        // is unchanged, but the summary has never been through the LLM.
        mem.request_final_summary("g1", "sig-a", 2_000, true);
        assert_eq!(
            mem.take_due_final_summary_requests(9_000, 1_000),
            vec![("g1".to_string(), "sig-a".to_string())],
            "force did not override the already-generated check"
        );

        // A genuinely new signature needs no force.
        mem.request_final_summary("g1", "sig-b", 10_000, false);
        assert_eq!(
            mem.take_due_final_summary_requests(11_000, 1_000),
            vec![("g1".to_string(), "sig-b".to_string())]
        );
    }

    #[test]
    fn the_repair_attempt_is_claimable_once_and_not_at_all_once_generated() {
        let mem = StoreMemory::new();
        assert!(mem.claim_final_summary_repair("g1"));
        assert!(
            !mem.claim_final_summary_repair("g1"),
            "the one repair attempt per process was claimed twice"
        );

        // A guardian that already has an LLM summary is never a repair candidate.
        let mem = StoreMemory::new();
        mem.mark_final_summary_generated("g2", "sig");
        assert!(!mem.claim_final_summary_repair("g2"));
    }

    #[test]
    fn the_secret_name_cache_round_trips_and_invalidates() {
        let mem = StoreMemory::new();
        assert_eq!(mem.secret_env_names_cached(), None);
        mem.cache_secret_env_names(BTreeSet::from(["TOKEN".to_string()]));
        assert_eq!(
            mem.secret_env_names_cached(),
            Some(BTreeSet::from(["TOKEN".to_string()]))
        );
        mem.invalidate_secret_env_names();
        assert_eq!(mem.secret_env_names_cached(), None);
    }

    #[test]
    fn thinking_stall_strikes_accumulate_and_reset_per_key() {
        let mem = StoreMemory::new();
        assert_eq!(mem.thinking_stall_strikes("s1"), 0);
        assert_eq!(mem.record_thinking_stall_strike("s1"), 1);
        assert_eq!(mem.record_thinking_stall_strike("s1"), 2);
        assert_eq!(mem.thinking_stall_strikes("s1"), 2);

        // A different session's key is independent.
        assert_eq!(mem.thinking_stall_strikes("s2"), 0);
        assert_eq!(mem.record_thinking_stall_strike("s2"), 1);
        assert_eq!(mem.thinking_stall_strikes("s1"), 2);

        // Only an explicit (human-triggered) reset clears the count -- there is
        // no automatic decay.
        mem.reset_thinking_stall_strikes("s1");
        assert_eq!(mem.thinking_stall_strikes("s1"), 0);
        assert_eq!(
            mem.thinking_stall_strikes("s2"),
            1,
            "resetting one session's strikes reset an unrelated session too"
        );
    }

    #[test]
    fn newer_preparation_generation_immediately_cancels_the_old_one() {
        let mem = StoreMemory::new();
        let (first_generation, first, _) = mem.begin_guardian_preparation("g1");
        assert!(!first.is_cancelled());

        let (second_generation, second, _) = mem.begin_guardian_preparation("g1");

        assert!(first.is_cancelled());
        assert!(!second.is_cancelled());
        assert!(second_generation > first_generation);
    }

    #[test]
    fn preparation_generations_share_an_exclusive_review_gate() {
        let mem = StoreMemory::new();
        let (_, _, first_gate) = mem.begin_guardian_preparation("g1");
        let first_lease = first_gate.lock();
        let (_, _, second_gate) = mem.begin_guardian_preparation("g1");

        assert!(second_gate.try_lock().is_none());
        drop(first_lease);
        assert!(second_gate.try_lock().is_some());
    }

    #[test]
    fn cancelling_preparation_never_waits_for_its_workspace_gate() {
        let mem = StoreMemory::new();
        let (_, token, gate) = mem.begin_guardian_preparation("g1");
        let _lease = gate.lock();

        let started = std::time::Instant::now();
        mem.cancel_guardian_preparation("g1");

        assert!(token.is_cancelled());
        assert!(
            started.elapsed() < std::time::Duration::from_millis(100),
            "a rebase would be blocked behind the old preparation checkout"
        );
    }

    #[test]
    fn reset_thinking_stall_strikes_for_run_clears_only_that_runs_keys() {
        let mem = StoreMemory::new();
        let branch_key = crate::tmux::session_name("guardian-g1", "AUTO_BUILD", "branch-a");
        let resolver_key = crate::tmux::session_name("guardian-g1", "resolver", "conflict-1");
        let unrelated_key = crate::tmux::session_name("guardian-g2", "AUTO_BUILD", "branch-a");
        mem.record_thinking_stall_strike(&branch_key);
        mem.record_thinking_stall_strike(&branch_key);
        mem.record_thinking_stall_strike(&resolver_key);
        mem.record_thinking_stall_strike(&unrelated_key);

        mem.reset_thinking_stall_strikes_for_run("guardian-g1");

        assert_eq!(mem.thinking_stall_strikes(&branch_key), 0);
        assert_eq!(mem.thinking_stall_strikes(&resolver_key), 0);
        assert_eq!(
            mem.thinking_stall_strikes(&unrelated_key),
            1,
            "resetting one review's strikes reset a different review's key too"
        );
    }
}
