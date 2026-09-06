//! RAL-339: backend-neutral detection of "autocompaction thrashing" -- an
//! agent repeatedly compacting its own context without enough real progress
//! between compactions to justify it. A cell that thrashes is failed at the
//! compaction boundary itself rather than let its process run to whatever
//! conclusion it eventually reaches, so its proof steps never start.
//!
//! [`ThrashTracker`] is the single, shared counter every backend
//! (`claude-code`, `pi`, `codex`) drives through the same two calls --
//! [`ThrashTracker::record_assistant_turn`] and
//! [`ThrashTracker::record_compaction`] -- so the thrash rule itself lives in
//! exactly one place regardless of which backend's stream produced the
//! signal. One tracker is scoped to a single backend `run()` call, which is
//! itself scoped to one cell/proof attempt (a fresh `ralphus-runner`
//! subprocess per attempt), so "reset on any cell/proof restart" falls out
//! for free -- there is nothing to explicitly reset.

use crate::cartographer::EventContext;

/// N: how many compactions must occur in one run before thrash detection can
/// fire at all, absent a project-level `.ralphus.toml` `[thrash]` override.
pub const DEFAULT_MAX_COMPACTIONS: u32 = 3;
/// M: the previous-compaction gap (in assistant turns) below which a
/// compaction at/after [`DEFAULT_MAX_COMPACTIONS`] counts as thrash, absent
/// an override.
pub const DEFAULT_MIN_TURN_GAP: u32 = 2;

/// Resolved N/M thresholds for one run, forwarded daemon -> runner per cell
/// via `CellSpec`/`RunOptions` (daemon-resolved from `.ralphus.toml`'s
/// `[thrash]` table -- the runner itself never reads project config, see
/// `runner/src/config.rs`'s own doc comment for why).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThrashThresholds {
    pub max_compactions: u32,
    pub min_turn_gap: u32,
}

impl Default for ThrashThresholds {
    fn default() -> Self {
        Self {
            max_compactions: DEFAULT_MAX_COMPACTIONS,
            min_turn_gap: DEFAULT_MIN_TURN_GAP,
        }
    }
}

/// Diagnostic detail for a detected thrash condition -- carried through
/// `BackendOutcome` into the failed `CellResult`'s error message and into the
/// Cartographer event's payload.
#[derive(Debug, Clone, PartialEq)]
pub struct ThrashDetail {
    pub compaction_count: u32,
    pub turns_since_previous_compaction: u32,
    pub max_compactions: u32,
    pub min_turn_gap: u32,
}

/// Tracks compaction count + assistant-turns-since-previous-compaction for
/// one run. Backend-agnostic: a backend calls [`Self::record_assistant_turn`]
/// on every assistant turn boundary it recognizes and
/// [`Self::record_compaction`] on every compaction (real or, for a backend
/// with no compaction event of its own, inferred) it recognizes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ThrashTracker {
    thresholds: ThrashThresholds,
    compaction_count: u32,
    turns_since_last_compaction: u32,
}

impl ThrashTracker {
    #[must_use]
    pub fn new(thresholds: ThrashThresholds) -> Self {
        Self {
            thresholds,
            compaction_count: 0,
            turns_since_last_compaction: 0,
        }
    }

    pub fn record_assistant_turn(&mut self) {
        self.turns_since_last_compaction = self.turns_since_last_compaction.saturating_add(1);
    }

    /// Records one compaction. Returns `Some(detail)` the moment the run
    /// crosses into thrash: this is (at least) the Nth compaction in the run
    /// (`max_compactions`) and its immediately preceding compaction happened
    /// fewer than M assistant turns ago (`min_turn_gap`). The very first
    /// compaction never thrashes -- there is no preceding compaction to
    /// compare its gap against.
    #[must_use]
    pub fn record_compaction(&mut self) -> Option<ThrashDetail> {
        self.compaction_count += 1;
        let gap = self.turns_since_last_compaction;
        self.turns_since_last_compaction = 0;

        let has_preceding_compaction = self.compaction_count > 1;
        if has_preceding_compaction
            && self.compaction_count >= self.thresholds.max_compactions
            && gap < self.thresholds.min_turn_gap
        {
            Some(ThrashDetail {
                compaction_count: self.compaction_count,
                turns_since_previous_compaction: gap,
                max_compactions: self.thresholds.max_compactions,
                min_turn_gap: self.thresholds.min_turn_gap,
            })
        } else {
            None
        }
    }
}

/// Emits the RAL-339 structured Cartographer event for a detected thrash
/// condition -- a stable, non-prose-matched event shape shared by every
/// backend, mirroring the shape of the existing per-backend compaction
/// events. `backend` still identifies which backend hit it (`"claude-code"`/
/// `"pi"`/`"codex"`), but `message` and the payload's keys are fixed so
/// nothing downstream ever needs to match any backend's own wording.
pub fn emit_thrash_event(backend: &str, detail: &ThrashDetail) {
    crate::cartographer::emit(
        backend,
        "autocompaction thrash detected",
        "error",
        EventContext::default(),
        serde_json::json!({
            "compaction_count": detail.compaction_count,
            "turns_since_previous_compaction": detail.turns_since_previous_compaction,
            "max_compactions": detail.max_compactions,
            "min_turn_gap": detail.min_turn_gap,
        }),
    );
}

/// The failed `CellResult`'s error message for a detected thrash condition --
/// backend-neutral text, not derived from any backend's own error prose.
#[must_use]
pub fn thrash_error_message(backend: &str, detail: &ThrashDetail) -> String {
    format!(
        "autocompaction thrashing: {backend} compacted its context {} times this run, \
         most recently only {} assistant turn(s) after the previous compaction \
         (thrash threshold: {}+ compactions with fewer than {} turns between them)",
        detail.compaction_count,
        detail.turns_since_previous_compaction,
        detail.max_compactions,
        detail.min_turn_gap
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn thresholds(max_compactions: u32, min_turn_gap: u32) -> ThrashThresholds {
        ThrashThresholds {
            max_compactions,
            min_turn_gap,
        }
    }

    #[test]
    fn a_single_isolated_compaction_never_thrashes() {
        let mut t = ThrashTracker::new(thresholds(3, 2));
        assert_eq!(t.record_compaction(), None);
    }

    #[test]
    fn healthy_cadence_with_many_compactions_never_thrashes() {
        // Every gap meets the M=2 threshold, however many compactions occur.
        let mut t = ThrashTracker::new(thresholds(3, 2));
        for _ in 0..10 {
            t.record_assistant_turn();
            t.record_assistant_turn();
            assert_eq!(t.record_compaction(), None);
        }
    }

    #[test]
    fn fewer_than_n_compactions_never_thrashes_even_with_a_zero_gap() {
        // Two tight-gap compactions (N=3 requires a third) must not thrash.
        let mut t = ThrashTracker::new(thresholds(3, 2));
        assert_eq!(t.record_compaction(), None); // 1st: no preceding compaction
        assert_eq!(t.record_compaction(), None); // 2nd: gap 0, but count 2 < N=3
    }

    #[test]
    fn the_nth_compaction_with_a_gap_under_m_thrashes() {
        let mut t = ThrashTracker::new(thresholds(3, 2));
        assert_eq!(t.record_compaction(), None); // 1st
        t.record_assistant_turn();
        assert_eq!(t.record_compaction(), None); // 2nd: gap 1 < M=2, but count 2 < N=3
        t.record_assistant_turn();
        let detail = t.record_compaction().expect("3rd compaction should thrash");
        assert_eq!(detail.compaction_count, 3);
        assert_eq!(detail.turns_since_previous_compaction, 1);
        assert_eq!(detail.max_compactions, 3);
        assert_eq!(detail.min_turn_gap, 2);
    }

    #[test]
    fn a_gap_exactly_at_the_threshold_does_not_thrash() {
        // M=2: a gap of exactly 2 turns is healthy, not thrash -- the rule is
        // a strict "fewer than M", not "at most M".
        let mut t = ThrashTracker::new(thresholds(3, 2));
        let _ = t.record_compaction();
        t.record_assistant_turn();
        let _ = t.record_compaction();
        t.record_assistant_turn();
        t.record_assistant_turn();
        assert_eq!(t.record_compaction(), None);
    }

    #[test]
    fn a_gap_one_below_the_threshold_thrashes() {
        // M=2: a gap of exactly 1 turn is the boundary case that does thrash.
        let mut t = ThrashTracker::new(thresholds(3, 2));
        let _ = t.record_compaction();
        t.record_assistant_turn();
        let _ = t.record_compaction();
        t.record_assistant_turn();
        let detail = t.record_compaction().expect("gap of 1 < M=2 should thrash");
        assert_eq!(detail.turns_since_previous_compaction, 1);
    }

    #[test]
    fn once_thrashed_a_later_compaction_can_thrash_again() {
        let mut t = ThrashTracker::new(thresholds(3, 2));
        let _ = t.record_compaction();
        let _ = t.record_compaction();
        assert!(t.record_compaction().is_some()); // 3rd, gap 0
        assert!(t.record_compaction().is_some()); // 4th, gap 0 again
    }

    #[test]
    fn custom_thresholds_are_honored() {
        let mut t = ThrashTracker::new(thresholds(2, 5));
        assert_eq!(t.record_compaction(), None); // 1st: no preceding
        for _ in 0..4 {
            t.record_assistant_turn();
        }
        // Gap of 4 < M=5, count 2 >= N=2 -> thrash.
        let detail = t
            .record_compaction()
            .expect("should thrash under custom thresholds");
        assert_eq!(detail.compaction_count, 2);
        assert_eq!(detail.turns_since_previous_compaction, 4);
    }

    #[test]
    fn custom_thresholds_with_a_healthy_gap_do_not_thrash() {
        let mut t = ThrashTracker::new(thresholds(2, 5));
        let _ = t.record_compaction();
        for _ in 0..5 {
            t.record_assistant_turn();
        }
        assert_eq!(t.record_compaction(), None);
    }

    #[test]
    fn thrash_error_message_is_backend_neutral_and_not_derived_from_any_backend_prose() {
        let detail = ThrashDetail {
            compaction_count: 3,
            turns_since_previous_compaction: 1,
            max_compactions: 3,
            min_turn_gap: 2,
        };
        let msg = thrash_error_message("pi", &detail);
        assert!(msg.contains("pi"));
        assert!(msg.contains('3'));
        assert!(msg.contains("autocompaction thrashing"));
    }
}
