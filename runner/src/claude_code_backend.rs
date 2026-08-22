//! The `claude-code` `ModelBackend`, ported from
//! `cli/src/ralphus/runner/claude_code_backend.py`. Drives `claude -p`
//! headlessly, parsing its `stream-json` event stream on stdout.

use std::io::{BufRead as _, BufReader};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use serde_json::Value;

use crate::backend::{BackendError, BackendOutcome, ModelBackend, RunOptions};
use crate::cli_agent_common::{
    RESUME_CONTINUATION_PROMPT, live_session_path, write_live_session_id, write_prompt_file,
};
use crate::shellcmd::{self, Env, SpawnArgs};
use crate::tools::Workspace;

const DEFAULT_PROGRAM: &str = "claude";
const RESULT_SUMMARY_TAIL_CHARS: usize = 2000;

pub struct ClaudeCodeBackend {
    /// Keeps the prompt file and live-session side-channel file on disk
    /// after the run for debugging, instead of deleting them in cleanup --
    /// mirrors `config.daemon.keep_temporary_files`.
    pub keep_temporary_files: bool,
}

impl ModelBackend for ClaudeCodeBackend {
    fn run(
        &self,
        prompt: &str,
        workspace: &Workspace,
        options: &RunOptions<'_>,
    ) -> Result<BackendOutcome, BackendError> {
        let resuming = options.resume_agent_session_id.is_some();
        let effective_prompt = if resuming {
            RESUME_CONTINUATION_PROMPT
        } else {
            prompt
        };
        let prompt_file =
            write_prompt_file(effective_prompt).map_err(|e| BackendError(e.to_string()))?;

        let program =
            std::env::var("RALPHUS_CLAUDE_COMMAND").unwrap_or_else(|_| DEFAULT_PROGRAM.to_string());
        let compound = crate::cli_agent_common::is_compound_command(&program);

        let mut base_args: Vec<String> = vec![
            "-p".to_string(),
            format!("@{}", prompt_file.display()),
            "--dangerously-skip-permissions".to_string(),
            "--verbose".to_string(),
            "--output-format".to_string(),
            "stream-json".to_string(),
        ];
        if let Some(id) = options.resume_agent_session_id {
            base_args.push("--resume".to_string());
            base_args.push(id.to_string());
        }
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

        let outcome = drive_stream_json(&mut child, workspace, options.timeout_sec);

        if !self.keep_temporary_files {
            let _ = std::fs::remove_file(&prompt_file);
            if let Some(p) = &prompt_file_for_system {
                let _ = std::fs::remove_file(p);
            }
            let _ = std::fs::remove_file(live_session_path(workspace.root()));
        }

        outcome
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
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn(),
            SpawnArgs::Argv(argv) => {
                let mut cmd = Command::new(&argv[0]);
                cmd.args(&argv[1..]);
                cmd.current_dir(workspace.root())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()
            }
        }
    } else {
        Command::new(program)
            .args(args)
            .current_dir(workspace.root())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
    }
}

fn drive_stream_json(
    child: &mut Child,
    workspace: &Workspace,
    timeout_sec: Option<u64>,
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

    let mut agent_session_id: Option<String> = None;
    let mut result_summary = String::new();
    let mut tokens_in = 0i64;
    let mut tokens_out = 0i64;
    let mut cost_usd = 0.0f64;
    let mut saw_result = false;

    for line in reader.lines().map_while(Result::ok) {
        let Ok(event) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        match event["type"].as_str() {
            Some("system") if event["subtype"] == "init" => {
                if agent_session_id.is_none() {
                    if let Some(id) = event["session_id"].as_str() {
                        agent_session_id = Some(id.to_string());
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
                // Claude's own text/tool-call activity -- the whole point of
                // the live tmux pane (RAL-102) is to let a human read this,
                // so print it plainly rather than folding it into a summary
                // line.
                if let Some(blocks) = event["message"]["content"].as_array() {
                    for block in blocks {
                        match block["type"].as_str() {
                            Some("text") => {
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
                        "live usage",
                        "info",
                        crate::cartographer::EventContext::default(),
                        serde_json::json!({"tokens_in": ti, "tokens_out": to, "cost_usd": live_cost}),
                    );
                }
            }
            Some("user") => {
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
                saw_result = true;
                let usage = &event["usage"];
                tokens_in = usage["input_tokens"].as_i64().unwrap_or(0);
                tokens_out = usage["output_tokens"].as_i64().unwrap_or(0);
                cost_usd = event["total_cost_usd"]
                    .as_f64()
                    .or_else(|| event["cost_usd"].as_f64())
                    .unwrap_or(0.0);
                let text = event["result"].as_str().unwrap_or_default();
                result_summary = tail(text, RESULT_SUMMARY_TAIL_CHARS);
            }
            _ => {}
        }
    }

    let status = wait_for_child(child, timeout_sec)
        .map_err(|e| BackendError(format!("claude-code: {e}")))?;
    if let Some(t) = stderr_thread {
        let _ = t.join();
    }

    if !saw_result {
        return Err(BackendError(format!(
            "claude-code exited ({status:?}) without a terminal result event"
        )));
    }

    Ok(BackendOutcome {
        summary: result_summary,
        tokens_in,
        tokens_out,
        cost_usd,
        agent_session_id,
    })
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
/// for the *live* progress estimate emitted mid-run (the final `SessionResult`
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

    #[test]
    fn estimate_cost_usd_scales_with_tokens() {
        let cheap = estimate_cost_usd("claude-haiku-4-5", 1_000_000, 0);
        let pricey = estimate_cost_usd("claude-opus-5", 1_000_000, 0);
        assert!(pricey > cheap);
    }
}
