//! Live actions: agent-run review actions that test a review's tip live.
//!
//! A live action is a prompt an agent executes against the review's prepared
//! checkout -- launching the product, driving it, and reporting what it saw.
//! Its prompts come from the live handoffs member cells posted
//! (`RALPHUS_LIVE_ACTION:`), deduplicated once per review; a user action whose
//! `prompt` is set runs through the same executor. See `docs/glossary.md`
//! ("live action", "live run", "live ref").
//!
//! This module owns one live run's execution: holding the checkout, running
//! the agent in tmux (so the board's Live View can follow it), recording its
//! cost, committing anything it left uncommitted, and pinning its commits on a
//! live ref so they survive the checkout being reset later.

use std::path::Path;
use std::time::Duration;

use crate::cancel::CancelToken;
use crate::guardian::GuardianView;
use crate::runner::{Runner, RunnerSpec};
use crate::workspace::Workspace;

/// The task name every live run's tmux session and transcript live under,
/// inside the review's `guardian-{id}` squad namespace.
pub const LIVE_ACTION_TASK: &str = "live-action";

/// How long a live run may hold its checkout before the hold is considered
/// abandoned. An agent session is bounded well below this by its own
/// timeouts; the margin only matters for a daemon that died mid-run.
pub const LIVE_RUN_HOLD_MAX_AGE: Duration = Duration::from_secs(12 * 60 * 60);

/// Cap on the agent report copied into the "live run finished" log row.
const REPORT_LOG_CHARS: usize = 4000;

/// The prefix every live ref lives under. Not `refs/heads/`, so live refs stay
/// out of branch listings and can never be pushed by accident.
pub const LIVE_REF_PREFIX: &str = "refs/ralphus/live";

/// Daemon-owned guidance every live-action agent receives, whatever the
/// action's own prompt says. The commit variant is used when the run's commits
/// will be carried back into the review.
pub const LIVE_ACTION_COMMIT_SYSTEM_PROMPT: &str = "\
You are running a live action for a ralphus review: you are testing the \
review's current tip, live, in this checkout. The build is already prepared \
here. Follow the steps in the prompt for real -- start the program, drive it, \
run the commands -- and observe what actually happens. Do not stop at reading \
code or running the unit tests: the point is to catch what only shows up when \
the thing runs (runtime errors, wiring and setup problems, behaviour that does \
not match the intent).\n\
\n\
If the prompt says the action is read-only, change nothing. Otherwise, when you \
find a problem you can fix, fix it, then stage and commit ONLY the files you \
changed with a normal, descriptive commit message that says what you saw while \
testing. Never push, never rebase, never amend or rewrite existing commits, and \
never switch branches -- ralphus carries your commits into the review and \
pushes them to the right place itself. Read-only git commands (status, diff, \
log) are fine. Never commit build output.\n\
\n\
Setup and runtime failures are real findings: if the program will not start, a \
command the steps rely on is missing, or the configuration is wrong, that is \
exactly what you are here to catch -- report it (and fix it if it is in this \
repository). Stop any process you started before you finish.\n\
\n\
End your reply with a short report: what you tested, what you observed, what \
you fixed (if anything), and anything still broken.";

/// The read-only variant: the run's edits are never carried back.
pub const LIVE_ACTION_READ_ONLY_SYSTEM_PROMPT: &str = "\
You are running a live action for a ralphus review: you are testing the \
review's current tip, live, in this checkout. The build is already prepared \
here. Follow the steps in the prompt for real -- start the program, drive it, \
run the commands -- and observe what actually happens. Do not stop at reading \
code or running the unit tests: the point is to catch what only shows up when \
the thing runs (runtime errors, wiring and setup problems, behaviour that does \
not match the intent).\n\
\n\
This run is read-only: do not edit, stage or commit anything, and never push. \
Setup and runtime failures are real findings: report them. Stop any process you \
started before you finish.\n\
\n\
End your reply with a short report: what you tested, what you observed, and \
anything broken (with the exact error and where it comes from).";

/// The marker a cell's agent writes a live handoff under.
pub const LIVE_ACTION_MARKER: &str = "RALPHUS_LIVE_ACTION:";

/// The explicit "this cannot be tested live" answer.
const NOTHING_TO_TEST_LIVE: &str = "(nothing to test live)";

/// Longest single handoff kept; longer text is cut.
const HANDOFF_MAX_CHARS: usize = 4000;

/// Most handoffs kept from one cell.
const MAX_HANDOFFS_PER_CELL: usize = 8;

/// Appended to every prompt cell's system prompt (`RunnerSpec::from_row`): the
/// question whose answer becomes the review's live actions.
pub const LIVE_HANDOFF_SYSTEM_PROMPT: &str = "## Live Test Handoff\n\
Operational note, not a change to your task: after this work is merged into a \
review, ralphus may test it live with a separate agent that has not seen this \
conversation. When you finish, and before any RALPHUS_GHOST section, say how \
you would test this change live if you had to: run the real program the way a \
user would, exercise what you changed, and check the result. Write each test on \
its own, starting on a new line with the exact marker 'RALPHUS_LIVE_ACTION:' \
followed by concrete steps -- which program or entry point, the exact commands, \
any setup it needs (ports, data, configuration), what to do, and what correct \
looks like. A test may span several lines; it ends at the next line that starts \
with a RALPHUS_ marker. Write one per distinct thing worth testing (for example \
one per product you touched). If this change cannot be meaningfully tested \
live (a pure refactor, documentation only), write \
'RALPHUS_LIVE_ACTION: (nothing to test live)'.";

/// The live handoffs one agent reply carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LiveHandoffs {
    /// The reply has no `RALPHUS_LIVE_ACTION:` marker at all.
    NoMarker,
    /// The agent said there is nothing to test live.
    NothingToTest,
    /// One or more test descriptions, in reply order.
    Tests(Vec<String>),
}

/// Extract every `RALPHUS_LIVE_ACTION:` handoff from an agent's reply. Each
/// marker starts one handoff, which runs until the next line that starts with
/// a `RALPHUS_` marker (another handoff, the ghost, a prophecy) or the end of
/// the reply.
#[must_use]
pub fn parse_live_handoffs(reply: &str) -> LiveHandoffs {
    fn finish(current: &mut Option<String>, tests: &mut Vec<String>) {
        if let Some(text) = current.take() {
            let text = text.trim();
            if !text.is_empty() && !text.eq_ignore_ascii_case(NOTHING_TO_TEST_LIVE) {
                tests.push(text.chars().take(HANDOFF_MAX_CHARS).collect());
            }
        }
    }
    let mut tests: Vec<String> = Vec::new();
    let mut saw_marker = false;
    let mut current: Option<String> = None;
    for line in reply.lines() {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix(LIVE_ACTION_MARKER) {
            finish(&mut current, &mut tests);
            saw_marker = true;
            current = Some(rest.trim().to_string());
        } else if trimmed.starts_with("RALPHUS_") {
            finish(&mut current, &mut tests);
        } else if let Some(text) = current.as_mut() {
            text.push('\n');
            text.push_str(line);
        }
    }
    finish(&mut current, &mut tests);
    tests.truncate(MAX_HANDOFFS_PER_CELL);
    match (saw_marker, tests.is_empty()) {
        (false, _) => LiveHandoffs::NoMarker,
        (true, true) => LiveHandoffs::NothingToTest,
        (true, false) => LiveHandoffs::Tests(tests),
    }
}

/// One cell's stored live handoffs.
#[derive(Debug, Clone, Default, serde::Serialize, PartialEq, Eq)]
pub struct CellLiveHandoffs {
    /// The test descriptions, in the order the agent wrote them.
    pub tests: Vec<String>,
    /// The agent said there is nothing to test live.
    pub nothing_to_test: bool,
    /// When they were recorded; `None` when the cell never posted any.
    pub recorded_at_ms: Option<i64>,
}

impl crate::store::Store {
    /// Replace a cell's live handoffs with the ones from its latest reply.
    /// A reply with no marker at all leaves nothing stored for the cell.
    ///
    /// # Errors
    ///
    /// On a database failure.
    pub fn replace_cell_live_handoffs(
        &self,
        squad_id: &str,
        task_idx: i64,
        cell_idx: i64,
        handoffs: &LiveHandoffs,
    ) -> crate::store::Result<()> {
        let now = crate::store::now_ms();
        self.conn.execute(
            "DELETE FROM cell_live_handoffs WHERE squad_id=? AND task_idx=? AND cell_idx=?",
            rusqlite::params![squad_id, task_idx, cell_idx],
        )?;
        let rows: Vec<(String, bool)> = match handoffs {
            LiveHandoffs::NoMarker => Vec::new(),
            LiveHandoffs::NothingToTest => vec![(String::new(), true)],
            LiveHandoffs::Tests(tests) => tests.iter().map(|t| (t.clone(), false)).collect(),
        };
        for (ordinal, (text, nothing)) in rows.iter().enumerate() {
            self.conn.execute(
                "INSERT INTO cell_live_handoffs
                    (squad_id, task_idx, cell_idx, ordinal, text, nothing_to_test, created_at_ms)
                 VALUES (?, ?, ?, ?, ?, ?, ?)",
                rusqlite::params![
                    squad_id,
                    task_idx,
                    cell_idx,
                    i64::try_from(ordinal).unwrap_or(i64::MAX),
                    text,
                    i64::from(*nothing),
                    now
                ],
            )?;
        }
        Ok(())
    }

    /// A cell's stored live handoffs.
    ///
    /// # Errors
    ///
    /// On a database failure.
    pub fn cell_live_handoffs(
        &self,
        squad_id: &str,
        task_idx: i64,
        cell_idx: i64,
    ) -> crate::store::Result<CellLiveHandoffs> {
        cell_live_handoffs_conn(&self.conn, squad_id, task_idx, cell_idx)
    }
}

/// [`crate::store::Store::cell_live_handoffs`] over a bare connection, for
/// read-pool callers.
///
/// # Errors
///
/// On a database failure.
pub fn cell_live_handoffs_conn(
    conn: &rusqlite::Connection,
    squad_id: &str,
    task_idx: i64,
    cell_idx: i64,
) -> crate::store::Result<CellLiveHandoffs> {
    let mut stmt = conn.prepare(
        "SELECT text, nothing_to_test, created_at_ms FROM cell_live_handoffs
         WHERE squad_id=? AND task_idx=? AND cell_idx=? ORDER BY ordinal",
    )?;
    let rows = stmt.query_map(rusqlite::params![squad_id, task_idx, cell_idx], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, i64>(1)? != 0,
            r.get::<_, i64>(2)?,
        ))
    })?;
    let mut out = CellLiveHandoffs::default();
    for row in rows {
        let (text, nothing, at) = row?;
        out.recorded_at_ms = Some(at);
        if nothing {
            out.nothing_to_test = true;
        } else {
            out.tests.push(text);
        }
    }
    Ok(out)
}

/// The live ref a run's commits are pinned on.
#[must_use]
pub fn live_ref_name(guardian_id: &str, run_id: &str) -> String {
    format!(
        "{LIVE_REF_PREFIX}/{}/{}",
        ref_safe(guardian_id),
        ref_safe(run_id)
    )
}

/// The tmux cell id one live run executes under.
#[must_use]
pub fn live_run_cell_id(action_index: usize, run_id: &str) -> String {
    format!("live-{action_index}-{}", ref_safe(run_id))
}

/// The tmux squad namespace for a review's agent runs.
#[must_use]
pub fn live_run_squad_id(guardian_id: &str) -> String {
    format!("guardian-{guardian_id}")
}

fn ref_safe(value: &str) -> String {
    value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// One live run to execute.
pub struct LiveRunRequest<'a> {
    pub guardian: &'a GuardianView,
    pub run_id: &'a str,
    pub action_index: usize,
    pub label: &'a str,
    pub prompt: &'a str,
    /// The checkout to run in (the action's prepared checkout).
    pub cwd: &'a str,
    /// Whether the run may commit fixes that will be carried back.
    pub allow_commits: bool,
}

/// How a live run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiveRunEnd {
    /// The agent finished (whatever it found).
    Finished,
    /// The agent or its setup failed before producing a result.
    Failed,
    /// The run was cancelled.
    Cancelled,
    /// The review's spend cap was already reached; nothing ran.
    OverBudget,
}

impl LiveRunEnd {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Finished => "finished",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::OverBudget => "over_budget",
        }
    }
}

/// What one live run produced.
#[derive(Debug, Clone)]
pub struct LiveRunOutcome {
    pub end: LiveRunEnd,
    /// The agent's final report.
    pub report: String,
    pub error: Option<String>,
    pub cost_usd: f64,
    /// The checkout's HEAD when the run started (T0).
    pub base_tip: Option<String>,
    /// Commits the run produced on top of `base_tip`, oldest first.
    pub commits: Vec<String>,
    /// The live ref the commits are pinned on, when there are any.
    pub live_ref: Option<String>,
    /// The tmux cell id the agent ran under.
    pub cell_id: String,
}

fn outcome(end: LiveRunEnd, cell_id: &str, error: Option<String>) -> LiveRunOutcome {
    LiveRunOutcome {
        end,
        report: String::new(),
        error,
        cost_usd: 0.0,
        base_tip: None,
        commits: Vec::new(),
        live_ref: None,
        cell_id: cell_id.to_string(),
    }
}

fn note(
    store: &crate::store_lock::StoreHandle,
    guardian_id: &str,
    level: crate::logging::LogLevel,
    message: &'static str,
    payload: serde_json::Value,
) {
    let guard = store.lock();
    let _ = guard.cartographer_log(crate::cartographer::CartographerEntry {
        level,
        source: "guardian",
        message,
        scope: Some("live_action"),
        squad_id: None,
        guardian_id: Some(guardian_id),
        cell_id: payload.get("cell_id").and_then(|v| v.as_str()),
        task: Some(LIVE_ACTION_TASK),
        log_path: None,
        payload: payload.clone(),
        admin_only: false,
    });
}

/// Whether the review has already spent its `maximum_budget_usd` cap.
fn over_budget(store: &crate::store_lock::StoreHandle, guardian_id: &str) -> bool {
    let guard = store.lock();
    let Ok(Some(cap)) = guard.guardian_maximum_budget_usd(guardian_id) else {
        return false;
    };
    guard
        .guardian_cost_total(guardian_id)
        .is_ok_and(|(_, _, spent)| spent >= cap)
}

/// Execute one live run to completion. Blocks for the agent's whole session,
/// so callers run it on a worker thread. Never holds the store lock while the
/// agent runs.
pub fn execute_live_run(
    store: &crate::store_lock::StoreHandle,
    runner: &dyn Runner,
    req: &LiveRunRequest<'_>,
    cancel: &CancelToken,
) -> LiveRunOutcome {
    let gid = req.guardian.id.as_str();
    let cell_id = live_run_cell_id(req.action_index, req.run_id);
    if over_budget(store, gid) {
        note(
            store,
            gid,
            crate::logging::LogLevel::WARNING,
            "live run skipped: review budget reached",
            serde_json::json!({"run_id": req.run_id, "cell_id": cell_id}),
        );
        return outcome(
            LiveRunEnd::OverBudget,
            &cell_id,
            Some("the review's maximum_budget_usd cap is already reached".to_string()),
        );
    }

    let _hold = match crate::checkout_hold::hold(
        req.cwd,
        &format!("live-run:{gid}:{}", req.run_id),
        LIVE_RUN_HOLD_MAX_AGE,
    ) {
        Ok(hold) => Some(hold),
        Err(e) => {
            crate::rlog!(
                WARNING,
                "ralphus [guardian] review {gid} live run {} could not record that it is using {}: {e}",
                req.run_id,
                req.cwd
            );
            None
        }
    };
    let ws = Workspace::for_guardian(store, gid, req.cwd);
    let base_tip = ws
        .git(&["rev-parse", "HEAD"])
        .ok()
        .map(|s| s.trim().to_string());

    let resolved = match crate::guardian_merge::resolve_resolver_agent(
        req.guardian.resolver_agent.as_deref(),
        req.guardian.resolver_model.as_deref(),
        store,
        Path::new(req.cwd),
    ) {
        Ok(resolved) => resolved,
        Err(message) => {
            let mut failed = outcome(
                LiveRunEnd::Failed,
                &cell_id,
                Some(format!("unresolvable agent: {message}")),
            );
            failed.base_tip = base_tip;
            return failed;
        }
    };

    // The review's own environment first (what its actions run under), then
    // the agent profile's, which carries the agent's own credentials.
    let mut env = req.guardian.manual_checks_env.clone();
    env.extend(resolved.env.clone());
    let system_prompt = if req.allow_commits {
        LIVE_ACTION_COMMIT_SYSTEM_PROMPT
    } else {
        LIVE_ACTION_READ_ONLY_SYSTEM_PROMPT
    };
    let mut spec = RunnerSpec {
        squad_id: live_run_squad_id(gid),
        task: LIVE_ACTION_TASK.to_string(),
        cell_id: cell_id.clone(),
        cwd: req.cwd.to_string(),
        prompt: Some(format!("Live action: {}\n\n{}", req.label, req.prompt)),
        command: None,
        shell: None,
        agent: resolved.backend.clone(),
        executable: resolved.executable.clone(),
        model: resolved.model.clone(),
        system_prompt: Some(system_prompt.to_string()),
        system_prompt_position: None,
        timeout_sec: None,
        budget_tokens: None,
        maximum_budget_usd: None,
        maximum_context: None,
        auto_compact_threshold: None,
        maximum_tool_output_tokens: None,
        effort: None,
        temperature: None,
        proof: false,
        trace_context: None,
        resume_agent_session_id: None,
        assigned_agent_session_id: None,
        env_overrides: env,
        machine: ws.machine().map(str::to_string),
        tool_arg_truncate_chars: None,
        thrash_max_compactions: None,
        thrash_min_turn_gap: None,
        allow_personal_settings: false,
        allow_personal_memory: false,
        waypoint_context: false,
        retry_attempt: 0,
        retry_after_unknown_default_seconds: crate::config::resolve(Path::new(req.cwd))
            .retry_after_unknown_default_seconds(),
        maximum_timeout: None,
        pass_score: None,
    };

    note(
        store,
        gid,
        crate::logging::LogLevel::INFO,
        "live run starting",
        serde_json::json!({
            "run_id": req.run_id,
            "cell_id": cell_id,
            "action_index": req.action_index,
            "label": req.label,
            "cwd": req.cwd,
            "base_tip": base_tip,
            "allow_commits": req.allow_commits,
            "agent": resolved.backend,
            "model": resolved.model,
        }),
    );
    crate::rlog!(
        INFO,
        "ralphus [guardian] review {gid} live run {} ({:?}) starting in {}",
        req.run_id,
        req.label,
        req.cwd
    );

    let result = crate::guardian_merge::run_agent_with_stall_recovery(
        store, &mut spec, runner, cancel, None,
    );
    let _ =
        crate::guardian_merge::record_guardian_call_cost(store, gid, None, "live_action", &result);

    let mut done = outcome(
        if cancel.is_cancelled() {
            LiveRunEnd::Cancelled
        } else if result.status == "done" {
            LiveRunEnd::Finished
        } else {
            LiveRunEnd::Failed
        },
        &cell_id,
        result.error.clone(),
    );
    done.report = result.summary.trim().to_string();
    done.cost_usd = result.cost_usd;
    done.base_tip.clone_from(&base_tip);

    if let Some(base) = base_tip.as_deref() {
        if req.allow_commits && done.end != LiveRunEnd::Cancelled {
            commit_leftover_edits(&ws, req.label);
        }
        done.commits = commits_since(&ws, base);
        if !done.commits.is_empty() {
            let live_ref = live_ref_name(gid, req.run_id);
            match ws.git(&["update-ref", &live_ref, "HEAD"]) {
                Ok(_) => done.live_ref = Some(live_ref),
                Err(e) => crate::rlog!(
                    ERROR,
                    "ralphus [guardian] review {gid} live run {} could not pin its commits on {live_ref}: {e}",
                    req.run_id
                ),
            }
        }
    }

    note(
        store,
        gid,
        if done.end == LiveRunEnd::Finished {
            crate::logging::LogLevel::INFO
        } else {
            crate::logging::LogLevel::WARNING
        },
        "live run finished",
        serde_json::json!({
            "run_id": req.run_id,
            "cell_id": cell_id,
            "end": done.end.as_str(),
            "error": done.error,
            "cost_usd": done.cost_usd,
            "base_tip": done.base_tip,
            "commits": done.commits,
            "live_ref": done.live_ref,
            "report": done.report.chars().take(REPORT_LOG_CHARS).collect::<String>(),
        }),
    );
    crate::rlog!(
        INFO,
        "ralphus [guardian] review {gid} live run {} ended {} with {} commit(s)",
        req.run_id,
        done.end.as_str(),
        done.commits.len()
    );
    done
}

/// Commit tracked-file edits the agent left uncommitted. Untracked files are
/// left alone: the prepared checkout holds build output, which must never be
/// committed into a review.
fn commit_leftover_edits(ws: &Workspace, label: &str) {
    if ws.git(&["add", "--update"]).is_err() {
        return;
    }
    // `diff --cached --quiet` exits non-zero exactly when something is staged.
    if ws.git(&["diff", "--cached", "--quiet"]).is_ok() {
        return;
    }
    let _ = ws.git(&[
        "commit",
        "--message",
        &format!("Uncommitted changes left by live action \"{label}\""),
    ]);
}

/// Commits on HEAD that are not on `base`, oldest first.
fn commits_since(ws: &Workspace, base: &str) -> Vec<String> {
    ws.git(&["rev-list", "--reverse", &format!("{base}..HEAD")])
        .map(|out| {
            out.lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// A runner standing in for the agent: runs `act` in the spec's cwd and
    /// records the spec it was given.
    struct ScriptedAgent {
        act: Box<dyn Fn(&Path) + Send + Sync>,
        reply: String,
        seen: parking_lot::Mutex<Vec<RunnerSpec>>,
    }

    impl Runner for ScriptedAgent {
        fn run(&self, spec: &RunnerSpec) -> crate::runner::RunnerResult {
            (self.act)(Path::new(&spec.cwd));
            self.seen.lock().push(spec.clone());
            let mut result = crate::runner::RunnerResult::failure("");
            result.status = "done".to_string();
            result.error = None;
            result.summary.clone_from(&self.reply);
            result.cost_usd = 0.25;
            result
        }
    }

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn repo(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ralphus-live-run-{tag}-{}-{}",
            std::process::id(),
            crate::store::now_ms()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        git(&dir, &["init", "-q", "-b", "main"]);
        std::fs::write(dir.join("app.txt"), "v1\n").unwrap();
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-q", "-m", "init"]);
        dir
    }

    fn setup(
        tag: &str,
    ) -> (
        Arc<crate::store_lock::StoreMutex>,
        GuardianView,
        std::path::PathBuf,
    ) {
        let store = Arc::new(crate::store_lock::StoreMutex::new(
            crate::store::Store::open_in_memory().unwrap(),
        ));
        let dir = repo(tag);
        let id = store
            .lock()
            .create_guardian("live", "main", &dir.to_string_lossy())
            .unwrap();
        let g = store.lock().get_guardian(&id).unwrap();
        (store, g, dir)
    }

    fn request<'a>(g: &'a GuardianView, cwd: &'a str, allow_commits: bool) -> LiveRunRequest<'a> {
        LiveRunRequest {
            guardian: g,
            run_id: "r1",
            action_index: 0,
            label: "Smoke test",
            prompt: "Start the app and check it greets.",
            cwd,
            allow_commits,
        }
    }

    #[test]
    fn a_live_run_runs_the_prompt_in_tmux_shape_and_records_its_report() {
        let (store, g, dir) = setup("report");
        let agent = ScriptedAgent {
            act: Box::new(|_| {}),
            reply: "Tested the greeting: it works.".to_string(),
            seen: parking_lot::Mutex::new(Vec::new()),
        };
        let cwd = dir.to_string_lossy().to_string();
        let out = execute_live_run(
            &store,
            &agent,
            &request(&g, &cwd, true),
            &CancelToken::never(),
        );

        assert_eq!(out.end, LiveRunEnd::Finished);
        assert_eq!(out.report, "Tested the greeting: it works.");
        assert!(out.commits.is_empty());
        assert!(out.live_ref.is_none());
        let seen = agent.seen.lock();
        let spec = &seen[0];
        assert_eq!(spec.squad_id, format!("guardian-{}", g.id));
        assert_eq!(spec.task, LIVE_ACTION_TASK);
        assert_eq!(spec.cell_id, "live-0-r1");
        assert!(spec.prompt.as_deref().unwrap().contains("Start the app"));
        assert_eq!(
            spec.system_prompt.as_deref(),
            Some(LIVE_ACTION_COMMIT_SYSTEM_PROMPT)
        );
        let (_, _, spent) = store.lock().guardian_cost_total(&g.id).unwrap();
        assert!(
            (spent - 0.25).abs() < 1e-9,
            "live run cost is charged to the review"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_live_runs_commits_and_leftover_edits_are_pinned_on_its_live_ref() {
        let (store, g, dir) = setup("commits");
        let agent = ScriptedAgent {
            act: Box::new(|cwd| {
                std::fs::write(cwd.join("app.txt"), "v2\n").unwrap();
                git(cwd, &["commit", "-qam", "Fix greeting found while testing"]);
                // A second, uncommitted edit plus untracked build output.
                std::fs::write(cwd.join("app.txt"), "v3\n").unwrap();
                std::fs::write(cwd.join("build.log"), "noise").unwrap();
            }),
            reply: "Fixed it.".to_string(),
            seen: parking_lot::Mutex::new(Vec::new()),
        };
        let cwd = dir.to_string_lossy().to_string();
        let base = git(&dir, &["rev-parse", "HEAD"]);
        let out = execute_live_run(
            &store,
            &agent,
            &request(&g, &cwd, true),
            &CancelToken::never(),
        );

        assert_eq!(out.base_tip.as_deref(), Some(base.as_str()));
        assert_eq!(
            out.commits.len(),
            2,
            "the agent's commit plus its leftover edit"
        );
        let live_ref = out.live_ref.clone().unwrap();
        assert_eq!(live_ref, live_ref_name(&g.id, "r1"));
        assert_eq!(
            git(&dir, &["rev-parse", &live_ref]),
            *out.commits.last().unwrap()
        );
        let tracked = git(&dir, &["ls-tree", "-r", "--name-only", &live_ref]);
        assert!(
            !tracked.contains("build.log"),
            "untracked output is never committed"
        );
        assert_eq!(git(&dir, &["show", &format!("{live_ref}:app.txt")]), "v3");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_read_only_run_gets_the_read_only_prompt_and_commits_nothing_itself() {
        let (store, g, dir) = setup("read-only");
        let agent = ScriptedAgent {
            act: Box::new(|cwd| std::fs::write(cwd.join("app.txt"), "edited\n").unwrap()),
            reply: "Broken: it crashes on start.".to_string(),
            seen: parking_lot::Mutex::new(Vec::new()),
        };
        let cwd = dir.to_string_lossy().to_string();
        let out = execute_live_run(
            &store,
            &agent,
            &request(&g, &cwd, false),
            &CancelToken::never(),
        );
        assert!(
            out.commits.is_empty(),
            "a read-only run's edits are never committed"
        );
        assert_eq!(
            agent.seen.lock()[0].system_prompt.as_deref(),
            Some(LIVE_ACTION_READ_ONLY_SYSTEM_PROMPT)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_live_run_holds_its_checkout_while_the_agent_runs() {
        let (store, g, dir) = setup("hold");
        let cwd = dir.to_string_lossy().to_string();
        let probe = cwd.clone();
        let held_during = Arc::new(parking_lot::Mutex::new(None));
        let held_flag = Arc::clone(&held_during);
        let agent = ScriptedAgent {
            act: Box::new(move |_| {
                *held_flag.lock() = Some(crate::checkout_hold::is_held(&probe));
            }),
            reply: String::new(),
            seen: parking_lot::Mutex::new(Vec::new()),
        };
        let _ = execute_live_run(
            &store,
            &agent,
            &request(&g, &cwd, true),
            &CancelToken::never(),
        );
        assert_eq!(*held_during.lock(), Some(true));
        assert!(
            !crate::checkout_hold::is_held(&cwd),
            "released when the run ends"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A squad with one task holding two prompt cells.
    pub(crate) fn insert_squad(store: &mut crate::store::Store) -> String {
        let file: ralphus_core::schema::TaskFile = toml::from_str(
            r#"
[[task]]
name = "build"
[[task.cell]]
id = "one"
cwd = "/repo"
prompt = "first"
[[task.cell]]
id = "two"
cwd = "/repo"
prompt = "second"
"#,
        )
        .unwrap();
        store.insert_squad(&file, None, false).unwrap()
    }

    #[test]
    fn parses_no_marker_as_no_marker() {
        assert_eq!(parse_live_handoffs("all done"), LiveHandoffs::NoMarker);
    }

    #[test]
    fn parses_several_multi_line_handoffs_and_stops_at_the_ghost() {
        let reply = "Implemented it.\n\
            RALPHUS_LIVE_ACTION: Start the board with `scripts/dev.sh`.\n\
            Open Reviews and expand a branch; the rate-limit row shows a countdown.\n\
            RALPHUS_LIVE_ACTION: Run `ralphus review show r1` and check the fields print.\n\
            RALPHUS_GHOST: - nothing odd";
        assert_eq!(
            parse_live_handoffs(reply),
            LiveHandoffs::Tests(vec![
                "Start the board with `scripts/dev.sh`.\nOpen Reviews and expand a branch; the rate-limit row shows a countdown."
                    .to_string(),
                "Run `ralphus review show r1` and check the fields print.".to_string(),
            ])
        );
    }

    #[test]
    fn a_prophecy_line_ends_a_handoff() {
        let reply = "RALPHUS_LIVE_ACTION: run it\nRALPHUS_PROPHECY: hazard: x\ntrailing prose";
        assert_eq!(
            parse_live_handoffs(reply),
            LiveHandoffs::Tests(vec!["run it".to_string()])
        );
    }

    #[test]
    fn parses_the_explicit_nothing_to_test_answer() {
        assert_eq!(
            parse_live_handoffs("RALPHUS_LIVE_ACTION: (Nothing to test live)\nRALPHUS_GHOST: -"),
            LiveHandoffs::NothingToTest
        );
        // A real test next to the "nothing" answer wins.
        assert_eq!(
            parse_live_handoffs(
                "RALPHUS_LIVE_ACTION: (nothing to test live)\nRALPHUS_LIVE_ACTION: run it"
            ),
            LiveHandoffs::Tests(vec!["run it".to_string()])
        );
    }

    #[test]
    fn handoffs_round_trip_and_a_new_reply_replaces_the_old_one() {
        let mut store = crate::store::Store::open_in_memory().unwrap();
        let squad = insert_squad(&mut store);
        store
            .replace_cell_live_handoffs(
                &squad,
                0,
                1,
                &LiveHandoffs::Tests(vec!["a".to_string(), "b".to_string()]),
            )
            .unwrap();
        let got = store.cell_live_handoffs(&squad, 0, 1).unwrap();
        assert_eq!(got.tests, vec!["a".to_string(), "b".to_string()]);
        assert!(!got.nothing_to_test);
        assert!(got.recorded_at_ms.is_some());

        store
            .replace_cell_live_handoffs(&squad, 0, 1, &LiveHandoffs::NothingToTest)
            .unwrap();
        let got = store.cell_live_handoffs(&squad, 0, 1).unwrap();
        assert!(got.tests.is_empty());
        assert!(got.nothing_to_test);

        store
            .replace_cell_live_handoffs(&squad, 0, 1, &LiveHandoffs::NoMarker)
            .unwrap();
        assert_eq!(
            store.cell_live_handoffs(&squad, 0, 1).unwrap(),
            CellLiveHandoffs::default()
        );
    }

    #[test]
    fn a_review_over_its_budget_runs_nothing() {
        let (store, g, dir) = setup("budget");
        store
            .lock()
            .set_guardian_maximum_budget_usd(&g.id, Some(0.0))
            .unwrap();
        let agent = ScriptedAgent {
            act: Box::new(|_| panic!("must not run")),
            reply: String::new(),
            seen: parking_lot::Mutex::new(Vec::new()),
        };
        let cwd = dir.to_string_lossy().to_string();
        let out = execute_live_run(
            &store,
            &agent,
            &request(&g, &cwd, true),
            &CancelToken::never(),
        );
        assert_eq!(out.end, LiveRunEnd::OverBudget);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
