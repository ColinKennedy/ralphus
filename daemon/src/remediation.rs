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

use crate::cancel::CancelToken;
use crate::runner::{Runner, RunnerResult, RunnerSpec};
use crate::store_lock::StoreHandle;

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
/// [`Runner::run_cancellable`] directly.
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
/// Takes the [`StoreHandle`] rather than a borrowed `&Store`, and locks it
/// only around the individual store writes below. A caller cannot hold the
/// store lock across this function: it drives `Runner::run_cancellable`, whose
/// tmux path takes the same lock itself (`SubprocessRunner::emit_tmux_note`),
/// and the store mutex is not reentrant -- holding it here deadlocked the
/// whole daemon for as long as the command ran, which is forever once the
/// runner is the thing waiting on the lock.
#[must_use]
pub fn run_command_with_remediation(
    store: &StoreHandle,
    runner: &dyn Runner,
    cancel: &CancelToken,
    command_spec: &RunnerSpec,
    mode: &str,
    remediation_attempts: Option<u32>,
    repair_agent: &RepairAgent<'_>,
) -> RunnerResult {
    let total_attempts = resolve_total_attempts(mode, remediation_attempts);

    let mut result = runner.run_cancellable(command_spec, cancel);
    let mut attempt = 1u32;
    while !result.is_done() && attempt < total_attempts && !cancel.is_cancelled() {
        let message =
            format!("command attempt {attempt}/{total_attempts} failed, starting repair pass");
        {
            let guard = store.lock();
            crate::cartographer::Note::new("remediation")
                .level(crate::logging::LogLevel::WARNING)
                .squad(&command_spec.squad_id)
                .cell(&command_spec.cell_id)
                .task(&command_spec.task)
                .emit(
                    &guard,
                    &message,
                    serde_json::json!({
                        "attempt": attempt,
                        "total_attempts": total_attempts,
                    }),
                );
        }
        run_repair_pass(store, runner, cancel, command_spec, attempt, repair_agent);
        if cancel.is_cancelled() {
            break;
        }
        result = runner.run_cancellable(command_spec, cancel);
        attempt += 1;
    }
    result
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
/// pass/fail is the next command execution.
///
/// Takes the [`StoreHandle`] for the same reason
/// [`run_command_with_remediation`] does: it runs the repair agent through
/// `Runner::run_cancellable`, so the store lock must not be held across it.
pub(crate) fn run_repair_pass(
    store: &StoreHandle,
    runner: &dyn Runner,
    cancel: &CancelToken,
    command_spec: &RunnerSpec,
    attempt: u32,
    repair_agent: &RepairAgent<'_>,
) {
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
        let error_detail = result.error.as_deref().unwrap_or("no detail");
        let message = format!("repair pass {attempt} did not complete cleanly");
        let guard = store.lock();
        crate::cartographer::Note::new("remediation")
            .level(crate::logging::LogLevel::WARNING)
            .squad(&command_spec.squad_id)
            .cell(&command_spec.cell_id)
            .task(&command_spec.task)
            .emit(
                &guard,
                &message,
                serde_json::json!({
                    "attempt": attempt,
                    "error": error_detail,
                }),
            );
    }
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
            retry_attempt: 0,
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

    /// Takes the store lock while it "runs", the way the real tmux runner
    /// does (`SubprocessRunner::emit_tmux_note`). Acquires it on a helper
    /// thread with a timeout rather than inline, so a regression reports a
    /// failed assertion instead of hanging the whole test suite.
    struct StoreLockingRunner {
        store: crate::store_lock::StoreHandle,
        could_lock: Mutex<Vec<bool>>,
    }

    impl Runner for StoreLockingRunner {
        fn run(&self, _spec: &RunnerSpec) -> RunnerResult {
            let store = std::sync::Arc::clone(&self.store);
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let _guard = store.lock();
                let _ = tx.send(());
            });
            let acquired = rx.recv_timeout(std::time::Duration::from_secs(5)).is_ok();
            self.could_lock.lock().unwrap().push(acquired);
            RunnerResult::failure("scripted failure")
        }
    }

    /// The store lock must not be held across `Runner::run_cancellable`.
    ///
    /// The real tmux runner takes the store lock itself while running a
    /// command, and the store mutex is not reentrant -- so a caller holding
    /// it here deadlocks that thread permanently. Because every subsystem
    /// shares the one store lock, that froze the entire daemon: HTTP
    /// handlers, scheduler, guardian workers and all.
    #[test]
    fn the_store_lock_is_never_held_while_the_runner_runs() {
        let store: crate::store_lock::StoreHandle = std::sync::Arc::new(
            crate::store_lock::StoreMutex::new(crate::store::Store::open_in_memory().unwrap()),
        );
        let runner = std::sync::Arc::new(StoreLockingRunner {
            store: std::sync::Arc::clone(&store),
            could_lock: Mutex::new(Vec::new()),
        });

        // Drive the call from a helper thread. A regression here does not
        // merely fail an assertion -- the store lock is not reentrant, so
        // holding it across the runner self-deadlocks the calling thread and
        // would otherwise hang the whole suite instead of reporting.
        let (tx, rx) = std::sync::mpsc::channel();
        let call_store = std::sync::Arc::clone(&store);
        let call_runner = std::sync::Arc::clone(&runner);
        std::thread::spawn(move || {
            let spec = command_spec("squad-1", "cell-1", "exit 1");
            let _ = run_command_with_remediation(
                &call_store,
                call_runner.as_ref(),
                &CancelToken::never(),
                &spec,
                ralphus_core::schema::COMMAND_MODE_REMEDIATING,
                Some(2),
                &repair_agent(),
            );
            let _ = tx.send(());
        });
        assert!(
            rx.recv_timeout(std::time::Duration::from_secs(60)).is_ok(),
            "run_command_with_remediation never returned -- the store lock is held \
             across a call that takes it again, which self-deadlocks the daemon"
        );

        let attempts = runner.could_lock.lock().unwrap().clone();
        assert!(
            !attempts.is_empty(),
            "the runner must have been invoked at least once"
        );
        assert!(
            attempts.iter().all(|acquired| *acquired),
            "the runner could not take the store lock while running ({attempts:?}) -- \
             something up the call stack is holding it across run_cancellable, which \
             deadlocks the daemon"
        );
    }

    #[test]
    fn succeeds_on_first_attempt_no_repair_invoked() {
        let store: crate::store_lock::StoreHandle = std::sync::Arc::new(
            crate::store_lock::StoreMutex::new(crate::store::Store::open_in_memory().unwrap()),
        );
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
        let store: crate::store_lock::StoreHandle = std::sync::Arc::new(
            crate::store_lock::StoreMutex::new(crate::store::Store::open_in_memory().unwrap()),
        );
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
        let store: crate::store_lock::StoreHandle = std::sync::Arc::new(
            crate::store_lock::StoreMutex::new(crate::store::Store::open_in_memory().unwrap()),
        );
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
        let store: crate::store_lock::StoreHandle = std::sync::Arc::new(
            crate::store_lock::StoreMutex::new(crate::store::Store::open_in_memory().unwrap()),
        );
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
    }

    #[test]
    fn repair_prompt_points_at_the_captured_file_instead_of_inlining_it() {
        let store: crate::store_lock::StoreHandle = std::sync::Arc::new(
            crate::store_lock::StoreMutex::new(crate::store::Store::open_in_memory().unwrap()),
        );
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
