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
        apply_context_settings(
            options.model,
            options.maximum_context,
            options.auto_compact_threshold,
        )?;

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

    fn supports_context_limits(&self) -> bool {
        true
    }
}

/// RAL-304 (revised): `pi` has no CLI flag or env var for a context-window
/// limit or an auto-compact threshold. Unlike the first RAL-304 pass (which
/// invented flat `settings.json` keys `pi` never read -- confirmed dead
/// against `pi`'s actual `compaction.ts` and `custom-provider.md`/`models.md`
/// docs), the real mechanism is two separate files under
/// `$PI_CODING_AGENT_DIR`:
///
/// - `models.json`: `providers.<provider>.modelOverrides.<model-id>.contextWindow`
///   overrides the model catalog's registered window for one model,
///   composed on top of pi's built-in providers (`docs/models.md`).
///   Targeting one model requires knowing its provider, so this only
///   applies when `RunOptions::model` is `"<provider>/<model-id>"` --
///   there is no daemon-side provider registry to resolve a bare model
///   name against.
/// - `settings.json`: `compaction.reserveTokens` is the real compaction
///   trigger (`shouldCompact()`: `contextTokens > contextWindow -
///   reserveTokens`, `compaction.ts`) -- an inverted, buffer-from-the-
///   ceiling shape, not `auto_compact_threshold`'s absolute crossing point.
///   Converting one into the other needs the ceiling itself, so
///   `auto_compact_threshold` is only accepted alongside `maximum_context`.
///
/// A no-op when neither field is set, so a `pi` cell that never touches
/// them never requires `PI_CODING_AGENT_DIR` at all.
fn apply_context_settings(
    model: Option<&str>,
    maximum_context: Option<u64>,
    auto_compact_threshold: Option<u64>,
) -> Result<(), BackendError> {
    if maximum_context.is_none() && auto_compact_threshold.is_none() {
        return Ok(());
    }
    let dir = std::env::var("PI_CODING_AGENT_DIR").map_err(|_| {
        BackendError(
            "pi: maximum_context/auto_compact_threshold require PI_CODING_AGENT_DIR to be set"
                .to_string(),
        )
    })?;
    apply_context_settings_in(
        Path::new(&dir),
        model,
        maximum_context,
        auto_compact_threshold,
    )
}

/// Hermetic core of [`apply_context_settings`], split out the same way
/// `config.rs::load`/`load_with` split env-reading from the logic -- this
/// workspace forbids `unsafe_code`, so a test can't use `std::env::set_var`
/// to exercise the `PI_CODING_AGENT_DIR` lookup in-process.
///
/// Validates both fields (provider-qualified model for `maximum_context`;
/// `auto_compact_threshold < maximum_context` when both are set) before
/// writing anything, so a rejected cell never leaves one file updated and
/// the other not.
fn apply_context_settings_in(
    dir: &Path,
    model: Option<&str>,
    maximum_context: Option<u64>,
    auto_compact_threshold: Option<u64>,
) -> Result<(), BackendError> {
    let provider_model = maximum_context
        .map(|_| {
            split_provider_model(model).ok_or_else(|| {
                BackendError(
                    "pi: maximum_context requires the cell's `model` to be \
                     \"<provider>/<model-id>\" so the override can target pi's models.json -- \
                     there is no default provider to fall back to"
                        .to_string(),
                )
            })
        })
        .transpose()?;
    let reserve_tokens = auto_compact_threshold
        .map(|threshold| {
            let max = maximum_context.ok_or_else(|| {
                BackendError(
                    "pi: auto_compact_threshold requires maximum_context to also be set -- pi \
                     has no absolute compaction-trigger setting, only a reserve-token buffer \
                     computed against the context-window ceiling"
                        .to_string(),
                )
            })?;
            max.checked_sub(threshold).ok_or_else(|| {
                BackendError(format!(
                    "pi: auto_compact_threshold ({threshold}) must be less than maximum_context ({max})"
                ))
            })
        })
        .transpose()?;

    if let (Some(v), Some((provider, model_id))) = (maximum_context, provider_model) {
        write_model_context_window(dir, provider, model_id, v)?;
    }
    if let Some(v) = reserve_tokens {
        write_compaction_reserve_tokens(dir, v)?;
    }
    Ok(())
}

/// Splits a `"<provider>/<model-id>"` string into its two halves, requiring
/// both to be non-empty. `None` for a bare model name (no provider prefix)
/// or no model at all.
fn split_provider_model(model: Option<&str>) -> Option<(&str, &str)> {
    let (provider, id) = model?.split_once('/')?;
    (!provider.is_empty() && !id.is_empty()).then_some((provider, id))
}

/// Merges `providers.<provider>.modelOverrides.<model_id>.contextWindow`
/// into `dir`'s `models.json`, preserving every other key at every level
/// (other providers, other models' overrides, and any other fields already
/// set on this same model's override, e.g. a user's own `maxTokens`/`cost`).
fn write_model_context_window(
    dir: &Path,
    provider: &str,
    model_id: &str,
    context_window: u64,
) -> Result<(), BackendError> {
    let path = dir.join("models.json");
    let mut root = read_json_object(&path)?;
    let model_entry = nested_object(
        &mut root,
        &path,
        &["providers", provider, "modelOverrides", model_id],
    )?;
    model_entry.insert("contextWindow".to_string(), Value::from(context_window));
    write_json_object(&path, &root)
}

/// Merges `compaction.reserveTokens` into `dir`'s `settings.json`,
/// preserving `compaction.keepRecentTokens`/`compaction.enabled` and every
/// other top-level key already there.
fn write_compaction_reserve_tokens(dir: &Path, reserve_tokens: u64) -> Result<(), BackendError> {
    let path = dir.join("settings.json");
    let mut root = read_json_object(&path)?;
    let compaction = nested_object(&mut root, &path, &["compaction"])?;
    compaction.insert("reserveTokens".to_string(), Value::from(reserve_tokens));
    write_json_object(&path, &root)
}

/// Reads `path` as a JSON object, treating a missing file as an empty one.
fn read_json_object(path: &Path) -> Result<serde_json::Map<String, Value>, BackendError> {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            let value: Value = serde_json::from_str(&text).map_err(|e| {
                BackendError(format!("pi: could not parse {}: {e}", path.display()))
            })?;
            value.as_object().cloned().ok_or_else(|| {
                BackendError(format!(
                    "pi: {} does not contain a JSON object at its top level",
                    path.display()
                ))
            })
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Default::default()),
        Err(e) => Err(BackendError(format!(
            "pi: could not read {}: {e}",
            path.display()
        ))),
    }
}

/// Walks/creates nested JSON objects along `keys`, returning a mutable
/// handle to the innermost one. Errors if an intermediate key already holds
/// a non-object value, naming the offending path.
fn nested_object<'a>(
    root: &'a mut serde_json::Map<String, Value>,
    path: &Path,
    keys: &[&str],
) -> Result<&'a mut serde_json::Map<String, Value>, BackendError> {
    let mut current = root;
    let mut walked = Vec::new();
    for key in keys {
        walked.push(*key);
        current = current
            .entry((*key).to_string())
            .or_insert_with(|| Value::Object(Default::default()))
            .as_object_mut()
            .ok_or_else(|| {
                BackendError(format!(
                    "pi: {} \"{}\" is not an object",
                    path.display(),
                    walked.join(".")
                ))
            })?;
    }
    Ok(current)
}

fn write_json_object(
    path: &Path,
    obj: &serde_json::Map<String, Value>,
) -> Result<(), BackendError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| BackendError(format!("pi: could not create {}: {e}", parent.display())))?;
    }
    let text = serde_json::to_string_pretty(obj)
        .map_err(|e| BackendError(format!("pi: could not serialize {}: {e}", path.display())))?;
    std::fs::write(path, text)
        .map_err(|e| BackendError(format!("pi: could not write {}: {e}", path.display())))
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
        Some("compaction_start") => {
            // Pi compacts a session's own conversation history once it nears
            // its context limit (or on `/compact`/RPC) -- confirmed by
            // reading `AgentSession`'s `_runAutoCompaction`/`compact()` in
            // the installed `@earendil-works/pi-coding-agent` package
            // (`dist/core/agent-session.js`): it `_emit`s `compaction_start`
            // then `compaction_end`, and `dist/modes/json-event.js`'s
            // `toJsonEvent` passes both straight through to `--mode json`
            // stdout unchanged (only `message_update` is reshaped). Surface
            // it the same way `claude_code_backend.rs` surfaces
            // `compact_boundary`, so this isn't silently swallowed by the
            // `_ => {}` arm below.
            let reason = event["reason"].as_str().unwrap_or("unknown");
            eprintln!("[compact] conversation history compaction starting (reason={reason})");
            crate::cartographer::emit(
                "pi",
                "conversation history compaction starting",
                "info",
                crate::cartographer::EventContext::default(),
                serde_json::json!({"reason": reason}),
            );
        }
        Some("compaction_end") => {
            // `result` is only present on success (`CompactionResult`:
            // `tokensBefore`/`estimatedTokensAfter`); a failed or aborted
            // compaction carries `errorMessage` instead and no `result`.
            let reason = event["reason"].as_str().unwrap_or("unknown");
            let aborted = event["aborted"].as_bool().unwrap_or(false);
            let tokens_before = event["result"]["tokensBefore"].as_i64();
            let estimated_tokens_after = event["result"]["estimatedTokensAfter"].as_i64();
            let error_message = event["errorMessage"].as_str();
            eprintln!(
                "[compact] conversation history compacted (reason={reason}, aborted={aborted}, tokensBefore={tokens_before:?}, estimatedTokensAfter={estimated_tokens_after:?})"
            );
            crate::cartographer::emit(
                "pi",
                "conversation history compacted",
                "warning",
                crate::cartographer::EventContext::default(),
                serde_json::json!({
                    "reason": reason,
                    "aborted": aborted,
                    "tokens_before": tokens_before,
                    "estimated_tokens_after": estimated_tokens_after,
                    "error_message": error_message,
                }),
            );
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
                ..Default::default()
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
    fn process_event_handles_compaction_events_without_panicking_or_mutating_usage() {
        // Compaction carries no session/usage/result data relevant to
        // ParseState -- this just confirms both events are read (not
        // swallowed by the catch-all) and leave tokens/cost untouched.
        let mut state = ParseState::default();
        let root = Path::new(".");
        process_event(
            &serde_json::json!({"type":"compaction_start","reason":"threshold"}),
            &mut state,
            root,
        );
        process_event(
            &serde_json::json!({
                "type":"compaction_end",
                "reason":"threshold",
                "aborted":false,
                "willRetry":false,
                "result":{"tokensBefore":164975,"estimatedTokensAfter":20000}
            }),
            &mut state,
            root,
        );
        assert_eq!(state.tokens_in, 0);
        assert_eq!(state.tokens_out, 0);
        assert!(!state.saw_terminal_event);
    }

    #[test]
    fn process_event_handles_failed_compaction_end_without_panicking() {
        let mut state = ParseState::default();
        let root = Path::new(".");
        process_event(
            &serde_json::json!({
                "type":"compaction_end",
                "reason":"overflow",
                "aborted":false,
                "willRetry":false,
                "errorMessage":"Auto-compaction failed: summarization error"
            }),
            &mut state,
            root,
        );
        assert!(!state.saw_terminal_event);
    }

    fn temp_settings_dir(label: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ralphus-pi-settings-test-{label}-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn apply_context_settings_writes_models_json_context_window() {
        let dir = temp_settings_dir("models-create");
        apply_context_settings_in(&dir, Some("openrouter/deepseek"), Some(100_000), None).unwrap();

        let text = std::fs::read_to_string(dir.join("models.json")).unwrap();
        let json: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(
            json["providers"]["openrouter"]["modelOverrides"]["deepseek"]["contextWindow"],
            serde_json::json!(100_000)
        );
        assert!(!dir.join("settings.json").exists());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn apply_context_settings_writes_compaction_reserve_tokens() {
        let dir = temp_settings_dir("reserve-create");
        apply_context_settings_in(
            &dir,
            Some("openrouter/deepseek"),
            Some(100_000),
            Some(80_000),
        )
        .unwrap();

        let text = std::fs::read_to_string(dir.join("settings.json")).unwrap();
        let json: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(
            json["compaction"]["reserveTokens"],
            serde_json::json!(20_000)
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn apply_context_settings_merges_onto_existing_keys() {
        let dir = temp_settings_dir("merge");
        std::fs::write(
            dir.join("models.json"),
            r#"{"providers":{"openrouter":{"modelOverrides":{"deepseek":{"maxTokens":4096}}}}}"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("settings.json"),
            r#"{"otherSetting":true,"compaction":{"keepRecentTokens":20000}}"#,
        )
        .unwrap();

        apply_context_settings_in(
            &dir,
            Some("openrouter/deepseek"),
            Some(100_000),
            Some(80_000),
        )
        .unwrap();

        let models: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("models.json")).unwrap())
                .unwrap();
        assert_eq!(
            models["providers"]["openrouter"]["modelOverrides"]["deepseek"]["maxTokens"],
            serde_json::json!(4096)
        );
        assert_eq!(
            models["providers"]["openrouter"]["modelOverrides"]["deepseek"]["contextWindow"],
            serde_json::json!(100_000)
        );

        let settings: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("settings.json")).unwrap())
                .unwrap();
        assert_eq!(settings["otherSetting"], serde_json::json!(true));
        assert_eq!(
            settings["compaction"]["keepRecentTokens"],
            serde_json::json!(20_000)
        );
        assert_eq!(
            settings["compaction"]["reserveTokens"],
            serde_json::json!(20_000)
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn apply_context_settings_errors_without_provider_qualified_model() {
        let dir = temp_settings_dir("no-provider");
        let err =
            apply_context_settings_in(&dir, Some("deepseek"), Some(100_000), None).unwrap_err();
        assert!(err.0.contains("provider"), "unexpected error: {}", err.0);
        assert!(!dir.join("models.json").exists());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn apply_context_settings_errors_when_threshold_set_without_maximum_context() {
        let dir = temp_settings_dir("no-max");
        let err = apply_context_settings_in(&dir, Some("openrouter/deepseek"), None, Some(80_000))
            .unwrap_err();
        assert!(
            err.0
                .contains("auto_compact_threshold requires maximum_context"),
            "unexpected error: {}",
            err.0
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn apply_context_settings_errors_when_threshold_exceeds_maximum() {
        let dir = temp_settings_dir("bad-order");
        let err = apply_context_settings_in(
            &dir,
            Some("openrouter/deepseek"),
            Some(80_000),
            Some(100_000),
        )
        .unwrap_err();
        assert!(
            err.0.contains("must be less than maximum_context"),
            "unexpected error: {}",
            err.0
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn apply_context_settings_is_a_noop_when_neither_field_is_set() {
        // Deliberately doesn't touch PI_CODING_AGENT_DIR -- a `pi` cell that
        // never sets either field must never require it.
        assert!(apply_context_settings(None, None, None).is_ok());
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
