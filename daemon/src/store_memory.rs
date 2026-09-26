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
}

impl StoreMemory {
    #[must_use]
    pub fn new() -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self::default())
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
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
