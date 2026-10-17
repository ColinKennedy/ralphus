//! RAL-678: log-based evidence that a PR/MR's failing CI check made progress
//! between two settled CI generations, even when the failing check set did not
//! shrink (a fail-fast test job stops at its first failure, so fixing one test
//! exposes the next while the check stays red).
//!
//! Everything here is pure: [`parse_log`] turns one check's log text into a
//! [`ParsedLog`], [`assess`] compares it with what the previous evaluated
//! generation recorded, and `Store::set_pr_failing_checks_with_evidence_at`
//! (in `pr.rs`) turns the assessment into a refund or a denial.
//!
//! Evidence levels, strongest first:
//! * **strong** -- a test that the earlier generation's log reported failed is
//!   explicitly reported passed in the same check's log, by the same parser.
//!   A test that merely disappears never qualifies.
//! * **weak** -- the completed-test count rose above the check's recorded
//!   high-water mark in a comparable suite (same parser, same stable total).
//!   Order never counts: only the number of completed tests does. Weak refunds
//!   are capped per campaign (`[review] auto_fix_weak_progress_refunds`).
//!
//! Anything unrecognised, ambiguous or unavailable yields no evidence.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

/// Parser names, as they appear in events and metric fields.
pub const PARSER_PYTEST: &str = "pytest";
pub const PARSER_NEXTEST: &str = "nextest";
pub const PARSER_NONE: &str = "none";

/// Decision rules (fixed set, safe as metric labels).
pub const RULE_STRICT_SUBSET: &str = "strict_subset";
pub const RULE_NAMED_TEST: &str = "named_test_transition";
pub const RULE_COMPLETED_COUNT: &str = "completed_count_increase";
pub const RULE_NONE: &str = "none";

/// Evidence levels (fixed set, safe as metric labels).
pub const EVIDENCE_FAILING_SET: &str = "failing_set";
pub const EVIDENCE_STRONG: &str = "strong";
pub const EVIDENCE_WEAK: &str = "weak";
pub const EVIDENCE_NONE: &str = "none";

/// One check's log reduced to test outcomes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedLog {
    pub parser: &'static str,
    pub passed: BTreeSet<String>,
    pub failed: BTreeSet<String>,
    /// Names reported with conflicting outcomes in this one log (retries,
    /// flaky tests, repeated names). Never usable as evidence.
    pub ambiguous: BTreeSet<String>,
    /// Suite size announced by the runner (`collected N items`,
    /// `Starting N tests`).
    pub total: Option<u64>,
}

/// What a check's log yielded for this evaluation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckLog {
    Parsed(ParsedLog),
    /// No log text could be fetched or retained.
    Unavailable,
    /// Log text existed but no known format was recognised (or two were).
    Unparsed,
}

/// One failing check's evidence for one generation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckEvidence {
    pub name: String,
    pub log: CheckLog,
    /// The local file whose contents were parsed (never the log itself).
    pub log_ref: Option<String>,
}

/// What is persisted per check between generations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Observation {
    pub parser: String,
    pub failed: BTreeSet<String>,
    pub completed: u64,
    pub total: Option<u64>,
}

/// Per-check best completed count in a comparable suite.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HighWater {
    pub parser: String,
    pub total: Option<u64>,
    pub completed: u64,
}

/// The persisted progress state (`auto_fix_progress` column).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProgressState {
    #[serde(default)]
    pub generation: Option<String>,
    #[serde(default)]
    pub checks: BTreeMap<String, Observation>,
    #[serde(default)]
    pub high_water: BTreeMap<String, HighWater>,
}

/// A check that earned a refund on its own evidence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Win {
    pub check: String,
    pub parser: &'static str,
    /// Human-readable previous/current observation for the event.
    pub previous: String,
    pub current: String,
}

/// The result of comparing the current generation's evidence with the stored
/// state.
#[derive(Debug, Default)]
pub struct Assessment {
    pub strong: Option<Win>,
    pub weak: Option<Win>,
    /// Why individual checks offered no evidence, e.g. `build: not_above_high_water`.
    pub rejections: Vec<String>,
    pub parser_failures: u32,
    pub logs_unavailable: u32,
    pub next: ProgressState,
}

impl Assessment {
    /// The parser to attribute a decision to when no win names one.
    #[must_use]
    pub fn first_parser(&self, evidence: &[CheckEvidence]) -> &'static str {
        evidence
            .iter()
            .find_map(|e| match &e.log {
                CheckLog::Parsed(p) => Some(p.parser),
                _ => None,
            })
            .unwrap_or(PARSER_NONE)
    }
}

/// Compare `evidence` (the current generation's failing checks) with `prev`.
/// `generation` stamps the returned state.
#[must_use]
pub fn assess(
    prev: &ProgressState,
    generation: Option<&str>,
    evidence: &[CheckEvidence],
) -> Assessment {
    let mut out = Assessment {
        next: ProgressState {
            generation: generation.map(str::to_string),
            checks: BTreeMap::new(),
            high_water: prev.high_water.clone(),
        },
        ..Assessment::default()
    };
    for ev in evidence {
        let parsed = match &ev.log {
            CheckLog::Parsed(p) => p,
            CheckLog::Unavailable => {
                out.logs_unavailable += 1;
                out.rejections.push(format!("{}: log_unavailable", ev.name));
                keep_previous(prev, &mut out.next, &ev.name);
                continue;
            }
            CheckLog::Unparsed => {
                out.parser_failures += 1;
                out.rejections
                    .push(format!("{}: unknown_log_format", ev.name));
                keep_previous(prev, &mut out.next, &ev.name);
                continue;
            }
        };
        let completed = parsed.passed.len() as u64;
        let previous = prev.checks.get(&ev.name);
        let high = prev.high_water.get(&ev.name);

        match previous {
            None => out
                .rejections
                .push(format!("{}: no_previous_observation", ev.name)),
            Some(p) if p.parser != parsed.parser => {
                out.rejections.push(format!("{}: parser_changed", ev.name))
            }
            Some(p) => {
                let transitioned: Vec<&String> = p
                    .failed
                    .iter()
                    .filter(|n| parsed.passed.contains(*n) && !parsed.ambiguous.contains(*n))
                    .collect();
                if let Some(first) = transitioned.first() {
                    if out.strong.is_none() {
                        out.strong = Some(Win {
                            check: ev.name.clone(),
                            parser: parsed.parser,
                            previous: format!("{} failed test(s)", p.failed.len()),
                            current: format!(
                                "{} previously failed test(s) now pass, e.g. {first}",
                                transitioned.len()
                            ),
                        });
                    }
                } else {
                    match comparable_total(p.total, parsed.total, high, parsed.parser) {
                        Err(reason) => out.rejections.push(format!("{}: {reason}", ev.name)),
                        Ok(()) => {
                            let mark = high.map_or(p.completed, |h| h.completed.max(p.completed));
                            if completed > mark {
                                if out.weak.is_none() {
                                    out.weak = Some(Win {
                                        check: ev.name.clone(),
                                        parser: parsed.parser,
                                        previous: format!(
                                            "high-water {mark} of {}",
                                            parsed.total.unwrap_or_default()
                                        ),
                                        current: format!(
                                            "{completed} of {}",
                                            parsed.total.unwrap_or_default()
                                        ),
                                    });
                                }
                            } else {
                                out.rejections
                                    .push(format!("{}: not_above_high_water", ev.name));
                            }
                        }
                    }
                }
            }
        }

        // Record this generation's observation and advance the high-water
        // mark -- or restart it when the suite is no longer comparable.
        out.next.checks.insert(
            ev.name.clone(),
            Observation {
                parser: parsed.parser.to_string(),
                failed: parsed.failed.clone(),
                completed,
                total: parsed.total,
            },
        );
        let carried = high
            .filter(|h| h.parser == parsed.parser && h.total.is_some() && h.total == parsed.total)
            .map_or(completed, |h| h.completed.max(completed));
        out.next.high_water.insert(
            ev.name.clone(),
            HighWater {
                parser: parsed.parser.to_string(),
                total: parsed.total,
                completed: carried,
            },
        );
    }
    out
}

fn keep_previous(prev: &ProgressState, next: &mut ProgressState, name: &str) {
    if let Some(obs) = prev.checks.get(name) {
        next.checks.insert(name.to_string(), obs.clone());
    }
}

/// Weak evidence needs a stable, known suite size across the previous
/// observation, the high-water mark and the current log.
fn comparable_total(
    previous: Option<u64>,
    current: Option<u64>,
    high: Option<&HighWater>,
    parser: &str,
) -> Result<(), &'static str> {
    let (Some(p), Some(c)) = (previous, current) else {
        return Err("suite_total_missing");
    };
    if p != c {
        return Err("suite_total_changed");
    }
    if let Some(h) = high {
        if h.parser != parser || h.total != Some(c) {
            return Err("suite_total_changed");
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Log parsing
// ---------------------------------------------------------------------------

/// Parse one check's log. `None` means no known format was recognised (or two
/// formats were mixed) -- the caller treats that as no evidence.
#[must_use]
pub fn parse_log(text: &str) -> Option<ParsedLog> {
    let mut pytest = Outcomes::default();
    let mut nextest = Outcomes::default();
    let mut pytest_total = None;
    let mut nextest_total = None;
    for raw in text.lines() {
        let line = clean_line(raw);
        if let Some(n) = pytest_collected(&line) {
            pytest_total.get_or_insert(n);
        }
        if let Some(n) = nextest_starting(&line) {
            nextest_total.get_or_insert(n);
        }
        if let Some((name, passed)) = pytest_outcome(&line) {
            pytest.record(name, passed);
        } else if let Some((name, passed)) = nextest_outcome(&line) {
            nextest.record(name, passed);
        }
    }
    let (parser, outcomes, total) = match (pytest.is_empty(), nextest.is_empty()) {
        (false, true) => (PARSER_PYTEST, pytest, pytest_total),
        (true, false) => (PARSER_NEXTEST, nextest, nextest_total),
        _ => return None,
    };
    Some(ParsedLog {
        parser,
        passed: outcomes.passed,
        failed: outcomes.failed,
        ambiguous: outcomes.ambiguous,
        total,
    })
}

#[derive(Default)]
struct Outcomes {
    passed: BTreeSet<String>,
    failed: BTreeSet<String>,
    ambiguous: BTreeSet<String>,
}

impl Outcomes {
    fn is_empty(&self) -> bool {
        self.passed.is_empty() && self.failed.is_empty() && self.ambiguous.is_empty()
    }

    fn record(&mut self, name: String, passed: bool) {
        if self.ambiguous.contains(&name) {
            return;
        }
        let (same, other) = if passed {
            (&mut self.passed, &mut self.failed)
        } else {
            (&mut self.failed, &mut self.passed)
        };
        if other.remove(&name) {
            self.ambiguous.insert(name);
        } else {
            same.insert(name);
        }
    }
}

/// Strip ANSI escapes and a leading GitHub Actions timestamp.
fn clean_line(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for n in chars.by_ref() {
                    if n.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
        } else {
            out.push(c);
        }
    }
    let trimmed = out.trim();
    if let Some((head, rest)) = trimmed.split_once(' ') {
        let looks_like_timestamp = head.len() >= 20
            && head.ends_with('Z')
            && head.as_bytes().get(4) == Some(&b'-')
            && head.contains('T');
        if looks_like_timestamp {
            return rest.trim().to_string();
        }
    }
    trimmed.to_string()
}

fn pytest_collected(line: &str) -> Option<u64> {
    let rest = line.strip_prefix("collected ")?;
    let n = rest.split_whitespace().next()?;
    // "collected 10 items" / "collected 1 item"; "collected 0 items" is no suite.
    let value = n.parse::<u64>().ok()?;
    (rest.contains("item") && value > 0).then_some(value)
}

fn nextest_starting(line: &str) -> Option<u64> {
    let rest = line.strip_prefix("Starting ")?;
    let mut parts = rest.split_whitespace();
    let n = parts.next()?.parse::<u64>().ok()?;
    (parts.next()?.starts_with("test") && n > 0).then_some(n)
}

/// `path::test PASSED [ 10%]`, `[gw0] [ 10%] PASSED path::test`, or the short
/// summary's `FAILED path::test - message`.
fn pytest_outcome(line: &str) -> Option<(String, bool)> {
    let tokens: Vec<&str> = line.split_whitespace().collect();
    for (i, token) in tokens.iter().enumerate() {
        let passed = match *token {
            "PASSED" => true,
            "FAILED" | "ERROR" => false,
            _ => continue,
        };
        let is_id = |t: &str| t.contains("::") && !t.starts_with('[');
        let before = i.checked_sub(1).and_then(|j| tokens.get(j));
        let name = before
            .filter(|t| is_id(t))
            .or_else(|| tokens.get(i + 1).filter(|t| is_id(t)))?;
        return Some(((*name).to_string(), passed));
    }
    None
}

/// `PASS [   0.012s] crate::bin test_name` / `FAIL [...] ...`. Retry (`TRY n`),
/// `FLAKY` and signal lines are deliberately not outcomes.
fn nextest_outcome(line: &str) -> Option<(String, bool)> {
    let (word, rest) = line.split_once(' ')?;
    let passed = match word {
        "PASS" => true,
        "FAIL" => false,
        _ => return None,
    };
    let rest = rest.trim_start();
    let rest = rest.strip_prefix('[')?;
    let (_, name) = rest.split_once(']')?;
    let name = name.split_whitespace().collect::<Vec<_>>().join(" ");
    (!name.is_empty()).then_some((name, passed))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pytest_log(total: u64, passed: &[&str], failed: &[&str]) -> String {
        let mut s = format!("collected {total} items\n");
        for p in passed {
            s.push_str(&format!("tests/t.py::{p} PASSED   [ 10%]\n"));
        }
        for f in failed {
            s.push_str(&format!("tests/t.py::{f} FAILED   [ 20%]\n"));
        }
        s
    }

    fn nextest_log(total: u64, passed: &[&str], failed: &[&str]) -> String {
        let mut s = format!("    Starting {total} tests across 2 binaries\n");
        for p in passed {
            s.push_str(&format!("        PASS [   0.011s] crate {p}\n"));
        }
        for f in failed {
            s.push_str(&format!("        FAIL [   0.011s] crate {f}\n"));
        }
        s
    }

    fn ev(name: &str, text: &str) -> CheckEvidence {
        CheckEvidence {
            name: name.into(),
            log: parse_log(text).map_or(CheckLog::Unparsed, CheckLog::Parsed),
            log_ref: None,
        }
    }

    fn state_after(text: &str) -> ProgressState {
        assess(&ProgressState::default(), Some("g0"), &[ev("test", text)]).next
    }

    #[test]
    fn parses_pytest_verbose_xdist_and_summary_lines() {
        let text = "2024-05-01T10:00:00.0000000Z collected 4 items\n\
                    tests/a.py::one PASSED [ 25%]\n\
                    [gw0] [ 50%] PASSED tests/a.py::two\n\
                    \u{1b}[31mFAILED\u{1b}[0m tests/a.py::three - assert 1 == 2\n";
        let p = parse_log(text).unwrap();
        assert_eq!(p.parser, PARSER_PYTEST);
        assert_eq!(p.total, Some(4));
        assert_eq!(p.passed.len(), 2);
        assert!(p.failed.contains("tests/a.py::three"));
    }

    #[test]
    fn parses_nextest_pass_fail_and_total() {
        let p = parse_log(&nextest_log(10, &["a", "b"], &["c"])).unwrap();
        assert_eq!(p.parser, PARSER_NEXTEST);
        assert_eq!(p.total, Some(10));
        assert_eq!(p.passed.len(), 2);
        assert!(p.failed.contains("crate c"));
    }

    #[test]
    fn unknown_or_mixed_formats_fail_closed() {
        assert!(parse_log("error: something broke\nexit code 1\n").is_none());
        let mixed = format!(
            "{}{}",
            pytest_log(2, &["a"], &[]),
            nextest_log(2, &["b"], &[])
        );
        assert!(parse_log(&mixed).is_none());
    }

    #[test]
    fn conflicting_outcomes_for_one_name_become_ambiguous() {
        let text = "tests/a.py::flaky FAILED\ntests/a.py::flaky PASSED\n";
        let p = parse_log(text).unwrap();
        assert!(p.ambiguous.contains("tests/a.py::flaky"));
        assert!(p.passed.is_empty() && p.failed.is_empty());
    }

    #[test]
    fn named_failed_to_passed_transition_is_strong_at_the_same_count() {
        for (before, after) in [
            (
                pytest_log(5, &["a"], &["b"]),
                pytest_log(5, &["a", "b"], &["c"]),
            ),
            (
                nextest_log(5, &["a"], &["b"]),
                nextest_log(5, &["a", "b"], &["c"]),
            ),
        ] {
            let prev = state_after(&before);
            let a = assess(&prev, Some("g1"), &[ev("test", &after)]);
            let win = a.strong.expect("strong evidence");
            assert_eq!(win.check, "test");
            assert!(a.weak.is_none());
        }
    }

    #[test]
    fn a_vanished_failure_is_not_a_pass() {
        let prev = state_after(&pytest_log(5, &["a"], &["b"]));
        let a = assess(
            &prev,
            Some("g1"),
            &[ev("test", &pytest_log(5, &["a"], &["c"]))],
        );
        assert!(a.strong.is_none());
    }

    #[test]
    fn a_flaky_name_in_the_current_log_is_not_a_pass() {
        let prev = state_after(&pytest_log(5, &["a"], &["b"]));
        let cur = "collected 5 items\ntests/t.py::b FAILED\ntests/t.py::b PASSED\n";
        let a = assess(&prev, Some("g1"), &[ev("test", cur)]);
        assert!(a.strong.is_none());
    }

    #[test]
    fn count_increase_is_weak_only_in_a_comparable_suite_above_high_water() {
        let prev = state_after(&pytest_log(10, &["a", "b"], &["c"]));
        // Different failure, more completed, same total: weak.
        let up = assess(
            &prev,
            Some("g1"),
            &[ev("test", &pytest_log(10, &["a", "b", "x"], &["d"]))],
        );
        assert!(up.strong.is_none());
        assert!(up.weak.is_some());
        // High-water carried forward.
        assert_eq!(up.next.high_water["test"].completed, 3);
        // Regression below the high-water mark: denied.
        let down = assess(
            &up.next,
            Some("g2"),
            &[ev("test", &pytest_log(10, &["a"], &["z"]))],
        );
        assert!(down.weak.is_none());
        assert!(
            down.rejections
                .iter()
                .any(|r| r.contains("not_above_high_water"))
        );
        // Suite size changed: not comparable.
        let resized = assess(
            &prev,
            Some("g1"),
            &[ev("test", &pytest_log(12, &["a", "b", "x"], &["d"]))],
        );
        assert!(resized.weak.is_none());
        assert!(
            resized
                .rejections
                .iter()
                .any(|r| r.contains("suite_total_changed"))
        );
    }

    #[test]
    fn missing_total_or_changed_parser_gives_no_weak_evidence() {
        let prev = state_after(&pytest_log(10, &["a"], &["c"]));
        let no_total = "tests/t.py::a PASSED\ntests/t.py::x PASSED\ntests/t.py::d FAILED\n";
        let a = assess(&prev, Some("g1"), &[ev("test", no_total)]);
        assert!(a.weak.is_none());
        let other = assess(
            &prev,
            Some("g1"),
            &[ev("test", &nextest_log(10, &["a", "b"], &["d"]))],
        );
        assert!(other.weak.is_none() && other.strong.is_none());
    }

    #[test]
    fn reordering_alone_is_not_progress() {
        let prev = state_after(&pytest_log(4, &["a", "b"], &["c"]));
        let a = assess(
            &prev,
            Some("g1"),
            &[ev("test", &pytest_log(4, &["b", "a"], &["c"]))],
        );
        assert!(a.strong.is_none() && a.weak.is_none());
    }

    #[test]
    fn unavailable_and_unparsed_logs_are_counted_and_keep_the_old_observation() {
        let prev = state_after(&pytest_log(4, &["a"], &["c"]));
        let a = assess(
            &prev,
            Some("g1"),
            &[
                CheckEvidence {
                    name: "test".into(),
                    log: CheckLog::Unavailable,
                    log_ref: None,
                },
                ev("lint", "garbage"),
            ],
        );
        assert_eq!((a.logs_unavailable, a.parser_failures), (1, 1));
        assert!(a.next.checks.contains_key("test"));
    }
}
