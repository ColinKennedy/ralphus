//! RAL-339/RAL-435: shared, purpose-neutral "N occurrences within M turns"
//! thrash detector. Originally written for `runner`'s autocompaction thrash
//! guard (`runner::thrash`, which now wraps this module -- see its own doc
//! comment), and reused verbatim here for `daemon`'s Pi rate-limit retry
//! guard (RAL-435): both are the exact same rule -- an occurrence at/after
//! the Nth time, whose immediately preceding occurrence happened fewer than M
//! "turns" ago, counts as thrashing -- applied to a different kind of
//! occurrence (a compaction vs. a rate-limit retry). Living in `ralphus-core`
//! lets both `runner` and `daemon` share one implementation and one set of
//! default constants despite neither crate depending on the other.

/// N: how many occurrences must happen in one tracked window before thrash
/// detection can fire at all, absent an override.
pub const DEFAULT_MAX_OCCURRENCES: u32 = 3;
/// M: the previous-occurrence gap (in "turns") below which an occurrence
/// at/after [`DEFAULT_MAX_OCCURRENCES`] counts as thrash, absent an override.
pub const DEFAULT_MIN_TURN_GAP: u32 = 2;

/// Resolved N/M thresholds for one tracked window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OccurrenceThresholds {
    pub max_occurrences: u32,
    pub min_turn_gap: u32,
}

impl Default for OccurrenceThresholds {
    fn default() -> Self {
        Self {
            max_occurrences: DEFAULT_MAX_OCCURRENCES,
            min_turn_gap: DEFAULT_MIN_TURN_GAP,
        }
    }
}

/// Diagnostic detail for a detected thrash condition.
#[derive(Debug, Clone, PartialEq)]
pub struct OccurrenceDetail {
    pub occurrence_count: u32,
    pub turns_since_previous_occurrence: u32,
    pub max_occurrences: u32,
    pub min_turn_gap: u32,
}

/// Tracks occurrence count + turns-since-previous-occurrence for one tracked
/// window. The caller decides what a "turn" and an "occurrence" mean --
/// assistant turns and compactions for `runner::thrash`, cell dispatch
/// attempts and rate-limit retries for `daemon`'s rate-limit guard -- and
/// drives this through the same two calls, [`Self::record_turn`] and
/// [`Self::record_occurrence`], so the thrash rule itself lives in exactly
/// one place.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct OccurrenceTracker {
    thresholds: OccurrenceThresholds,
    occurrence_count: u32,
    turns_since_last_occurrence: u32,
}

impl OccurrenceTracker {
    #[must_use]
    pub fn new(thresholds: OccurrenceThresholds) -> Self {
        Self {
            thresholds,
            occurrence_count: 0,
            turns_since_last_occurrence: 0,
        }
    }

    pub fn record_turn(&mut self) {
        self.turns_since_last_occurrence = self.turns_since_last_occurrence.saturating_add(1);
    }

    /// Records one occurrence. Returns `Some(detail)` the moment the tracked
    /// window crosses into thrash: this is (at least) the Nth occurrence
    /// (`max_occurrences`) and its immediately preceding occurrence happened
    /// fewer than M turns ago (`min_turn_gap`). The very first occurrence
    /// never thrashes -- there is no preceding occurrence to compare its gap
    /// against.
    #[must_use]
    pub fn record_occurrence(&mut self) -> Option<OccurrenceDetail> {
        self.occurrence_count += 1;
        let gap = self.turns_since_last_occurrence;
        self.turns_since_last_occurrence = 0;

        let has_preceding_occurrence = self.occurrence_count > 1;
        if has_preceding_occurrence
            && self.occurrence_count >= self.thresholds.max_occurrences
            && gap < self.thresholds.min_turn_gap
        {
            Some(OccurrenceDetail {
                occurrence_count: self.occurrence_count,
                turns_since_previous_occurrence: gap,
                max_occurrences: self.thresholds.max_occurrences,
                min_turn_gap: self.thresholds.min_turn_gap,
            })
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn thresholds(max_occurrences: u32, min_turn_gap: u32) -> OccurrenceThresholds {
        OccurrenceThresholds {
            max_occurrences,
            min_turn_gap,
        }
    }

    #[test]
    fn a_single_isolated_occurrence_never_thrashes() {
        let mut t = OccurrenceTracker::new(thresholds(3, 2));
        assert_eq!(t.record_occurrence(), None);
    }

    #[test]
    fn healthy_cadence_with_many_occurrences_never_thrashes() {
        let mut t = OccurrenceTracker::new(thresholds(3, 2));
        for _ in 0..10 {
            t.record_turn();
            t.record_turn();
            assert_eq!(t.record_occurrence(), None);
        }
    }

    #[test]
    fn fewer_than_n_occurrences_never_thrashes_even_with_a_zero_gap() {
        let mut t = OccurrenceTracker::new(thresholds(3, 2));
        assert_eq!(t.record_occurrence(), None); // 1st: no preceding occurrence
        assert_eq!(t.record_occurrence(), None); // 2nd: gap 0, but count 2 < N=3
    }

    #[test]
    fn the_nth_occurrence_with_a_gap_under_m_thrashes() {
        let mut t = OccurrenceTracker::new(thresholds(3, 2));
        assert_eq!(t.record_occurrence(), None); // 1st
        t.record_turn();
        assert_eq!(t.record_occurrence(), None); // 2nd: gap 1 < M=2, but count 2 < N=3
        t.record_turn();
        let detail = t.record_occurrence().expect("3rd occurrence should thrash");
        assert_eq!(detail.occurrence_count, 3);
        assert_eq!(detail.turns_since_previous_occurrence, 1);
        assert_eq!(detail.max_occurrences, 3);
        assert_eq!(detail.min_turn_gap, 2);
    }

    #[test]
    fn a_gap_exactly_at_the_threshold_does_not_thrash() {
        let mut t = OccurrenceTracker::new(thresholds(3, 2));
        let _ = t.record_occurrence();
        t.record_turn();
        let _ = t.record_occurrence();
        t.record_turn();
        t.record_turn();
        assert_eq!(t.record_occurrence(), None);
    }

    #[test]
    fn a_gap_one_below_the_threshold_thrashes() {
        let mut t = OccurrenceTracker::new(thresholds(3, 2));
        let _ = t.record_occurrence();
        t.record_turn();
        let _ = t.record_occurrence();
        t.record_turn();
        let detail = t.record_occurrence().expect("gap of 1 < M=2 should thrash");
        assert_eq!(detail.turns_since_previous_occurrence, 1);
    }

    #[test]
    fn once_thrashed_a_later_occurrence_can_thrash_again() {
        let mut t = OccurrenceTracker::new(thresholds(3, 2));
        let _ = t.record_occurrence();
        let _ = t.record_occurrence();
        assert!(t.record_occurrence().is_some()); // 3rd, gap 0
        assert!(t.record_occurrence().is_some()); // 4th, gap 0 again
    }

    #[test]
    fn custom_thresholds_are_honored() {
        let mut t = OccurrenceTracker::new(thresholds(2, 5));
        assert_eq!(t.record_occurrence(), None); // 1st: no preceding
        for _ in 0..4 {
            t.record_turn();
        }
        let detail = t
            .record_occurrence()
            .expect("should thrash under custom thresholds");
        assert_eq!(detail.occurrence_count, 2);
        assert_eq!(detail.turns_since_previous_occurrence, 4);
    }

    #[test]
    fn custom_thresholds_with_a_healthy_gap_do_not_thrash() {
        let mut t = OccurrenceTracker::new(thresholds(2, 5));
        let _ = t.record_occurrence();
        for _ in 0..5 {
            t.record_turn();
        }
        assert_eq!(t.record_occurrence(), None);
    }
}
