//! RAL-536: detects a thinking-capable agent's `RALPHUS_THINKING:`-tagged
//! output (RAL-434, `crate::runner::THINKING_MARKER`) degenerating into a
//! repetition loop -- the model producing near-identical "Hmm." / "Hmph." /
//! "H-mm" style filler forever instead of making progress. This mirrors
//! RAL-241 (pane-quiet, `SubprocessRunner::check_stall_escalation`) and
//! RAL-308 (CPU-flat, [`crate::cpu_stall`]) as a third, independent stall
//! signal: none of the three shares state or supersedes another, since each
//! catches a different failure mode (pane silence, CPU idling, content
//! repetition despite the pane staying visibly busy).
//!
//! # Definition
//!
//! Detection is content-based, not marker-count-based: a rolling window of
//! the most recent [`crate::config::AgentHealthConfig::thinking_stall_window_lines`]
//! thinking lines is kept, and every time the window is full, this computes
//! the ratio of distinct words to total words across it (after excluding any
//! line that is itself boilerplate -- see [`is_stoplisted`]). A ratio under
//! [`crate::config::AgentHealthConfig::thinking_stall_diversity_threshold`]
//! counts as one low-diversity sample. Tripping requires
//! [`crate::config::AgentHealthConfig::thinking_stall_min_consecutive_samples`]
//! *consecutive* low-diversity samples spanning at least
//! [`crate::config::AgentHealthConfig::thinking_stall_min_span_ms`] -- both
//! guard against a short burst of naturally repetitive phrasing (e.g.
//! acknowledging a tool result the same way twice) reading as a stall.
//!
//! # Reset behavior
//!
//! Per the RAL-536 interview, three things reset the consecutive-sample
//! streak to zero:
//!
//! - [`ThinkingStallDetector::reset`] -- called by the caller when a tool
//!   call or any other non-thinking output is observed. Tool-call lines are
//!   a wholly separate event stream (`RALPHUS_EVENT:`-marked) from thinking
//!   lines; this type only ever sees a line the caller explicitly routes to
//!   [`ThinkingStallDetector::observe_thinking_line`]. A caller that instead
//!   routes tool-call/poll-loop lines through `reset()` is what makes a
//!   legitimate repeated tool call (explicitly out of scope for this
//!   detector) unable to ever trip it -- nothing in this module enforces
//!   that separation on its own, it only trusts the caller's routing.
//! - A sample whose ratio is back at or above the threshold ("materially
//!   changed thinking") also resets the streak, inline, without needing an
//!   explicit `reset()` call.
//!
//! This per-attempt state (the window and the streak) is intentionally
//! *not* persisted in `store_memory.rs`: it is scoped to one running attempt
//! (one `run_via_tmux_attempt` call), held as a plain local variable, and
//! discarded when that attempt ends -- exactly like `TranscriptTailer`. What
//! *does* need to persist across attempts -- the three-consecutive-
//! automatic-restarts counter the RAL-536 interview added on top of this
//! detector -- lives in [`crate::store_memory::StoreMemory`] instead, since
//! it must survive the very restart this detector triggers, and is reset
//! only by a human resume/restart/retry action, never by an automatic one.
//!
//! # Scope
//!
//! This module has no notion of cells, proofs, or review/guardian-merge
//! agents -- it only ever sees a stream of lines and timestamps. The scope
//! decision (which kinds of tagged-thinking agent work get checked) lives
//! entirely in the callers that construct a detector and feed it lines:
//! `crate::runner::SubprocessRunner::run_via_tmux_attempt` is the single
//! choke point that covers cells, proof steps, and guardian-merge agents
//! uniformly, since all three are just different `RunnerSpec`s run through
//! that one function.

use std::collections::{HashSet, VecDeque};

/// Phrases the `pi` backend is known to restate verbatim from its own system
/// prompt or tool-use scaffolding -- naming a tool it's about to invoke,
/// echoing an instruction back, or other constant boilerplate that is not
/// genuine reasoning content. A thinking line that contains one of these
/// phrases is excluded from the rolling window entirely (never counted as
/// either a distinct-content sample or a repeat), so restating the same
/// instruction across several lines is never mistaken for the model looping
/// on its own output.
///
/// Necessarily incomplete and backend-prompt-specific: if `pi`'s system
/// prompt wording changes, this list needs a matching update or it will
/// under- (never over-) protect. A stale/missing entry can only ever make
/// the detector see *more* lines as real content, never fewer -- it cannot
/// cause a false trip on its own, only a missed one -- so the failure mode
/// of letting this list go stale is reduced sensitivity, not false
/// escalation.
const STOPLIST: &[&str] = &[
    "let me think about this step by step",
    "i will use the available tools to accomplish this task",
    "i have access to the following tools",
    "let me check the tool results",
    "i should follow the system prompt instructions",
    "let me re-read the instructions",
    "i need to use a tool to proceed",
    "let me proceed step by step",
    "according to my instructions",
    "i am an autonomous coding agent",
];

/// True when `line`, once trimmed and casefolded, is boilerplate rather than
/// real reasoning content -- either empty, or containing one of
/// [`STOPLIST`]'s known-constant phrases.
#[must_use]
pub fn is_stoplisted(line: &str) -> bool {
    let normalized = line.trim().to_lowercase();
    if normalized.is_empty() {
        return true;
    }
    STOPLIST.iter().any(|phrase| normalized.contains(phrase))
}

/// Split `line` into lowercased, punctuation-stripped words. Punctuation is
/// stripped from *within* a whitespace-delimited word (not split on), so a
/// stylized near-duplicate like `"H-mm"` normalizes to the same token as
/// `"Hmm"` -- the point of the whole detector is catching exactly this kind
/// of not-quite-identical filler.
fn words(line: &str) -> Vec<String> {
    line.split_whitespace()
        .map(|w| {
            w.chars()
                .filter(|c| c.is_alphanumeric())
                .collect::<String>()
                .to_lowercase()
        })
        .filter(|w| !w.is_empty())
        .collect()
}

/// One trip event: every threshold satisfied on this line.
#[derive(Debug, Clone, PartialEq)]
pub struct StallSample {
    /// The most recent non-boilerplate thinking line observed -- the "last
    /// meaningful pre-stall output" carried into an automatic restart's
    /// recovery context.
    pub last_line: String,
    pub consecutive_samples: u32,
    pub span_ms: i64,
}

/// Per-attempt rolling-window vocabulary-diversity tracker. See the module
/// doc for the full algorithm and reset rules.
pub struct ThinkingStallDetector {
    window_lines: usize,
    diversity_threshold: f64,
    min_consecutive_samples: u32,
    min_span_ms: i64,
    window: VecDeque<String>,
    consecutive_low_diversity: u32,
    streak_since_ms: i64,
    last_line: Option<String>,
}

impl ThinkingStallDetector {
    #[must_use]
    pub fn new(config: &crate::config::AgentHealthConfig) -> Self {
        Self {
            window_lines: config.thinking_stall_window_lines(),
            diversity_threshold: config.thinking_stall_diversity_threshold(),
            min_consecutive_samples: config.thinking_stall_min_consecutive_samples(),
            min_span_ms: config.thinking_stall_min_span_ms(),
            window: VecDeque::new(),
            consecutive_low_diversity: 0,
            streak_since_ms: 0,
            last_line: None,
        }
    }

    /// A tool call or any other non-thinking output was observed -- resets
    /// the low-diversity streak and clears the rolling window, since content
    /// preceding a real action is no longer relevant to "is the model
    /// looping on itself right now".
    pub fn reset(&mut self) {
        self.window.clear();
        self.consecutive_low_diversity = 0;
        self.streak_since_ms = 0;
    }

    /// Feed one `RALPHUS_THINKING:`-tagged line (marker already stripped)
    /// observed at `now_ms`. Returns `Some` the moment every trip condition
    /// -- window full, ratio under threshold, enough consecutive samples,
    /// enough elapsed span -- is first satisfied.
    pub fn observe_thinking_line(&mut self, line: &str, now_ms: i64) -> Option<StallSample> {
        if is_stoplisted(line) {
            return None;
        }
        self.last_line = Some(line.to_string());
        if self.window.len() == self.window_lines {
            self.window.pop_front();
        }
        self.window.push_back(line.to_string());
        if self.window.len() < self.window_lines {
            return None;
        }

        let mut total = 0usize;
        let mut distinct = HashSet::new();
        for w in self.window.iter().flat_map(|l| words(l)) {
            total += 1;
            distinct.insert(w);
        }
        if total == 0 {
            return None;
        }
        let ratio = distinct.len() as f64 / total as f64;

        if ratio >= self.diversity_threshold {
            // Materially changed thinking -- reset the streak but keep the
            // window itself (unlike `reset()`), since this is still genuine
            // thinking content, just diverse enough right now.
            self.consecutive_low_diversity = 0;
            return None;
        }

        if self.consecutive_low_diversity == 0 {
            self.streak_since_ms = now_ms;
        }
        self.consecutive_low_diversity += 1;
        let span_ms = now_ms - self.streak_since_ms;
        if self.consecutive_low_diversity < self.min_consecutive_samples
            || span_ms < self.min_span_ms
        {
            return None;
        }
        Some(StallSample {
            last_line: self.last_line.clone().unwrap_or_default(),
            consecutive_samples: self.consecutive_low_diversity,
            span_ms,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AgentHealthConfig;

    fn config(
        window_lines: i64,
        diversity_threshold: f64,
        min_consecutive_samples: i64,
        min_span_ms: i64,
    ) -> AgentHealthConfig {
        AgentHealthConfig {
            thinking_stall_window_lines: Some(window_lines),
            thinking_stall_diversity_threshold: Some(diversity_threshold),
            thinking_stall_min_consecutive_samples: Some(min_consecutive_samples),
            thinking_stall_min_span_ms: Some(min_span_ms),
        }
    }

    /// The RAL-536 motivating case: not byte-identical lines, but stylized
    /// near-duplicates of the same filler word -- must still trip.
    #[test]
    fn near_duplicate_thinking_lines_trip_the_detector() {
        let mut d = ThinkingStallDetector::new(&config(4, 0.6, 2, 0));
        let lines = ["Hmm.", "H-mm.", "Hmph.", "Hmm.", "H-mm.", "Hmph."];
        let mut tripped = false;
        let mut t = 0i64;
        for line in lines {
            t += 1000;
            if d.observe_thinking_line(line, t).is_some() {
                tripped = true;
                break;
            }
        }
        assert!(
            tripped,
            "near-duplicate thinking lines never tripped the detector"
        );
    }

    /// Tool-call lines are a wholly separate stream: a caller that routes
    /// them through `reset()` (as `run_via_tmux_attempt` does) instead of
    /// `observe_thinking_line` can never trip the detector on a repeated
    /// tool call / poll loop, even when the underlying text is exactly the
    /// content that trips the detector above when fed as thinking output.
    #[test]
    fn repeated_identical_tool_call_lines_never_trip_the_detector() {
        let mut never_fed_as_thinking = ThinkingStallDetector::new(&config(4, 0.6, 2, 0));
        for _ in 0..20 {
            // A repeated poll/tool-call line is routed through `reset()`,
            // never `observe_thinking_line` -- this is the routing contract
            // `run_via_tmux_attempt` relies on.
            never_fed_as_thinking.reset();
        }
        // No trip is even possible: the window was never fed a single line.

        // Sanity check: the exact same repeated content, if it *had* been
        // routed as thinking output, is able to trip -- proving the above
        // isn't just "the detector never trips at all".
        let mut fed_as_thinking = ThinkingStallDetector::new(&config(4, 0.6, 2, 0));
        let mut tripped = false;
        for i in 0..20 {
            if fed_as_thinking
                .observe_thinking_line("Hmm.", i * 1000)
                .is_some()
            {
                tripped = true;
            }
        }
        assert!(
            tripped,
            "sanity check failed: identical content fed as thinking must be able to trip"
        );
    }

    #[test]
    fn varied_thinking_output_never_trips() {
        let mut d = ThinkingStallDetector::new(&config(4, 0.6, 2, 0));
        let lines = [
            "Looking at the failing test, the assertion expects a sorted list.",
            "The bug is in the comparator -- it flips greater-than and less-than.",
            "Fixing the comparator now and re-running the test suite locally.",
            "Tests pass; checking whether other callers depended on the old order.",
            "No other callers found via grep, so this change is safe to land.",
        ];
        let mut t = 0i64;
        for line in lines {
            t += 1000;
            assert!(
                d.observe_thinking_line(line, t).is_none(),
                "varied thinking content incorrectly tripped the detector"
            );
        }
    }

    #[test]
    fn stoplisted_boilerplate_repeated_across_lines_never_trips() {
        let mut d = ThinkingStallDetector::new(&config(4, 0.6, 2, 0));
        let mut t = 0i64;
        for _ in 0..20 {
            t += 1000;
            assert!(
                d.observe_thinking_line("I have access to the following tools.", t)
                    .is_none(),
                "repeated stoplisted boilerplate incorrectly tripped the detector"
            );
        }
    }

    /// A mix of boilerplate and genuinely varied lines must not trip just
    /// because the boilerplate repeats -- the boilerplate lines are excluded
    /// from the window rather than counted as low-diversity content.
    #[test]
    fn stoplisted_boilerplate_interleaved_with_varied_content_never_trips() {
        let mut d = ThinkingStallDetector::new(&config(4, 0.6, 2, 0));
        let interleaved = [
            "I have access to the following tools.",
            "Reading the config module to find the default timeout value.",
            "I have access to the following tools.",
            "The default is thirty seconds, set in load_budget_config.",
            "I have access to the following tools.",
            "Updating the test to assert against that constant directly.",
        ];
        let mut t = 0i64;
        for line in interleaved {
            t += 1000;
            assert!(
                d.observe_thinking_line(line, t).is_none(),
                "boilerplate interleaved with varied content incorrectly tripped the detector"
            );
        }
    }

    /// `reset()` (a tool call / non-thinking output) must zero the
    /// consecutive-sample streak, not just delay reaching it -- proven by a
    /// streak that would trip if it continued past the reset, but does not.
    #[test]
    fn reset_clears_an_in_progress_low_diversity_streak() {
        let mut d = ThinkingStallDetector::new(&config(2, 0.6, 2, 0));
        // First low-diversity sample: window fills on the second line.
        assert!(d.observe_thinking_line("Hmm.", 1000).is_none());
        assert!(
            d.observe_thinking_line("H-mm.", 2000).is_none(),
            "one low-diversity sample must not trip a 2-consecutive-sample threshold"
        );

        d.reset();

        // If the streak had NOT been reset, this second low-diversity
        // sample would be the 2nd consecutive one and would trip. Since it
        // must not trip, this proves `reset()` zeroed the counter.
        assert!(d.observe_thinking_line("Hmm.", 3000).is_none());
        assert!(
            d.observe_thinking_line("H-mm.", 4000).is_none(),
            "reset() did not clear the consecutive-low-diversity streak"
        );
    }

    /// A sample whose ratio is back at/above the threshold ("materially
    /// changed thinking") resets the streak inline, without an explicit
    /// `reset()` call.
    #[test]
    fn materially_changed_thinking_resets_the_streak_without_an_explicit_reset() {
        let mut d = ThinkingStallDetector::new(&config(2, 0.6, 2, 0));
        assert!(d.observe_thinking_line("Hmm.", 1000).is_none());
        assert!(d.observe_thinking_line("H-mm.", 2000).is_none());

        // A genuinely different, diverse line resets the streak in place.
        assert!(
            d.observe_thinking_line("Reading the failing test output now.", 3000)
                .is_none()
        );

        // Back to low-diversity content: this must be sample 1 of a fresh
        // streak, not sample 3 of the old one, so it must not trip yet.
        assert!(d.observe_thinking_line("Hmm.", 4000).is_none());
        assert!(
            d.observe_thinking_line("H-mm.", 5000).is_none(),
            "a materially-diverse sample did not reset the low-diversity streak"
        );
    }

    #[test]
    fn min_span_ms_delays_tripping_until_the_streak_covers_enough_time() {
        let mut d = ThinkingStallDetector::new(&config(2, 0.6, 2, 10_000));
        assert!(d.observe_thinking_line("Hmm.", 0).is_none());
        assert!(
            d.observe_thinking_line("H-mm.", 1000).is_none(),
            "2 consecutive samples spanning only 1s must not trip a 10s minimum span"
        );
        assert!(
            d.observe_thinking_line("Hmm.", 12_000).is_some(),
            "2 consecutive samples spanning over 10s must trip"
        );
    }
}
