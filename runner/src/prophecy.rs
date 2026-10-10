//! Runner-side half of the prophecy subsystem (`docs/prophecy-design.md`,
//! phase 2): recognizing a `RALPHUS_PROPHECY:` marker in the agent's final
//! reply text. Mirrors `execute.rs::parse_ghost`'s marker-parsing shape, with
//! one deliberate difference -- a prophecy is append-only (§3 of the design
//! doc), so [`parse_prophecies`] collects *every* matching line rather than
//! just the last one the way `parse_ghost` does for its single handoff blob.
//!
//! Kept as its own module (like `cartographer.rs`/`thrash.rs`) rather than
//! folded into `execute.rs`, since [`ProphecyMarker`] is also the shape
//! serialized onto `CellResult`/`RunnerResult` (`spec.rs`) as the at-exit
//! backstop transport that crosses the daemon<->runner provider boundary --
//! see §6.1 of the design doc.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

/// The marker every standalone prophecy line starts with. Must be matched as
/// the start of a trimmed line, never a substring scan -- the same
/// false-positive hazard `daemon/src/runner.rs::pane_shows_done_sentinel`
/// documents for `RALPHUS_TMUX_DONE`: an agent whose *work* is about this
/// exact mechanism (ralphus agents develop ralphus) can otherwise trip the
/// parser reading its own output. Kept in sync with
/// `docs/special-syntax.md`.
const PROPHECY_MARKER: &str = "RALPHUS_PROPHECY:";

/// Closed set of recognized kinds, mirrored from
/// `daemon/src/prophecy.rs::ProphecyKind` (§11.1 of the design doc) --
/// duplicated as plain strings rather than shared as a type, since this
/// crate has no dependency on `ralphus-daemon`. A line naming any other
/// kind is treated as malformed and dropped, the same way the daemon drops
/// an unparseable `RALPHUS_EVENT:` payload rather than guessing.
const KNOWN_KINDS: [&str; 5] = ["discovery", "decision", "hazard", "deferred", "unconfirmed"];

/// The `RALPHUS_EVENT` message a backend emits for each newly found marker, so
/// the daemon files it the moment the agent writes it rather than only when
/// the run exits. `daemon/src/runner.rs::PROPHECY_MESSAGE` keys on this exact
/// string, so the two must stay in sync. Payload: `{kind, body}`.
pub const PROPHECY_MESSAGE: &str = "prophecy";

/// One `RALPHUS_PROPHECY:` marker found in the agent's reply -- the wire
/// shape carried on `CellResult`/`RunnerResult.prophecies`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProphecyMarker {
    /// One of [`KNOWN_KINDS`].
    pub kind: String,
    pub body: String,
}

/// Scans `text` for every standalone line of the exact form
/// `RALPHUS_PROPHECY: <kind>: <note>`. Unrecognized `kind`, a line with no
/// `:` separator, or an empty note are dropped silently -- the runner has no
/// daemon connection of its own to log a warning to, and a single malformed
/// line must never fail the whole cell.
#[must_use]
pub fn parse_prophecies(text: &str) -> Vec<ProphecyMarker> {
    let prefix = format!("{PROPHECY_MARKER} ");
    text.lines()
        .filter_map(|line| {
            let rest = line.trim_start().strip_prefix(&prefix)?;
            let (kind, body) = rest.split_once(':')?;
            let kind = kind.trim().to_lowercase();
            if !KNOWN_KINDS.contains(&kind.as_str()) {
                return None;
            }
            let body = body.trim();
            if body.is_empty() {
                return None;
            }
            Some(ProphecyMarker {
                kind,
                body: body.to_string(),
            })
        })
        .collect()
}

/// Incremental, deduplicating marker scan over a run's assistant text.
///
/// A backend feeds it each assistant text block as that block is parsed, so
/// every byte is read once and a marker is reported (and emitted as a live
/// [`PROPHECY_MESSAGE`] event) the moment it is written -- it survives a
/// compaction, cancel, timeout or kill that would lose a closing message.
/// Only ever hand it assistant-authored text: tool results and echoed
/// prompts must not be scanned, or a tool printing a marker line would be
/// recorded as an insight.
///
/// A marker already seen (same kind and whitespace-normalized body) is
/// ignored, so one restated in the closing message, replayed by a resumed
/// session, or re-extracted from a terminal event yields a single entry.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ProphecyScanner {
    seen: HashSet<(String, String)>,
    markers: Vec<ProphecyMarker>,
}

impl ProphecyScanner {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Scans `text`, records every marker not seen before and returns just
    /// those new ones.
    pub fn scan(&mut self, text: &str) -> Vec<ProphecyMarker> {
        let mut fresh = Vec::new();
        for marker in parse_prophecies(text) {
            let key = (
                marker.kind.clone(),
                marker.body.split_whitespace().collect::<Vec<_>>().join(" "),
            );
            if self.seen.insert(key) {
                self.markers.push(marker.clone());
                fresh.push(marker);
            }
        }
        fresh
    }

    /// As [`Self::scan`], and emits each new marker as a [`PROPHECY_MESSAGE`]
    /// event attributed to `source`. The daemon fills in squad/task/cell from
    /// the owning spec, like it does for live usage.
    pub fn scan_and_emit(&mut self, source: &str, text: &str) {
        for marker in self.scan(text) {
            crate::cartographer::emit(
                source,
                PROPHECY_MESSAGE,
                "info",
                crate::cartographer::EventContext::default(),
                serde_json::json!({"kind": marker.kind, "body": marker.body}),
            );
        }
    }

    /// Every distinct marker found so far, in the order found.
    #[must_use]
    pub fn markers(&self) -> Vec<ProphecyMarker> {
        self.markers.clone()
    }

    /// Folds already-found markers (a backend invocation's, a later round's)
    /// into this scanner without emitting them again.
    pub fn absorb(&mut self, markers: &[ProphecyMarker]) {
        for marker in markers {
            self.scan(&format!(
                "{PROPHECY_MARKER} {}: {}",
                marker.kind, marker.body
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_one_marker() {
        let markers = parse_prophecies(
            "did some work\nRALPHUS_PROPHECY: hazard: left a race condition unresolved",
        );
        assert_eq!(
            markers,
            vec![ProphecyMarker {
                kind: "hazard".to_string(),
                body: "left a race condition unresolved".to_string(),
            }]
        );
    }

    #[test]
    fn collects_every_marker_not_just_the_last() {
        let markers = parse_prophecies(
            "RALPHUS_PROPHECY: discovery: found the real cause\n\
             more text\n\
             RALPHUS_PROPHECY: decision: took the simpler fix",
        );
        assert_eq!(markers.len(), 2);
        assert_eq!(markers[0].kind, "discovery");
        assert_eq!(markers[1].kind, "decision");
    }

    #[test]
    fn no_marker_is_empty() {
        assert!(parse_prophecies("nothing to see here").is_empty());
    }

    #[test]
    fn unrecognized_kind_is_dropped() {
        assert!(parse_prophecies("RALPHUS_PROPHECY: mystery: not a real kind").is_empty());
    }

    #[test]
    fn missing_colon_separator_is_dropped() {
        assert!(parse_prophecies("RALPHUS_PROPHECY: just some text with no kind").is_empty());
    }

    #[test]
    fn empty_body_is_dropped() {
        assert!(parse_prophecies("RALPHUS_PROPHECY: hazard:").is_empty());
    }

    #[test]
    fn substring_occurrence_is_not_a_match() {
        // Mirrors `pane_shows_done_sentinel`'s own false-positive test: the
        // marker text appearing mid-line (e.g. quoted in prose, or as part
        // of a grep/Read result) must not be mistaken for a real marker.
        assert!(
            parse_prophecies("see RALPHUS_PROPHECY: hazard: nope, still mid-sentence").is_empty()
        );
        assert!(
            parse_prophecies("grep result: 42:RALPHUS_PROPHECY: hazard: prefixed, not standalone")
                .is_empty()
        );
    }

    #[test]
    fn parses_the_unconfirmed_kind() {
        let markers =
            parse_prophecies("RALPHUS_PROPHECY: unconfirmed: the regression test could not run");
        assert_eq!(markers.len(), 1);
        assert_eq!(markers[0].kind, "unconfirmed");
    }

    #[test]
    fn leading_whitespace_is_still_standalone() {
        let markers = parse_prophecies("   RALPHUS_PROPHECY: deferred: left for later");
        assert_eq!(markers.len(), 1);
        assert_eq!(markers[0].kind, "deferred");
    }

    #[test]
    fn scanner_reports_only_new_markers() {
        let mut scanner = ProphecyScanner::new();
        let first = scanner.scan("RALPHUS_PROPHECY: hazard: left a race");
        assert_eq!(first.len(), 1);
        let again = scanner.scan("later\nRALPHUS_PROPHECY: hazard:   left  a race\n");
        assert!(again.is_empty(), "a restated marker is not new");
        let other = scanner.scan("RALPHUS_PROPHECY: decision: left a race");
        assert_eq!(other.len(), 1, "a different kind is a different marker");
        assert_eq!(scanner.markers().len(), 2);
    }

    #[test]
    fn scanner_absorb_dedupes_against_found_markers() {
        let mut scanner = ProphecyScanner::new();
        scanner.scan("RALPHUS_PROPHECY: discovery: a");
        scanner.absorb(&[
            ProphecyMarker {
                kind: "discovery".into(),
                body: "a".into(),
            },
            ProphecyMarker {
                kind: "hazard".into(),
                body: "b".into(),
            },
        ]);
        assert_eq!(scanner.markers().len(), 2);
    }
}
