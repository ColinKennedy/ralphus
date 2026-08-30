//! The `claude-code` `ModelBackend`, ported from
//! `cli/src/ralphus/runner/claude_code_backend.py`. Drives `claude -p`
//! headlessly, parsing its `stream-json` event stream on stdout.

use std::io::{BufRead as _, BufReader, Write as _};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;

use crate::backend::{BackendError, BackendOutcome, ModelBackend, RunOptions};
use crate::cli_agent_common::{live_session_path, write_live_session_id, write_prompt_file};
use crate::shellcmd::{self, Env, SpawnArgs};
use crate::tools::Workspace;

/// How often [`spawn_stdin_closer`]'s thread polls for the visible turn's
/// terminal `result` event before closing stdin.
const RESULT_POLL_INTERVAL: Duration = Duration::from_millis(200);

const DEFAULT_PROGRAM: &str = "claude";
const RESULT_SUMMARY_TAIL_CHARS: usize = 2000;

pub struct ClaudeCodeBackend {
    /// Keeps the prompt file and live-session side-channel file on disk
    /// after the run for debugging, instead of deleting them in cleanup --
    /// mirrors `config.daemon.keep_temporary_files`.
    pub keep_temporary_files: bool,
    pub program_override: Option<String>,
}

impl ModelBackend for ClaudeCodeBackend {
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
        let program = self.program_override.clone().unwrap_or_else(|| {
            std::env::var("RALPHUS_CLAUDE_COMMAND").unwrap_or_else(|_| DEFAULT_PROGRAM.to_string())
        });
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

        let mut child = spawn(&program, compound, &base_args, workspace)
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
        let outcome = drive_stream_json(
            &mut child,
            workspace,
            options.timeout_sec,
            assigned_session_id,
            &saw_result,
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

fn spawn(
    program: &str,
    compound: bool,
    args: &[String],
    workspace: &Workspace,
) -> std::io::Result<Child> {
    if compound {
        let shell = shellcmd::resolve_shell(None);
        let _ = shellcmd::detect_parent_shell(&Env::from_process()); // documents intent; resolve_shell already covers detection
        let line = shellcmd::build_compound_command_line(&shell, program, args);
        match shellcmd::shell_spawn_args(&shell, &line) {
            SpawnArgs::RawShellLine(raw) => Command::new("cmd")
                .arg("/C")
                .arg(raw)
                .current_dir(workspace.root())
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn(),
            SpawnArgs::Argv(argv) => {
                let mut cmd = Command::new(&argv[0]);
                cmd.args(&argv[1..]);
                cmd.current_dir(workspace.root())
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()
            }
        }
    } else {
        Command::new(program)
            .args(args)
            .current_dir(workspace.root())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
    }
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

    let mut state = ParseState::default();
    for line in reader.lines().map_while(Result::ok) {
        let Ok(event) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        process_event(&event, &mut state, workspace, assigned_session_id);
        // RAL-288: tells the closer thread (`spawn_stdin_closer`) the
        // visible turn's terminal `result` event has been seen, so it may
        // stop polling and close stdin.
        if state.saw_result {
            saw_result_flag.store(true, Ordering::SeqCst);
        }
    }
    // Safety net: if the stream ended without a trailing `assistant`/`result`
    // event to close it (e.g. the process died mid-turn), don't leave a
    // dangling partial line in the pane.
    if state.printed_text_delta {
        finish_delta_line();
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
        cost_usd: state.cost_usd,
        agent_session_id: state.agent_session_id,
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
    cost_usd: f64,
    saw_result: bool,
    /// RAL-288 Stage 2: `--include-partial-messages` streams token-level text
    /// deltas via `stream_event` ahead of the complete `assistant` message
    /// that follows -- this tracks whether the current block was already
    /// rendered that way, so the `assistant` handling doesn't print the same
    /// text twice.
    printed_text_delta: bool,
}

/// Handles one parsed stream-json line, updating `state` and printing to the
/// live tmux pane (RAL-102) as a side effect. `assigned_session_id` is the id
/// pre-assigned at spawn time (RAL-288 Stage 1), if any, used only to warn on
/// a mismatch against Claude's own report.
fn process_event(
    event: &Value,
    state: &mut ParseState,
    workspace: &Workspace,
    assigned_session_id: Option<&str>,
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
        Some("assistant") => {
            // Claude's own text/tool-call activity -- the whole point of the
            // live tmux pane (RAL-102) is to let a human read this, so print
            // it plainly rather than folding it into a summary line. `text`
            // blocks are skipped here when the deltas above already streamed
            // this turn's text -- printing both would duplicate the whole
            // response in the pane.
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
                            let args = format_tool_input(&block["input"]);
                            eprintln!("[tool] {name}({args})");
                        }
                        _ => {}
                    }
                }
            }
            let usage = &event["message"]["usage"];
            let ti = usage["input_tokens"].as_i64().unwrap_or(0);
            let to = usage["output_tokens"].as_i64().unwrap_or(0);
            if ti > 0 || to > 0 {
                let model = event["message"]["model"].as_str().unwrap_or("");
                let live_cost = estimate_cost_usd(model, ti, to);
                crate::cartographer::emit(
                    "claude-code",
                    crate::cartographer::LIVE_USAGE_MESSAGE,
                    "info",
                    crate::cartographer::EventContext::default(),
                    serde_json::json!({"tokens_in": ti, "tokens_out": to, "cost_usd": live_cost}),
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
/// (RAL-102), mirroring the old `claude_code_backend.py`'s `_format_tool_input`.
fn format_tool_input(input: &Value) -> String {
    let Some(obj) = input.as_object() else {
        return String::new();
    };
    obj.iter()
        .map(|(key, value)| {
            let text = match value {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            let text = if text.chars().count() > 80 {
                let truncated: String = text.chars().take(80).collect();
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
/// for the *live* progress estimate emitted mid-run (the final `CellResult`
/// always uses the authoritative `total_cost_usd` from the terminal `result`
/// event). Deliberately conservative (falls back to the priciest tier) since
/// this feeds the RAL-161 cost-cap kill switch -- overestimating triggers an
/// early check rather than letting an actual overspend slip through.
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
        );
        // No assertion beyond "did not panic and left state untouched" --
        // this event carries no session/usage/result data of its own.
        assert_eq!(state, ParseState::default());
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
        );
        assert_eq!(state.agent_session_id.as_deref(), Some("sess-1"));
        assert_eq!(state.tokens_in, 12);
        assert_eq!(state.tokens_out, 34);
        assert!((state.cost_usd - 0.56).abs() < f64::EPSILON);
        assert_eq!(state.result_summary, "all done");
        assert!(state.saw_result);
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
        );
        assert_eq!(
            state.result_summary,
            "the background job finished, here is the real answer"
        );
        assert_eq!(state.tokens_in, 5);
        assert_eq!(state.tokens_out, 9);
        assert!((state.cost_usd - 0.20).abs() < f64::EPSILON);
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
}
