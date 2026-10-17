//! Live-test results in PR/MR descriptions (RAL-672).
//!
//! A review's finished live runs are written into each of its open PR/MR
//! descriptions as one managed `## Live-Test Results` section between stable
//! markers. Everything outside the markers is left exactly as the forge holds
//! it, so a section can be created or replaced on a PR that predates the run.
//!
//! The pieces:
//! * the structured result a live agent proposes ([`parse_result_line`]) and
//!   the recipe commands ralphus accepts from it ([`match_recipe`]): a command
//!   survives only if it matches a recorded `RALPHUS_TOOL:` shell call, so
//!   there is no second model judgment;
//! * the renderer ([`render_section`]) and the marker splice
//!   ([`splice_section`]), both byte-stable;
//! * the evidence a run keeps (`guardian_live_run_evidence`) and the stable
//!   branch attribution of each action (`guardian_live_action_branches`);
//! * the durable per-PR refresh queue (`pr_live_result_refresh`) with bounded
//!   backoff, drained by the maintenance sweep so a failed forge call is
//!   retried, also after a daemon restart, without touching the run's result.

use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::live_actions::LiveRunEnd;
use crate::live_runs::LiveRunRow;
use crate::store::{Result, Store, now_ms};

/// Opens the managed section.
pub const SECTION_START: &str = "<!-- ralphus:live-test-results:start -->";
/// Closes the managed section.
pub const SECTION_END: &str = "<!-- ralphus:live-test-results:end -->";
/// The marker line a live agent ends its reply with.
pub const RESULT_MARKER: &str = "RALPHUS_LIVE_RESULT:";
/// Most runs per action shown in a PR, newest first.
pub const MAX_RUNS_PER_ACTION: usize = 3;
/// Failures after which the review's watchers are told once.
const NOTIFY_AFTER_ATTEMPTS: i64 = 5;
/// First retry delay; doubles per attempt up to [`MAX_BACKOFF_MS`].
const BASE_BACKOFF_MS: i64 = 30_000;
const MAX_BACKOFF_MS: i64 = 30 * 60_000;
/// Longest prompt/report text rendered into a description.
const MAX_TEXT_CHARS: usize = 6_000;

// ---- the verdict ------------------------------------------------------------

/// What a live run concluded, separate from the runner's `finished` state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Passed,
    Failed,
    Inconclusive,
}

impl Verdict {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Passed => "passed",
            Self::Failed => "failed",
            Self::Inconclusive => "inconclusive",
        }
    }

    fn parse(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "passed" | "pass" => Self::Passed,
            "failed" | "fail" => Self::Failed,
            _ => Self::Inconclusive,
        }
    }
}

/// The structured result a live agent proposes in its final reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedResult {
    pub verdict: Verdict,
    pub summary: Option<String>,
    pub fix_attempted: Option<bool>,
    pub fix_confirmed: Option<bool>,
    /// Commands the agent says reproduce its test, before they are checked
    /// against the recorded shell calls.
    pub recipe: Vec<String>,
}

/// The last `RALPHUS_LIVE_RESULT:` line of a reply, parsed. `None` when there
/// is none or its JSON is unusable.
#[must_use]
pub fn parse_result_line(reply: &str) -> Option<ParsedResult> {
    let line = reply
        .lines()
        .rev()
        .map(str::trim_start)
        .find_map(|l| l.strip_prefix(RESULT_MARKER))?;
    let value: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
    let object = value.as_object()?;
    let text = |key: &str| {
        object
            .get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let flag = |key: &str| object.get(key).and_then(serde_json::Value::as_bool);
    let fix_attempted = flag("fix_attempted");
    // A fix cannot be confirmed without having been attempted.
    let fix_confirmed = match (fix_attempted, flag("fix_confirmed")) {
        (Some(false), Some(true)) => Some(false),
        (_, confirmed) => confirmed,
    };
    Some(ParsedResult {
        verdict: Verdict::parse(object.get("verdict").and_then(|v| v.as_str()).unwrap_or("")),
        summary: text("summary"),
        fix_attempted,
        fix_confirmed,
        recipe: object
            .get("recipe")
            .and_then(serde_json::Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|v| v.as_str())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default(),
    })
}

/// The verdict a run is recorded with: the agent's, but only when the runner
/// finished and the agent actually reported one -- anything else is
/// inconclusive, never a guess.
#[must_use]
pub fn final_verdict(end: LiveRunEnd, parsed: Option<&ParsedResult>) -> Verdict {
    match (end, parsed) {
        (LiveRunEnd::Finished, Some(p)) => p.verdict,
        _ => Verdict::Inconclusive,
    }
}

// ---- recorded shell calls and the recipe -----------------------------------

/// One shell tool call recorded in a run's transcript.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedCall {
    pub command: String,
    pub cwd: Option<String>,
    /// `None` when no result half was recorded.
    pub succeeded: Option<bool>,
}

/// One accepted reproduction step.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RecipeStep {
    pub command: String,
    pub cwd: String,
    /// 1-based position among the run's recorded shell calls.
    pub order: usize,
    /// `succeeded`, `failed` or `no result recorded`.
    pub result: String,
}

const SHELL_TOOLS: &[&str] = &[
    "bash",
    "powershell",
    "shell",
    "exec_command",
    "local_shell",
    "run_shell_command",
    "command_execution",
    "execute",
];

fn command_from_input(input: &str) -> Option<(String, Option<String>)> {
    let trimmed = input.trim();
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) {
        let object = value.as_object()?;
        let command = match object.get("command").or_else(|| object.get("cmd"))? {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Array(parts) => parts.last()?.as_str()?.to_string(),
            _ => return None,
        };
        let cwd = ["cwd", "workdir", "working_directory"]
            .iter()
            .find_map(|k| object.get(*k).and_then(|v| v.as_str()))
            .map(str::to_string);
        return Some((command, cwd));
    }
    (!trimmed.is_empty()).then(|| (trimmed.to_string(), None))
}

/// The shell calls recorded in a transcript, in the order they were made.
/// Call and result halves are merged by id, like the Agent Log does.
#[must_use]
pub fn extract_shell_calls(transcript: &str) -> Vec<RecordedCall> {
    let mut order: Vec<String> = Vec::new();
    let mut calls: std::collections::HashMap<String, RecordedCall> =
        std::collections::HashMap::new();
    let mut results: std::collections::HashMap<String, bool> = std::collections::HashMap::new();
    for line in transcript.lines() {
        let Some(json) = line.trim_start().strip_prefix("RALPHUS_TOOL: ") else {
            continue;
        };
        let Ok(record) = serde_json::from_str::<serde_json::Value>(json) else {
            continue;
        };
        let Some(id) = record.get("id").and_then(|v| v.as_str()) else {
            continue;
        };
        if let Some(is_error) = record.get("is_error").and_then(serde_json::Value::as_bool) {
            results.insert(id.to_string(), !is_error);
        }
        let tool = record
            .get("tool")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        if !SHELL_TOOLS.contains(&tool.as_str()) || calls.contains_key(id) {
            continue;
        }
        let Some((command, cwd)) = record
            .get("input")
            .and_then(|v| v.as_str())
            .and_then(command_from_input)
        else {
            continue;
        };
        order.push(id.to_string());
        calls.insert(
            id.to_string(),
            RecordedCall {
                command,
                cwd,
                succeeded: None,
            },
        );
    }
    order
        .into_iter()
        .filter_map(|id| {
            let mut call = calls.remove(&id)?;
            call.succeeded = results.get(&id).copied();
            Some(call)
        })
        .collect()
}

/// Keep only the proposed recipe commands that match a recorded shell call,
/// each recorded call at most once, ordered as they were run, with the
/// recorded cwd (or `default_cwd`) and result. Unmatched commands are dropped.
#[must_use]
pub fn match_recipe(
    proposed: &[String],
    calls: &[RecordedCall],
    default_cwd: &str,
) -> Vec<RecipeStep> {
    let mut used = vec![false; calls.len()];
    let mut steps = Vec::new();
    for command in proposed {
        let wanted = command.trim();
        if wanted.is_empty() {
            continue;
        }
        let Some(i) = (0..calls.len()).find(|&i| !used[i] && calls[i].command.trim() == wanted)
        else {
            continue;
        };
        used[i] = true;
        steps.push(RecipeStep {
            command: calls[i].command.trim().to_string(),
            cwd: calls[i]
                .cwd
                .clone()
                .unwrap_or_else(|| default_cwd.to_string()),
            order: i + 1,
            result: match calls[i].succeeded {
                Some(true) => "succeeded",
                Some(false) => "failed",
                None => "no result recorded",
            }
            .to_string(),
        });
    }
    steps.sort_by_key(|s| s.order);
    steps
}

/// All shell calls recorded for one live run's agent session.
#[must_use]
pub fn recorded_calls_for_run(guardian_id: &str, cell_id: &str) -> Vec<RecordedCall> {
    let name = crate::cell_session::session_name(
        &crate::live_actions::live_run_squad_id(guardian_id),
        crate::live_actions::LIVE_ACTION_TASK,
        cell_id,
    );
    let mut calls = Vec::new();
    for attempt in crate::terminal_log::list_attempts(&name) {
        if let Some(text) = crate::terminal_log::read_attempt(&name, attempt.attempt) {
            calls.extend(extract_shell_calls(&text));
        }
    }
    calls
}

// ---- evidence ---------------------------------------------------------------

/// What a run sent and concluded, from `guardian_live_run_evidence`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunEvidence {
    pub sent_prompt: Option<String>,
    pub sent_system_prompt: Option<String>,
    pub verdict: Option<String>,
    pub verdict_summary: Option<String>,
    pub fix_attempted: Option<bool>,
    pub fix_confirmed: Option<bool>,
    pub recipe: Vec<RecipeStep>,
    pub branch_ids: Vec<String>,
    pub integration_branch_id: Option<String>,
    pub integration_pushed: Option<bool>,
}

/// The branches (`guardian_branches.id`) an action's source cells belong to,
/// in source order, without repeats.
#[must_use]
pub fn resolve_branch_ids(
    conn: &rusqlite::Connection,
    guardian_id: &str,
    source_cells: &[String],
) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for uri in source_cells {
        let mut parts = uri.rsplitn(3, ':');
        let (Some(cell), Some(task), Some(head)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        let (Ok(cell), Ok(task)) = (cell.parse::<i64>(), task.parse::<i64>()) else {
            continue;
        };
        let Some(squad) = head.strip_prefix("cell:") else {
            continue;
        };
        let found: Option<String> = conn
            .query_row(
                "SELECT gb.id FROM cells c
                 JOIN guardian_branches gb
                   ON gb.guardian_id = ?1 AND gb.branch = c.review_branch
                 WHERE c.squad_id = ?2 AND c.task_idx = ?3 AND c.idx = ?4
                 LIMIT 1",
                rusqlite::params![guardian_id, squad, task, cell],
                |r| r.get(0),
            )
            .ok();
        if let Some(id) = found {
            if !out.contains(&id) {
                out.push(id);
            }
        }
    }
    out
}

fn opt_bool(v: Option<bool>) -> Option<i64> {
    v.map(i64::from)
}

impl Store {
    fn ensure_run_evidence(&self, run_id: &str) -> Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO guardian_live_run_evidence (run_id) VALUES (?)",
            rusqlite::params![run_id],
        )?;
        Ok(())
    }

    /// The branches a live action is attributed to.
    ///
    /// # Errors
    ///
    /// On a database failure.
    pub fn live_action_branch_ids(
        &self,
        guardian_id: &str,
        action_idx: i64,
    ) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT branch_id FROM guardian_live_action_branches
             WHERE guardian_id=? AND action_idx=? ORDER BY rowid",
        )?;
        let rows = stmt.query_map(rusqlite::params![guardian_id, action_idx], |r| r.get(0))?;
        Ok(rows.collect::<std::result::Result<Vec<String>, _>>()?)
    }

    /// Freeze the branch attribution of a run (once: later calls keep the
    /// first value).
    ///
    /// # Errors
    ///
    /// On a database failure.
    pub fn freeze_live_run_branches(&self, run_id: &str, branch_ids: &[String]) -> Result<()> {
        self.ensure_run_evidence(run_id)?;
        self.conn.execute(
            "UPDATE guardian_live_run_evidence SET branch_ids=? WHERE run_id=? AND branch_ids IS NULL",
            rusqlite::params![serde_json::to_string(branch_ids).unwrap_or_default(), run_id],
        )?;
        Ok(())
    }

    /// Record the exact prompt and system prompt sent to the runner.
    ///
    /// # Errors
    ///
    /// On a database failure.
    pub fn record_live_run_sent(
        &self,
        run_id: &str,
        prompt: Option<&str>,
        system_prompt: Option<&str>,
    ) -> Result<()> {
        self.ensure_run_evidence(run_id)?;
        self.conn.execute(
            "UPDATE guardian_live_run_evidence SET sent_prompt=?, sent_system_prompt=? WHERE run_id=?",
            rusqlite::params![prompt, system_prompt, run_id],
        )?;
        Ok(())
    }

    /// Record the verdict, fix flags and accepted recipe of a run.
    ///
    /// # Errors
    ///
    /// On a database failure.
    pub fn record_live_run_verdict(
        &self,
        run_id: &str,
        verdict: Verdict,
        parsed: Option<&ParsedResult>,
        recipe: &[RecipeStep],
    ) -> Result<()> {
        self.ensure_run_evidence(run_id)?;
        self.conn.execute(
            "UPDATE guardian_live_run_evidence
             SET verdict=?, verdict_summary=?, fix_attempted=?, fix_confirmed=?, recipe=?
             WHERE run_id=?",
            rusqlite::params![
                verdict.as_str(),
                parsed.and_then(|p| p.summary.clone()),
                opt_bool(parsed.and_then(|p| p.fix_attempted)),
                opt_bool(parsed.and_then(|p| p.fix_confirmed)),
                serde_json::to_string(recipe).unwrap_or_default(),
                run_id
            ],
        )?;
        Ok(())
    }

    /// Record where a run's commits were carried and whether that was pushed.
    ///
    /// # Errors
    ///
    /// On a database failure.
    pub fn record_live_run_destination(
        &self,
        run_id: &str,
        branch_id: &str,
        pushed: Option<bool>,
    ) -> Result<()> {
        self.ensure_run_evidence(run_id)?;
        self.conn.execute(
            "UPDATE guardian_live_run_evidence SET integration_branch_id=?, integration_pushed=?
             WHERE run_id=?",
            rusqlite::params![branch_id, opt_bool(pushed), run_id],
        )?;
        Ok(())
    }

    /// A run's evidence; empty when none was recorded.
    ///
    /// # Errors
    ///
    /// On a database failure.
    pub fn live_run_evidence(&self, run_id: &str) -> Result<RunEvidence> {
        use rusqlite::OptionalExtension as _;
        let row = self
            .conn
            .query_row(
                "SELECT sent_prompt, sent_system_prompt, verdict, verdict_summary, fix_attempted,
                        fix_confirmed, recipe, branch_ids, integration_branch_id, integration_pushed
                 FROM guardian_live_run_evidence WHERE run_id=?",
                rusqlite::params![run_id],
                |r| {
                    let recipe: Option<String> = r.get(6)?;
                    let branches: Option<String> = r.get(7)?;
                    let flag = |i| r.get::<_, Option<i64>>(i).map(|v| v.map(|n| n != 0));
                    Ok(RunEvidence {
                        sent_prompt: r.get(0)?,
                        sent_system_prompt: r.get(1)?,
                        verdict: r.get(2)?,
                        verdict_summary: r.get(3)?,
                        fix_attempted: flag(4)?,
                        fix_confirmed: flag(5)?,
                        recipe: recipe
                            .and_then(|s| serde_json::from_str(&s).ok())
                            .unwrap_or_default(),
                        branch_ids: branches
                            .and_then(|s| serde_json::from_str(&s).ok())
                            .unwrap_or_default(),
                        integration_branch_id: r.get(8)?,
                        integration_pushed: flag(9)?,
                    })
                },
            )
            .optional()?;
        Ok(row.unwrap_or_default())
    }

    /// Persist a PR row's description (called once the forge accepted it).
    ///
    /// # Errors
    ///
    /// On a database failure.
    pub fn set_pr_description(&self, pr_id: &str, description: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE guardian_pull_requests SET description=?, updated_at_ms=? WHERE id=?",
            rusqlite::params![description, now_ms(), pr_id],
        )?;
        Ok(())
    }

    /// Ask for a PR's results section to be re-rendered. Replaces a pending
    /// request, which makes it due immediately.
    ///
    /// # Errors
    ///
    /// On a database failure.
    pub fn request_pr_live_refresh(&self, pr_id: &str, guardian_id: &str) -> Result<()> {
        let now = now_ms();
        self.conn.execute(
            "INSERT INTO pr_live_result_refresh
                (pr_id, guardian_id, requested_at_ms, attempts, next_attempt_at_ms)
             VALUES (?, ?, ?, 0, ?)
             ON CONFLICT(pr_id) DO UPDATE SET
                requested_at_ms = excluded.requested_at_ms,
                attempts = 0,
                next_attempt_at_ms = excluded.next_attempt_at_ms",
            rusqlite::params![pr_id, guardian_id, now, now],
        )?;
        Ok(())
    }

    /// Ask for one PR/MR to re-render its results when its review has live
    /// runs; a PR created, adopted, replaced or promoted after the run gets
    /// the section this way.
    ///
    /// # Errors
    ///
    /// On a database failure.
    pub fn request_live_refresh_for_pr(&self, pr_id: &str, guardian_id: &str) -> Result<()> {
        let any: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM guardian_live_runs WHERE guardian_id=? AND kind='live'",
            rusqlite::params![guardian_id],
            |r| r.get(0),
        )?;
        if any > 0 {
            self.request_pr_live_refresh(pr_id, guardian_id)?;
        }
        Ok(())
    }

    /// Ask for a refresh of every open PR/MR of a review that has live runs.
    ///
    /// # Errors
    ///
    /// On a database failure.
    pub fn request_live_refresh_for_guardian(&self, guardian_id: &str) -> Result<()> {
        let any: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM guardian_live_runs WHERE guardian_id=? AND kind='live'",
            rusqlite::params![guardian_id],
            |r| r.get(0),
        )?;
        if any == 0 {
            return Ok(());
        }
        for pr in self.list_pull_requests_for_guardian(guardian_id)? {
            if pr.state == "open" && pr.pr_number.is_some() && pr.superseded_by.is_none() {
                self.request_pr_live_refresh(&pr.id, guardian_id)?;
            }
        }
        Ok(())
    }

    /// Pending refreshes whose next attempt is due: `(pr_id, guardian_id,
    /// requested_at_ms, attempts)`.
    ///
    /// # Errors
    ///
    /// On a database failure.
    pub fn due_pr_live_refreshes(&self, now: i64) -> Result<Vec<(String, String, i64, i64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT pr_id, guardian_id, requested_at_ms, attempts FROM pr_live_result_refresh
             WHERE next_attempt_at_ms <= ? ORDER BY next_attempt_at_ms, pr_id",
        )?;
        let rows = stmt.query_map(rusqlite::params![now], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// Every pending refresh as `(pr_id, attempts, last_error)`.
    ///
    /// # Errors
    ///
    /// On a database failure.
    pub fn pending_pr_live_refreshes(&self) -> Result<Vec<(String, i64, Option<String>)>> {
        let mut stmt = self.conn.prepare(
            "SELECT pr_id, attempts, last_error FROM pr_live_result_refresh ORDER BY pr_id",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// The refresh is done, unless it was requested again since `requested_at_ms`.
    ///
    /// # Errors
    ///
    /// On a database failure.
    pub fn finish_pr_live_refresh(&self, pr_id: &str, requested_at_ms: i64) -> Result<()> {
        self.conn.execute(
            "DELETE FROM pr_live_result_refresh WHERE pr_id=? AND requested_at_ms<=?",
            rusqlite::params![pr_id, requested_at_ms],
        )?;
        Ok(())
    }

    /// Record a failed attempt and schedule the next after a bounded backoff.
    ///
    /// # Errors
    ///
    /// On a database failure.
    pub fn fail_pr_live_refresh(&self, pr_id: &str, attempts: i64, error: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE pr_live_result_refresh
             SET attempts=?, next_attempt_at_ms=?, last_error=? WHERE pr_id=?",
            rusqlite::params![attempts, now_ms() + backoff_ms(attempts), error, pr_id],
        )?;
        Ok(())
    }
}

/// Delay before attempt number `attempts + 1`: 30 s doubling, capped at 30 min.
#[must_use]
pub fn backoff_ms(attempts: i64) -> i64 {
    let shift = u32::try_from(attempts.saturating_sub(1).clamp(0, 16)).unwrap_or(16);
    (BASE_BACKOFF_MS << shift).min(MAX_BACKOFF_MS)
}

// ---- rendering --------------------------------------------------------------

/// One run as the section renders it.
#[derive(Debug, Clone)]
pub struct SectionRun {
    pub run: LiveRunRow,
    pub evidence: RunEvidence,
    /// The destination branch's name, when known.
    pub destination_branch: Option<String>,
}

fn clip(text: &str) -> String {
    let redacted = ralphus_core::redact::redact_secrets(text);
    if redacted.chars().count() <= MAX_TEXT_CHARS {
        return redacted.into_owned();
    }
    let kept: String = redacted.chars().take(MAX_TEXT_CHARS).collect();
    format!("{kept}\n[... cut]")
}

/// A code fence longer than any backtick run in `text`.
fn fenced(text: &str) -> String {
    let mut longest = 0;
    let mut run = 0;
    for c in text.chars() {
        if c == '`' {
            run += 1;
            longest = longest.max(run);
        } else {
            run = 0;
        }
    }
    let fence = "`".repeat((longest + 1).max(3));
    format!("{fence}text\n{}\n{fence}", text.trim_end())
}

fn when(ms: Option<i64>) -> String {
    ms.and_then(chrono::DateTime::from_timestamp_millis)
        .map_or_else(
            || "unknown time".to_string(),
            |t| t.format("%Y-%m-%d %H:%M UTC").to_string(),
        )
}

fn yes_no(v: Option<bool>) -> &'static str {
    match v {
        Some(true) => "yes",
        Some(false) => "no",
        None => "unknown",
    }
}

fn one_line(text: &str) -> String {
    clip(text).split_whitespace().collect::<Vec<_>>().join(" ")
}

fn render_run(out: &mut String, item: &SectionRun) {
    let run = &item.run;
    let ev = &item.evidence;
    let verdict = ev.verdict.as_deref().unwrap_or("inconclusive");
    let _ = writeln!(out, "#### {} (`{}`)\n", one_line(&run.label), run.run_id);
    let _ = writeln!(
        out,
        "- Run: {} at {}; runner state `{}`",
        if run.trigger == "auto" {
            "automatic"
        } else {
            "manual"
        },
        when(run.started_at_ms.or(Some(run.queued_at_ms))),
        run.state
    );
    let _ = write!(out, "- Verdict: **{verdict}**");
    match ev.verdict_summary.as_deref() {
        Some(summary) => {
            let _ = writeln!(out, " - {}", one_line(summary));
        }
        None => {
            let _ = writeln!(out, " (the agent reported no summary)");
        }
    }
    let _ = writeln!(
        out,
        "- Fix attempted: {}; fix confirmed: {}",
        yes_no(ev.fix_attempted),
        yes_no(ev.fix_confirmed)
    );
    if run.commits.is_empty() {
        let _ = writeln!(out, "- Commits: none");
    } else {
        let shas: Vec<String> = run
            .commits
            .iter()
            .map(|c| format!("`{}`", c.chars().take(12).collect::<String>()))
            .collect();
        let _ = writeln!(out, "- Commits: {}", shas.join(", "));
    }
    let _ = write!(out, "- Integration: `{}`", run.integration);
    if let Some(detail) = run.integration_detail.as_deref() {
        let _ = write!(out, " - {}", one_line(detail));
    }
    let _ = writeln!(out);
    if run.integration != "none" {
        let destination = item
            .destination_branch
            .as_deref()
            .or(ev.integration_branch_id.as_deref())
            .map_or_else(|| "not recorded".to_string(), |b| format!("`{b}`"));
        let pushed = match ev.integration_pushed {
            Some(true) => "pushed",
            Some(false) => "not pushed",
            None => "push result not recorded",
        };
        let _ = writeln!(out, "- Destination branch: {destination} ({pushed})");
    }
    if let Some(error) = run.error.as_deref() {
        let _ = writeln!(out, "- Runner error: {}", one_line(error));
    }
    let _ = writeln!(out, "\nObserved outcome:\n");
    match run.report.as_deref().filter(|r| !r.trim().is_empty()) {
        Some(report) => {
            let _ = writeln!(out, "{}\n", fenced(&clip(report)));
        }
        None => {
            let _ = writeln!(out, "No report was recorded.\n");
        }
    }
    let _ = writeln!(out, "Reproduction:\n");
    if ev.recipe.is_empty() {
        let _ = writeln!(
            out,
            "No reproduction commands matching recorded shell calls are available for this run.\n"
        );
    } else {
        for step in &ev.recipe {
            let _ = writeln!(
                out,
                "{}. in `{}`: {} ({})",
                step.order,
                clip(&step.cwd),
                fenced(&clip(&step.command)),
                step.result
            );
        }
        let _ = writeln!(out);
    }
    let _ = writeln!(out, "<details><summary>Exact prompt sent</summary>\n");
    match ev.sent_prompt.as_deref() {
        Some(prompt) => {
            let _ = writeln!(out, "{}\n", fenced(&clip(prompt)));
        }
        None => {
            let _ = writeln!(out, "Not recorded.\n");
        }
    }
    if let Some(system) = ev.sent_system_prompt.as_deref() {
        let _ = writeln!(out, "System prompt:\n\n{}\n", fenced(&clip(system)));
    }
    let _ = writeln!(out, "</details>\n");
}

/// The managed section for a set of runs, markers included, or `None` when
/// there is nothing to show. Runs are grouped by action; each action shows its
/// newest [`MAX_RUNS_PER_ACTION`] runs, newest first.
#[must_use]
pub fn render_section(runs: &[SectionRun]) -> Option<String> {
    let mut actions: std::collections::BTreeMap<i64, Vec<&SectionRun>> =
        std::collections::BTreeMap::new();
    for item in runs {
        actions.entry(item.run.action_idx).or_default().push(item);
    }
    if actions.is_empty() {
        return None;
    }
    let mut out = format!("{SECTION_START}\n## Live-Test Results\n\n");
    for items in actions.values_mut() {
        items.sort_by(|a, b| {
            (b.run.queued_at_ms, &b.run.run_id).cmp(&(a.run.queued_at_ms, &a.run.run_id))
        });
        for item in items.iter().take(MAX_RUNS_PER_ACTION) {
            render_run(&mut out, item);
        }
    }
    out.push_str(SECTION_END);
    Some(out)
}

/// Put `section` (markers included) into `current`: replace an existing
/// managed section in place, otherwise append it. Text outside the markers is
/// never touched.
#[must_use]
pub fn splice_section(current: &str, section: &str) -> String {
    if let Some(start) = current.find(SECTION_START) {
        if let Some(end_rel) = current[start..].find(SECTION_END) {
            let end = start + end_rel + SECTION_END.len();
            return format!("{}{}{}", &current[..start], section, &current[end..]);
        }
    }
    let base = current.trim_end();
    if base.is_empty() {
        section.to_string()
    } else {
        format!("{base}\n\n{section}")
    }
}

// ---- publishing -------------------------------------------------------------

/// What a refresh of one PR did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshOutcome {
    /// The forge description was updated.
    Updated,
    /// The forge description already held this exact section.
    Unchanged,
    /// Nothing to publish (no PR number, closed PR, no runs for it).
    Skipped,
}

type ClientFor<'a> = &'a dyn Fn(
    &crate::pr::PullRequestView,
    &crate::guardian::GuardianView,
) -> Option<crate::forge::ForgeClient>;

fn normalize_newlines(text: &str) -> String {
    text.replace("\r\n", "\n")
}

/// Runs of the review shown on the PR for `branch_id` (`None` = the combined
/// PR, which shows all of them): finished live runs, attributed by their frozen
/// branch ids.
fn section_runs(
    store: &crate::store_lock::StoreHandle,
    pr: &crate::pr::PullRequestView,
    guardian: &crate::guardian::GuardianView,
) -> Result<Vec<SectionRun>> {
    let guard = store.lock();
    let all = crate::live_runs::guardian_live_runs_conn(&guard.conn, &guardian.id)?;
    let mut out = Vec::new();
    for run in all {
        if run.kind != "live" || matches!(run.state.as_str(), "queued" | "running") {
            continue;
        }
        let evidence = guard.live_run_evidence(&run.run_id)?;
        let attributed = if evidence.branch_ids.is_empty() {
            guard.live_action_branch_ids(&guardian.id, run.action_idx)?
        } else {
            evidence.branch_ids.clone()
        };
        let on_this_pr = match pr.branch_id.as_deref() {
            None => true,
            Some(branch) => attributed.is_empty() || attributed.iter().any(|b| b == branch),
        };
        if !on_this_pr {
            continue;
        }
        let destination_branch = evidence.integration_branch_id.as_deref().and_then(|id| {
            guardian
                .branches
                .iter()
                .find(|b| b.id == id)
                .map(|b| b.branch.clone())
        });
        out.push(SectionRun {
            run,
            evidence,
            destination_branch,
        });
    }
    Ok(out)
}

/// Re-render one PR's results section into its forge description. The current
/// description is read from the forge right before the update, and the local
/// row is written only after the forge accepted the new text.
///
/// # Errors
///
/// A message when the PR cannot be loaded, no forge client is available, or
/// the forge read/update fails.
pub fn refresh_pr_with(
    store: &crate::store_lock::StoreHandle,
    pr_id: &str,
    client_for: ClientFor<'_>,
) -> std::result::Result<RefreshOutcome, String> {
    let (pr, guardian) = {
        let guard = store.lock();
        let pr = guard.get_pull_request(pr_id).map_err(|e| e.to_string())?;
        let guardian = guard
            .get_guardian(&pr.guardian_id)
            .map_err(|e| e.to_string())?;
        (pr, guardian)
    };
    let Some(number) = pr.pr_number else {
        return Ok(RefreshOutcome::Skipped);
    };
    if pr.state != "open" || pr.superseded_by.is_some() {
        return Ok(RefreshOutcome::Skipped);
    }
    let runs = section_runs(store, &pr, &guardian).map_err(|e| e.to_string())?;
    let Some(section) = render_section(&runs) else {
        return Ok(RefreshOutcome::Skipped);
    };
    let client = client_for(&pr, &guardian)
        .ok_or_else(|| format!("no forge client for {} on PR #{number}", pr.repo))?;
    let current = client.get_pull_request_description(number)?;
    let updated = splice_section(&current, &section);
    if normalize_newlines(&updated) == normalize_newlines(&current) {
        let _ = store.lock().set_pr_description(&pr.id, &current);
        return Ok(RefreshOutcome::Unchanged);
    }
    client.update_pull_request_description(number, &updated)?;
    let _ = store.lock().set_pr_description(&pr.id, &updated);
    crate::cartographer::Note::new("guardian")
        .guardian(&guardian.id)
        .scope("live_action")
        .emit(
            &store.lock(),
            "live-test results published to PR description",
            serde_json::json!({
                "pr_id": pr.id,
                "pr_number": number,
                "repo": pr.repo,
                "runs": runs.len(),
            }),
        );
    Ok(RefreshOutcome::Updated)
}

/// [`refresh_pr_with`] using the PR row's own forge and repo (fork-aware).
///
/// # Errors
///
/// See [`refresh_pr_with`].
pub fn refresh_pr(
    store: &crate::store_lock::StoreHandle,
    pr_id: &str,
) -> std::result::Result<RefreshOutcome, String> {
    refresh_pr_with(store, pr_id, &|pr, guardian| {
        let root = std::path::PathBuf::from(&guardian.git_root);
        let forge_cfg = crate::config::resolve_forge(&root);
        crate::pr::forge_client_for_pr(
            store,
            &root,
            &guardian.base_branch,
            &forge_cfg,
            &pr.repo,
            guardian.owner.as_deref(),
        )
    })
}

/// Attempt every due pending refresh once. A failure is logged, scheduled for
/// a later retry and, after repeated failures, reported through the review's
/// watchers; it never changes any run's recorded result.
pub fn process_pr_refreshes_with(
    store: &crate::store_lock::StoreHandle,
    client_for: ClientFor<'_>,
) {
    let due = store
        .lock()
        .due_pr_live_refreshes(now_ms())
        .unwrap_or_default();
    for (pr_id, guardian_id, requested_at_ms, attempts) in due {
        match refresh_pr_with(store, &pr_id, client_for) {
            Ok(outcome) => {
                crate::rlog!(
                    INFO,
                    "ralphus [guardian] review {guardian_id} live-test results for PR row {pr_id}: {outcome:?}"
                );
                let _ = store.lock().finish_pr_live_refresh(&pr_id, requested_at_ms);
            }
            Err(message) => {
                let attempts = attempts + 1;
                crate::rlog!(
                    WARNING,
                    "ralphus [guardian] review {guardian_id} live-test results for PR row {pr_id} failed (attempt {attempts}): {message}"
                );
                let guard = store.lock();
                let _ = guard.fail_pr_live_refresh(&pr_id, attempts, &message);
                crate::cartographer::Note::new("guardian")
                    .guardian(&guardian_id)
                    .scope("live_action")
                    .emit(
                        &guard,
                        "live-test results refresh failed",
                        serde_json::json!({
                            "pr_id": pr_id,
                            "attempts": attempts,
                            "error": message,
                            "retry_in_ms": backoff_ms(attempts),
                        }),
                    );
                if attempts == NOTIFY_AFTER_ATTEMPTS {
                    let _ = guard.enqueue_error_mailbox_message(
                        crate::mailbox::MailboxPriority::Normal,
                        &format!(
                            "Review {guardian_id}: the Live-Test Results section could not be \
                             written to the PR description after {attempts} attempts: {message}"
                        ),
                        &crate::mailbox::Remediation::ManualInterventionRequired {
                            guidance: "ralphus keeps retrying with a growing delay; check the \
                                       forge token and that the PR is still open, then wait \
                                       for the next retry or re-run a live action to request \
                                       a fresh refresh"
                                .to_string(),
                        },
                        None,
                        None,
                        None,
                        Some(&format!("guardian:{guardian_id}")),
                        Some("live_actions"),
                    );
                }
            }
        }
    }
}

/// Drain the refresh queue off the caller's thread (the scheduler tick), one
/// pass at a time.
pub fn process_pr_refreshes_background(store: &crate::store_lock::StoreHandle) {
    static RUNNING: AtomicBool = AtomicBool::new(false);
    if store
        .lock()
        .due_pr_live_refreshes(now_ms())
        .unwrap_or_default()
        .is_empty()
        || RUNNING.swap(true, Ordering::SeqCst)
    {
        return;
    }
    let store = std::sync::Arc::clone(store);
    std::thread::spawn(move || {
        let client_for = |pr: &crate::pr::PullRequestView,
                          guardian: &crate::guardian::GuardianView| {
            let root = std::path::PathBuf::from(&guardian.git_root);
            let forge_cfg = crate::config::resolve_forge(&root);
            crate::pr::forge_client_for_pr(
                &store,
                &root,
                &guardian.base_branch,
                &forge_cfg,
                &pr.repo,
                guardian.owner.as_deref(),
            )
        };
        process_pr_refreshes_with(&store, &client_for);
        RUNNING.store(false, Ordering::SeqCst);
    });
}

/// Fold a finished live run's evidence into the store: the sent prompts, the
/// verdict and the recipe checked against the run's recorded shell calls.
pub fn record_run_evidence(
    store: &crate::store_lock::StoreHandle,
    guardian_id: &str,
    run: &LiveRunRow,
    outcome: &crate::live_actions::LiveRunOutcome,
) {
    let parsed = parse_result_line(&outcome.raw_reply);
    let verdict = final_verdict(outcome.end, parsed.as_ref());
    let recipe = match (&parsed, run.cwd.as_deref()) {
        (Some(p), Some(cwd)) if !p.recipe.is_empty() => {
            let calls = recorded_calls_for_run(guardian_id, &outcome.cell_id);
            match_recipe(&p.recipe, &calls, cwd)
        }
        _ => Vec::new(),
    };
    let guard = store.lock();
    let _ = guard.record_live_run_sent(
        &run.run_id,
        outcome.sent_prompt.as_deref(),
        outcome.sent_system_prompt.as_deref(),
    );
    let _ = guard.record_live_run_verdict(&run.run_id, verdict, parsed.as_ref(), &recipe);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_row(run_id: &str, action_idx: i64, queued: i64) -> LiveRunRow {
        LiveRunRow {
            run_id: run_id.to_string(),
            guardian_id: "g".into(),
            kind: "live".into(),
            action_idx,
            label: "Board shows reviews".into(),
            trigger: "auto".into(),
            state: "finished".into(),
            cwd: Some("/w".into()),
            base_tip: None,
            live_ref: None,
            commits: vec!["0123456789abcdef".into()],
            report: Some("It worked.".into()),
            error: None,
            cost_usd: None,
            cell_id: None,
            integration: "integrated".into(),
            integration_detail: Some("cherry-picked onto the top branch and pushed".into()),
            queued_at_ms: queued,
            started_at_ms: Some(queued),
            finished_at_ms: Some(queued + 1),
        }
    }

    fn section_run(run_id: &str, action_idx: i64, queued: i64) -> SectionRun {
        SectionRun {
            run: run_row(run_id, action_idx, queued),
            evidence: RunEvidence {
                sent_prompt: Some("Live action: x\n\nstart it".into()),
                sent_system_prompt: Some("system".into()),
                verdict: Some("passed".into()),
                verdict_summary: Some("greeting shown".into()),
                fix_attempted: Some(true),
                fix_confirmed: Some(false),
                recipe: vec![RecipeStep {
                    command: "cargo run".into(),
                    cwd: "/w".into(),
                    order: 2,
                    result: "succeeded".into(),
                }],
                integration_branch_id: Some("branch-1".into()),
                integration_pushed: Some(true),
                ..RunEvidence::default()
            },
            destination_branch: Some("feature-top".into()),
        }
    }

    #[test]
    fn a_missing_or_unusable_result_is_inconclusive_not_guessed() {
        assert!(parse_result_line("all good").is_none());
        assert!(parse_result_line("RALPHUS_LIVE_RESULT: not json").is_none());
        assert_eq!(
            final_verdict(LiveRunEnd::Finished, None),
            Verdict::Inconclusive
        );
        let parsed = parse_result_line(r#"RALPHUS_LIVE_RESULT: {"verdict": "passed"}"#);
        assert_eq!(
            final_verdict(LiveRunEnd::Finished, parsed.as_ref()),
            Verdict::Passed
        );
        // The runner not finishing overrides whatever the agent claimed.
        assert_eq!(
            final_verdict(LiveRunEnd::Failed, parsed.as_ref()),
            Verdict::Inconclusive
        );
        let odd = parse_result_line(r#"RALPHUS_LIVE_RESULT: {"verdict": "great"}"#).unwrap();
        assert_eq!(odd.verdict, Verdict::Inconclusive);
    }

    #[test]
    fn a_fix_cannot_be_confirmed_without_being_attempted() {
        let p = parse_result_line(
            r#"RALPHUS_LIVE_RESULT: {"verdict":"failed","fix_attempted":false,"fix_confirmed":true,"summary":" s ","recipe":["a","b"]}"#,
        )
        .unwrap();
        assert_eq!(p.fix_confirmed, Some(false));
        assert_eq!(p.summary.as_deref(), Some("s"));
        assert_eq!(p.recipe, vec!["a".to_string(), "b".to_string()]);
    }

    fn tool_line(id: &str, tool: &str, input: &str, is_error: Option<bool>) -> String {
        let mut v = serde_json::json!({"id": id, "tool": tool, "input": input});
        if let Some(e) = is_error {
            v["is_error"] = serde_json::json!(e);
            v["output"] = serde_json::json!("out");
        }
        format!("RALPHUS_TOOL: {v}")
    }

    #[test]
    fn the_recipe_keeps_only_commands_that_were_actually_run() {
        let transcript = [
            tool_line("1", "Read", r#"{"file_path":"x"}"#, None),
            tool_line(
                "2",
                "Bash",
                r#"{"command":"cargo build","cwd":"/w/sub"}"#,
                None,
            ),
            tool_line("2", "Bash", "", Some(false)),
            tool_line("3", "Bash", r#"{"command":"curl localhost"}"#, None),
            tool_line("3", "Bash", "", Some(true)),
            tool_line("4", "Bash", r#"{"command":"ls"}"#, None),
            "plain text RALPHUS_TOOL: ignored".to_string(),
        ]
        .join("\n");
        let calls = extract_shell_calls(&transcript);
        assert_eq!(calls.len(), 3);
        let steps = match_recipe(
            &[
                "curl localhost".into(),
                "rm -rf /".into(),
                "cargo build".into(),
                "ls".into(),
            ],
            &calls,
            "/w",
        );
        let summary: Vec<_> = steps
            .iter()
            .map(|s| {
                (
                    s.order,
                    s.command.as_str(),
                    s.cwd.as_str(),
                    s.result.as_str(),
                )
            })
            .collect();
        assert_eq!(
            summary,
            vec![
                (1, "cargo build", "/w/sub", "succeeded"),
                (2, "curl localhost", "/w", "failed"),
                (3, "ls", "/w", "no result recorded"),
            ]
        );
    }

    #[test]
    fn splicing_replaces_only_the_managed_section() {
        let section = render_section(&[section_run("r1", 0, 10)]).unwrap();
        let user = "My own notes.\n\nSee also #12.";
        let first = splice_section(user, &section);
        assert!(first.starts_with(user));
        assert_eq!(first.matches(SECTION_START).count(), 1);

        let newer = render_section(&[section_run("r2", 0, 20)]).unwrap();
        let after = format!("{first}\n\nTrailing user text.");
        let second = splice_section(&after, &newer);
        assert!(second.starts_with(user));
        assert!(second.ends_with("Trailing user text."));
        assert!(second.contains("`r2`") && !second.contains("`r1`"));
        // Byte-stable on a re-render with the same data.
        assert_eq!(splice_section(&second, &newer), second);
    }

    #[test]
    fn at_most_three_runs_per_action_newest_first() {
        let runs: Vec<_> = (0..5)
            .map(|i| section_run(&format!("run{i}"), 0, 100 + i))
            .chain([section_run("other", 1, 5)])
            .collect();
        let section = render_section(&runs).unwrap();
        let pos = |id: &str| section.find(&format!("`{id}`"));
        assert!(pos("run4").unwrap() < pos("run3").unwrap());
        assert!(pos("run3").unwrap() < pos("run2").unwrap());
        assert!(pos("run1").is_none() && pos("run0").is_none());
        assert!(pos("other").is_some());
        assert!(render_section(&[]).is_none());
    }

    #[test]
    fn unknown_details_are_stated_and_secrets_redacted() {
        let mut item = section_run("r1", 0, 1);
        item.evidence = RunEvidence::default();
        item.run.report = Some("ran ANTHROPIC_API_KEY=NOTAREALKEY run".into());
        let section = render_section(&[item]).unwrap();
        assert!(section.contains("**inconclusive**"));
        assert!(section.contains("push result not recorded") || section.contains("not recorded"));
        assert!(section.contains("No reproduction commands"));
        assert!(!section.contains("NOTAREALKEY"));
    }

    #[test]
    fn the_exact_prompt_and_destination_are_rendered() {
        let section = render_section(&[section_run("r1", 0, 1)]).unwrap();
        assert!(section.contains("Live action: x\n\nstart it"));
        assert!(section.contains("Destination branch: `feature-top` (pushed)"));
        assert!(section.contains("2. in `/w`"));
    }

    #[test]
    fn backoff_grows_and_is_capped() {
        assert_eq!(backoff_ms(1), 30_000);
        assert_eq!(backoff_ms(2), 60_000);
        assert_eq!(backoff_ms(40), MAX_BACKOFF_MS);
    }

    #[test]
    fn a_refresh_request_survives_failure_until_the_forge_accepts() {
        let store = Store::open_in_memory().unwrap();
        store.request_pr_live_refresh("pr-1", "g").unwrap();
        let due = store.due_pr_live_refreshes(now_ms() + 1).unwrap();
        assert_eq!(due.len(), 1);
        store.fail_pr_live_refresh("pr-1", 1, "boom").unwrap();
        // Failed: still pending, but not due before its backoff has elapsed.
        assert!(store.due_pr_live_refreshes(now_ms()).unwrap().is_empty());
        assert_eq!(
            store.pending_pr_live_refreshes().unwrap(),
            vec![("pr-1".to_string(), 1, Some("boom".to_string()))]
        );
        assert_eq!(
            store
                .due_pr_live_refreshes(now_ms() + backoff_ms(1) + 1)
                .unwrap()
                .len(),
            1
        );
        // A request made while an attempt is in flight is not lost.
        let requested = due[0].2;
        store.request_pr_live_refresh("pr-1", "g").unwrap();
        store.finish_pr_live_refresh("pr-1", requested - 1).unwrap();
        assert_eq!(store.pending_pr_live_refreshes().unwrap().len(), 1);
        store.finish_pr_live_refresh("pr-1", i64::MAX).unwrap();
        assert!(store.pending_pr_live_refreshes().unwrap().is_empty());
    }

    /// A store holding one review with a finished live run and one open PR
    /// (`#7`) whose description predates the run.
    fn published_fixture(
        forge: &str,
        repo: &str,
    ) -> (crate::store_lock::StoreHandle, String, String) {
        let raw = Store::open_in_memory().unwrap();
        let gid = raw.create_guardian("live", "main", "/repo").unwrap();
        let run_id = raw
            .queue_live_run(
                &gid,
                crate::live_runs::LiveRunKind::Live,
                0,
                "Greeting",
                "auto",
            )
            .unwrap();
        raw.conn
            .execute(
                "UPDATE guardian_live_runs SET state='finished', report='It worked.' WHERE run_id=?",
                rusqlite::params![run_id],
            )
            .unwrap();
        raw.record_live_run_sent(&run_id, Some("the exact prompt"), None)
            .unwrap();
        let pr_id = raw
            .create_pull_request(
                &gid,
                None,
                forge,
                repo,
                "alias",
                "main",
                "t",
                "stale",
                Some(7),
                None,
            )
            .unwrap();
        let store: crate::store_lock::StoreHandle =
            std::sync::Arc::new(crate::store_lock::StoreMutex::new(raw));
        (store, gid, pr_id)
    }

    #[test]
    fn publishing_changes_only_the_description_on_each_forge_and_persists_after_acceptance() {
        for (kind, mock_repo, path, field, method) in [
            (
                crate::forge::ForgeKind::GitHub,
                "acme/widget",
                "/repos/acme/widget/pulls/7",
                "body",
                tiny_http::Method::Patch,
            ),
            (
                crate::forge::ForgeKind::GitLab,
                "acme%2Fwidget",
                "/projects/acme%2Fwidget/merge_requests/7",
                "description",
                tiny_http::Method::Put,
            ),
        ] {
            let forge_name = if field == "body" { "github" } else { "gitlab" };
            let (store, _gid, pr_id) = published_fixture(forge_name, "fork/widget");
            let mock = crate::mock_forge::MockForge::start(move |server| {
                let get = server.recv();
                assert_eq!(get.url(), path);
                let body = serde_json::json!({ field: "My notes." }).to_string();
                get.respond(tiny_http::Response::from_string(body).with_status_code(200))
                    .unwrap();
                let mut write = server.recv();
                assert_eq!(write.method(), &method);
                assert_eq!(write.url(), path);
                let mut sent = String::new();
                std::io::Read::read_to_string(write.as_reader(), &mut sent).unwrap();
                write
                    .respond(tiny_http::Response::from_string("{}").with_status_code(200))
                    .unwrap();
                sent
            });
            let client = std::cell::RefCell::new(Some(mock.client(kind, mock_repo)));
            let routed_repo = std::cell::RefCell::new(String::new());
            let outcome = refresh_pr_with(&store, &pr_id, &|pr, _| {
                routed_repo.replace(pr.repo.clone());
                client.borrow_mut().take()
            })
            .unwrap();
            assert_eq!(outcome, RefreshOutcome::Updated);
            // The PR row's own forge repo (a fork) is what the client is routed by.
            assert_eq!(&*routed_repo.borrow(), "fork/widget");
            let sent: serde_json::Value = serde_json::from_str(&mock.finish()).unwrap();
            let object = sent.as_object().unwrap();
            // Only the description is sent: never the base, state or title.
            assert_eq!(object.len(), 1);
            let text = object[field].as_str().unwrap();
            assert!(text.starts_with(
                "My notes.

"
            ));
            assert!(text.contains(SECTION_START) && text.contains("the exact prompt"));
            let stored = store.lock().get_pull_request(&pr_id).unwrap();
            assert_eq!(stored.description, text);
            assert_eq!(stored.base_ref, "main");
        }
    }

    #[test]
    fn a_failed_publish_is_retried_later_and_keeps_the_local_description() {
        let (store, gid, pr_id) = published_fixture("github", "acme/widget");
        store
            .lock()
            .request_live_refresh_for_guardian(&gid)
            .unwrap();
        process_pr_refreshes_with(&store, &|_, _| None);
        let pending = store.lock().pending_pr_live_refreshes().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].1, 1);
        assert!(pending[0].2.as_deref().unwrap().contains("no forge client"));
        assert_eq!(
            store.lock().get_pull_request(&pr_id).unwrap().description,
            "stale"
        );
        // Not due again before its backoff has passed, and the run is untouched.
        assert!(
            store
                .lock()
                .due_pr_live_refreshes(now_ms())
                .unwrap()
                .is_empty()
        );
        let runs = crate::live_runs::guardian_live_runs_conn(&store.lock().conn, &gid).unwrap();
        assert_eq!(runs[0].state, "finished");
    }
}
