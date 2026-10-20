//! Live-test results in PR/MR descriptions (RAL-672).
//!
//! A review's finished live runs are written into each of its open PR/MR
//! descriptions as one managed `## Live-Test Results` section between stable
//! markers. Everything outside the markers is left exactly as the forge holds
//! it, so a section can be created or replaced on a PR that predates the run.
//!
//! The pieces:
//! * the structured result a live agent proposes ([`parse_result_line`]) and
//!   the recipe commands it proposes, each annotated by [`match_recipe`] as run
//!   exactly, run as a variant, or not run, by comparing it with the commands
//!   inside the recorded `RALPHUS_TOOL:` shell calls (ignoring navigation,
//!   redirection and output filters), so there is no second model judgment;
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
use crate::live_command_units::{Unit, command_units};
use crate::live_runs::LiveRunRow;
use crate::store::{Result, Store, now_ms};

/// Opens the managed section.
pub const SECTION_START: &str = "<!-- ralphus:live-test-results:start -->";
/// Closes the managed section.
pub const SECTION_END: &str = "<!-- ralphus:live-test-results:end -->";
/// Opens the managed ralphus-details section (insights and appraisals) of an
/// adopted PR/MR.
pub const DETAILS_START: &str = "<!-- ralphus:details:start -->";
/// Closes the managed ralphus-details section.
pub const DETAILS_END: &str = "<!-- ralphus:details:end -->";
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
/// Longest runner error shown in the PR body; the full text stays on the run record.
const MAX_ERROR_CHARS: usize = 100;

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
    /// The shell tool that ran it, lower-cased (`bash`, `powershell`, ...).
    pub shell: String,
}

/// How a proposed recipe command relates to the run's recorded shell calls.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepMatch {
    /// The recorded call is the proposed command, as written.
    #[default]
    Exact,
    /// The proposed command is one command inside a larger recorded call
    /// (`cd app && npm test 2>&1 | tee x`).
    Variant,
    /// No recorded call contains it.
    NotRun,
}

impl StepMatch {
    /// Whether a recorded call backs this step.
    #[must_use]
    pub fn ran(self) -> bool {
        self != Self::NotRun
    }
}

/// One proposed reproduction step and whether the agent was recorded running it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RecipeStep {
    /// The recommended command, as matched (as proposed when not run).
    pub command: String,
    /// The shell tool the call ran in (empty on rows stored before it was kept,
    /// and on a step that was not run).
    #[serde(default)]
    pub shell: String,
    /// The complete recorded tool call it was found in (empty on rows stored
    /// before it was kept, and on a step that was not run).
    #[serde(default)]
    pub recorded_command: String,
    pub cwd: String,
    /// 1-based position among the run's recorded shell calls; 0 when not run.
    pub order: usize,
    /// `succeeded`, `failed` or `no result recorded`; empty when not run.
    pub result: String,
    /// Rows stored before steps were annotated hold only matched steps.
    #[serde(default)]
    pub status: StepMatch,
}

/// The recipe as stored: the steps plus a marker that they were annotated.
/// Rows written before annotation are a bare array of matched steps, where an
/// empty list cannot tell "nothing proposed" from "nothing matched".
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(untagged)]
enum StoredRecipe {
    Annotated {
        annotated: bool,
        steps: Vec<RecipeStep>,
    },
    Legacy(Vec<RecipeStep>),
}

/// The recipe's steps and whether it was annotated (so unmatched commands are
/// present and an empty list truly means no recipe was proposed).
fn decode_recipe(raw: &str) -> (Vec<RecipeStep>, bool) {
    match serde_json::from_str::<StoredRecipe>(raw) {
        Ok(StoredRecipe::Annotated { annotated, steps }) => (steps, annotated),
        Ok(StoredRecipe::Legacy(steps)) => (steps, false),
        Err(_) => (Vec::new(), false),
    }
}

fn encode_recipe(steps: &[RecipeStep]) -> String {
    serde_json::to_string(&StoredRecipe::Annotated {
        annotated: true,
        steps: steps.to_vec(),
    })
    .unwrap_or_default()
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
                shell: tool,
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

/// Annotate every proposed recipe command against the recorded shell calls
/// (see [`command_units`]), each recorded command used at most once. A command
/// that equals a recorded call (trimmed) is [`StepMatch::Exact`]; one found
/// inside a different recorded call is [`StepMatch::Variant`]; one found
/// nowhere is [`StepMatch::NotRun`] and carries no recorded call, result or
/// order. Matched steps come first, ordered as they were run, with the whole
/// recorded invocation, its cwd (or `default_cwd`) and result; unmatched steps
/// follow in the order proposed. The commands of one proposed entry must match
/// in the order they were written and ran; one written out of order is not run.
#[must_use]
pub fn match_recipe(
    proposed: &[String],
    calls: &[RecordedCall],
    default_cwd: &str,
) -> Vec<RecipeStep> {
    let recorded: Vec<Vec<Unit>> = calls.iter().map(|c| command_units(&c.command)).collect();
    let mut used: Vec<Vec<bool>> = recorded.iter().map(|u| vec![false; u.len()]).collect();
    let mut matched: Vec<(usize, usize, String)> = Vec::new();
    let mut unmatched: Vec<String> = Vec::new();
    for entry in proposed {
        // The commands of one entry run in the order written, so each must
        // match a recorded command later than the one before it.
        let mut after: Option<(usize, usize)> = None;
        for unit in command_units(entry) {
            let found = recorded.iter().enumerate().find_map(|(i, units)| {
                units
                    .iter()
                    .enumerate()
                    .find(|(j, u)| {
                        !used[i][*j] && after.is_none_or(|a| (i, *j) > a) && unit.matches(u)
                    })
                    .map(|(j, _)| (i, j))
            });
            if let Some((i, j)) = found {
                used[i][j] = true;
                after = Some((i, j));
                matched.push((i, j, unit.shown_for(&recorded[i][j])));
            } else {
                unmatched.push(unit.text());
            }
        }
    }
    matched.sort_by_key(|(i, j, _)| (*i, *j));
    let mut steps: Vec<RecipeStep> = matched
        .into_iter()
        .map(|(i, _, command)| {
            let recorded_command = calls[i].command.trim().to_string();
            RecipeStep {
                status: if recorded_command == command.trim() {
                    StepMatch::Exact
                } else {
                    StepMatch::Variant
                },
                command,
                shell: calls[i].shell.clone(),
                recorded_command,
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
            }
        })
        .collect();
    steps.extend(unmatched.into_iter().map(|command| RecipeStep {
        command,
        shell: String::new(),
        recorded_command: String::new(),
        cwd: default_cwd.to_string(),
        order: 0,
        result: String::new(),
        status: StepMatch::NotRun,
    }));
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
    /// Whether `recipe` holds every proposed command, so an empty list means
    /// none was proposed. False on rows stored before steps were annotated.
    pub recipe_annotated: bool,
    /// The result the agent first reported, as JSON, when a recheck revised it.
    pub original_result: Option<String>,
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
                encode_recipe(recipe),
                run_id
            ],
        )?;
        Ok(())
    }

    /// Replace a run's verdict with the one its recheck produced, keeping the
    /// result first reported (verdict, summary, fix flags, recipe) as JSON in
    /// `original_result`. Only the first revision's original is kept.
    ///
    /// # Errors
    ///
    /// On a database failure.
    pub fn record_live_run_revision(
        &self,
        run_id: &str,
        verdict: Verdict,
        parsed: &ParsedResult,
        recipe: &[RecipeStep],
    ) -> Result<()> {
        let before = self.live_run_evidence(run_id)?;
        if before.original_result.is_none() {
            let original = serde_json::json!({
                "verdict": before.verdict,
                "summary": before.verdict_summary,
                "fix_attempted": before.fix_attempted,
                "fix_confirmed": before.fix_confirmed,
                "recipe": before.recipe,
            })
            .to_string();
            self.conn.execute(
                "UPDATE guardian_live_run_evidence SET original_result=? WHERE run_id=?",
                rusqlite::params![original, run_id],
            )?;
        }
        self.record_live_run_verdict(run_id, verdict, Some(parsed), recipe)
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
                        fix_confirmed, recipe, branch_ids, integration_branch_id, integration_pushed,
                        original_result
                 FROM guardian_live_run_evidence WHERE run_id=?",
                rusqlite::params![run_id],
                |r| {
                    let (recipe, recipe_annotated) = r
                        .get::<_, Option<String>>(6)?
                        .map_or((Vec::new(), false), |s| decode_recipe(&s));
                    let branches: Option<String> = r.get(7)?;
                    let flag = |i| r.get::<_, Option<i64>>(i).map(|v| v.map(|n| n != 0));
                    Ok(RunEvidence {
                        sent_prompt: r.get(0)?,
                        sent_system_prompt: r.get(1)?,
                        verdict: r.get(2)?,
                        verdict_summary: r.get(3)?,
                        fix_attempted: flag(4)?,
                        fix_confirmed: flag(5)?,
                        recipe,
                        recipe_annotated,
                        branch_ids: branches
                            .and_then(|s| serde_json::from_str(&s).ok())
                            .unwrap_or_default(),
                        integration_branch_id: r.get(8)?,
                        integration_pushed: flag(9)?,
                        original_result: r.get(10)?,
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

    /// Ask for a refresh of every open PR/MR whose body carries the details
    /// section and whose review has recorded a prophecy since the row was last
    /// written, so late prophecies (a restarted cell, a review's own conflict
    /// notes) reach the reviewer.
    ///
    /// # Errors
    ///
    /// On a database failure.
    pub fn request_details_refreshes(&self) -> Result<()> {
        let mut stmt = self.conn.prepare(
            "SELECT id, guardian_id, updated_at_ms FROM guardian_pull_requests
             WHERE state='open' AND pr_number IS NOT NULL AND superseded_by IS NULL
               AND instr(description, ?) > 0",
        )?;
        let rows: Vec<(String, String, i64)> = stmt
            .query_map(rusqlite::params![DETAILS_START], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })?
            .collect::<std::result::Result<_, _>>()?;
        for (pr_id, guardian_id, updated_at_ms) in rows {
            let newest = self
                .list_all_prophecies_for_guardian(&guardian_id)?
                .iter()
                .map(|p| p.created_at_ms)
                .max();
            if newest.is_some_and(|created| created > updated_at_ms) {
                self.request_pr_live_refresh(&pr_id, &guardian_id)?;
            }
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

/// `text` redacted, collapsed to one line, and cut to `max_chars` chars with a
/// trailing `...` only when something was cut.
fn elide_right(text: &str, max_chars: usize) -> String {
    let redacted = ralphus_core::redact::redact_secrets(text);
    let flat = redacted.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max_chars {
        return flat;
    }
    let kept: String = flat.chars().take(max_chars).collect();
    format!("{kept}...")
}

fn one_line(text: &str) -> String {
    clip(text).split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `(icon, word)` for a recorded verdict.
fn verdict_badge(verdict: Option<&str>) -> (&'static str, &'static str) {
    match verdict {
        Some("passed") => ("✅", "Passed"),
        Some("failed") => ("❌", "Failed"),
        _ => ("⚠️", "Inconclusive"),
    }
}

/// `cwd` relative to the run's checkout: empty for the checkout itself, the
/// relative path inside it, or `cwd` unchanged when it is elsewhere.
fn location(cwd: &str, checkout: Option<&str>) -> String {
    let norm = |p: &str| p.replace('\\', "/").trim_end_matches('/').to_string();
    let path = norm(cwd);
    let Some(root) = checkout.map(norm).filter(|r| !r.is_empty()) else {
        return cwd.to_string();
    };
    if path.eq_ignore_ascii_case(&root) {
        return String::new();
    }
    match path.get(..=root.len()) {
        Some(head) if head.eq_ignore_ascii_case(&format!("{root}/")) => {
            path[root.len() + 1..].to_string()
        }
        _ => cwd.to_string(),
    }
}

/// The agent's report as Markdown when it can be shown as-is, otherwise
/// fenced: an unbalanced fence, an HTML comment or a `<details>` tag would
/// break out of the collapsed block it sits in.
fn report_markdown(report: &str) -> String {
    let text = clip(report);
    let fences = text
        .lines()
        .filter(|l| {
            let l = l.trim_start();
            l.starts_with("```") || l.starts_with("~~~")
        })
        .count();
    let lower = text.to_ascii_lowercase();
    let unsafe_html = ["<details", "</details", "<!--", "-->"]
        .iter()
        .any(|t| lower.contains(t));
    if fences % 2 == 1 || unsafe_html {
        fenced(&text)
    } else {
        text.trim().to_string()
    }
}

/// Whether the agent's "passed" verdict or confirmed fix has no recorded shell
/// command behind it: the recipe was annotated and not one proposed command
/// matched. Rows stored before annotation are never flagged. The claim itself
/// is left as the agent made it; this only qualifies it.
#[must_use]
pub fn recipe_unbacked(ev: &RunEvidence) -> bool {
    ev.recipe_annotated && !ev.recipe.iter().any(|s| s.status.ran())
}

/// A "passed" verdict or confirmed fix that [`recipe_unbacked`] qualifies.
#[must_use]
pub fn verdict_unbacked(ev: &RunEvidence) -> bool {
    recipe_unbacked(ev)
        && (ev.verdict.as_deref() == Some("passed") || ev.fix_confirmed == Some(true))
}

const UNBACKED: &str = "not backed by a recorded command";

/// The reproduction block of a run: the coverage line, the commands the agent
/// was recorded running, and, separately, the ones it was not.
fn render_recipe(out: &mut String, ev: &RunEvidence, checkout: Option<&str>) {
    let ran: Vec<RecipeStep> = ev
        .recipe
        .iter()
        .filter(|s| s.status.ran())
        .cloned()
        .collect();
    let not_run: Vec<&RecipeStep> = ev.recipe.iter().filter(|s| !s.status.ran()).collect();
    if ev.recipe.is_empty() {
        let text = if ev.recipe_annotated {
            "The agent proposed no reproduction commands.\n"
        } else {
            "No reproduction commands matching recorded shell calls are available for this run.\n"
        };
        let _ = writeln!(out, "{text}");
        return;
    }
    let exact = ran.iter().filter(|s| s.status == StepMatch::Exact).count();
    let _ = write!(
        out,
        "{} of {} proposed command{} {} run by the agent",
        ran.len(),
        ev.recipe.len(),
        if ev.recipe.len() == 1 { "" } else { "s" },
        if ran.len() == 1 { "was" } else { "were" },
    );
    if !ran.is_empty() {
        let _ = write!(
            out,
            " ({exact} exactly, {} as a variant)",
            ran.len() - exact
        );
    }
    let _ = writeln!(out, ".\n");
    if ran.is_empty() {
        let _ = writeln!(
            out,
            "No reproduction commands matching recorded shell calls are available for this run.\n"
        );
    } else {
        render_reproduction(out, &ran, checkout);
    }
    if !not_run.is_empty() {
        let list: Vec<&str> = not_run.iter().map(|s| s.command.as_str()).collect();
        let block = fenced(&clip(&list.join("\n")));
        let _ = writeln!(
            out,
            "**Suggested, not run by the agent** (no recorded shell call ran these; do not read them as tested):\n\n{block}\n"
        );
    }
}

/// The `**Reproduce**` heading and code block for a run's accepted recipe,
/// with a `# in <dir>` line wherever the directory changes.
fn render_reproduction(out: &mut String, steps: &[RecipeStep], checkout: Option<&str>) {
    let dirs: Vec<String> = steps
        .iter()
        .map(|s| location(&clip(&s.cwd), checkout))
        .collect();
    let one_dir = dirs.windows(2).all(|w| w[0] == w[1]);
    let place = match dirs.first() {
        Some(dir) if one_dir && !dir.is_empty() => format!("in `{dir}`"),
        _ => "from the repo root".to_string(),
    };
    let shells: std::collections::BTreeSet<&str> = steps.iter().map(|s| s.shell.as_str()).collect();
    let (shell_name, language) = match shells.into_iter().collect::<Vec<_>>().as_slice() {
        ["powershell"] => (", PowerShell", "powershell"),
        ["bash"] => (", Bash", "sh"),
        _ => ("", "text"),
    };
    let mut lines: Vec<String> = Vec::new();
    let mut current_dir: Option<&str> = None;
    for (step, dir) in steps.iter().zip(&dirs) {
        if !one_dir && current_dir != Some(dir.as_str()) {
            lines.push(if dir.is_empty() {
                "# in the repo root".to_string()
            } else {
                format!("# in {dir}")
            });
            current_dir = Some(dir.as_str());
        }
        lines.push(step.command.clone());
    }
    let block = fenced(&clip(&lines.join("\n"))).replacen("text\n", &format!("{language}\n"), 1);
    let _ = writeln!(out, "**Reproduce** ({place}{shell_name}):\n\n{block}\n");
}

/// The collapsed "Run details": the fix outcome, any commits and where they
/// went, and each recorded shell call the recipe came from.
fn render_run_details(out: &mut String, item: &SectionRun) {
    let run = &item.run;
    let ev = &item.evidence;
    let destination = item
        .destination_branch
        .as_deref()
        .or(ev.integration_branch_id.as_deref())
        .map(|b| format!("`{b}`"));
    let _ = writeln!(out, "<details><summary>Run details</summary>\n");
    let _ = writeln!(out, "- {}", fix_line(item, destination.as_deref()));
    if !run.commits.is_empty() {
        let shas: Vec<String> = run
            .commits
            .iter()
            .map(|c| format!("`{}`", c.chars().take(12).collect::<String>()))
            .collect();
        let _ = writeln!(out, "- Commits: {}", shas.join(", "));
    }
    if run.integration != "none" {
        if let Some(detail) = run.integration_detail.as_deref() {
            let _ = writeln!(out, "- Carrying the fix: {}", one_line(detail));
        }
        let pushed = match ev.integration_pushed {
            Some(true) => "pushed",
            Some(false) => "not pushed",
            None => "push result not recorded",
        };
        let _ = writeln!(
            out,
            "- Destination branch: {} ({pushed})",
            destination.as_deref().unwrap_or("not recorded")
        );
    }
    if !ev.recipe.iter().any(|s| s.status.ran()) {
        let _ = writeln!(out, "- No recorded shell call matched the agent's recipe.");
    }
    // Several steps can come from one call; show each call once.
    let mut shown = std::collections::HashSet::new();
    let calls: Vec<&RecipeStep> = ev
        .recipe
        .iter()
        .filter(|s| !s.recorded_command.is_empty() && shown.insert(s.order))
        .collect();
    for step in &calls {
        let label = if calls.len() == 1 {
            String::new()
        } else {
            format!(" {}", step.order)
        };
        let block = fenced(&clip(&step.recorded_command))
            .lines()
            .map(|line| format!("  {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        let _ = writeln!(
            out,
            "- Recorded shell call{label} ({}):\n\n{block}\n",
            step.result
        );
    }
    let _ = writeln!(out, "\n</details>\n");
}

fn render_run(out: &mut String, item: &SectionRun) {
    let run = &item.run;
    let ev = &item.evidence;
    let (icon, verdict) = verdict_badge(ev.verdict.as_deref());
    let _ = writeln!(out, "### {icon} {}", one_line(&run.label));
    let _ = writeln!(out, "<!-- ralphus live run: {} -->\n", run.run_id);
    let trigger = if run.trigger == "auto" {
        "automatic"
    } else {
        "manual"
    };
    let unbacked = if recipe_unbacked(ev) && ev.verdict.as_deref() == Some("passed") {
        format!(" ({UNBACKED})")
    } else {
        String::new()
    };
    let _ = write!(
        out,
        "**{verdict}**{unbacked} · {trigger} run on the review's tip · {}",
        when(run.started_at_ms.or(Some(run.queued_at_ms)))
    );
    if run.state != "finished" {
        let _ = write!(out, " · runner state `{}`", run.state);
    }
    let _ = writeln!(out, "\n");
    match ev.verdict_summary.as_deref() {
        Some(summary) => {
            let _ = writeln!(out, "{}\n", one_line(summary));
        }
        None => {
            let _ = writeln!(out, "The agent reported no summary.\n");
        }
    }
    if let Some(error) = run.error.as_deref() {
        let _ = writeln!(
            out,
            "**Runner error:** {}\n",
            elide_right(error, MAX_ERROR_CHARS)
        );
    }
    render_recipe(out, ev, run.cwd.as_deref());
    let _ = writeln!(out, "<details><summary>What the agent observed</summary>\n");
    match run.report.as_deref().filter(|r| !r.trim().is_empty()) {
        Some(report) => {
            let _ = writeln!(out, "{}\n", report_markdown(report));
        }
        None => {
            let _ = writeln!(out, "No report was recorded.\n");
        }
    }
    let _ = writeln!(out, "</details>\n");
    let _ = writeln!(out, "<details><summary>Steps it was given</summary>\n");
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
    render_run_details(out, item);
}

/// One line per earlier run of an action, collapsed under its newest run.
fn render_earlier_runs(out: &mut String, items: &[&SectionRun]) {
    if items.is_empty() {
        return;
    }
    let _ = writeln!(
        out,
        "<details><summary>Earlier runs of this live test</summary>\n"
    );
    for item in items {
        let (icon, verdict) = verdict_badge(item.evidence.verdict.as_deref());
        let summary = item
            .evidence
            .verdict_summary
            .as_deref()
            .map_or_else(|| "The agent reported no summary.".to_string(), one_line);
        let _ = writeln!(
            out,
            "- {icon} **{verdict}** · {} · {summary} <!-- ralphus live run: {} -->",
            when(item.run.started_at_ms.or(Some(item.run.queued_at_ms))),
            item.run.run_id
        );
    }
    let _ = writeln!(out, "\n</details>\n");
}

/// The one-sentence fix outcome shown in the run details.
fn fix_line(item: &SectionRun, destination: Option<&str>) -> String {
    let run = &item.run;
    let ev = &item.evidence;
    let onto = destination.map_or_else(|| "the top branch".to_string(), str::to_string);
    if ev.fix_confirmed == Some(true) {
        let line = match run.integration.as_str() {
            "integrated" => format!("Fix: confirmed and carried onto {onto}"),
            "pending" | "integrating" => format!("Fix: confirmed; being carried onto {onto}"),
            _ => "Fix: confirmed but could not be carried".to_string(),
        };
        return if recipe_unbacked(ev) {
            format!("{line} ({UNBACKED})")
        } else {
            line
        };
    }
    if ev.fix_attempted == Some(true) || !run.commits.is_empty() {
        return "Fix: attempted but not confirmed".to_string();
    }
    "Fix: not required".to_string()
}
/// The managed section for a set of runs, markers included, or `None` when
/// there is nothing to show. Each action gets a heading with its newest run in
/// full; up to [`MAX_RUNS_PER_ACTION`]` - 1` earlier runs follow it collapsed,
/// newest first.
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
        items.truncate(MAX_RUNS_PER_ACTION);
        if let Some((newest, earlier)) = items.split_first() {
            render_run(&mut out, newest);
            render_earlier_runs(&mut out, earlier);
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
    splice_between(current, section, SECTION_START, SECTION_END)
}

/// [`splice_section`] for the ralphus-details marker pair.
#[must_use]
pub fn splice_details_section(current: &str, section: &str) -> String {
    splice_between(current, section, DETAILS_START, DETAILS_END)
}

/// The ralphus-details section (markers included) for `prophecies` and
/// `appraisals`, or `None` when both are empty. The appraisal block is
/// truncated to fit `max_chars` after the prophecy block.
#[must_use]
pub fn render_details_section(
    prophecies: &[crate::prophecy::ProphecyView],
    appraisals: &[crate::appraisal::ReviewAppraisal],
    max_chars: usize,
) -> Option<String> {
    let insights = crate::pr::format_prophecy_details_block(prophecies);
    let used = insights.as_ref().map_or(0, |b| b.chars().count() + 2)
        + DETAILS_START.len()
        + DETAILS_END.len()
        + "\n## Ralphus Details\n\n\n".len();
    let appraisal_block =
        crate::appraisal_pr::render_pr_block(appraisals, max_chars.saturating_sub(used));
    let body = [appraisal_block, insights]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join("\n\n");
    if body.is_empty() {
        return None;
    }
    Some(format!(
        "{DETAILS_START}\n## Ralphus Details\n\n{body}\n{DETAILS_END}"
    ))
}

fn splice_between(current: &str, section: &str, start_marker: &str, end_marker: &str) -> String {
    if let Some(start) = current.find(start_marker) {
        if let Some(end_rel) = current[start..].find(end_marker) {
            let end = start + end_rel + end_marker.len();
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
    let section = render_section(&runs);
    // A PR whose body carries the details section keeps it current.
    let has_details = pr.description.contains(DETAILS_START);
    if section.is_none() && !has_details {
        return Ok(RefreshOutcome::Skipped);
    }
    let client = client_for(&pr, &guardian)
        .ok_or_else(|| format!("no forge client for {} on PR #{number}", pr.repo))?;
    let current = client.get_pull_request_description(number)?;
    let mut updated = match &section {
        Some(section) => splice_section(&current, section),
        None => current.clone(),
    };
    let mut details_published: (Vec<i64>, Vec<crate::appraisal::ReviewAppraisal>) =
        (Vec::new(), Vec::new());
    if has_details || current.contains(DETAILS_START) {
        let (prophecies, appraisals) = {
            let guard = store.lock();
            let prophecies = guard
                .list_all_prophecies_for_guardian(&guardian.id)
                .map_err(|e| e.to_string())?
                .into_iter()
                .filter(|p| p.pr_id.as_deref().is_none_or(|owner| owner == pr.id))
                .collect::<Vec<_>>();
            let appraisals = guard
                .list_all_appraisals_for_guardian(&guardian.id)
                .map_err(|e| e.to_string())?
                .into_iter()
                .filter(|a| {
                    a.appraisal
                        .pr_id
                        .as_deref()
                        .is_none_or(|owner| owner == pr.id)
                })
                .collect::<Vec<_>>();
            (prophecies, appraisals)
        };
        let prophecies = crate::pr::deterministic_relevant_prophecies(&prophecies);
        let appraisals = if crate::appraisal_pr::post_appraisals_enabled(&guardian) {
            appraisals
        } else {
            Vec::new()
        };
        let outside = splice_details_section(&updated, "").chars().count();
        if let Some(details) = render_details_section(
            &prophecies,
            &appraisals,
            crate::appraisal_pr::PR_BODY_BUDGET.saturating_sub(outside + 2),
        ) {
            updated = splice_details_section(&updated, &details);
            details_published = (
                prophecies
                    .iter()
                    .filter(|p| p.published_at_ms.is_none())
                    .map(|p| p.id)
                    .collect(),
                appraisals
                    .into_iter()
                    .filter(|a| a.appraisal.published_at_ms.is_none())
                    .collect(),
            );
        }
    }
    if normalize_newlines(&updated) == normalize_newlines(&current) {
        let _ = store.lock().set_pr_description(&pr.id, &current);
        return Ok(RefreshOutcome::Unchanged);
    }
    client.update_pull_request_description(number, &updated)?;
    {
        let guard = store.lock();
        let _ = guard.set_pr_description(&pr.id, &updated);
        let _ = guard.mark_prophecies_published(&details_published.0, &pr.id);
        for posted in &details_published.1 {
            let a = &posted.appraisal;
            let _ = guard.mark_appraisal_published(&a.entity_uri, a.attempt, &pr.id);
        }
    }
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
    let due = {
        let guard = store.lock();
        let _ = guard.request_details_refreshes();
        guard.due_pr_live_refreshes(now_ms()).unwrap_or_default()
    };
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

// ---- the recheck ------------------------------------------------------------

/// System prompt of the one resumed turn that re-examines a run's result.
pub const RECHECK_SYSTEM_PROMPT: &str = "\
You are re-examining the result of a live test you just ran. You may re-run commands in this \
checkout to back your claims, but make no edits and no commits. Reply with a corrected final \
`RALPHUS_LIVE_RESULT:` line (the same JSON shape as before) whose `recipe` lists only commands \
you actually ran, exactly as recorded.";

/// Whether a finished run's recipe leaves something to recheck: a step not run
/// or only run as part of a different command, or a verdict no recorded command backs.
#[must_use]
pub fn needs_recheck(ev: &RunEvidence) -> bool {
    verdict_unbacked(ev)
        || (ev.recipe_annotated
            && ev
                .recipe
                .iter()
                .any(|s| matches!(s.status, StepMatch::NotRun | StepMatch::Variant)))
}

/// The recorded call sharing the most words with `command`, if any shares one.
fn closest_recorded<'a>(command: &str, calls: &'a [RecordedCall]) -> Option<&'a str> {
    let words: std::collections::HashSet<&str> = command.split_whitespace().collect();
    calls
        .iter()
        .map(|c| {
            let shared = c
                .command
                .split_whitespace()
                .filter(|w| words.contains(w))
                .count();
            (shared, c.command.as_str())
        })
        .filter(|(shared, _)| *shared > 0)
        .max_by_key(|(shared, _)| *shared)
        .map(|(_, command)| command)
}

/// The recheck's user prompt: each recipe step, how it matched the recorded
/// calls, and the recorded command (or, for a step not run, the closest one).
#[must_use]
pub fn recheck_prompt(ev: &RunEvidence, calls: &[RecordedCall]) -> String {
    let mut out = String::from(
        "Your recorded shell calls do not fully back the recipe you reported. Each step below \
         shows how it matched what you actually ran.\n\n\
         | Recipe step | Match | Recorded command |\n|---|---|---|\n",
    );
    let cell = |text: &str| text.replace('|', "\\|").replace('\n', " ");
    for step in &ev.recipe {
        let (status, recorded) = match step.status {
            StepMatch::Exact => ("exact", step.recorded_command.clone()),
            StepMatch::Variant => ("variant", step.recorded_command.clone()),
            StepMatch::NotRun => (
                "not run",
                closest_recorded(&step.command, calls)
                    .map_or_else(|| "(none)".to_string(), |c| format!("closest: {c}")),
            ),
        };
        out.push_str(&format!(
            "| {} | {status} | {} |\n",
            cell(&step.command),
            cell(&recorded)
        ));
    }
    if ev.recipe.is_empty() {
        out.push_str("| (no recipe reported) | not run | (none) |\n");
    }
    out.push_str(
        "\nRe-run any step you still stand behind, then end your reply with a corrected \
         `RALPHUS_LIVE_RESULT:` line. If a step cannot be backed, leave it out and lower the \
         verdict accordingly.",
    );
    out
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
    let recipe = match &parsed {
        Some(p) if !p.recipe.is_empty() => {
            let calls = recorded_calls_for_run(guardian_id, &outcome.cell_id);
            match_recipe(&p.recipe, &calls, run.cwd.as_deref().unwrap_or_default())
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

    #[test]
    fn details_and_live_sections_coexist_and_replace_in_place() {
        let live = format!(
            "{SECTION_START}
live
{SECTION_END}"
        );
        let details = format!(
            "{DETAILS_START}
one
{DETAILS_END}"
        );
        let body = splice_section("Human text", &live);
        let body = splice_details_section(&body, &details);
        assert!(body.starts_with(
            "Human text

"
        ));
        assert!(body.contains(&live) && body.contains(&details));
        let newer = format!(
            "{DETAILS_START}
two
{DETAILS_END}"
        );
        let again = splice_details_section(&body, &newer);
        assert_eq!(again.matches(DETAILS_START).count(), 1);
        assert!(again.contains(&live) && again.contains("two") && !again.contains("one"));
        let live2 = format!(
            "{SECTION_START}
live2
{SECTION_END}"
        );
        let both = splice_section(&again, &live2);
        assert!(
            both.contains("two")
                && both.contains("live2")
                && !both.contains(
                    "
live
"
                )
        );
    }

    #[test]
    fn elide_right_cuts_long_multiline_text_to_one_line() {
        let long = "runner produced no result file: boom\nlast pane output:\n".to_string()
            + &"tool call {json}\n".repeat(400);
        let out = elide_right(&long, MAX_ERROR_CHARS);
        assert!(!out.contains('\n'));
        assert!(out.ends_with("..."));
        assert_eq!(out.chars().count(), MAX_ERROR_CHARS + 3);
        assert!(out.starts_with("runner produced no result file: boom last pane output:"));
    }

    #[test]
    fn elide_right_leaves_short_text_alone() {
        let exact = "x".repeat(MAX_ERROR_CHARS);
        assert_eq!(elide_right(&exact, MAX_ERROR_CHARS), exact);
        assert_eq!(elide_right("a\n  b", MAX_ERROR_CHARS), "a b");
    }

    #[test]
    fn elide_right_cuts_on_char_boundaries() {
        let out = elide_right(&"é".repeat(150), MAX_ERROR_CHARS);
        assert_eq!(out, format!("{}...", "é".repeat(MAX_ERROR_CHARS)));
    }

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
                    shell: String::new(),
                    recorded_command: "cd w && cargo run | head".into(),
                    cwd: "/w".into(),
                    order: 2,
                    result: "succeeded".into(),
                    status: StepMatch::Variant,
                }],
                recipe_annotated: true,
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
    fn the_recipe_annotates_every_command_with_whether_it_was_run() {
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
                    s.status,
                )
            })
            .collect();
        assert_eq!(
            summary,
            vec![
                (1, "cargo build", "/w/sub", "succeeded", StepMatch::Exact),
                (2, "curl localhost", "/w", "failed", StepMatch::Exact),
                (3, "ls", "/w", "no result recorded", StepMatch::Exact),
                (0, "rm -rf /", "/w", "", StepMatch::NotRun),
            ]
        );
    }

    #[test]
    fn a_compound_recipe_must_keep_the_order_the_commands_ran_in() {
        let call = |c: &str| RecordedCall {
            command: c.into(),
            cwd: Some("/w".into()),
            succeeded: Some(true),
            shell: "bash".into(),
        };
        let commands = |proposed: &[&str], recorded: &[&str]| -> Vec<String> {
            let proposed: Vec<String> = proposed.iter().map(|p| (*p).to_string()).collect();
            let recorded: Vec<RecordedCall> = recorded.iter().map(|r| call(r)).collect();
            match_recipe(&proposed, &recorded, "/d")
                .into_iter()
                .filter(|s| s.status.ran())
                .map(|s| s.command)
                .collect()
        };
        assert_eq!(commands(&["a && b"], &["a && b"]), ["a", "b"]);
        assert_eq!(commands(&["a && c"], &["a && b && c"]), ["a", "c"]);
        // Written out of order: the out-of-order command is not matched.
        assert_eq!(commands(&["b && a"], &["a && b"]), ["b"]);
        assert_eq!(commands(&["c && a && b"], &["a && b && c"]), ["c"]);
        assert_eq!(commands(&["b && a"], &["a", "b"]), ["b"]);
        // Separate entries are independent; the result is in run order.
        assert_eq!(commands(&["b", "a"], &["a && b"]), ["a", "b"]);
        // A repeated command matches once per time it ran.
        assert_eq!(commands(&["a && a"], &["a"]), ["a"]);
        assert_eq!(commands(&["a && a"], &["a && b && a"]), ["a", "a"]);
    }

    #[test]
    fn a_wrapper_outside_a_shell_invocation_still_unwraps_a_single_command() {
        let call = |c: &str| RecordedCall {
            command: c.into(),
            cwd: Some("/w".into()),
            succeeded: Some(true),
            shell: "bash".into(),
        };
        let run = |proposed: &str, recorded: &str| -> Vec<RecipeStep> {
            match_recipe(&[proposed.to_string()], &[call(recorded)], "/d")
                .into_iter()
                .filter(|s| s.status.ran())
                .collect()
        };
        let steps = run("foo", "sudo bash -c 'foo'");
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].command, "sudo foo");
        assert_eq!(steps[0].recorded_command, "sudo bash -c 'foo'");
        assert_eq!(run("sudo foo", "sudo bash -c 'foo'").len(), 1);
        assert_eq!(
            run("foo", "timeout 5 bash -c 'foo'")[0].command,
            "timeout 5 foo"
        );
        assert!(run("timeout 9 foo", "timeout 5 bash -c 'foo'").is_empty());
        assert!(run("foo", "FOO=1 bash -c 'foo'").is_empty());
        assert_eq!(run("FOO=1 foo", "FOO=1 bash -c 'foo'").len(), 1);
        // Several commands under one bound are not compared piece by piece.
        assert!(run("a", "sudo bash -c 'a && b'").is_empty());
    }

    #[test]
    fn a_bounding_wrapper_is_restored_and_a_standalone_command_is_kept() {
        let call = |c: &str| RecordedCall {
            command: c.into(),
            cwd: Some("/w".into()),
            succeeded: Some(true),
            shell: "bash".into(),
        };
        let run = |proposed: &str, recorded: &str| -> Vec<RecipeStep> {
            match_recipe(&[proposed.to_string()], &[call(recorded)], "/d")
                .into_iter()
                .filter(|s| s.status.ran())
                .collect()
        };
        let steps = run("foo --bar", "timeout 100 foo --bar | head");
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].command, "timeout 100 foo --bar");
        assert_eq!(steps[0].recorded_command, "timeout 100 foo --bar | head");
        assert_eq!(
            run("pytest", "source .venv/bin/activate && pytest").len(),
            1
        );
        let kept = run(
            "source .venv/bin/activate && pytest",
            "source .venv/bin/activate && pytest",
        );
        assert_eq!(kept.len(), 2);
        assert!(
            run(
                "source other/bin/activate",
                "source .venv/bin/activate && pytest"
            )
            .is_empty()
        );
        assert!(run("foo", "yes | foo").is_empty());
    }

    #[test]
    fn command_variants_match_and_keep_the_recorded_call() {
        let call = |c: &str| RecordedCall {
            command: c.into(),
            cwd: Some("/w".into()),
            succeeded: Some(true),
            shell: "bash".into(),
        };
        let run = |proposed: &[&str], recorded: &str| -> Vec<RecipeStep> {
            let p: Vec<String> = proposed.iter().map(|s| (*s).to_string()).collect();
            match_recipe(&p, &[call(recorded)], "/d")
                .into_iter()
                .filter(|s| s.status.ran())
                .collect()
        };
        for (proposed, recorded) in [
            (
                "fizz buzz --count 300",
                "fizz buzz --count 300 | head -n 10",
            ),
            (
                "cargo test -p app",
                "cargo test -p app 2>&1 | grep -E 'FAILED|error'",
            ),
            (
                "npm test && npm run lint",
                "npm test | tee test.log && npm run lint | tail -n 20",
            ),
            ("Get-Service", "Get-Service | Select-Object -First 10"),
            (
                "dotnet test",
                "dotnet test 2>&1 | Select-String 'Failed|Error'",
            ),
            ("dir /b", "dir /b | findstr /i \".rs\""),
            ("type app.log", "type app.log | findstr /i \"error\""),
            ("some-command --check", "some-command --check 2>&1 | more"),
            ("do_something foo --bar", "cd /x && do_something foo --bar"),
            (
                "fizz buzz --count 300 && another one",
                "fizz buzz --count 300 | head -n 10 && another one | tail -n 20",
            ),
        ] {
            let steps = run(&[proposed], recorded);
            assert!(!steps.is_empty(), "{recorded}");
            assert!(steps.iter().all(|s| s.recorded_command == recorded
                && s.cwd == "/w"
                && s.order == 1
                && s.status != StepMatch::NotRun));
        }
        let steps = run(&["npm test"], "npm test && npm run deploy");
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].command, "npm test");
        let steps = run(&["npm test && npm run lint"], "npm test");
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].status, StepMatch::Exact);
        let all = match_recipe(
            &["npm test && npm run lint".into()],
            &[call("npm test")],
            "/d",
        );
        assert_eq!(all.len(), 2);
        assert_eq!(all[1].command, "npm run lint");
        assert_eq!(all[1].status, StepMatch::NotRun);
        assert!(run(&["rm -rf /"], "ls").is_empty());
        assert!(run(&["npm test"], "MODE=production npm test").is_empty());
        assert_eq!(
            run(
                &["MODE=production npm test"],
                "MODE=production npm test | head"
            )
            .len(),
            1
        );
        assert!(run(&["echo a"], "echo 'a && b'").is_empty());
    }

    #[test]
    fn the_section_shows_the_recipe_and_collapses_the_recorded_call() {
        let section = render_section(&[section_run("r1", 0, 10)]).unwrap();
        assert!(section.contains("**Reproduce** (from the repo root):\n\n```text\ncargo run\n```"));
        let details = section.find("<summary>Run details</summary>").unwrap();
        let call = section.find("cd w && cargo run | head").unwrap();
        assert!(details < call);
    }

    fn step_with(command: &str, status: StepMatch) -> RecipeStep {
        RecipeStep {
            command: command.into(),
            shell: String::new(),
            recorded_command: if status.ran() {
                command.into()
            } else {
                String::new()
            },
            cwd: "/w".into(),
            order: usize::from(status.ran()),
            result: if status.ran() {
                "succeeded".into()
            } else {
                String::new()
            },
            status,
        }
    }

    fn evidence_with(steps: Vec<RecipeStep>, annotated: bool) -> RunEvidence {
        RunEvidence {
            verdict: Some("passed".into()),
            verdict_summary: Some("works".into()),
            fix_attempted: Some(true),
            fix_confirmed: Some(true),
            recipe: steps,
            recipe_annotated: annotated,
            ..RunEvidence::default()
        }
    }

    fn render_with(ev: RunEvidence) -> String {
        let mut run = section_run("r1", 0, 10);
        run.evidence = ev;
        render_section(&[run]).unwrap()
    }

    #[test]
    fn the_section_shows_coverage_and_lists_unrun_commands_apart() {
        let section = render_with(evidence_with(
            vec![
                step_with("cargo build", StepMatch::Exact),
                step_with("cargo test", StepMatch::NotRun),
            ],
            true,
        ));
        assert!(section.contains("1 of 2 proposed commands was run by the agent"));
        let ran = section.find("**Reproduce**").unwrap();
        let heading = section.find("**Suggested, not run by the agent**").unwrap();
        assert!(ran < heading);
        assert!(section[heading..].contains("cargo test"));
        assert!(!section[ran..heading].contains("cargo test"));
    }

    #[test]
    fn an_empty_recipe_says_whether_none_was_proposed_or_none_ran() {
        let none = render_with(evidence_with(Vec::new(), true));
        assert!(none.contains("The agent proposed no reproduction commands."));
        let unrun = render_with(evidence_with(
            vec![step_with("cargo test", StepMatch::NotRun)],
            true,
        ));
        assert!(unrun.contains("0 of 1 proposed command were run"));
        assert!(unrun.contains("No reproduction commands matching recorded shell calls"));
        assert!(unrun.contains("**Suggested, not run by the agent**"));
    }

    #[test]
    fn a_legacy_recipe_decodes_as_matched_and_is_never_flagged() {
        let legacy = r#"[{"command":"ls","shell":"","recorded_command":"ls","cwd":"/w","order":1,"result":"succeeded"}]"#;
        let (steps, annotated) = decode_recipe(legacy);
        assert!(!annotated);
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].status, StepMatch::Exact);
        assert!(!verdict_unbacked(&evidence_with(Vec::new(), false)));
        assert!(!verdict_unbacked(&evidence_with(steps, false)));
        let (steps, annotated) =
            decode_recipe(&encode_recipe(&[step_with("ls", StepMatch::NotRun)]));
        assert!(annotated);
        assert_eq!(steps[0].status, StepMatch::NotRun);
    }

    #[test]
    fn a_verdict_without_a_matched_command_is_qualified_but_not_rewritten() {
        let ev = evidence_with(vec![step_with("cargo test", StepMatch::NotRun)], true);
        assert!(verdict_unbacked(&ev));
        let section = render_with(ev);
        assert!(section.contains("**Passed** (not backed by a recorded command)"));
        assert!(
            section
                .contains("not backed by a recorded command")
                .then_some(())
                .is_some()
        );
        let none = evidence_with(Vec::new(), true);
        assert!(verdict_unbacked(&none));
        assert_eq!(none.verdict.as_deref(), Some("passed"));
        let failed = RunEvidence {
            verdict: Some("failed".into()),
            fix_confirmed: Some(false),
            ..evidence_with(Vec::new(), true)
        };
        assert!(!verdict_unbacked(&failed));
    }

    #[test]
    fn a_matched_command_backs_the_verdict() {
        let ev = evidence_with(vec![step_with("cargo build", StepMatch::Variant)], true);
        assert!(!verdict_unbacked(&ev));
        assert!(!render_with(ev).contains("not backed by a recorded command"));
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
        assert!(
            second.contains("ralphus live run: r2 ") && !second.contains("ralphus live run: r1 ")
        );
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
        let pos = |id: &str| section.find(&format!("ralphus live run: {id} "));
        assert!(pos("run4").unwrap() < pos("run3").unwrap());
        assert!(pos("run3").unwrap() < pos("run2").unwrap());
        assert!(pos("run1").is_none() && pos("run0").is_none());
        assert!(pos("other").is_some());
        // One heading per action: the newest run in full, earlier ones collapsed.
        assert_eq!(section.matches("### ").count(), 2);
        let earlier = section
            .find("<summary>Earlier runs of this live test</summary>")
            .unwrap();
        assert!(pos("run4").unwrap() < earlier && earlier < pos("run3").unwrap());
        assert!(render_section(&[]).is_none());
    }

    #[test]
    fn unknown_details_are_stated_and_secrets_redacted() {
        let mut item = section_run("r1", 0, 1);
        item.evidence = RunEvidence::default();
        item.run.report = Some("ran ANTHROPIC_API_KEY=NOTAREALKEY run".into());
        let section = render_section(&[item]).unwrap();
        assert!(section.contains("### ⚠️ ") && section.contains("**Inconclusive**"));
        assert!(section.contains("The agent reported no summary."));
        assert!(section.contains("Destination branch: `feature-top` (push result not recorded)"));
        assert!(section.contains("No reproduction commands"));
        assert!(section.contains("- No recorded shell call matched the agent's recipe."));
        assert!(!section.contains("NOTAREALKEY"));
    }

    #[test]
    fn the_exact_prompt_and_destination_are_rendered() {
        let section = render_section(&[section_run("r1", 0, 1)]).unwrap();
        let steps = section
            .find("<details><summary>Steps it was given</summary>\n\n```text\nLive action: x\n\nstart it\n```\n\nSystem prompt:\n\n```text\nsystem\n```")
            .unwrap();
        let details = section.find("<summary>Run details</summary>").unwrap();
        assert!(steps < details);
        assert!(section.contains("Destination branch: `feature-top` (pushed)"));
    }

    #[test]
    fn each_action_is_a_heading_with_its_verdict_then_the_collapsed_details() {
        let section = render_section(&[section_run("lr9-1", 0, 1)]).unwrap();
        let at = |s: &str| section.find(s).unwrap();
        // The internal id is only an HTML comment, never visible text.
        assert!(section.contains(
            "### ✅ Board shows reviews\n<!-- ralphus live run: lr9-1 -->\n\n**Passed** · automatic run on the review's tip · "
        ));
        assert_eq!(section.matches("lr9-1").count(), 1);
        assert!(at("**Passed**") < at("greeting shown"));
        assert!(at("greeting shown") < at("**Reproduce**"));
        assert!(at("**Reproduce**") < at("<summary>What the agent observed</summary>"));
        assert!(
            at("<summary>What the agent observed</summary>")
                < at("<summary>Steps it was given</summary>")
        );
        assert!(at("<summary>Steps it was given</summary>") < at("<summary>Run details</summary>"));
        // The fix outcome is a sentence, not internal state names.
        assert!(section.contains("- Fix: attempted but not confirmed\n"));
        assert!(!section.contains("Integration:") && !section.contains("Commits: none"));
        assert_eq!(
            section.matches("<details>").count(),
            section.matches("</details>").count()
        );
        // The report is shown as Markdown, not as a code block.
        assert!(section.contains("<summary>What the agent observed</summary>\n\nIt worked.\n"));
    }

    #[test]
    fn a_powershell_recipe_says_so_and_keeps_every_fence_on_its_own_line() {
        let recorded = "moon run app:build 2>&1 | Select-Object -Last 20; app hello; app --help";
        let transcript = [
            tool_line(
                "1",
                "PowerShell",
                &serde_json::json!({ "command": recorded }).to_string(),
                None,
            ),
            tool_line("1", "PowerShell", "", Some(false)),
        ]
        .join("\n");
        let recipe: Vec<String> = ["moon run app:build", "app hello", "app --help"]
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        let mut item = section_run("r1", 0, 10);
        item.run.cwd = Some("C:\\repo\\.git\\.ralphus\\g\\g1\\review-postmerge".into());
        item.evidence.recipe = match_recipe(
            &recipe,
            &extract_shell_calls(&transcript),
            item.run.cwd.as_deref().unwrap(),
        );
        let section = render_section(&[item]).unwrap();
        assert!(section.contains(
            "**Reproduce** (from the repo root, PowerShell):\n\n```powershell\nmoon run app:build\napp hello\napp --help\n```\n"
        ));
        // The one call all three steps came from is listed once.
        assert_eq!(
            section
                .matches("- Recorded shell call (succeeded):")
                .count(),
            1
        );
        // GitHub only opens a fence at the start of a line (after a list
        // item's indentation); a mid-line fence turns the rest into code.
        for line in section.lines() {
            if let Some(at) = line.find("```") {
                assert!(line[..at].trim().is_empty(), "mid-line fence: {line:?}");
            }
        }
    }

    #[test]
    fn commands_from_several_directories_say_where_each_runs() {
        let mut item = section_run("r1", 0, 10);
        let step = |command: &str, cwd: &str, order: usize| RecipeStep {
            command: command.into(),
            shell: "bash".into(),
            recorded_command: command.into(),
            cwd: cwd.into(),
            order,
            result: "succeeded".into(),
            status: StepMatch::Exact,
        };
        item.evidence.recipe = vec![step("make", "/w", 1), step("npm test", "/w/web", 2)];
        let section = render_section(&[item]).unwrap();
        assert!(section.contains(
            "**Reproduce** (from the repo root, Bash):\n\n```sh\n# in the repo root\nmake\n# in web\nnpm test\n```\n"
        ));
        assert!(section.contains("- Recorded shell call 1 (succeeded):"));
        assert!(section.contains("- Recorded shell call 2 (succeeded):"));
    }

    #[test]
    fn a_report_that_would_break_its_block_is_fenced() {
        assert_eq!(report_markdown("- **Build:** ok"), "- **Build:** ok");
        assert!(report_markdown("half a fence\n```\nnever closed").starts_with("```"));
        assert!(report_markdown("ends early </details> oops").starts_with("```"));
    }

    #[test]
    fn the_fix_line_reflects_what_happened() {
        let fix = |confirmed, attempted, integration: &str, commits: bool| {
            let mut item = section_run("r1", 0, 1);
            item.evidence.fix_confirmed = confirmed;
            item.evidence.fix_attempted = attempted;
            item.run.integration = integration.into();
            if !commits {
                item.run.commits.clear();
            }
            fix_line(&item, Some("`top`"))
        };
        assert_eq!(
            fix(Some(true), Some(true), "integrated", true),
            "Fix: confirmed and carried onto `top`"
        );
        assert_eq!(
            fix(Some(true), Some(true), "failed", true),
            "Fix: confirmed but could not be carried"
        );
        assert_eq!(
            fix(Some(false), Some(true), "none", true),
            "Fix: attempted but not confirmed"
        );
        assert_eq!(
            fix(Some(false), Some(false), "none", false),
            "Fix: not required"
        );
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
