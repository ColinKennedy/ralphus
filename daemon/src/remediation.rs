//! RAL-487/RAL-488: runtime for a **remediating command** -- a `command`-kind
//! cell or proof step whose failed attempts are handed to an agent to repair
//! instead of failing outright (the `mode = "remediating"` default; see
//! `ralphus_core::schema::CellDef::mode`/`ProofStep::mode`). `mode = "raw"`
//! is the no-agent, single-attempt escape hatch and never reaches the retry
//! loop below.
//!
//! The critical design point (RAL-487's technical notes): a failed attempt's
//! full stdout+stderr can be arbitrarily large, so the repair agent is never
//! handed that output inline in its prompt. Instead it gets a *file path* --
//! the same attempt-scoped terminal-log file every cell/proof already writes
//! (`daemon/src/terminal_log.rs`) -- plus that file's size in human units, so
//! the agent can decide how much of it to read (tail, grep, a byte range)
//! rather than being forced to ingest the whole thing.

use std::path::{Path, PathBuf};

use crate::cancel::CancelToken;
use crate::runner::{Runner, RunnerResult, RunnerSpec};
use crate::store::Store;
use crate::vcs::Vcs;

/// Fixed, code-authored system prompt for every remediation pass. Mirrors
/// `guardian_merge.rs`'s `CONFLICT_RESOLVER_SYSTEM_PROMPT` precedent: a
/// narrowly-scoped repair agent, not a general-purpose one. RAL-487's
/// technical notes require this prohibition explicitly -- the repair agent's
/// only job is to fix the underlying issue; the orchestrator alone decides
/// pass/fail by re-running the configured command.
const REMEDIATION_SYSTEM_PROMPT: &str = "You are repairing a failed command so a subsequent re-run of that exact \
     command will succeed. You are NOT verifying your own fix -- the orchestrator \
     will re-run the configured command automatically once you finish.\n\
     \n\
     Do NOT run tests, formatters, linters, the command itself, `git commit`, \
     `git push`, or any other verification/side-effecting command. Your only job \
     is to read the failure output, diagnose the underlying issue, and edit the \
     files needed to fix it. Running the command yourself wastes the very \
     verification step the orchestrator exists to own.\n\
     \n\
     The prompt tells you where the failed attempt's full output was captured and \
     how large that file is. Read only as much of it as you need -- the tail is \
     usually the most useful part of a build/test failure, so prefer `tail`, \
     `grep`, or reading a specific byte range over reading the whole file, \
     especially when it is large.";

/// How the caller resolves a `command`-kind cell/proof step's agent identity
/// for a remediation pass -- the *cell's* own resolved `agent`/`model`
/// (never `"raw"`, which is what a plain command execution itself uses to
/// skip any `ModelBackend`).
pub struct RepairAgent<'a> {
    pub agent: &'a str,
    pub executable: Option<&'a str>,
    pub model: Option<&'a str>,
}

/// Run `command_spec` (a `command`-kind [`RunnerSpec`]), and on failure --
/// when `mode` resolves to `"remediating"` -- hand a repair agent a pointer
/// to the failed attempt's captured output and retry, up to
/// `remediation_attempts` total command executions. Returns the final
/// [`RunnerResult`]: the first successful attempt's result, or the last
/// attempt's failure once the budget is exhausted.
///
/// Safe to call unconditionally for any `command`-kind spec regardless of
/// `mode`: `mode = "raw"` (or a missing/zero attempt budget) degenerates to
/// exactly one command execution, identical to calling
/// [`Runner::run_cancellable`] directly -- including never touching `vcs`,
/// so a raw command has zero VCS side effects, same as before this feature
/// existed.
///
/// Each retried command execution reuses `command_spec.cell_id` unchanged
/// (so the board's live-view/peek endpoints keep addressing the same
/// session they already do for a plain command cell/proof) -- which means a
/// retry's terminal-log attempt file overwrites the previous attempt's own.
/// That's fine for this loop's own purposes: the failed output a repair pass
/// needs is always read *before* that pass triggers the next command
/// execution. Each repair pass itself runs under a distinct
/// `"{cell_id}-remediate-{n}"` cell id, so it never collides with the
/// command's own session and stays individually inspectable on the board.
///
/// RAL-488 interview Q1: `vcs` (when `Some`, resolved by the caller via
/// [`crate::vcs::for_project_root`]) snapshots `command_spec.cwd`'s working
/// tree once, before the first command execution, and restores it
/// immediately before every repair pass -- so each repair pass starts from
/// the same pristine baseline instead of compounding on the previous failed
/// command's or failed repair's own edits. `None` (no registered project, or
/// an unresolvable `vcs` kind) degrades gracefully: the retry loop still
/// runs, just without the clean-slate guarantee, logged once at snapshot
/// time by [`resolve_vcs_for_remediation`].
#[must_use]
pub fn run_command_with_remediation(
    store: &Store,
    runner: &dyn Runner,
    cancel: &CancelToken,
    command_spec: &RunnerSpec,
    mode: &str,
    remediation_attempts: Option<u32>,
    repair_agent: &RepairAgent<'_>,
    vcs: Option<&dyn Vcs>,
) -> RunnerResult {
    let total_attempts = resolve_total_attempts(mode, remediation_attempts);

    let snapshot = if total_attempts > 1 {
        SnapshotGuard::new(vcs.and_then(|v| snapshot_worktree_before_retries(v, command_spec)))
    } else {
        SnapshotGuard::none()
    };

    let mut result = runner.run_cancellable(command_spec, cancel);
    let mut attempt = 1u32;
    let mut last_repair_result: Option<RunnerResult> = None;
    while !result.is_done() && attempt < total_attempts && !cancel.is_cancelled() {
        crate::rlog!(
            WARNING,
            "ralphus [remediation] squad={} task={} cell={} command attempt {attempt}/{total_attempts} \
             failed, starting repair pass",
            command_spec.squad_id,
            command_spec.task,
            command_spec.cell_id,
        );
        let message =
            format!("command attempt {attempt}/{total_attempts} failed, starting repair pass");
        crate::cartographer::Note::new("remediation")
            .level(crate::logging::LogLevel::WARNING)
            .squad(&command_spec.squad_id)
            .cell(&command_spec.cell_id)
            .task(&command_spec.task)
            .emit(
                store,
                &message,
                serde_json::json!({
                    "attempt": attempt,
                    "total_attempts": total_attempts,
                }),
            );
        if let (Some(v), Some(dir)) = (vcs, snapshot.path()) {
            restore_worktree_before_repair(v, command_spec, dir, attempt);
        }
        last_repair_result = Some(run_repair_pass(
            store,
            runner,
            cancel,
            command_spec,
            attempt,
            repair_agent,
        ));
        if cancel.is_cancelled() {
            break;
        }
        result = runner.run_cancellable(command_spec, cancel);
        attempt += 1;
    }
    // RAL-488 interview Q3: once the budget is exhausted, neither the last
    // command's own output nor the repair agent's own last words are enough
    // on their own to debug from -- report both, clearly labeled, rather
    // than picking one.
    if !result.is_done() {
        if let Some(repair) = &last_repair_result {
            result.error = Some(combine_exhaustion_diagnostics(&result, repair));
        }
    }
    result
}

/// Directory a remediation retry loop's worktree snapshot for `command_spec`
/// lives under -- outside `command_spec.cwd` itself (required by
/// [`Vcs::snapshot_worktree`]) and unique per squad/task/cell so concurrent
/// cells never collide. Mirrors `terminal_log.rs`'s own
/// `state_dir().join("terminal_logs")` convention for daemon-owned storage
/// that lives outside any project checkout.
fn snapshot_dir_for(command_spec: &RunnerSpec) -> PathBuf {
    let session_name = crate::tmux::session_name(
        &command_spec.squad_id,
        &command_spec.task,
        &command_spec.cell_id,
    );
    crate::state_dir()
        .join("remediation_snapshots")
        .join(session_name)
}

/// Resolve `cwd`'s project [`Vcs`] adapter for a remediation retry loop.
/// `None` (an unregistered project, or a registered `vcs` kind with no
/// adapter) just means the retry loop proceeds without the clean-slate
/// snapshot/restore guarantee -- logged here, not treated as fatal, the same
/// tolerance every other step of this loop gives VCS bookkeeping.
pub(crate) fn resolve_vcs_for_remediation(
    store: &crate::store_lock::StoreHandle,
    cwd: &str,
) -> Option<Box<dyn Vcs>> {
    let guard = store.lock();
    match crate::vcs::for_project_root(&guard, Path::new(cwd)) {
        Ok(vcs) => Some(vcs),
        Err(e) => {
            crate::rlog!(
                WARNING,
                "ralphus [remediation] could not resolve a vcs adapter for {cwd}, remediation \
                 retries will run without a clean-slate worktree snapshot: {e}"
            );
            let _ = guard.cartographer_log(crate::cartographer::CartographerEntry {
                level: crate::logging::LogLevel::WARNING,
                source: "remediation",
                message: "could not resolve vcs adapter for remediation retries",
                scope: Some("remediation"),
                squad_id: None,
                guardian_id: None,
                cell_id: None,
                task: None,
                log_path: None,
                payload: serde_json::json!({"cwd": cwd, "error": e.to_string()}),
                admin_only: false,
            });
            None
        }
    }
}

/// Snapshot `command_spec.cwd`'s working tree before the retry loop's first
/// command attempt, so [`restore_worktree_before_repair`] can undo whatever
/// a failed attempt (command or repair pass) left behind before the next
/// repair pass starts. Best-effort: a failed snapshot is logged and the
/// loop proceeds without the clean-slate guarantee rather than failing the
/// whole remediation attempt over VCS bookkeeping.
pub(crate) fn snapshot_worktree_before_retries(
    vcs: &dyn Vcs,
    command_spec: &RunnerSpec,
) -> Option<PathBuf> {
    let dir = snapshot_dir_for(command_spec);
    match vcs.snapshot_worktree(Path::new(&command_spec.cwd), &dir) {
        Ok(()) => Some(dir),
        Err(e) => {
            // ralphus[ignore-rlog-pair]: VCS snapshot failure is a best-effort integrity concern logged for diagnostics; the remediation loop degrades gracefully and proceeds without snapshot/restore
            crate::rlog!(
                WARNING,
                "ralphus [remediation] squad={} task={} cell={} could not snapshot the worktree \
                 before remediation retries, repair passes will run without a clean-slate \
                 guarantee: {e}",
                command_spec.squad_id,
                command_spec.task,
                command_spec.cell_id,
            );
            None
        }
    }
}

/// Restore a prior [`snapshot_worktree_before_retries`] snapshot immediately
/// before repair pass `attempt` runs, so it starts from the exact
/// pre-remediation state -- not the just-failed command attempt's own
/// output/artifacts, and not (from the second repair pass on) the previous
/// repair agent's own unsuccessful edits. Best-effort, matching
/// [`run_repair_pass`]'s own tolerance for a failed pass: the orchestrator's
/// actual pass/fail signal is always the next command execution, never this
/// bookkeeping step.
pub(crate) fn restore_worktree_before_repair(
    vcs: &dyn Vcs,
    command_spec: &RunnerSpec,
    snapshot_dir: &Path,
    attempt: u32,
) {
    if let Err(e) = vcs.restore_worktree(Path::new(&command_spec.cwd), snapshot_dir) {
        // ralphus[ignore-rlog-pair]: VCS restore failure within retry loop is best-effort recovery; the loop continues to the next repair pass attempt without snapshot/restore
        crate::rlog!(
            WARNING,
            "ralphus [remediation] squad={} task={} cell={} could not restore the worktree \
             snapshot before repair pass {attempt}: {e}",
            command_spec.squad_id,
            command_spec.task,
            command_spec.cell_id,
        );
    }
}

/// Remove a snapshot directory created by [`snapshot_worktree_before_retries`]
/// once a retry loop is done with it (either outcome). Best-effort: a
/// leftover snapshot directory is disk-space clutter, never a correctness
/// problem, so its removal failing is not worth surfacing.
pub(crate) fn cleanup_worktree_snapshot(snapshot_dir: &Path) {
    let _ = std::fs::remove_dir_all(snapshot_dir);
}

/// RAII guard around an optional [`snapshot_worktree_before_retries`]
/// directory -- removes it via [`cleanup_worktree_snapshot`] on drop. Exists
/// so a retry loop with more than one early-return path (`scheduler.rs`'s
/// cell-dispatch loop returns early on a lost rate-limit race, e.g.) still
/// cleans up its snapshot instead of leaking a directory under
/// `state_dir()` on every such exit.
pub(crate) struct SnapshotGuard(Option<PathBuf>);

impl SnapshotGuard {
    pub(crate) fn new(dir: Option<PathBuf>) -> Self {
        Self(dir)
    }

    pub(crate) fn none() -> Self {
        Self(None)
    }

    pub(crate) fn path(&self) -> Option<&Path> {
        self.0.as_deref()
    }
}

impl Drop for SnapshotGuard {
    fn drop(&mut self) {
        if let Some(dir) = &self.0 {
            cleanup_worktree_snapshot(dir);
        }
    }
}

/// `result`'s own summary+error, formatted the same way every existing
/// caller already builds a human-readable diagnostic from a [`RunnerResult`]
/// (see `scheduler.rs`'s proof-dispatch `"command"` branch) -- unified here
/// so [`combine_exhaustion_diagnostics`] and that call site read identically.
pub(crate) fn diagnostic_text(result: &RunnerResult) -> String {
    let text = match &result.error {
        Some(err) if result.summary.is_empty() => err.clone(),
        Some(err) => format!("{}\n{err}", result.summary),
        None => result.summary.clone(),
    };
    if text.trim().is_empty() {
        "(no output captured)".to_string()
    } else {
        text
    }
}

/// RAL-488 interview Q3: the exhaustion failure message, carrying both the
/// last command attempt's own raw output and the last repair agent's own
/// diagnosis, clearly labeled -- the command output says what broke, the
/// repair agent's own words say what it *thought* it fixed and often hint at
/// why the fix didn't hold.
pub(crate) fn combine_exhaustion_diagnostics(
    command_result: &RunnerResult,
    last_repair: &RunnerResult,
) -> String {
    format!(
        "Remediation attempts exhausted.\n\n\
         --- Last command output ---\n{}\n\n\
         --- Last repair agent diagnosis ---\n{}",
        diagnostic_text(command_result),
        diagnostic_text(last_repair),
    )
}

/// Total command executions a `command`-kind spec gets: `remediation_attempts`
/// (at least 1) when `mode` resolves to `"remediating"`, or exactly 1
/// (today's unchanged single-attempt behavior) for `"raw"` or any other
/// value. Shared by [`run_command_with_remediation`] and by
/// `scheduler.rs`'s cell-dispatch path, which drives its own loop around
/// [`run_repair_pass`] instead of this module's top-level function (a cell's
/// own dispatch already wraps a single command execution in
/// `run_cell_with_rate_limit_retries`, whose `SemaphorePermit` plumbing this
/// module has no reason to know about).
#[must_use]
pub(crate) fn resolve_total_attempts(mode: &str, remediation_attempts: Option<u32>) -> u32 {
    if mode == ralphus_core::schema::COMMAND_MODE_REMEDIATING {
        remediation_attempts.unwrap_or(1).max(1)
    } else {
        1
    }
}

/// One repair pass: locate the failed attempt's captured output, build the
/// fixed system prompt + dynamic file-pointer prompt, and run it as its own
/// cell under a distinct id. Best-effort -- a repair pass that itself fails
/// to run (backend error, timeout, ...) is logged and the caller simply
/// retries the command anyway, since the only thing that actually decides
/// pass/fail is the next command execution. Returns the pass's own
/// [`RunnerResult`] so a caller can report it (RAL-488 interview Q3) once the
/// overall retry loop exhausts its budget.
pub(crate) fn run_repair_pass(
    store: &Store,
    runner: &dyn Runner,
    cancel: &CancelToken,
    command_spec: &RunnerSpec,
    attempt: u32,
    repair_agent: &RepairAgent<'_>,
) -> RunnerResult {
    let session_name = crate::tmux::session_name(
        &command_spec.squad_id,
        &command_spec.task,
        &command_spec.cell_id,
    );
    let output_note = match crate::terminal_log::latest_attempt(&session_name) {
        Some(n) => {
            let path = crate::terminal_log::attempt_path(&session_name, n);
            match std::fs::metadata(&path) {
                Ok(meta) => format!(
                    "The failed command's full output (stdout+stderr) was captured to:\n{}\n\
                     ({}). Read only as much of it as you need to diagnose the failure.",
                    path.display(),
                    format_size(meta.len()),
                ),
                Err(e) => format!(
                    "The failed command's output should have been captured to {}, but it \
                     could not be read ({e}). Proceed from whatever context you already have.",
                    path.display(),
                ),
            }
        }
        None => "The failed command's output was not captured (no terminal log found for this \
                  session). Proceed from whatever context you already have."
            .to_string(),
    };
    let command = command_spec
        .command
        .as_deref()
        .unwrap_or("<unknown command>");
    let prompt = format!(
        "A command you must fix just failed (nonzero exit) in `{}`:\n\n    {command}\n\n{output_note}\n\n\
         Diagnose why it failed and fix the underlying issue in the code.",
        command_spec.cwd,
    );

    let mut spec = command_spec.clone();
    spec.cell_id = format!("{}-remediate-{attempt}", command_spec.cell_id);
    spec.prompt = Some(prompt);
    spec.command = None;
    spec.agent = repair_agent.agent.to_string();
    spec.executable = repair_agent.executable.map(str::to_string);
    spec.model = repair_agent.model.map(str::to_string);
    spec.system_prompt = Some(REMEDIATION_SYSTEM_PROMPT.to_string());
    spec.system_prompt_position = Some("append".to_string());
    spec.proof = false;
    spec.resume_agent_session_id = None;
    spec.assigned_agent_session_id = None;

    let result = runner.run_cancellable(&spec, cancel);
    if !result.is_done() {
        crate::rlog!(
            WARNING,
            "ralphus [remediation] squad={} task={} cell={} repair pass {attempt} did not \
             complete cleanly: {}",
            command_spec.squad_id,
            command_spec.task,
            command_spec.cell_id,
            result.error.as_deref().unwrap_or("no detail"),
        );
        let error_detail = result.error.as_deref().unwrap_or("no detail");
        let message = format!("repair pass {attempt} did not complete cleanly");
        crate::cartographer::Note::new("remediation")
            .level(crate::logging::LogLevel::WARNING)
            .squad(&command_spec.squad_id)
            .cell(&command_spec.cell_id)
            .task(&command_spec.task)
            .emit(
                store,
                &message,
                serde_json::json!({
                    "attempt": attempt,
                    "error": error_detail,
                }),
            );
    }
    result
}

/// Human-readable size, matching RAL-487's requirement that the repair agent
/// be told the captured output's size in KB/MB up front (not just handed a
/// bare path) so it can judge up front whether a full read is reasonable.
fn format_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    let b = bytes as f64;
    if b >= MB {
        format!("{:.1} MB", b / MB)
    } else if b >= KB {
        format!("{:.1} KB", b / KB)
    } else {
        format!("{bytes} bytes")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn format_size_picks_the_right_unit() {
        assert_eq!(format_size(500), "500 bytes");
        assert_eq!(format_size(2048), "2.0 KB");
        assert_eq!(format_size(5 * 1024 * 1024), "5.0 MB");
    }

    fn command_spec(squad_id: &str, cell_id: &str, command: &str) -> RunnerSpec {
        RunnerSpec {
            squad_id: squad_id.to_string(),
            task: "t".to_string(),
            cell_id: cell_id.to_string(),
            cwd: ".".to_string(),
            prompt: None,
            command: Some(command.to_string()),
            agent: "raw".to_string(),
            executable: None,
            model: None,
            system_prompt: None,
            system_prompt_position: None,
            timeout_sec: None,
            budget_tokens: None,
            maximum_budget_usd: None,
            maximum_context: None,
            auto_compact_threshold: None,
            maximum_tool_output_tokens: None,
            proof: false,
            trace_context: None,
            resume_agent_session_id: None,
            assigned_agent_session_id: None,
            env_overrides: std::collections::BTreeMap::new(),
            machine: None,
            tool_arg_truncate_chars: None,
            thrash_max_compactions: None,
            thrash_min_turn_gap: None,
            allow_personal_settings: false,
            allow_personal_memory: false,
            maximum_timeout: None,
        }
    }

    fn done() -> RunnerResult {
        RunnerResult {
            status: "done".to_string(),
            ..RunnerResult::failure("unused")
        }
    }

    fn failed() -> RunnerResult {
        RunnerResult::failure("boom")
    }

    /// Returns scripted results in order, one per `run_cancellable` call
    /// regardless of which spec is passed, and records every spec seen so
    /// assertions can inspect what the remediation loop actually built.
    struct ScriptedRunner {
        results: Mutex<std::collections::VecDeque<RunnerResult>>,
        seen: Mutex<Vec<RunnerSpec>>,
    }

    impl ScriptedRunner {
        fn new(results: Vec<RunnerResult>) -> Self {
            Self {
                results: Mutex::new(results.into()),
                seen: Mutex::new(Vec::new()),
            }
        }
    }

    impl Runner for ScriptedRunner {
        fn run(&self, spec: &RunnerSpec) -> RunnerResult {
            self.seen.lock().unwrap().push(spec.clone());
            self.results
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| RunnerResult::failure("no more scripted results"))
        }
    }

    fn repair_agent() -> RepairAgent<'static> {
        RepairAgent {
            agent: "claude-code",
            executable: None,
            model: Some("claude-sonnet-5"),
        }
    }

    #[test]
    fn succeeds_on_first_attempt_no_repair_invoked() {
        let store = crate::store::Store::open_in_memory().unwrap();
        let runner = ScriptedRunner::new(vec![done()]);
        let spec = command_spec("squad-1", "cell-1", "exit 0");
        let result = run_command_with_remediation(
            &store,
            &runner,
            &CancelToken::never(),
            &spec,
            ralphus_core::schema::COMMAND_MODE_REMEDIATING,
            Some(3),
            &repair_agent(),
            None,
        );
        assert!(result.is_done());
        assert_eq!(
            runner.seen.lock().unwrap().len(),
            1,
            "no repair pass expected"
        );
    }

    #[test]
    fn raw_mode_never_remediates_even_after_failure() {
        let store = crate::store::Store::open_in_memory().unwrap();
        let runner = ScriptedRunner::new(vec![failed(), failed(), failed()]);
        let spec = command_spec("squad-1", "cell-1", "exit 1");
        let result = run_command_with_remediation(
            &store,
            &runner,
            &CancelToken::never(),
            &spec,
            ralphus_core::schema::COMMAND_MODE_RAW,
            None,
            &repair_agent(),
            None,
        );
        assert!(!result.is_done());
        assert_eq!(
            runner.seen.lock().unwrap().len(),
            1,
            "raw mode must run exactly once, never remediate"
        );
    }

    #[test]
    fn remediates_and_retries_until_success() {
        // attempt 1 (command) fails -> repair pass runs -> attempt 2 (command) succeeds.
        let store = crate::store::Store::open_in_memory().unwrap();
        let runner = ScriptedRunner::new(vec![failed(), done(), done()]);
        let spec = command_spec("squad-1", "cell-1", "cargo build");
        let result = run_command_with_remediation(
            &store,
            &runner,
            &CancelToken::never(),
            &spec,
            ralphus_core::schema::COMMAND_MODE_REMEDIATING,
            Some(3),
            &repair_agent(),
            None,
        );
        assert!(result.is_done());
        let seen = runner.seen.lock().unwrap();
        // failed command attempt, repair pass, successful retry.
        assert_eq!(seen.len(), 3);
        // The command attempts reuse the original cell id (so the board's
        // live-view keeps addressing the same session); the repair pass runs
        // under a distinct one.
        assert_eq!(seen[0].cell_id, "cell-1");
        assert_eq!(seen[0].command.as_deref(), Some("cargo build"));
        assert_eq!(seen[1].cell_id, "cell-1-remediate-1");
        assert!(
            seen[1].command.is_none(),
            "repair pass is a prompt, not a command"
        );
        assert_eq!(seen[1].agent, "claude-code");
        assert_eq!(seen[1].model.as_deref(), Some("claude-sonnet-5"));
        assert_eq!(
            seen[1].system_prompt.as_deref(),
            Some(REMEDIATION_SYSTEM_PROMPT)
        );
        assert_eq!(seen[2].cell_id, "cell-1");
        assert_eq!(seen[2].command.as_deref(), Some("cargo build"));
    }

    #[test]
    fn exhausts_attempts_and_fails() {
        let store = crate::store::Store::open_in_memory().unwrap();
        let runner = ScriptedRunner::new(vec![failed(), failed(), failed()]);
        let spec = command_spec("squad-1", "cell-1", "cargo build");
        let result = run_command_with_remediation(
            &store,
            &runner,
            &CancelToken::never(),
            &spec,
            ralphus_core::schema::COMMAND_MODE_REMEDIATING,
            Some(2),
            &repair_agent(),
            None,
        );
        assert!(!result.is_done());
        let seen = runner.seen.lock().unwrap();
        // 2 command attempts (the budget) + exactly 1 repair pass between them
        // -- no repair pass after the final failed attempt, since nothing
        // would retry it.
        assert_eq!(seen.len(), 3);
        assert_eq!(seen[0].cell_id, "cell-1");
        assert_eq!(seen[1].cell_id, "cell-1-remediate-1");
        assert_eq!(seen[2].cell_id, "cell-1");
        // RAL-488 interview Q3: on exhaustion, the failure surfaces both the
        // last command output and the last repair agent's own diagnosis.
        let error = result.error.as_deref().unwrap_or_default();
        assert!(error.contains("Remediation attempts exhausted"));
        assert!(error.contains("Last command output"));
        assert!(error.contains("Last repair agent diagnosis"));
    }

    #[test]
    fn repair_prompt_points_at_the_captured_file_instead_of_inlining_it() {
        let store = crate::store::Store::open_in_memory().unwrap();
        let _guard = TestRootGuard::new("remediation-repair-prompt");
        let spec = command_spec("squad-2", "cell-2", "cargo test");
        let session_name = crate::tmux::session_name(&spec.squad_id, &spec.task, &spec.cell_id);
        let huge_output = "e2e-test-marker-line\n".repeat(10_000);
        // `latest_attempt` (which the remediation loop uses to find the most
        // recent attempt number) scans `.raw` files -- write one directly,
        // plus the finalized `.log` sibling a real completed run would also
        // have via `write_attempt`/`write_attempt_from_raw_transcript`.
        let raw_path = crate::terminal_log::raw_transcript_path(&session_name, 0);
        std::fs::create_dir_all(raw_path.parent().unwrap()).unwrap();
        std::fs::write(&raw_path, huge_output.as_bytes()).unwrap();
        crate::terminal_log::write_attempt(&session_name, 0, &huge_output, 100_000);

        let runner = ScriptedRunner::new(vec![failed(), done(), done()]);
        let result = run_command_with_remediation(
            &store,
            &runner,
            &CancelToken::never(),
            &spec,
            ralphus_core::schema::COMMAND_MODE_REMEDIATING,
            Some(3),
            &repair_agent(),
            None,
        );
        assert!(result.is_done());
        let seen = runner.seen.lock().unwrap();
        let repair_prompt = seen[1].prompt.as_deref().unwrap_or_default();
        let expected_path = crate::terminal_log::attempt_path(&session_name, 0);
        assert!(
            repair_prompt.contains(&expected_path.to_string_lossy().into_owned()),
            "repair prompt should point at the attempt file: {repair_prompt}"
        );
        assert!(
            repair_prompt.contains("KB") || repair_prompt.contains("bytes"),
            "repair prompt should report the file's size: {repair_prompt}"
        );
        assert!(
            !repair_prompt.contains("e2e-test-marker-line"),
            "repair prompt must not inline the captured output: {repair_prompt}"
        );
    }

    /// Writes to `tracked.txt` in an isolated temp git repo to model a
    /// command that only passes once the file reads `"fixed\n"`, and a
    /// repair pass that (on its second invocation) writes that fix --
    /// letting [`vcs_restore_undoes_a_failed_repairs_edits_before_the_next_pass`]
    /// prove that [`restore_worktree_before_repair`] actually undoes the
    /// *first* repair pass's botched edit before the second one runs, not
    /// just that the API is wired up.
    struct RepairEditingRunner {
        tracked_file: std::path::PathBuf,
        observed_before_edit: Mutex<Vec<String>>,
    }

    impl Runner for RepairEditingRunner {
        fn run(&self, spec: &RunnerSpec) -> RunnerResult {
            if spec.command.is_some() {
                let contents = std::fs::read_to_string(&self.tracked_file).unwrap_or_default();
                if contents == "fixed\n" {
                    RunnerResult {
                        status: "done".to_string(),
                        ..RunnerResult::failure("unused")
                    }
                } else {
                    RunnerResult::failure("tracked.txt is not fixed yet")
                }
            } else {
                let before = std::fs::read_to_string(&self.tracked_file).unwrap_or_default();
                self.observed_before_edit.lock().unwrap().push(before);
                let new_contents = if spec.cell_id.ends_with("-remediate-2") {
                    "fixed\n"
                } else {
                    "bad edit\n"
                };
                std::fs::write(&self.tracked_file, new_contents).unwrap();
                RunnerResult {
                    status: "done".to_string(),
                    ..RunnerResult::failure("unused")
                }
            }
        }
    }

    #[test]
    fn vcs_restore_undoes_a_failed_repairs_edits_before_the_next_pass() {
        let dir = std::env::temp_dir().join("ral488-remediation-vcs-restore");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(
            std::process::Command::new("git")
                .args(["init", "--quiet", "--initial-branch", "main"])
                .current_dir(&dir)
                .status()
                .expect("git init")
                .success()
        );
        let tracked_file = dir.join("tracked.txt");
        std::fs::write(&tracked_file, "original\n").unwrap();
        for args in [
            ["add", "."].as_slice(),
            [
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "--message",
                "init",
            ]
            .as_slice(),
        ] {
            assert!(
                std::process::Command::new("git")
                    .args(args)
                    .current_dir(&dir)
                    .status()
                    .expect("git setup")
                    .success()
            );
        }

        let runner = RepairEditingRunner {
            tracked_file: tracked_file.clone(),
            observed_before_edit: Mutex::new(Vec::new()),
        };
        let mut spec = command_spec("squad-vcs", "cell-vcs", "check-fixed");
        spec.cwd = dir.to_string_lossy().into_owned();

        let result = run_command_with_remediation(
            &runner,
            &CancelToken::never(),
            &spec,
            ralphus_core::schema::COMMAND_MODE_REMEDIATING,
            Some(3),
            &repair_agent(),
            Some(&crate::vcs::GitVcs as &dyn Vcs),
        );

        assert!(
            result.is_done(),
            "the second repair pass's fix should make the third command attempt pass"
        );
        let observed = runner.observed_before_edit.lock().unwrap();
        assert_eq!(observed.len(), 2, "two repair passes should have run");
        assert_eq!(
            observed[0], "original\n",
            "first repair pass starts from the pristine snapshot"
        );
        assert_eq!(
            observed[1], "original\n",
            "restore before the second repair pass must have undone the first \
             repair pass's own botched edit, not left it in place"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Redirects `terminal_log`'s storage root to an isolated temp directory
    /// for the lifetime of the guard, mirroring `terminal_log.rs`'s own
    /// `TempRoot` test helper -- `set_test_root` is `pub(crate)`, so this
    /// module can drive it directly instead of duplicating terminal_log's
    /// private `_in` helpers.
    struct TestRootGuard(std::path::PathBuf);

    impl TestRootGuard {
        fn new(unique: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("ralphus-test-remediation-{unique}"));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            crate::terminal_log::set_test_root(dir.clone());
            Self(dir)
        }
    }

    impl Drop for TestRootGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}
