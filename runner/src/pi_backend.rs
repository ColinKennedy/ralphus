//! The `pi` `ModelBackend`. Drives `pi` in JSON mode non-interactively,
//! parsing its JSONL event stream on stdout.

use std::io::{BufRead as _, BufReader, Write as _};
use std::path::Path;
use std::process::{Child, Command, Stdio};

use serde_json::Value;

use crate::backend::{BackendError, BackendOutcome, ModelBackend, RunOptions};
use crate::cli_agent_common::{live_session_path, write_live_session_id};
use crate::shellcmd::{self, Env, SpawnArgs};
use crate::tools::Workspace;

const DEFAULT_PROGRAM: &str = "pi";
const SUMMARY_TAIL_CHARS: usize = 2000;

pub struct PiBackend {
    pub keep_temporary_files: bool,
    pub program_override: Option<String>,
}

impl ModelBackend for PiBackend {
    fn run(
        &self,
        prompt: &str,
        workspace: &Workspace,
        options: &RunOptions<'_>,
    ) -> Result<BackendOutcome, BackendError> {
        let program = self.program_override.clone().unwrap_or_else(|| {
            std::env::var("RALPHUS_PI_COMMAND").unwrap_or_else(|_| DEFAULT_PROGRAM.to_string())
        });
        let compound = crate::cli_agent_common::is_compound_command(&program);

        // Always send the cell's own prompt, even on resume -- matches
        // claude-code/codex (RAL-248 AC3): cross-cell session sharing needs
        // the new cell's task text, not a generic "continue".
        let args = build_args(prompt, options);

        let mut child = spawn(&program, compound, &args, workspace)
            .map_err(|e| BackendError(format!("could not spawn {program}: {e}")))?;

        print_header(options.model, workspace);
        let outcome = drive_json_events(&mut child, workspace)?;

        if !self.keep_temporary_files {
            let _ = std::fs::remove_file(live_session_path(workspace.root()));
        }

        Ok(outcome)
    }
}

fn build_args(prompt: &str, options: &RunOptions<'_>) -> Vec<String> {
    let mut args = vec![
        "--mode".to_string(),
        "json".to_string(),
        "--approve".to_string(),
    ];
    if let Some(id) = options.resume_agent_session_id {
        args.push("--session".to_string());
        args.push(id.to_string());
    }
    if let Some(model) = options.model {
        args.push("--model".to_string());
        args.push(model.to_string());
    }
    if let Some(sp) = options.append_system_prompt {
        args.push("--append-system-prompt".to_string());
        args.push(sp.to_string());
    }
    args.push("-p".to_string());
    args.push(prompt.to_string());
    args
}

fn spawn(
    program: &str,
    compound: bool,
    args: &[String],
    workspace: &Workspace,
) -> std::io::Result<Child> {
    if compound {
        let shell = shellcmd::resolve_shell(None);
        let _ = shellcmd::detect_parent_shell(&Env::from_process());
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

fn drive_json_events(
    child: &mut Child,
    workspace: &Workspace,
) -> Result<BackendOutcome, BackendError> {
    let stderr = child.stderr.take();
    let stderr_thread = stderr.map(|s| {
        std::thread::spawn(move || {
            let reader = BufReader::new(s);
            for line in reader.lines().map_while(Result::ok) {
                crate::cartographer::emit(
                    "pi",
                    &line,
                    "debug",
                    crate::cartographer::EventContext::default(),
                    Value::Null,
                );
            }
        })
    });

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| BackendError("pi: no stdout pipe".to_string()))?;
    let reader = BufReader::new(stdout);

    let mut state = ParseState::default();
    for line in reader.lines().map_while(Result::ok) {
        let Ok(event) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        process_event(&event, &mut state, workspace.root());
    }

    let status = child.wait().map_err(|e| BackendError(format!("pi: {e}")))?;
    if let Some(t) = stderr_thread {
        let _ = t.join();
    }

    if !state.saw_terminal_event {
        return Err(BackendError(format!(
            "pi exited ({status:?}) without a terminal message/agent event"
        )));
    }

    Ok(BackendOutcome {
        summary: tail(&state.latest_assistant_message, SUMMARY_TAIL_CHARS),
        tokens_in: state.tokens_in,
        tokens_out: state.tokens_out,
        cost_usd: state.cost_usd,
        agent_session_id: state.agent_session_id,
        abandoned_background_job: None,
    })
}

#[derive(Default)]
struct ParseState {
    agent_session_id: Option<String>,
    latest_assistant_message: String,
    tokens_in: i64,
    tokens_out: i64,
    cost_usd: f64,
    saw_terminal_event: bool,
    printed_text_delta: bool,
}

fn process_event(event: &Value, state: &mut ParseState, workspace_root: &Path) {
    match event["type"].as_str() {
        Some("session") => {
            if let Some(id) = event["id"].as_str() {
                state.agent_session_id = Some(id.to_string());
                write_live_session_id(workspace_root, id);
                crate::cartographer::emit(
                    "pi",
                    "session-id known",
                    "info",
                    crate::cartographer::EventContext::default(),
                    serde_json::json!({"agent_session_id": id}),
                );
            }
        }
        Some("message_update") => {
            let usage = &event["usage"];
            let tokens_in = usage["input"].as_i64().unwrap_or(0);
            let tokens_out = usage["output"].as_i64().unwrap_or(0);
            let cost_usd = usage["cost"]["total"].as_f64().unwrap_or(0.0);
            if tokens_in > 0 || tokens_out > 0 || cost_usd > 0.0 {
                state.tokens_in = tokens_in;
                state.tokens_out = tokens_out;
                state.cost_usd = cost_usd;
                crate::cartographer::emit(
                    "pi",
                    crate::cartographer::LIVE_USAGE_MESSAGE,
                    "info",
                    crate::cartographer::EventContext::default(),
                    serde_json::json!({
                        "tokens_in": tokens_in,
                        "tokens_out": tokens_out,
                        "cost_usd": cost_usd
                    }),
                );
            }
            if let Some(delta) = event["assistantMessageEvent"]["delta"].as_str() {
                if !delta.is_empty() {
                    print_delta(delta);
                    state.printed_text_delta = true;
                }
            }
        }
        Some("message_end") => {
            if event["message"]["role"].as_str() == Some("assistant") {
                let text = extract_message_text(&event["message"]);
                if !text.is_empty() {
                    state.latest_assistant_message = text;
                    state.saw_terminal_event = true;
                }
            }
            if state.printed_text_delta {
                finish_delta_line();
                state.printed_text_delta = false;
            }
        }
        Some("agent_end") => {
            if let Some(last) = event["messages"].as_array().and_then(|messages| {
                messages
                    .iter()
                    .rev()
                    .find(|m| m["role"].as_str() == Some("assistant"))
            }) {
                let text = extract_message_text(last);
                if !text.is_empty() {
                    state.latest_assistant_message = text;
                    state.saw_terminal_event = true;
                }
            }
            if state.printed_text_delta {
                finish_delta_line();
                state.printed_text_delta = false;
            }
        }
        _ => {}
    }
}

fn extract_message_text(message: &Value) -> String {
    extract_text(&message["content"]).trim().to_string()
}

fn extract_text(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Array(items) => items.iter().map(extract_text).collect::<Vec<_>>().join(""),
        Value::Object(map) => {
            if let Some(text) = map.get("text").and_then(Value::as_str) {
                return text.to_string();
            }
            if let Some(content) = map.get("content") {
                return extract_text(content);
            }
            String::new()
        }
        _ => String::new(),
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

#[allow(clippy::print_stdout)]
fn print_header(model: Option<&str>, workspace: &Workspace) {
    println!(
        "Pi · model={}\ncwd: {}\n",
        model.unwrap_or("default"),
        workspace.root().display()
    );
}

#[allow(clippy::print_stdout)]
fn print_delta(delta: &str) {
    print!("{delta}");
    let _ = std::io::stdout().flush();
}

#[allow(clippy::print_stdout)]
fn finish_delta_line() {
    println!();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_args_includes_resume_model_and_system_prompt() {
        let args = build_args(
            "do the work",
            &RunOptions {
                model: Some("openrouter/deepseek"),
                append_system_prompt: Some("be terse"),
                resume_agent_session_id: Some("sess-123"),
                assigned_agent_session_id: None,
                timeout_sec: None,
                tool_arg_truncate_chars: None,
            },
        );
        assert!(args.windows(2).any(|w| w == ["--session", "sess-123"]));
        assert!(
            args.windows(2)
                .any(|w| w == ["--model", "openrouter/deepseek"])
        );
        assert!(
            args.windows(2)
                .any(|w| w == ["--append-system-prompt", "be terse"])
        );
        assert_eq!(args[0], "--mode");
        assert!(args.contains(&"--approve".to_string()));
        assert!(args.contains(&"-p".to_string()));
        assert_eq!(args.last().map(String::as_str), Some("do the work"));
    }

    #[test]
    fn process_event_captures_session_usage_and_summary() {
        let mut state = ParseState::default();
        let root = Path::new(".");
        process_event(
            &serde_json::json!({"type":"session","id":"pi-session-1"}),
            &mut state,
            root,
        );
        process_event(
            &serde_json::json!({
                "type":"message_update",
                "usage":{"input":12,"output":34,"cost":{"total":0.56}},
                "assistantMessageEvent":{"type":"text_delta","delta":"hello"}
            }),
            &mut state,
            root,
        );
        process_event(
            &serde_json::json!({
                "type":"message_end",
                "message":{"role":"assistant","content":[{"type":"text","text":"hello world"}]}
            }),
            &mut state,
            root,
        );
        assert_eq!(state.agent_session_id.as_deref(), Some("pi-session-1"));
        assert_eq!(state.tokens_in, 12);
        assert_eq!(state.tokens_out, 34);
        assert!((state.cost_usd - 0.56).abs() < f64::EPSILON);
        assert_eq!(state.latest_assistant_message, "hello world");
        assert!(state.saw_terminal_event);
    }

    #[test]
    fn extract_text_handles_nested_content_blocks() {
        let message = serde_json::json!({
            "content":[
                {"type":"text","text":"alpha"},
                {"type":"container","content":[{"type":"text","text":" beta"}]}
            ]
        });
        assert_eq!(extract_message_text(&message), "alpha beta");
    }
}
