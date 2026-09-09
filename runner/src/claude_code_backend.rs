//! The `claude-code` `ModelBackend`, ported from
//! `cli/src/ralphus/runner/claude_code_backend.py`. Drives `claude -p`
//! headlessly, parsing its `stream-json` event stream on stdout.

use std::io::{BufRead as _, BufReader, Write as _};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;

use crate::backend::{
    BACKGROUND_JOB_NUDGE_PROMPT, BackendError, BackendOutcome, ModelBackend, RunOptions,
};
use crate::cli_agent_common::{live_session_path, write_live_session_id, write_prompt_file};
use crate::shellcmd::{self, Env};
use crate::tools::Workspace;

/// How often [`spawn_stdin_closer`]'s thread polls for the visible turn's
/// terminal `result` event before closing stdin.
const RESULT_POLL_INTERVAL: Duration = Duration::from_millis(200);

const DEFAULT_PROGRAM: &str = "claude";
const RESULT_SUMMARY_TAIL_CHARS: usize = 2000;
/// Fallback for [`format_tool_input`]'s truncation length (RAL-303) when
/// `RunOptions::tool_arg_truncate_chars` is unset -- mirrors
/// `ralphus_daemon::config::DEFAULT_TOOL_ARG_TRUNCATE_CHARS`, but this crate
/// does not depend on the daemon crate, so the value is duplicated rather
/// than shared (this only matters for a hand-authored `CellSpec` that omits
/// the key; a daemon-dispatched cell always sends the resolved value).
const DEFAULT_TOOL_ARG_TRUNCATE_CHARS: usize = 200;

pub struct ClaudeCodeBackend {
    /// Keeps the prompt file and live-session side-channel file on disk
    /// after the run for debugging, instead of deleting them in cleanup --
    /// mirrors `config.daemon.keep_temporary_files`.
    pub keep_temporary_files: bool,
    pub program_override: Option<String>,
}

impl ClaudeCodeBackend {
    fn program(&self) -> String {
        self.program_override.clone().unwrap_or_else(|| {
            std::env::var("RALPHUS_CLAUDE_COMMAND").unwrap_or_else(|_| DEFAULT_PROGRAM.to_string())
        })
    }
}

impl ModelBackend for ClaudeCodeBackend {
    fn preflight(&self) -> Result<(), BackendError> {
        crate::cli_agent_common::preflight_default_program(
            &self.program(),
            self.program_override.is_some() || std::env::var_os("RALPHUS_CLAUDE_COMMAND").is_some(),
        )
    }

    fn run(
        &self,
        prompt: &str,
        workspace: &Workspace,
        options: &RunOptions<'_>,
    ) -> Result<BackendOutcome, BackendError> {
        // Always deliver the cell's own prompt, even when resuming a session.
        // A resumed conversation treats it as a fresh user turn layered on
        // top of the accumulated context (RAL-248 AC3): cross-cell session
        // sharing needs the *new* cell's task text, not a generic "continue"
        // — the session's tool/file state still carries forward and the new
        // prompt does not corrupt it (codex already does the same: it
        // streams `prompt` straight into a resumed thread).
        //
        // RAL-288 Stage 2: delivered as the first stream-json user turn on
        // stdin (see `write_initial_turn` below) rather than a one-shot
        // `-p @promptfile` argv value -- that's what makes this invocation a
        // real bidirectional session instead of a fire-and-forget subprocess,
        // which async tool calls (Monitor, ScheduleWakeup) need to finish
        // properly, and is what lets a mid-task detach (Stage 6) cleanly stop
        // it before a real interactive resume session safely takes over.
        let program = self.program();
        let compound = crate::cli_agent_common::is_compound_command(&program);

        let mut base_args: Vec<String> = vec![
            "-p".to_string(),
            "--input-format".to_string(),
            "stream-json".to_string(),
            "--output-format".to_string(),
            "stream-json".to_string(),
            "--replay-user-messages".to_string(),
            "--include-partial-messages".to_string(),
            "--dangerously-skip-permissions".to_string(),
            "--verbose".to_string(),
        ];
        base_args.extend(session_id_args(options));
        if let Some(model) = options.model {
            base_args.push("--model".to_string());
            base_args.push(model.to_string());
        }
        base_args.extend(setting_sources_args(options));
        let claude_config_dir = isolated_claude_config_dir(options, workspace);

        let mut prompt_file_for_system: Option<std::path::PathBuf> = None;
        if let Some(sp) = options.append_system_prompt {
            if compound {
                // A compound (shell-routed) launcher needs the system prompt
                // passed via a file: multiline text is shell-sensitive on
                // Windows (an embedded newline can truncate a cmd.exe line).
                let path = write_prompt_file(sp).map_err(|e| BackendError(e.to_string()))?;
                base_args.push("--append-system-prompt-file".to_string());
                base_args.push(path.display().to_string());
                prompt_file_for_system = Some(path);
            } else {
                base_args.push("--append-system-prompt".to_string());
                base_args.push(sp.to_string());
            }
        }

        let mut child = spawn(
            &program,
            compound,
            &base_args,
            workspace,
            options.auto_compact_threshold,
            options.maximum_tool_output_tokens,
            claude_config_dir.as_deref(),
        )
        .map_err(|e| BackendError(format!("could not spawn {program}: {e}")))?;

        // Human-readable header for the live tmux pane (RAL-102) -- everything
        // below this is Claude's own text/tool-call activity, not runner logging.
        print_header(options.model, workspace);

        // RAL-288 Stage 2: stdin is shared with the closer thread below (it
        // closes stdin once the visible turn's terminal `result` event is
        // seen), so it's held behind an `Arc<Mutex<_>>` rather than owned
        // outright by either side.
        let Some(stdin) = child.stdin.take() else {
            let _ = child.kill();
            let _ = child.wait();
            return Err(BackendError("claude-code: no stdin pipe".to_string()));
        };
        let stdin = Arc::new(Mutex::new(stdin));

        if let Err(e) = write_initial_turn(&stdin, prompt) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(BackendError(format!(
                "claude-code: failed to write initial prompt to stdin: {e}"
            )));
        }

        let keep_polling = Arc::new(AtomicBool::new(true));
        let saw_result = Arc::new(AtomicBool::new(false));
        let closer_thread = spawn_stdin_closer(
            Arc::clone(&stdin),
            Arc::clone(&keep_polling),
            Arc::clone(&saw_result),
        );
        // `run()` never writes to stdin again after the initial turn -- the
        // closer thread is now the sole owner of the live `Arc` clone, so
        // dropping this one lets stdin actually close once that thread does.
        drop(stdin);

        // Only meaningful when `--session-id` was actually passed above (not
        // resuming) -- otherwise there is nothing to cross-check against.
        let assigned_session_id = if options.resume_agent_session_id.is_none() {
            options.assigned_agent_session_id
        } else {
            None
        };
        let tool_arg_truncate_chars = options
            .tool_arg_truncate_chars
            .map_or(DEFAULT_TOOL_ARG_TRUNCATE_CHARS, |n| n as usize);
        let thrash_thresholds = crate::thrash::ThrashThresholds {
            max_compactions: options
                .thrash_max_compactions
                .unwrap_or(crate::thrash::DEFAULT_MAX_COMPACTIONS),
            min_turn_gap: options
                .thrash_min_turn_gap
                .unwrap_or(crate::thrash::DEFAULT_MIN_TURN_GAP),
        };
        let outcome = drive_stream_json(
            &mut child,
            workspace,
            options.timeout_sec,
            assigned_session_id,
            &saw_result,
            tool_arg_truncate_chars,
            thrash_thresholds,
        );

        keep_polling.store(false, Ordering::SeqCst);
        let _ = closer_thread.join();

        if !self.keep_temporary_files {
            if let Some(p) = &prompt_file_for_system {
                let _ = std::fs::remove_file(p);
            }
            let _ = std::fs::remove_file(live_session_path(workspace.root()));
        }

        outcome
    }

    fn nudge(
        &self,
        workspace: &Workspace,
        options: &RunOptions<'_>,
    ) -> Result<Option<BackendOutcome>, BackendError> {
        Ok(Some(self.run(
            BACKGROUND_JOB_NUDGE_PROMPT,
            workspace,
            options,
        )?))
    }

    // `supports_maximum_context` is deliberately left at the trait's default
    // (`false`): the claude CLI's only related lever,
    // `CLAUDE_CODE_MAX_OUTPUT_TOKENS`, reserves output-generation budget out
    // of the same fixed context window rather than bounding the window
    // itself (`prompt_tokens + max_tokens <= context_window` is enforced
    // server-side), so raising it shrinks room for history instead of
    // capping it. There is no claude-code lever that does what
    // `maximum_context` promises.

    fn supports_auto_compact_threshold(&self) -> bool {
        true
    }

    fn supports_maximum_tool_output_tokens(&self) -> bool {
        true
    }
}

/// The `--resume <id>` / `--session-id <id>` argument pair for a claude-code
/// invocation. Mutually exclusive: resuming a known session always wins over
/// a fresh pre-assigned one (RAL-288 Stage 1) -- a session being resumed
/// already has a real id, so there is nothing left to pre-assign.
fn session_id_args(options: &RunOptions<'_>) -> Vec<String> {
    if let Some(id) = options.resume_agent_session_id {
        vec!["--resume".to_string(), id.to_string()]
    } else if let Some(id) = options.assigned_agent_session_id {
        vec!["--session-id".to_string(), id.to_string()]
    } else {
        Vec::new()
    }
}

/// RAL-336: `--setting-sources ""` disables loading settings from the
/// user/project/local sources, independent of where `CLAUDE_CONFIG_DIR`
/// points -- the settings-specific isolation lever. Empty when personal
/// settings are allowed.
fn setting_sources_args(options: &RunOptions<'_>) -> Vec<String> {
    if options.allow_personal_settings {
        Vec::new()
    } else {
        vec!["--setting-sources".to_string(), String::new()]
    }
}

/// The env var Claude Code's own CLI reads to override its auto-compact
/// trigger threshold, taking a plain absolute token count and precedence
/// over the `/autocompact` command, the `--autocompact` flag, and the
/// `autoCompactWindow` setting (code.claude.com/docs/en/model-config.md).
/// This is the real delivery mechanism `RunOptions::auto_compact_threshold`
/// maps onto for this backend (RAL-304).
const AUTO_COMPACT_WINDOW_ENV: &str = "CLAUDE_CODE_AUTO_COMPACT_WINDOW";

/// The env var Claude Code's CLI reads to cap how many tokens a single
/// file-read tool result may inject into the conversation -- the real
/// delivery mechanism `RunOptions::maximum_tool_output_tokens` maps onto for this
/// backend (RAL-333).
const FILE_READ_MAX_OUTPUT_TOKENS_ENV: &str = "CLAUDE_CODE_FILE_READ_MAX_OUTPUT_TOKENS";

fn spawn(
    program: &str,
    compound: bool,
    args: &[String],
    workspace: &Workspace,
    auto_compact_threshold: Option<u64>,
    maximum_tool_output_tokens: Option<u64>,
    claude_config_dir: Option<&std::path::Path>,
) -> std::io::Result<Child> {
    if compound {
        let shell = shellcmd::resolve_shell(None);
        let _ = shellcmd::detect_parent_shell(&Env::from_process()); // documents intent; resolve_shell already covers detection
        let line = crate::cli_agent_common::shell_command_line(&shell, program, args);
        let mut cmd =
            shellcmd::command_for_spawn_args(shellcmd::shell_spawn_args(&shell, &line), args)?;
        cmd.current_dir(workspace.root())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        apply_auto_compact_env(&mut cmd, auto_compact_threshold);
        apply_maximum_tool_output_tokens_env(&mut cmd, maximum_tool_output_tokens);
        apply_claude_config_dir_env(&mut cmd, claude_config_dir);
        cmd.spawn()
    } else {
        let mut cmd = Command::new(program);
        cmd.args(args)
            .current_dir(workspace.root())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        apply_auto_compact_env(&mut cmd, auto_compact_threshold);
        apply_maximum_tool_output_tokens_env(&mut cmd, maximum_tool_output_tokens);
        apply_claude_config_dir_env(&mut cmd, claude_config_dir);
        cmd.spawn()
    }
}

/// Sets [`AUTO_COMPACT_WINDOW_ENV`] on `cmd` when `auto_compact_threshold` is
/// set -- a no-op otherwise, mirroring the trigger already used by
/// `context_limit_args` (codex) and `apply_context_settings` (pi) for the
/// same `RunOptions::auto_compact_threshold` field.
fn apply_auto_compact_env(cmd: &mut Command, auto_compact_threshold: Option<u64>) {
    if let Some(v) = auto_compact_threshold {
        cmd.env(AUTO_COMPACT_WINDOW_ENV, v.to_string());
    }
}

/// Sets [`FILE_READ_MAX_OUTPUT_TOKENS_ENV`] on `cmd` when
/// `maximum_tool_output_tokens` is set -- a no-op otherwise, same shape as
/// [`apply_auto_compact_env`] (RAL-333).
fn apply_maximum_tool_output_tokens_env(
    cmd: &mut Command,
    maximum_tool_output_tokens: Option<u64>,
) {
    if let Some(v) = maximum_tool_output_tokens {
        cmd.env(FILE_READ_MAX_OUTPUT_TOKENS_ENV, v.to_string());
    }
}

/// Sets `CLAUDE_CONFIG_DIR` on `cmd` when RAL-336 isolation resolved a
/// redirect directory -- a no-op otherwise, leaving the child's inherited
/// `CLAUDE_CONFIG_DIR` (if any) untouched so an operator who opts fully back
/// in sees unchanged behavior.
fn apply_claude_config_dir_env(cmd: &mut Command, claude_config_dir: Option<&std::path::Path>) {
    if let Some(dir) = claude_config_dir {
        cmd.env("CLAUDE_CONFIG_DIR", dir);
    }
}

/// Resolves the `CLAUDE_CONFIG_DIR` this run's child process should see
/// (RAL-336): `None` when personal memory is allowed (today's behavior --
/// whatever the child inherits is used unmodified), else a per-worktree
/// isolated directory that the operator's real global `CLAUDE.md` is never
/// read from. Claude Code's `--setting-sources ""` flag (added separately in
/// `run()`) already gates settings independent of this directory, so
/// `settings.json` is only worth preserving into the isolated dir when
/// personal settings are still allowed -- otherwise that flag already blocks
/// it regardless of where `CLAUDE_CONFIG_DIR` points. A stored
/// `.credentials.json` login is preserved unconditionally: isolating
/// personal config/memory is not the same decision as forcing a re-login.
fn isolated_claude_config_dir(
    options: &RunOptions<'_>,
    workspace: &Workspace,
) -> Option<std::path::PathBuf> {
    if options.allow_personal_memory {
        return None;
    }
    let real_dir = std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(std::path::PathBuf::from)
        .or_else(|| crate::agent_isolation::home_dir().map(|h| h.join(".claude")));
    let isolated = crate::agent_isolation::isolated_config_dir(workspace.root(), "claude-code");
    crate::agent_isolation::preserve_auth_file(real_dir.as_deref(), &isolated, ".credentials.json");
    if options.allow_personal_settings {
        crate::agent_isolation::preserve_auth_file(real_dir.as_deref(), &isolated, "settings.json");
    }
    Some(isolated)
}

/// Writes the cell's prompt as the first stream-json user turn on the
/// shared stdin handle. The pipe is deliberately never closed here (or right
/// after this call) -- see [`spawn_stdin_closer`] and the caller in `run()`
/// for why: a cell's own agent can issue a self-issued async tool call
/// (Monitor, ScheduleWakeup) and keep working after this initial turn
/// appears to end, and closing stdin too early would cut that off.
fn write_initial_turn(stdin: &Arc<Mutex<ChildStdin>>, prompt: &str) -> std::io::Result<()> {
    let mut guard = stdin
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let turn = serde_json::json!({
        "type": "user",
        "message": {"role": "user", "content": prompt},
        "parent_tool_use_id": Value::Null,
    });
    writeln!(guard, "{turn}")?;
    guard.flush()
}

/// Closes stdin once the visible turn's terminal `result` event has been
/// seen (RAL-288 Stage 2), so Claude sees EOF and exits instead of waiting
/// indefinitely for a next turn that is never coming -- live-confirmed
/// against the real `claude` binary that never closing stdin at all hangs
/// every cell forever, since `--input-format stream-json` does not
/// auto-exit once idle after a turn.
///
/// A deliberate mid-task detach (RAL-288 Stage 6) does not go through this
/// thread at all -- the daemon kills the cell's whole tmux session directly
/// (`Store`/`Detachments`, see `daemon/src/server.rs::detach_and_open_agent`),
/// which works regardless of what this thread is doing and is what makes a
/// safe handoff to a real interactive `claude --resume` session possible.
fn spawn_stdin_closer(
    stdin: Arc<Mutex<ChildStdin>>,
    keep_polling: Arc<AtomicBool>,
    saw_result: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        while keep_polling.load(Ordering::SeqCst) {
            if saw_result.load(Ordering::SeqCst) {
                break;
            }
            std::thread::sleep(RESULT_POLL_INTERVAL);
        }
        drop(stdin);
    })
}

/// Reads a claude-code stream-json invocation's output through to the
/// process's own natural stdout EOF -- not just its first `result` event.
///
/// RAL-288 Stage 2: a cell's own agent can issue a self-issued async
/// tool call (Monitor, ScheduleWakeup) and end its *visible* turn (a
/// `result` event) while that call is still outstanding; Claude Code's own
/// internal harness is what keeps the underlying `claude` process alive and
/// eventually emits a further `assistant`/`result` pair once it resolves --
/// ralphus's only job is to not treat the first `result` as "the cell is
/// done" and not close its own end of the pipe prematurely. This loop
/// already reads every line up to true EOF regardless of how many `result`
/// events appear along the way (each one simply overwrites the previous
/// summary/usage in `state`, so the *last* one wins), and
/// [`write_initial_turn`] deliberately leaves `child.stdin` open rather than
/// closing it right after the first write -- this function is what finally
/// closes it, once Claude's own stdout close tells us it is genuinely done.
/// Reproduced live against `squad-000000000027`/RAL-281's `cargo-test` proof
/// step under the *old* one-shot `-p @promptfile` invocation, which had no
/// such persistence and simply exited (and lost the pending async result)
/// the instant the first turn ended.
fn drive_stream_json(
    child: &mut Child,
    workspace: &Workspace,
    timeout_sec: Option<u64>,
    assigned_session_id: Option<&str>,
    saw_result_flag: &AtomicBool,
    tool_arg_truncate_chars: usize,
    thrash_thresholds: crate::thrash::ThrashThresholds,
) -> Result<BackendOutcome, BackendError> {
    // Drain stderr on a background thread concurrently with the main-thread
    // stdout reader below -- avoids a pipe-buffer deadlock if Claude Code
    // writes enough diagnostic output to fill the stderr pipe.
    let stderr = child.stderr.take();
    let stderr_thread = stderr.map(|s| {
        std::thread::spawn(move || {
            let reader = BufReader::new(s);
            for line in reader.lines().map_while(Result::ok) {
                crate::cartographer::emit(
                    "claude-code",
                    &line,
                    "debug",
                    crate::cartographer::EventContext::default(),
                    serde_json::Value::Null,
                );
            }
        })
    });

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| BackendError("claude-code: no stdout pipe".to_string()))?;
    let reader = BufReader::new(stdout);

    let mut state = ParseState {
        thrash: crate::thrash::ThrashTracker::new(thrash_thresholds),
        ..ParseState::default()
    };
    for line in reader.lines().map_while(Result::ok) {
        let Ok(event) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        process_event(
            &event,
            &mut state,
            workspace,
            assigned_session_id,
            tool_arg_truncate_chars,
        );
        // RAL-288: tells the closer thread (`spawn_stdin_closer`) the
        // visible turn's terminal `result` event has been seen, so it may
        // stop polling and close stdin.
        if state.saw_result {
            saw_result_flag.store(true, Ordering::SeqCst);
        }
        // RAL-339: stop reading at the compaction boundary itself -- a safe
        // stop point -- instead of waiting for Claude's own stdout to reach
        // EOF, which could be arbitrarily many further (thrashing) turns away.
        if state.compaction_thrash.is_some() {
            break;
        }
    }
    // Safety net: if the stream ended without a trailing `assistant`/`result`
    // event to close it (e.g. the process died mid-turn), don't leave a
    // dangling partial line in the pane.
    if state.printed_text_delta {
        finish_delta_line();
    }

    if let Some(detail) = state.compaction_thrash {
        // RAL-339: the run is thrashing -- kill the child now rather than let
        // `wait_for_child` wait for a natural exit that may be arbitrarily far
        // off, then fail the cell with whatever was captured live so far.
        let _ = child.kill();
        let _ = child.wait();
        if let Some(t) = stderr_thread {
            let _ = t.join();
        }
        return Ok(BackendOutcome {
            summary: state.result_summary,
            tokens_in: state.tokens_in,
            tokens_out: state.tokens_out,
            cache_creation_tokens: state.cache_creation_tokens,
            cache_read_tokens: state.cache_read_tokens,
            cost_usd: state.cost_usd,
            agent_session_id: state.agent_session_id,
            abandoned_background_job: state.open_background_job,
            compaction_thrash: Some(detail),
            compaction_input_tokens: state.compaction_input_tokens,
            compaction_count: state.compaction_count,
        });
    }

    // Claude's own stdout has already hit EOF at this point -- it decided it
    // is done (including any self-issued async wait, see this function's own
    // doc comment above), not us. The caller (`run()`) is what closes the
    // shared stdin handle, once it and the closer thread (RAL-288 Stage 2,
    // `spawn_stdin_closer`) both agree this call returning means the same
    // thing.
    let status = wait_for_child(child, timeout_sec)
        .map_err(|e| BackendError(format!("claude-code: {e}")))?;
    if let Some(t) = stderr_thread {
        let _ = t.join();
    }

    if !state.saw_result {
        return Err(BackendError(format!(
            "claude-code exited ({status:?}) without a terminal result event"
        )));
    }

    Ok(BackendOutcome {
        summary: state.result_summary,
        tokens_in: state.tokens_in,
        tokens_out: state.tokens_out,
        cache_creation_tokens: state.cache_creation_tokens,
        cache_read_tokens: state.cache_read_tokens,
        cost_usd: state.cost_usd,
        agent_session_id: state.agent_session_id,
        abandoned_background_job: state.open_background_job,
        compaction_thrash: None,
        compaction_input_tokens: state.compaction_input_tokens,
        compaction_count: state.compaction_count,
    })
}

/// Accumulated state built up across one claude-code stream-json invocation's
/// output lines, mirroring `pi_backend.rs::ParseState`'s split between
/// per-event handling ([`process_event`], directly unit-tested) and the
/// outer stdout-reading loop (not directly tested; relies on
/// [`process_event`]'s coverage instead).
#[derive(Debug, Default, PartialEq)]
struct ParseState {
    agent_session_id: Option<String>,
    result_summary: String,
    tokens_in: i64,
    tokens_out: i64,
    cache_creation_tokens: i64,
    cache_read_tokens: i64,
    cost_usd: f64,
    /// RAL-373: running total of `compactMetadata.preTokens` across every
    /// `compact_boundary` event seen so far -- the context size Claude Code
    /// discarded and re-summarized at each compaction, billed at the
    /// uncached input rate. Accumulated with `+=` in a field the
    /// `Some("result")` arm below never touches, since that arm's five
    /// plain assignments would otherwise silently overwrite it.
    compaction_input_tokens: i64,
    /// RAL-373: count of `compact_boundary` events seen, incremented every
    /// time regardless of whether `preTokens` was reported. Lets a `0`
    /// `compaction_input_tokens` alongside a nonzero count read as "N
    /// compactions happened, sizes not reported by this Claude Code
    /// version" rather than "no compaction happened".
    compaction_count: i64,
    saw_result: bool,
    /// RAL-288 Stage 2: `--include-partial-messages` streams token-level text
    /// deltas via `stream_event` ahead of the complete `assistant` message
    /// that follows -- this tracks whether the current block was already
    /// rendered that way, so the `assistant` handling doesn't print the same
    /// text twice.
    printed_text_delta: bool,
    /// RAL-292: the most recent `Bash` tool call's rendered args, launched
    /// with `run_in_background: true`, that hasn't yet been followed by a
    /// `BashOutput`/`KillShell` call checking on it. Still `Some` once the
    /// turn's terminal `result` event fires means the turn ended without
    /// ever checking that job's actual outcome.
    open_background_job: Option<String>,
    /// RAL-339: shared compaction-thrash counter for this run (see
    /// `crate::thrash`).
    thrash: crate::thrash::ThrashTracker,
    /// RAL-339: set the moment [`Self::thrash`] reports the run has crossed
    /// into thrash -- the outer read loop in [`drive_stream_json`] checks
    /// this after every event and stops (kills the child) rather than
    /// waiting for stdout EOF.
    compaction_thrash: Option<crate::thrash::ThrashDetail>,
}

/// Handles one parsed stream-json line, updating `state` and printing to the
/// live tmux pane (RAL-102) as a side effect. `assigned_session_id` is the id
/// pre-assigned at spawn time (RAL-288 Stage 1), if any, used only to warn on
/// a mismatch against Claude's own report. `tool_arg_truncate_chars` (RAL-303)
/// bounds how much of a `tool_use` argument value is rendered before
/// [`format_tool_input`] truncates it.
fn process_event(
    event: &Value,
    state: &mut ParseState,
    workspace: &Workspace,
    assigned_session_id: Option<&str>,
    tool_arg_truncate_chars: usize,
) {
    match event["type"].as_str() {
        Some("stream_event") => {
            let inner = &event["event"];
            if inner["type"].as_str() == Some("content_block_delta") {
                if let Some(text) = inner["delta"]["text"].as_str() {
                    if !text.is_empty() {
                        print_delta(text);
                        state.printed_text_delta = true;
                    }
                }
            }
        }
        Some("system") if event["subtype"] == "init" => {
            if state.agent_session_id.is_none() {
                if let Some(id) = event["session_id"].as_str() {
                    // RAL-288 Stage 1: Claude's own report is always the one
                    // persisted below -- if it disagrees with the id we
                    // pre-assigned via `--session-id`, that's worth a
                    // WARNING (it would mean the pre-assignment silently
                    // didn't take), but never worth failing the cell over.
                    if let Some(expected) = assigned_session_id {
                        if expected != id {
                            crate::cartographer::emit(
                                "claude-code",
                                "assigned session-id mismatch",
                                "warning",
                                crate::cartographer::EventContext::default(),
                                serde_json::json!({"assigned": expected, "actual": id}),
                            );
                        }
                    }
                    state.agent_session_id = Some(id.to_string());
                    write_live_session_id(workspace.root(), id);
                    crate::cartographer::emit(
                        "claude-code",
                        "session-id known",
                        "info",
                        crate::cartographer::EventContext::default(),
                        serde_json::json!({"agent_session_id": id}),
                    );
                }
            }
        }
        Some("system") if event["subtype"] == "compact_boundary" => {
            // Claude Code auto- (or manually) compacts a session's own
            // conversation history once it nears its context limit -- this
            // can happen mid-turn on any `--resume`'d call, headless or not.
            // Surface it: silently dropping this into the `_ => {}` arm
            // below made compaction invisible to both the live tmux pane
            // and Cartographer, indistinguishable from "never happened".
            let trigger = event["compactMetadata"]["trigger"]
                .as_str()
                .unwrap_or("unknown");
            let pre_tokens = event["compactMetadata"]["preTokens"].as_i64().unwrap_or(0);
            // RAL-373: `preTokens` is the size of the context this
            // compaction just summarized away -- billed uncached, and the
            // whole reason `cost_usd` and the token columns diverge on any
            // cell that compacts. Incremented every time regardless of
            // whether `preTokens` was reported (see `compaction_count`'s
            // doc comment on `ParseState`).
            state.compaction_input_tokens += pre_tokens;
            state.compaction_count += 1;
            print_line(&format!(
                "[compact] conversation history compacted (trigger={trigger}, preTokens={pre_tokens})"
            ));
            crate::cartographer::emit(
                "claude-code",
                "conversation history compacted",
                "warning",
                crate::cartographer::EventContext::default(),
                serde_json::json!({"trigger": trigger, "pre_tokens": pre_tokens}),
            );
            // RAL-339: track compaction cadence and fail closed the moment
            // it crosses into thrash -- see `crate::thrash` for the rule.
            if let Some(detail) = state.thrash.record_compaction() {
                print_line(&format!(
                    "[thrash] autocompaction thrashing detected: {} compactions, \
                     most recently {} turn(s) after the previous one",
                    detail.compaction_count, detail.turns_since_previous_compaction
                ));
                crate::thrash::emit_thrash_event("claude-code", &detail);
                state.compaction_thrash = Some(detail);
            }
        }
        Some("assistant") => {
            // Claude's own text/tool-call activity -- the whole point of the
            // live tmux pane (RAL-102) is to let a human read this, so print
            // it plainly rather than folding it into a summary line. `text`
            // blocks are skipped here when the deltas above already streamed
            // this turn's text -- printing both would duplicate the whole
            // response in the pane.
            //
            // RAL-339: this event fires once per assistant turn, making it
            // the natural "turns since previous compaction" tick.
            state.thrash.record_assistant_turn();
            let already_streamed = state.printed_text_delta;
            if state.printed_text_delta {
                finish_delta_line();
                state.printed_text_delta = false;
            }
            if let Some(blocks) = event["message"]["content"].as_array() {
                for block in blocks {
                    match block["type"].as_str() {
                        Some("text") if !already_streamed => {
                            if let Some(text) = block["text"].as_str() {
                                if !text.is_empty() {
                                    print_line(text);
                                }
                            }
                        }
                        Some("tool_use") => {
                            let name = block["name"].as_str().unwrap_or("tool");
                            let args = format_tool_input(&block["input"], tool_arg_truncate_chars);
                            eprintln!("[tool] {name}({args})");
                            if name == "Bash"
                                && block["input"]["run_in_background"].as_bool() == Some(true)
                            {
                                state.open_background_job = Some(args);
                            } else if name == "BashOutput" || name == "KillShell" {
                                state.open_background_job = None;
                            }
                        }
                        _ => {}
                    }
                }
            }
            let usage = &event["message"]["usage"];
            let ti = usage["input_tokens"].as_i64().unwrap_or(0);
            let to = usage["output_tokens"].as_i64().unwrap_or(0);
            // RAL-326: for an agentic session these two are routinely larger
            // than `input_tokens` by an order of magnitude, so a live usage
            // snapshot that omits them reads as implausibly cheap.
            let cc = usage["cache_creation_input_tokens"].as_i64().unwrap_or(0);
            let cr = usage["cache_read_input_tokens"].as_i64().unwrap_or(0);
            if ti > 0 || to > 0 || cc > 0 || cr > 0 {
                state.cache_creation_tokens = cc;
                state.cache_read_tokens = cr;
                let model = event["message"]["model"].as_str().unwrap_or("");
                let live_cost = estimate_cost_usd(model, ti + cc + cr, to);
                crate::cartographer::emit(
                    "claude-code",
                    crate::cartographer::LIVE_USAGE_MESSAGE,
                    "info",
                    crate::cartographer::EventContext::default(),
                    serde_json::json!({
                        "tokens_in": ti,
                        "tokens_out": to,
                        "cache_creation_tokens": cc,
                        "cache_read_tokens": cr,
                        "cost_usd": live_cost,
                    }),
                );
            }
        }
        Some("user") => {
            // RAL-288: `--replay-user-messages` echoes the initial prompt we
            // wrote back as a plain-string "user" message -- render it
            // distinctly so the terminal log shows who said what, preferring
            // this authoritative echo over a locally-guessed `[you]` line.
            if let Some(text) = event["message"]["content"].as_str() {
                if !text.is_empty() {
                    print_line(&format!("[you] {text}"));
                }
            }
            // Tool results fed back to Claude -- shown for the same reason.
            if let Some(blocks) = event["message"]["content"].as_array() {
                for block in blocks {
                    if block["type"].as_str() != Some("tool_result") {
                        continue;
                    }
                    let text = tool_result_text(&block["content"]);
                    if text.is_empty() {
                        continue;
                    }
                    let text = head(&text, 500);
                    let label = if block["is_error"].as_bool().unwrap_or(false) {
                        "error"
                    } else {
                        "result"
                    };
                    eprintln!("[{label}] {text}");
                }
            }
        }
        Some("result") => {
            if state.printed_text_delta {
                finish_delta_line();
                state.printed_text_delta = false;
            }
            state.saw_result = true;
            let usage = &event["usage"];
            state.tokens_in = usage["input_tokens"].as_i64().unwrap_or(0);
            state.tokens_out = usage["output_tokens"].as_i64().unwrap_or(0);
            state.cache_creation_tokens =
                usage["cache_creation_input_tokens"].as_i64().unwrap_or(0);
            state.cache_read_tokens = usage["cache_read_input_tokens"].as_i64().unwrap_or(0);
            state.cost_usd = event["total_cost_usd"]
                .as_f64()
                .or_else(|| event["cost_usd"].as_f64())
                .unwrap_or(0.0);
            let text = event["result"].as_str().unwrap_or_default();
            state.result_summary = tail(text, RESULT_SUMMARY_TAIL_CHARS);
        }
        _ => {}
    }
}

fn wait_for_child(
    child: &mut Child,
    timeout_sec: Option<u64>,
) -> std::io::Result<std::process::ExitStatus> {
    let Some(secs) = timeout_sec else {
        return child.wait();
    };
    let deadline = Duration::from_secs(secs);
    let start = std::time::Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        if start.elapsed() >= deadline {
            let _ = child.kill();
            return child.wait();
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn tail(text: &str, limit: usize) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= limit {
        trimmed.to_string()
    } else {
        trimmed
            .chars()
            .rev()
            .take(limit)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect()
    }
}

/// Truncates `text` to its first `limit` chars, appending `…` if it was cut
/// -- the head-truncation mirror of [`tail`] above, used for tool-result
/// previews where the start of the output matters more than the end.
fn head(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        text.to_string()
    } else {
        let truncated: String = text.chars().take(limit).collect();
        format!("{truncated}…")
    }
}

/// Human-readable header for the live tmux pane (RAL-102) -- printed once,
/// before Claude's own streamed text/tool-call activity below it.
#[allow(clippy::print_stdout)] // intentional: this process runs tmux-wrapped (see `daemon/src/runner.rs::run_via_tmux`), so stdout is the live pane, not the daemon<->runner JSON channel (that contract is file-based here -- see `main.rs`'s `--result-file` handling)
fn print_header(model: Option<&str>, workspace: &Workspace) {
    println!(
        "Claude Code · model={}\ncwd: {}\n",
        model.unwrap_or("default"),
        workspace.root().display()
    );
}

/// Prints one line of Claude's own streamed text output to the live tmux
/// pane -- the whole point of RAL-102 is to let a human read this.
#[allow(clippy::print_stdout)] // intentional: see `print_header`
fn print_line(text: &str) {
    println!("{text}");
}

/// Prints one `--include-partial-messages` text delta with no trailing
/// newline, flushed immediately so the pane renders token-by-token instead
/// of buffering a whole line (RAL-288 Stage 2). Mirrors
/// `pi_backend.rs::print_delta`.
#[allow(clippy::print_stdout)] // intentional: see `print_header`
fn print_delta(delta: &str) {
    print!("{delta}");
    let _ = std::io::Write::flush(&mut std::io::stdout());
}

/// Ends a run of [`print_delta`] calls with the newline they withheld.
/// Mirrors `pi_backend.rs::finish_delta_line`.
#[allow(clippy::print_stdout)] // intentional: see `print_header`
fn finish_delta_line() {
    println!();
}

/// Renders a `tool_use` block's input compactly for the live tmux pane
/// (RAL-102), mirroring the old `claude_code_backend.py`'s
/// `_format_tool_input`. `truncate_chars` (RAL-303) is the per-value
/// character budget before a trailing `…` is appended -- configurable via
/// `[live_view] tool_arg_truncate_chars`, since the fixed 80-char cutoff this
/// used to hardcode made exactly the tool calls an operator most needs to
/// read (file edits, shell commands, diffs) illegible.
fn format_tool_input(input: &Value, truncate_chars: usize) -> String {
    let Some(obj) = input.as_object() else {
        return String::new();
    };
    obj.iter()
        .map(|(key, value)| {
            let text = match value {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            let text = if text.chars().count() > truncate_chars {
                let truncated: String = text.chars().take(truncate_chars).collect();
                format!("{truncated}…")
            } else {
                text
            };
            format!("{key}={text:?}")
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Extracts human-readable text from a `tool_result` block's content
/// (RAL-102): either a plain string or a list of content blocks (e.g.
/// `{"type": "text", "text": "..."}`) per the Claude Code stream-json schema.
fn tool_result_text(content: &Value) -> String {
    if let Some(s) = content.as_str() {
        return s.to_string();
    }
    let Some(blocks) = content.as_array() else {
        return String::new();
    };
    blocks
        .iter()
        .filter(|b| b["type"].as_str() == Some("text"))
        .filter_map(|b| b["text"].as_str())
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Approximate per-million-token pricing by model-name substring, used only
/// for the *live* progress estimate emitted mid-run (a normally-completed
/// `CellResult` uses the authoritative `total_cost_usd` from the terminal
/// `result` event). Deliberately conservative (falls back to the priciest
/// tier) since this feeds the RAL-161 cost-cap kill switch -- overestimating
/// triggers an early check rather than letting an actual overspend slip
/// through.
///
/// `tokens_in` is *total* billed input: uncached input plus cache-creation
/// plus cache-read tokens (RAL-326). Anthropic bills the two cache tiers at
/// different rates than uncached input, but pricing them all at the uncached
/// rate keeps this on the conservative side of the real figure, which is the
/// only property the kill switch depends on.
fn estimate_cost_usd(model: &str, tokens_in: i64, tokens_out: i64) -> f64 {
    let m = model.to_lowercase();
    let (rate_in, rate_out) = if m.contains("haiku") {
        (1.0, 5.0)
    } else if m.contains("sonnet") || m.contains("fable") || m.contains("mythos") {
        (3.0, 15.0)
    } else {
        (15.0, 75.0)
    };
    (tokens_in as f64 * rate_in + tokens_out as f64 * rate_out) / 1_000_000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_keeps_trailing_text() {
        assert_eq!(tail(&"x".repeat(10), 3), "xxx");
    }

    // ── auto-compact env delivery (RAL-304) ────────────────────────────────

    #[test]
    fn apply_auto_compact_env_sets_the_var_when_threshold_is_some() {
        let mut cmd = Command::new("echo");
        apply_auto_compact_env(&mut cmd, Some(80_000));
        let val = cmd
            .get_envs()
            .find(|(k, _)| *k == AUTO_COMPACT_WINDOW_ENV)
            .and_then(|(_, v)| v);
        assert_eq!(val, Some(std::ffi::OsStr::new("80000")));
    }

    #[test]
    fn apply_auto_compact_env_is_a_noop_when_threshold_is_none() {
        let mut cmd = Command::new("echo");
        apply_auto_compact_env(&mut cmd, None);
        assert!(!cmd.get_envs().any(|(k, _)| k == AUTO_COMPACT_WINDOW_ENV));
    }

    // ── format_tool_input truncation length (RAL-303) ─────────────────────

    #[test]
    fn format_tool_input_truncates_at_the_configured_length_not_a_hardcoded_80() {
        let input = serde_json::json!({"command": "x".repeat(150)});
        // A length below the old hardcoded 80 must actually take effect --
        // this is the case that would silently pass if the truncation
        // length parameter were plumbed through but never read.
        let short = format_tool_input(&input, 10);
        assert_eq!(
            short,
            format!("command={:?}", format!("{}…", "x".repeat(10)))
        );
        // A length above the old hardcoded 80 must also take effect -- the
        // whole point of RAL-303 was to let an operator raise the cutoff.
        let long = format_tool_input(&input, 120);
        assert_eq!(
            long,
            format!("command={:?}", format!("{}…", "x".repeat(120)))
        );
    }

    #[test]
    fn format_tool_input_does_not_truncate_a_value_at_or_under_the_configured_length() {
        let input = serde_json::json!({"command": "short"});
        assert_eq!(
            format_tool_input(&input, DEFAULT_TOOL_ARG_TRUNCATE_CHARS),
            "command=\"short\""
        );
    }

    #[test]
    fn process_event_honors_a_non_default_truncation_length_for_a_tool_use_block() {
        // End-to-end through `process_event` (not just `format_tool_input`
        // directly), so a regression that stops threading the parameter down
        // from `RunOptions` to `format_tool_input` is caught here too.
        let mut state = ParseState::default();
        let ws = test_workspace();
        process_event(
            &tool_use_event("Bash", serde_json::json!({"command": "x".repeat(50)})),
            &mut state,
            &ws,
            None,
            5,
        );
        // No direct stdout capture here (mirrors this file's other
        // `process_event` tests), but `open_background_job` is set from the
        // same rendered `args` string `format_tool_input` produced, so it
        // doubles as a window into the truncation actually applied when the
        // call is a backgrounded Bash job.
        process_event(
            &tool_use_event(
                "Bash",
                serde_json::json!({"command": "x".repeat(50), "run_in_background": true}),
            ),
            &mut state,
            &ws,
            None,
            5,
        );
        let job = state.open_background_job.expect("background job recorded");
        assert_eq!(job, "command=\"xxxxx…\", run_in_background=\"true\"");
    }

    fn test_workspace() -> Workspace {
        Workspace::create(std::env::temp_dir()).unwrap()
    }

    #[test]
    fn process_event_renders_you_line_for_a_replayed_plain_string_user_turn() {
        // Can't capture stdout directly here, but a plain-string "user"
        // message (as opposed to the tool_result content-block-array shape)
        // must not panic and must fall through the existing tool_result loop
        // as a no-op, since `as_array()` on a string returns `None`.
        let mut state = ParseState::default();
        let ws = test_workspace();
        process_event(
            &serde_json::json!({"type":"user","message":{"content":"keep going"}}),
            &mut state,
            &ws,
            None,
            DEFAULT_TOOL_ARG_TRUNCATE_CHARS,
        );
        // No assertion beyond "did not panic and left state untouched" --
        // this event carries no session/usage/result data of its own.
        assert_eq!(state, ParseState::default());
    }

    #[test]
    fn process_event_handles_compact_boundary_without_panicking_or_mutating_other_state() {
        // Compaction carries no session/usage/result data of its own -- this
        // just proves the new arm doesn't panic on a missing/malformed
        // `compactMetadata`, doesn't fall through to the catch-all, and
        // (RAL-373) accumulates `preTokens` and the compaction count without
        // touching anything else. A single, isolated compaction never
        // thrashes (RAL-339), so `compaction_thrash` stays `None` even
        // though the tracker itself now records the one compaction.
        let mut state = ParseState::default();
        let ws = test_workspace();
        process_event(
            &serde_json::json!({
                "type":"system",
                "subtype":"compact_boundary",
                "compactMetadata":{"trigger":"auto","preTokens":164975}
            }),
            &mut state,
            &ws,
            None,
            DEFAULT_TOOL_ARG_TRUNCATE_CHARS,
        );
        assert_eq!(state.compaction_thrash, None);
        assert_eq!(state.compaction_input_tokens, 164975);
        assert_eq!(state.compaction_count, 1);
        assert_eq!(
            state,
            ParseState {
                thrash: state.thrash,
                compaction_input_tokens: state.compaction_input_tokens,
                compaction_count: state.compaction_count,
                ..ParseState::default()
            }
        );
    }

    /// RAL-373: `preTokens` missing (older Claude Code / malformed event)
    /// still increments the count -- this is the "N compactions, sizes not
    /// reported" case `compaction_count` exists to make self-describing.
    #[test]
    fn process_event_counts_a_compaction_even_when_pre_tokens_is_absent() {
        let mut state = ParseState::default();
        let ws = test_workspace();
        process_event(
            &serde_json::json!({
                "type":"system",
                "subtype":"compact_boundary",
                "compactMetadata":{"trigger":"auto"}
            }),
            &mut state,
            &ws,
            None,
            DEFAULT_TOOL_ARG_TRUNCATE_CHARS,
        );
        assert_eq!(state.compaction_input_tokens, 0);
        assert_eq!(state.compaction_count, 1);
    }

    /// RAL-373: multiple compactions across a run must accumulate, not
    /// overwrite -- this is the difference between `+=` and `=` that the
    /// ticket calls out as "the one way to get this wrong".
    #[test]
    fn process_event_accumulates_compaction_tokens_across_multiple_boundaries() {
        let mut state = ParseState::default();
        let ws = test_workspace();
        for pre_tokens in [100_000, 120_000, 90_000] {
            process_event(
                &serde_json::json!({
                    "type":"system",
                    "subtype":"compact_boundary",
                    "compactMetadata":{"trigger":"auto","preTokens":pre_tokens}
                }),
                &mut state,
                &ws,
                None,
                DEFAULT_TOOL_ARG_TRUNCATE_CHARS,
            );
        }
        assert_eq!(state.compaction_input_tokens, 310_000);
        assert_eq!(state.compaction_count, 3);
    }

    /// RAL-373: the `Some("result")` arm is five plain assignments, not
    /// `+=` -- proves it does not clobber the compaction accumulators a
    /// prior `compact_boundary` already built up.
    #[test]
    fn process_event_result_event_does_not_clobber_compaction_accumulators() {
        let mut state = ParseState::default();
        let ws = test_workspace();
        process_event(
            &serde_json::json!({
                "type":"system",
                "subtype":"compact_boundary",
                "compactMetadata":{"trigger":"auto","preTokens":115_000}
            }),
            &mut state,
            &ws,
            None,
            DEFAULT_TOOL_ARG_TRUNCATE_CHARS,
        );
        process_event(
            &serde_json::json!({
                "type":"result",
                "usage":{"input_tokens":12,"output_tokens":34},
                "total_cost_usd":0.56,
                "result":"all done"
            }),
            &mut state,
            &ws,
            None,
            DEFAULT_TOOL_ARG_TRUNCATE_CHARS,
        );
        assert_eq!(state.compaction_input_tokens, 115_000);
        assert_eq!(state.compaction_count, 1);
    }

    /// RAL-339: the default thresholds (N=3, M=2) -- three compactions with
    /// zero assistant turns between the second and third must fail closed at
    /// the third compaction's own event, not later.
    #[test]
    fn process_event_flags_thrash_on_the_default_thresholds() {
        let mut state = ParseState::default();
        let ws = test_workspace();
        let compact = serde_json::json!({
            "type":"system",
            "subtype":"compact_boundary",
            "compactMetadata":{"trigger":"auto","preTokens":100000}
        });
        process_event(
            &compact,
            &mut state,
            &ws,
            None,
            DEFAULT_TOOL_ARG_TRUNCATE_CHARS,
        );
        assert_eq!(state.compaction_thrash, None);
        process_event(
            &compact,
            &mut state,
            &ws,
            None,
            DEFAULT_TOOL_ARG_TRUNCATE_CHARS,
        );
        assert_eq!(state.compaction_thrash, None);
        process_event(
            &compact,
            &mut state,
            &ws,
            None,
            DEFAULT_TOOL_ARG_TRUNCATE_CHARS,
        );
        let detail = state
            .compaction_thrash
            .expect("third rapid compaction should thrash");
        assert_eq!(detail.compaction_count, 3);
        assert_eq!(detail.turns_since_previous_compaction, 0);
    }

    /// RAL-339: the same three compactions, but with enough assistant turns
    /// between each, must never thrash -- healthy long-running compaction
    /// cadence is not a failure.
    #[test]
    fn process_event_does_not_flag_thrash_when_turns_separate_compactions_healthily() {
        let mut state = ParseState::default();
        let ws = test_workspace();
        let compact = serde_json::json!({
            "type":"system",
            "subtype":"compact_boundary",
            "compactMetadata":{"trigger":"auto","preTokens":100000}
        });
        let assistant_turn = serde_json::json!({
            "type":"assistant",
            "message":{"content":[{"type":"text","text":"working"}]}
        });
        for _ in 0..3 {
            process_event(
                &compact,
                &mut state,
                &ws,
                None,
                DEFAULT_TOOL_ARG_TRUNCATE_CHARS,
            );
            assert_eq!(state.compaction_thrash, None);
            process_event(
                &assistant_turn,
                &mut state,
                &ws,
                None,
                DEFAULT_TOOL_ARG_TRUNCATE_CHARS,
            );
            process_event(
                &assistant_turn,
                &mut state,
                &ws,
                None,
                DEFAULT_TOOL_ARG_TRUNCATE_CHARS,
            );
        }
        assert_eq!(state.compaction_thrash, None);
    }

    #[test]
    fn process_event_captures_session_id_usage_and_result() {
        let mut state = ParseState::default();
        let ws = test_workspace();
        process_event(
            &serde_json::json!({"type":"system","subtype":"init","session_id":"sess-1"}),
            &mut state,
            &ws,
            None,
            DEFAULT_TOOL_ARG_TRUNCATE_CHARS,
        );
        process_event(
            &serde_json::json!({
                "type":"result",
                "usage":{"input_tokens":12,"output_tokens":34},
                "total_cost_usd":0.56,
                "result":"all done"
            }),
            &mut state,
            &ws,
            None,
            DEFAULT_TOOL_ARG_TRUNCATE_CHARS,
        );
        assert_eq!(state.agent_session_id.as_deref(), Some("sess-1"));
        assert_eq!(state.tokens_in, 12);
        assert_eq!(state.tokens_out, 34);
        assert!((state.cost_usd - 0.56).abs() < f64::EPSILON);
        assert_eq!(state.result_summary, "all done");
        assert!(state.saw_result);
    }

    /// RAL-326: an agentic claude-code session bills most of its input through
    /// the two prompt-cache tiers, so a `result` event read without them
    /// produces the implausible "input 34" the ticket was filed over. They are
    /// captured as their own fields rather than folded into `tokens_in`, which
    /// keeps meaning uncached input.
    #[test]
    fn process_event_captures_prompt_cache_tokens_from_the_result_event() {
        let mut state = ParseState::default();
        let ws = test_workspace();
        process_event(
            &serde_json::json!({
                "type":"result",
                "usage":{
                    "input_tokens":34,
                    "output_tokens":5374,
                    "cache_creation_input_tokens":320_114,
                    "cache_read_input_tokens":7_204_990
                },
                "total_cost_usd":0.6807,
                "result":"done"
            }),
            &mut state,
            &ws,
            None,
            DEFAULT_TOOL_ARG_TRUNCATE_CHARS,
        );
        assert_eq!(state.tokens_in, 34, "uncached input keeps its old meaning");
        assert_eq!(state.tokens_out, 5374);
        assert_eq!(state.cache_creation_tokens, 320_114);
        assert_eq!(state.cache_read_tokens, 7_204_990);
    }

    /// The live per-turn snapshot half of the same capture. This one also
    /// feeds the RAL-161 cost cap, and is what the lost-cell fallback records
    /// permanently -- an omission here under-reports both places.
    #[test]
    fn process_event_captures_prompt_cache_tokens_from_a_live_assistant_turn() {
        let mut state = ParseState::default();
        let ws = test_workspace();
        process_event(
            &serde_json::json!({
                "type":"assistant",
                "message":{
                    "model":"claude-opus-5",
                    "content":[],
                    "usage":{
                        "input_tokens":4,
                        "output_tokens":90,
                        "cache_creation_input_tokens":1_200,
                        "cache_read_input_tokens":48_000
                    }
                }
            }),
            &mut state,
            &ws,
            None,
            DEFAULT_TOOL_ARG_TRUNCATE_CHARS,
        );
        assert_eq!(state.cache_creation_tokens, 1_200);
        assert_eq!(state.cache_read_tokens, 48_000);
    }

    /// A turn whose only billed input is cache reads must still register: the
    /// pre-RAL-326 guard keyed on `input_tokens`/`output_tokens` alone, so a
    /// fully cache-served turn recorded nothing at all.
    #[test]
    fn process_event_records_a_turn_billed_entirely_to_the_cache() {
        let mut state = ParseState::default();
        let ws = test_workspace();
        process_event(
            &serde_json::json!({
                "type":"assistant",
                "message":{
                    "model":"claude-opus-5",
                    "content":[],
                    "usage":{"input_tokens":0,"output_tokens":0,"cache_read_input_tokens":52_000}
                }
            }),
            &mut state,
            &ws,
            None,
            DEFAULT_TOOL_ARG_TRUNCATE_CHARS,
        );
        assert_eq!(state.cache_read_tokens, 52_000);
    }

    /// RAL-373: the reconciliation check the original bug would have failed.
    /// A synthetic stream carries three known-size compactions plus a
    /// `result` event with known per-tier usage; `total_cost_usd` is set to
    /// what those tiers actually cost at real Anthropic Sonnet rates
    /// (uncached in/out/cache-write/cache-read, from the ticket's worked
    /// table -- deliberately *not* `estimate_cost_usd`'s single-rate
    /// approximation, which exists only to be conservative for the live
    /// kill switch), plus a small residual standing in for compaction's
    /// unrecorded summary-output tokens. Pricing every recorded column --
    /// including the new `compaction_input_tokens`, billed uncached like
    /// the rest of compaction's input -- must land within ~10% of that
    /// recorded cost. Pricing the same columns *without*
    /// `compaction_input_tokens` (i.e. today's pre-fix behavior) must miss
    /// by far more than that: that gap is the bug.
    #[test]
    fn reconciliation_pricing_recorded_columns_matches_recorded_cost_usd() {
        const RATE_UNCACHED_IN: f64 = 3.0;
        const RATE_OUT: f64 = 15.0;
        const RATE_CACHE_WRITE: f64 = 3.75;
        const RATE_CACHE_READ: f64 = 0.30;

        let mut state = ParseState::default();
        let ws = test_workspace();
        let compaction = serde_json::json!({
            "type":"system",
            "subtype":"compact_boundary",
            "compactMetadata":{"trigger":"auto","preTokens":50_000}
        });
        for _ in 0..3 {
            process_event(
                &compaction,
                &mut state,
                &ws,
                None,
                DEFAULT_TOOL_ARG_TRUNCATE_CHARS,
            );
        }

        let tokens_in = 1_000i64;
        let tokens_out = 2_000i64;
        let cache_creation = 5_000i64;
        let cache_read = 200_000i64;

        let priced_without_compaction = (tokens_in as f64 * RATE_UNCACHED_IN
            + tokens_out as f64 * RATE_OUT
            + cache_creation as f64 * RATE_CACHE_WRITE
            + cache_read as f64 * RATE_CACHE_READ)
            / 1_000_000.0;
        // Compaction input is billed uncached (this is the measurement the
        // ticket verified against live data, not an assumption).
        let compaction_input_tokens = 3 * 50_000i64;
        let compaction_cost = compaction_input_tokens as f64 * RATE_UNCACHED_IN / 1_000_000.0;
        let priced_with_compaction = priced_without_compaction + compaction_cost;
        // ~2.5% stand-in for compaction's unrecorded summary-output tokens
        // (a few thousand per compaction, not currently reported by Claude
        // Code) -- folded into the tolerance below per the ticket.
        let recorded_cost_usd = priced_with_compaction * 1.025;

        process_event(
            &serde_json::json!({
                "type":"result",
                "usage":{
                    "input_tokens":tokens_in,
                    "output_tokens":tokens_out,
                    "cache_creation_input_tokens":cache_creation,
                    "cache_read_input_tokens":cache_read,
                },
                "total_cost_usd":recorded_cost_usd,
                "result":"done"
            }),
            &mut state,
            &ws,
            None,
            DEFAULT_TOOL_ARG_TRUNCATE_CHARS,
        );

        assert_eq!(state.compaction_input_tokens, compaction_input_tokens);
        assert_eq!(state.compaction_count, 3);
        assert!((state.cost_usd - recorded_cost_usd).abs() < f64::EPSILON);

        let priced_from_recorded_columns = (state.tokens_in as f64 * RATE_UNCACHED_IN
            + state.tokens_out as f64 * RATE_OUT
            + state.cache_creation_tokens as f64 * RATE_CACHE_WRITE
            + state.cache_read_tokens as f64 * RATE_CACHE_READ
            + state.compaction_input_tokens as f64 * RATE_UNCACHED_IN)
            / 1_000_000.0;
        let relative_error = (priced_from_recorded_columns - state.cost_usd).abs() / state.cost_usd;
        assert!(
            relative_error < 0.10,
            "priced {priced_from_recorded_columns} vs recorded {}: {relative_error:.3} relative error",
            state.cost_usd
        );

        // The bug this ticket fixes: the same pricing but omitting
        // `compaction_input_tokens` (i.e. what every recorded cell showed
        // before this ticket) must fail reconciliation by far more than the
        // tolerance above.
        let priced_without_compaction_column = (state.tokens_in as f64 * RATE_UNCACHED_IN
            + state.tokens_out as f64 * RATE_OUT
            + state.cache_creation_tokens as f64 * RATE_CACHE_WRITE
            + state.cache_read_tokens as f64 * RATE_CACHE_READ)
            / 1_000_000.0;
        let relative_error_without_compaction =
            (priced_without_compaction_column - state.cost_usd).abs() / state.cost_usd;
        assert!(
            relative_error_without_compaction > 0.10,
            "expected omitting compaction_input_tokens to badly miss reconciliation, \
             but it was within tolerance: {relative_error_without_compaction:.3}"
        );
    }

    /// The live estimate feeds the RAL-161 kill switch, so it must price the
    /// cache tiers rather than ignore them -- ignoring them is what let a
    /// multi-million-token session read as pennies.
    #[test]
    fn estimate_cost_usd_counts_cache_tokens_as_billed_input() {
        let uncached_only = estimate_cost_usd("claude-opus-5", 1_000, 0);
        let with_cache = estimate_cost_usd("claude-opus-5", 1_000 + 500_000, 0);
        assert!(
            with_cache > uncached_only,
            "cache tokens must raise, never lower, the conservative estimate"
        );
    }

    #[test]
    fn process_event_streamed_delta_is_not_duplicated_by_the_assistant_block() {
        let mut state = ParseState::default();
        let ws = test_workspace();
        process_event(
            &serde_json::json!({
                "type":"stream_event",
                "event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"hi"}}
            }),
            &mut state,
            &ws,
            None,
            DEFAULT_TOOL_ARG_TRUNCATE_CHARS,
        );
        assert!(
            state.printed_text_delta,
            "delta must be recorded as printed"
        );
        process_event(
            &serde_json::json!({
                "type":"assistant",
                "message":{"content":[{"type":"text","text":"hi there"}]}
            }),
            &mut state,
            &ws,
            None,
            DEFAULT_TOOL_ARG_TRUNCATE_CHARS,
        );
        // The assistant branch must have noticed the already-streamed text and
        // reset the flag (closing the delta line) rather than re-printing the
        // whole block -- there is no direct way to assert stdout wasn't
        // written twice from here, so this checks the state transition that
        // gates it.
        assert!(!state.printed_text_delta);
    }

    #[test]
    fn process_event_falls_back_to_printing_text_when_no_delta_preceded_it() {
        // A turn that (for whatever reason) produced no `stream_event` deltas
        // must still render its text via the `assistant` block -- content
        // must never be silently dropped.
        let mut state = ParseState::default();
        let ws = test_workspace();
        assert!(!state.printed_text_delta);
        process_event(
            &serde_json::json!({
                "type":"assistant",
                "message":{"content":[{"type":"text","text":"no deltas here"}]}
            }),
            &mut state,
            &ws,
            None,
            DEFAULT_TOOL_ARG_TRUNCATE_CHARS,
        );
        assert!(!state.printed_text_delta);
    }

    #[test]
    fn process_event_a_second_result_event_overwrites_the_first() {
        // RAL-288 Stage 2: a self-issued async tool call (Monitor,
        // ScheduleWakeup) can produce a *second* turn -- and thus a second
        // `result` event -- later in the same still-open stream. The last one
        // observed must be authoritative, not the first.
        let mut state = ParseState::default();
        let ws = test_workspace();
        process_event(
            &serde_json::json!({
                "type":"result",
                "usage":{"input_tokens":1,"output_tokens":1},
                "total_cost_usd":0.01,
                "result":"waiting on a background job"
            }),
            &mut state,
            &ws,
            None,
            DEFAULT_TOOL_ARG_TRUNCATE_CHARS,
        );
        process_event(
            &serde_json::json!({
                "type":"result",
                "usage":{"input_tokens":5,"output_tokens":9},
                "total_cost_usd":0.20,
                "result":"the background job finished, here is the real answer"
            }),
            &mut state,
            &ws,
            None,
            DEFAULT_TOOL_ARG_TRUNCATE_CHARS,
        );
        assert_eq!(
            state.result_summary,
            "the background job finished, here is the real answer"
        );
        assert_eq!(state.tokens_in, 5);
        assert_eq!(state.tokens_out, 9);
        assert!((state.cost_usd - 0.20).abs() < f64::EPSILON);
    }

    fn tool_use_event(name: &str, input: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "type": "assistant",
            "message": {
                "content": [{"type": "tool_use", "name": name, "input": input}],
                "usage": {},
            },
        })
    }

    #[test]
    fn process_event_flags_a_backgrounded_bash_call_left_unchecked_at_turn_end() {
        // RAL-292: the RAL-280/281 bug -- a `Bash` call with
        // `run_in_background: true` that nothing ever follows up on before
        // the turn's terminal `result` event fires.
        let mut state = ParseState::default();
        let ws = test_workspace();
        process_event(
            &tool_use_event(
                "Bash",
                serde_json::json!({"command": "cargo build", "run_in_background": true}),
            ),
            &mut state,
            &ws,
            None,
            DEFAULT_TOOL_ARG_TRUNCATE_CHARS,
        );
        process_event(
            &serde_json::json!({
                "type": "result",
                "usage": {"input_tokens": 1, "output_tokens": 1},
                "total_cost_usd": 0.01,
                "result": "done, build is running in the background"
            }),
            &mut state,
            &ws,
            None,
            DEFAULT_TOOL_ARG_TRUNCATE_CHARS,
        );
        assert!(state.open_background_job.is_some());
    }

    #[test]
    fn process_event_does_not_flag_a_foreground_bash_call() {
        let mut state = ParseState::default();
        let ws = test_workspace();
        process_event(
            &tool_use_event("Bash", serde_json::json!({"command": "cargo build"})),
            &mut state,
            &ws,
            None,
            DEFAULT_TOOL_ARG_TRUNCATE_CHARS,
        );
        assert!(state.open_background_job.is_none());
    }

    #[test]
    fn process_event_clears_the_flag_once_bash_output_is_checked() {
        let mut state = ParseState::default();
        let ws = test_workspace();
        process_event(
            &tool_use_event(
                "Bash",
                serde_json::json!({"command": "cargo build", "run_in_background": true}),
            ),
            &mut state,
            &ws,
            None,
            DEFAULT_TOOL_ARG_TRUNCATE_CHARS,
        );
        process_event(
            &tool_use_event("BashOutput", serde_json::json!({"bash_id": "1"})),
            &mut state,
            &ws,
            None,
            DEFAULT_TOOL_ARG_TRUNCATE_CHARS,
        );
        assert!(state.open_background_job.is_none());
    }

    #[test]
    fn process_event_clears_the_flag_once_the_job_is_killed() {
        let mut state = ParseState::default();
        let ws = test_workspace();
        process_event(
            &tool_use_event(
                "Bash",
                serde_json::json!({"command": "cargo build", "run_in_background": true}),
            ),
            &mut state,
            &ws,
            None,
            DEFAULT_TOOL_ARG_TRUNCATE_CHARS,
        );
        process_event(
            &tool_use_event("KillShell", serde_json::json!({"shell_id": "1"})),
            &mut state,
            &ws,
            None,
            DEFAULT_TOOL_ARG_TRUNCATE_CHARS,
        );
        assert!(state.open_background_job.is_none());
    }

    #[test]
    fn process_event_a_later_background_job_reopens_the_flag_after_a_check() {
        let mut state = ParseState::default();
        let ws = test_workspace();
        process_event(
            &tool_use_event(
                "Bash",
                serde_json::json!({"command": "cargo build", "run_in_background": true}),
            ),
            &mut state,
            &ws,
            None,
            DEFAULT_TOOL_ARG_TRUNCATE_CHARS,
        );
        process_event(
            &tool_use_event("BashOutput", serde_json::json!({"bash_id": "1"})),
            &mut state,
            &ws,
            None,
            DEFAULT_TOOL_ARG_TRUNCATE_CHARS,
        );
        process_event(
            &tool_use_event(
                "Bash",
                serde_json::json!({"command": "cargo test", "run_in_background": true}),
            ),
            &mut state,
            &ws,
            None,
            DEFAULT_TOOL_ARG_TRUNCATE_CHARS,
        );
        assert!(state.open_background_job.is_some());
    }

    #[test]
    fn process_event_trusts_claudes_own_session_id_over_a_mismatched_assigned_one() {
        let mut state = ParseState::default();
        let ws = test_workspace();
        process_event(
            &serde_json::json!({"type":"system","subtype":"init","session_id":"real-id"}),
            &mut state,
            &ws,
            Some("assigned-id"),
            DEFAULT_TOOL_ARG_TRUNCATE_CHARS,
        );
        assert_eq!(state.agent_session_id.as_deref(), Some("real-id"));
    }

    #[test]
    fn estimate_cost_usd_scales_with_tokens() {
        let cheap = estimate_cost_usd("claude-haiku-4-5", 1_000_000, 0);
        let pricey = estimate_cost_usd("claude-opus-5", 1_000_000, 0);
        assert!(pricey > cheap);
    }

    #[test]
    fn session_id_args_passes_session_id_when_assigned_and_not_resuming() {
        let options = RunOptions {
            assigned_agent_session_id: Some("assigned-id"),
            ..Default::default()
        };
        assert_eq!(
            session_id_args(&options),
            vec!["--session-id".to_string(), "assigned-id".to_string()]
        );
    }

    #[test]
    fn session_id_args_prefers_resume_over_an_assigned_id() {
        let options = RunOptions {
            resume_agent_session_id: Some("resume-id"),
            assigned_agent_session_id: Some("assigned-id"),
            ..Default::default()
        };
        assert_eq!(
            session_id_args(&options),
            vec!["--resume".to_string(), "resume-id".to_string()]
        );
    }

    #[test]
    fn session_id_args_empty_when_neither_is_set() {
        assert!(session_id_args(&RunOptions::default()).is_empty());
    }

    // ── RAL-336 agent isolation ────────────────────────────────────────────

    #[test]
    fn setting_sources_args_disables_settings_by_default() {
        // `RunOptions::default()` -- `allow_personal_settings` is `false`.
        assert_eq!(
            setting_sources_args(&RunOptions::default()),
            vec!["--setting-sources".to_string(), String::new()]
        );
    }

    #[test]
    fn setting_sources_args_is_empty_when_personal_settings_are_allowed() {
        let options = RunOptions {
            allow_personal_settings: true,
            ..Default::default()
        };
        assert!(setting_sources_args(&options).is_empty());
    }

    #[test]
    fn apply_claude_config_dir_env_sets_the_var_when_some() {
        let dir = std::env::temp_dir().join("ralphus-claude-isolation-test");
        let mut cmd = Command::new("echo");
        apply_claude_config_dir_env(&mut cmd, Some(&dir));
        let val = cmd
            .get_envs()
            .find(|(k, _)| *k == "CLAUDE_CONFIG_DIR")
            .and_then(|(_, v)| v);
        assert_eq!(val, Some(dir.as_os_str()));
    }

    #[test]
    fn apply_claude_config_dir_env_is_a_noop_when_none() {
        let mut cmd = Command::new("echo");
        apply_claude_config_dir_env(&mut cmd, None);
        assert!(!cmd.get_envs().any(|(k, _)| k == "CLAUDE_CONFIG_DIR"));
    }

    #[test]
    fn isolated_claude_config_dir_is_none_when_personal_memory_is_allowed() {
        let options = RunOptions {
            allow_personal_memory: true,
            ..Default::default()
        };
        let ws = test_workspace();
        assert_eq!(isolated_claude_config_dir(&options, &ws), None);
    }

    #[test]
    fn isolated_claude_config_dir_is_some_when_personal_memory_is_isolated() {
        let options = RunOptions::default();
        let ws = test_workspace();
        let dir = isolated_claude_config_dir(&options, &ws).expect("isolated dir");
        assert!(dir.to_string_lossy().contains("claude-code"));
    }
}
