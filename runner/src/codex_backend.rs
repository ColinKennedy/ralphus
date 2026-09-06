//! The `codex` `ModelBackend`, ported from
//! `cli/src/ralphus/runner/codex_backend.py`. Drives `codex exec`
//! non-interactively, prompt piped over stdin, parsing its JSONL
//! `ThreadEvent` stream on stdout.

use std::io::{BufRead as _, BufReader, Write as _};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use serde_json::Value;

use crate::backend::{BackendError, BackendOutcome, ModelBackend, RunOptions};
use crate::cli_agent_common::{live_session_path, write_live_session_id};
use crate::shellcmd::{self, Env, SpawnArgs};
use crate::tools::Workspace;

const DEFAULT_PROGRAM: &str = "codex";

pub struct CodexBackend {
    pub keep_temporary_files: bool,
    pub program_override: Option<String>,
}

impl ModelBackend for CodexBackend {
    fn run(
        &self,
        prompt: &str,
        workspace: &Workspace,
        options: &RunOptions<'_>,
    ) -> Result<BackendOutcome, BackendError> {
        let program = self.program_override.clone().unwrap_or_else(|| {
            std::env::var("RALPHUS_CODEX_COMMAND").unwrap_or_else(|_| DEFAULT_PROGRAM.to_string())
        });
        let compound = crate::cli_agent_common::is_compound_command(&program);

        // `-c developer_instructions=...` (and the RAL-304 context-limit
        // overrides from `context_limit_args`) must precede `exec` -- Codex's
        // own root-level arg parser is the only one that recognizes `-c`;
        // `exec`'s subcommand struct deliberately does not re-declare it.
        let mut args: Vec<String> = Vec::new();
        if let Some(sp) = options.append_system_prompt {
            args.push("-c".to_string());
            args.push(format!("developer_instructions={sp}"));
        }
        args.extend(context_limit_args(options));
        args.push("exec".to_string());
        args.push("--json".to_string());
        args.push("--dangerously-bypass-approvals-and-sandbox".to_string());
        args.push("--skip-git-repo-check".to_string());
        args.push("-C".to_string());
        args.push(workspace.root().display().to_string());
        if let Some(model) = options.model {
            args.push("-m".to_string());
            args.push(model.to_string());
        }
        if let Some(id) = options.resume_agent_session_id {
            args.push("resume".to_string());
            args.push(id.to_string());
        }
        args.push("-".to_string());

        let codex_home = isolated_codex_home(options, workspace);
        let mut child = spawn(&program, compound, &args, workspace, codex_home.as_deref())
            .map_err(|e| BackendError(format!("could not spawn {program}: {e}")))?;

        // Human-readable header for the live tmux pane (RAL-102) -- everything
        // below this is Codex's own text/tool activity, not runner logging.
        print_header(options.model, workspace);

        // Stdin is written on its own thread, same reason as the daemon's
        // background stderr drain elsewhere: writing then closing stdin
        // could otherwise deadlock against Codex's own stdout/stderr filling
        // up before it starts reading.
        if let Some(mut stdin) = child.stdin.take() {
            let prompt = prompt.to_string();
            std::thread::spawn(move || {
                let _ = stdin.write_all(prompt.as_bytes());
            });
        }

        let thrash_thresholds = crate::thrash::ThrashThresholds {
            max_compactions: options
                .thrash_max_compactions
                .unwrap_or(crate::thrash::DEFAULT_MAX_COMPACTIONS),
            min_turn_gap: options
                .thrash_min_turn_gap
                .unwrap_or(crate::thrash::DEFAULT_MIN_TURN_GAP),
        };
        let outcome = drive_thread_events(
            &mut child,
            workspace,
            options.timeout_sec,
            thrash_thresholds,
        );

        if !self.keep_temporary_files {
            let _ = std::fs::remove_file(live_session_path(workspace.root()));
        }

        outcome
    }

    fn supports_maximum_context(&self) -> bool {
        true
    }

    fn supports_auto_compact_threshold(&self) -> bool {
        true
    }

    fn supports_tool_output_max_tokens(&self) -> bool {
        true
    }
}

/// RAL-304/RAL-333: the `-c key=value` argument pairs that deliver
/// `RunOptions::maximum_context`/`RunOptions::auto_compact_threshold`/
/// `RunOptions::tool_output_max_tokens` to `codex` -- there is no dedicated
/// flag for any of these, only config-override keys (mirrors
/// `developer_instructions`'s precedent for `system_prompt`). Must be spliced
/// into the arg list before `exec` -- see the caller's comment.
fn context_limit_args(options: &RunOptions<'_>) -> Vec<String> {
    let mut args = Vec::new();
    if let Some(v) = options.maximum_context {
        args.push("-c".to_string());
        args.push(format!("model_context_window={v}"));
    }
    if let Some(v) = options.auto_compact_threshold {
        args.push("-c".to_string());
        args.push(format!("model_auto_compact_token_limit={v}"));
    }
    if let Some(v) = options.tool_output_max_tokens {
        args.push("-c".to_string());
        args.push(format!("tool_output_token_limit={v}"));
    }
    args
}

/// RAL-336: `CODEX_HOME` is Codex's single config-directory lever -- it
/// can't cleanly separate personal settings from personal memory, so
/// isolation engages whenever either opt-in is off (favoring
/// over-isolation). `None` leaves the child's inherited `CODEX_HOME` (if
/// any) untouched, preserving today's behavior when both are allowed.
fn isolated_codex_home(
    options: &RunOptions<'_>,
    workspace: &Workspace,
) -> Option<std::path::PathBuf> {
    if options.allow_personal_settings && options.allow_personal_memory {
        return None;
    }
    let real_dir = std::env::var_os("CODEX_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| crate::agent_isolation::home_dir().map(|h| h.join(".codex")));
    let isolated = crate::agent_isolation::isolated_config_dir(workspace.root(), "codex");
    // Preserved unconditionally: isolating personal config/memory is not the
    // same decision as forcing a re-login.
    crate::agent_isolation::preserve_auth_file(real_dir.as_deref(), &isolated, "auth.json");
    if options.allow_personal_settings {
        // Settings opted back in but memory did not (the single lever can't
        // separate the two) -- copy the settings file into the isolated dir
        // so personal settings still take effect there.
        crate::agent_isolation::preserve_auth_file(real_dir.as_deref(), &isolated, "config.toml");
    }
    Some(isolated)
}

fn spawn(
    program: &str,
    compound: bool,
    args: &[String],
    workspace: &Workspace,
    codex_home: Option<&std::path::Path>,
) -> std::io::Result<Child> {
    if compound {
        let shell = shellcmd::resolve_shell(None);
        let _ = shellcmd::detect_parent_shell(&Env::from_process());
        let line = shellcmd::build_compound_command_line(&shell, program, args);
        match shellcmd::shell_spawn_args(&shell, &line) {
            SpawnArgs::RawShellLine(raw) => {
                let mut cmd = Command::new("cmd");
                cmd.arg("/C")
                    .arg(raw)
                    .current_dir(workspace.root())
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped());
                apply_codex_home_env(&mut cmd, codex_home);
                cmd.spawn()
            }
            SpawnArgs::Argv(argv) => {
                let mut cmd = Command::new(&argv[0]);
                cmd.args(&argv[1..]);
                cmd.current_dir(workspace.root())
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped());
                apply_codex_home_env(&mut cmd, codex_home);
                cmd.spawn()
            }
        }
    } else {
        let mut cmd = Command::new(program);
        cmd.args(args)
            .current_dir(workspace.root())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        apply_codex_home_env(&mut cmd, codex_home);
        cmd.spawn()
    }
}

/// Sets `CODEX_HOME` on `cmd` when RAL-336 isolation resolved a redirect
/// directory -- a no-op otherwise.
fn apply_codex_home_env(cmd: &mut Command, codex_home: Option<&std::path::Path>) {
    if let Some(dir) = codex_home {
        cmd.env("CODEX_HOME", dir);
    }
}

/// RAL-339: floor below which a large-percentage `input_tokens` drop between
/// two turns is not treated as an inferred compaction -- a tiny early turn
/// (e.g. `5` tokens then `0`) would trivially satisfy a 10x-drop test on its
/// own, so the previous turn's `input_tokens` must clear this floor first.
const CODEX_COMPACTION_INFERENCE_MIN_TOKENS: i64 = 1000;

/// RAL-339: whether a turn's own `input_tokens` looks like the result of a
/// mid-run compaction versus the previous turn's `input_tokens` -- an
/// order-of-magnitude (>= 10x) drop, gated by
/// [`CODEX_COMPACTION_INFERENCE_MIN_TOKENS`] so a tiny early turn can't
/// trivially satisfy the ratio. `None` (no previous turn yet) never infers a
/// compaction -- there is nothing to compare against.
fn codex_infers_compaction(
    previous_turn_input_tokens: Option<i64>,
    turn_input_tokens: i64,
) -> bool {
    previous_turn_input_tokens.is_some_and(|prev| {
        prev >= CODEX_COMPACTION_INFERENCE_MIN_TOKENS && turn_input_tokens * 10 <= prev
    })
}

fn drive_thread_events(
    child: &mut Child,
    workspace: &Workspace,
    timeout_sec: Option<u64>,
    thrash_thresholds: crate::thrash::ThrashThresholds,
) -> Result<BackendOutcome, BackendError> {
    let stderr = child.stderr.take();
    let stderr_thread = stderr.map(|s| {
        std::thread::spawn(move || {
            let reader = BufReader::new(s);
            for line in reader.lines().map_while(Result::ok) {
                crate::cartographer::emit(
                    "codex",
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
        .ok_or_else(|| BackendError("codex: no stdout pipe".to_string()))?;
    let reader = BufReader::new(stdout);

    let mut agent_session_id: Option<String> = None;
    let mut latest_agent_message = String::new();
    let mut tokens_in = 0i64;
    let mut tokens_out = 0i64;
    // Codex exposes no cache-write tier at all (see the `turn.completed` arm
    // below) -- always 0, but kept as a named field for parity with the
    // other backends and to make that absence explicit rather than silent.
    let cache_creation_tokens = 0i64;
    let mut cache_read_tokens = 0i64;
    let mut turn_error: Option<String> = None;
    let mut saw_turn = false;
    let mut previous_turn_input_tokens: Option<i64> = None;
    let mut thrash = crate::thrash::ThrashTracker::new(thrash_thresholds);
    let mut compaction_thrash: Option<crate::thrash::ThrashDetail> = None;

    for line in reader.lines().map_while(Result::ok) {
        let Ok(event) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        match event["type"].as_str() {
            Some("thread.started") => {
                if let Some(id) = event["thread_id"].as_str() {
                    agent_session_id = Some(id.to_string());
                    write_live_session_id(workspace.root(), id);
                    crate::cartographer::emit(
                        "codex",
                        "thread-id known",
                        "info",
                        crate::cartographer::EventContext::default(),
                        serde_json::json!({"agent_session_id": id}),
                    );
                }
            }
            Some("item.completed") => {
                let item = &event["item"];
                match item["type"].as_str() {
                    Some("agent_message") => {
                        // The model's own reply -- the whole point of the
                        // live tmux pane is to let a human read this.
                        if let Some(text) = item["text"].as_str() {
                            if !text.is_empty() {
                                latest_agent_message = text.to_string();
                                print_line(text);
                            }
                        }
                    }
                    Some("command_execution") => {
                        let command = item["command"].as_str().unwrap_or("");
                        let status = item["status"].as_str().unwrap_or("");
                        eprintln!("[tool] exec({command:?}) status={status}");
                    }
                    Some("error") => {
                        let message = item["message"].as_str().unwrap_or("");
                        eprintln!("[error] {message}");
                    }
                    _ => {}
                }
            }
            Some("turn.completed") => {
                saw_turn = true;
                // Raw (pre-split) input_tokens: compaction inference below
                // compares this turn-over-turn, which needs the same total
                // Codex itself measures the auto-compact threshold against.
                let turn_input_tokens = event["usage"]["input_tokens"].as_i64().unwrap_or(0);
                let (turn_uncached_input_tokens, turn_cache_read_tokens, turn_output_tokens) =
                    split_turn_usage(&event["usage"]);
                // Accumulate (not overwrite) -- a multi-turn `codex exec` run
                // reports per-turn usage; summing is what makes a long
                // session's token count reflect total spend (RAL-187).
                tokens_in += turn_uncached_input_tokens;
                tokens_out += turn_output_tokens;
                cache_read_tokens += turn_cache_read_tokens;

                // RAL-339: `codex exec --json` has no compaction-boundary
                // event of its own (see the comment on the catch-all arm
                // below) -- infer one, best-effort, from an order-of-
                // magnitude drop in this turn's own `input_tokens` versus the
                // previous turn's, which is what a resumed thread crossing
                // `model_auto_compact_token_limit` looks like from the
                // outside. `record_assistant_turn` and `record_compaction`
                // are mutually exclusive per turn -- a turn is either the
                // (inferred) compaction boundary or a normal turn tick, never
                // both, mirroring claude-code/pi's distinct event types.
                if codex_infers_compaction(previous_turn_input_tokens, turn_input_tokens) {
                    eprintln!(
                        "[compact] conversation history compaction inferred (input_tokens {previous_turn_input_tokens:?} -> {turn_input_tokens})"
                    );
                    crate::cartographer::emit(
                        "codex",
                        "conversation history compaction inferred",
                        "warning",
                        crate::cartographer::EventContext::default(),
                        serde_json::json!({
                            "previous_input_tokens": previous_turn_input_tokens,
                            "input_tokens": turn_input_tokens,
                        }),
                    );
                    if let Some(detail) = thrash.record_compaction() {
                        eprintln!(
                            "[thrash] autocompaction thrashing detected: {} compactions, \
                             most recently {} turn(s) after the previous one",
                            detail.compaction_count, detail.turns_since_previous_compaction
                        );
                        crate::thrash::emit_thrash_event("codex", &detail);
                        compaction_thrash = Some(detail);
                    }
                } else {
                    thrash.record_assistant_turn();
                }
                previous_turn_input_tokens = Some(turn_input_tokens);
            }
            Some("turn.failed") => {
                turn_error = event["error"]["message"]
                    .as_str()
                    .map(str::to_string)
                    .or_else(|| Some("codex turn failed".to_string()));
            }
            Some("error") => {
                turn_error = event["message"].as_str().map(str::to_string);
            }
            // No arm observes a real compaction-boundary event here: `codex
            // exec --json`'s `ThreadEvent` stream (`thread.started`/
            // `turn.started`/`item.completed`/`turn.completed`/
            // `turn.failed`/`error`, matched above) never emits one.
            // Confirmed empirically -- a `codex exec` thread resumed with
            // `model_auto_compact_token_limit` already exceeded shows the
            // next turn's `input_tokens` drop by an order of magnitude
            // (context was compacted) with no corresponding event on stdout
            // -- which is exactly what `turn.completed`'s own arm above
            // infers a compaction from, best-effort. The richer `codex
            // app-server` JSON-RPC protocol does carry compaction-related
            // state (its binary exposes `ContextCompacted`/
            // `compaction_request`/`compaction_response`), but that's a
            // persistent per-session RPC connection, not the one-shot `exec`
            // subprocess this backend spawns per cell -- consuming it would
            // need a different spawn model entirely, not just a new match
            // arm. Contrast `pi_backend.rs::process_event`'s
            // `compaction_start`/`compaction_end` arms, which pi's
            // `--mode json` does surface directly.
            _ => {}
        }
        // RAL-339: stop reading at the (inferred) compaction boundary itself
        // -- a safe stop point -- instead of waiting for codex's own stdout
        // to reach EOF, which could be arbitrarily many further (thrashing)
        // turns away.
        if compaction_thrash.is_some() {
            break;
        }
    }

    if let Some(detail) = compaction_thrash {
        // RAL-339: the run is thrashing -- kill the child now rather than
        // wait for a natural exit that may be arbitrarily far off, then fail
        // the cell with whatever was captured live so far.
        let _ = child.kill();
        let _ = child.wait();
        if let Some(t) = stderr_thread {
            let _ = t.join();
        }
        return Ok(BackendOutcome {
            summary: latest_agent_message,
            tokens_in,
            tokens_out,
            cache_creation_tokens,
            cache_read_tokens,
            cost_usd: 0.0,
            agent_session_id,
            abandoned_background_job: None,
            compaction_thrash: Some(detail),
        });
    }

    let status =
        wait_for_child(child, timeout_sec).map_err(|e| BackendError(format!("codex: {e}")))?;
    if let Some(t) = stderr_thread {
        let _ = t.join();
    }

    if let Some(err) = turn_error {
        return Err(BackendError(err));
    }
    if !saw_turn {
        return Err(BackendError(format!(
            "codex exited ({status:?}) without a completed turn"
        )));
    }

    Ok(BackendOutcome {
        summary: latest_agent_message,
        tokens_in,
        tokens_out,
        cache_creation_tokens,
        cache_read_tokens,
        // No cost field at all in Codex's own output -- the board renders
        // this as "N/A" rather than "$0.0000" so it never misreads as free
        // (RAL-187).
        cost_usd: 0.0,
        agent_session_id,
        abandoned_background_job: None,
        compaction_thrash: None,
    })
}

/// Splits a `turn.completed` event's `usage` object into
/// `(uncached_input_tokens, cache_read_tokens, output_tokens)`.
///
/// RAL-326: Codex's `TokenUsage` only exposes `input_tokens`,
/// `cached_input_tokens`, and `output_tokens` -- there is no cache-*write*
/// field at all (confirmed against upstream: openai/codex#32479 tracks
/// adding one, meaning today's CLI has none to read). Unlike claude-code's
/// `cache_creation_input_tokens`/`cache_read_input_tokens`, which are
/// additional to `input_tokens`, Codex's `cached_input_tokens` is a SUBSET
/// already counted inside `input_tokens` (same convention as the OpenAI
/// Responses API it wraps: `input_tokens_details.cached_tokens` partitions
/// `input_tokens`, it doesn't add to it). So it must be subtracted out here
/// to keep `tokens_in` meaning "uncached input" the same way it does for
/// every other backend.
fn split_turn_usage(usage: &Value) -> (i64, i64, i64) {
    let input = usage["input_tokens"].as_i64().unwrap_or(0);
    let cached = usage["cached_input_tokens"].as_i64().unwrap_or(0);
    let output = usage["output_tokens"].as_i64().unwrap_or(0);
    ((input - cached).max(0), cached, output)
}

/// Human-readable header for the live tmux pane (RAL-102) -- printed once,
/// before Codex's own streamed text/tool activity below it.
#[allow(clippy::print_stdout)] // intentional: this process runs tmux-wrapped (see `daemon/src/runner.rs::run_via_tmux`), so stdout is the live pane, not the daemon<->runner JSON channel (that contract is file-based here -- see `main.rs`'s `--result-file` handling)
fn print_header(model: Option<&str>, workspace: &Workspace) {
    println!(
        "Codex · model={}
cwd: {}
",
        model.unwrap_or("default"),
        workspace.root().display()
    );
}

/// Prints one of Codex's own agent-message replies to the live tmux pane --
/// the whole point of RAL-102 is to let a human read this.
#[allow(clippy::print_stdout)] // intentional: see `print_header`
fn print_line(text: &str) {
    println!("{text}");
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

#[cfg(test)]
mod tests {
    use super::*;

    /// RAL-326: `cached_input_tokens` is a subset of `input_tokens`, not
    /// additional to it -- the field must be subtracted out, not summed in,
    /// or the board double-counts it as both "input" and "cache read".
    #[test]
    fn split_turn_usage_subtracts_cached_tokens_out_of_input() {
        let (uncached, cached, output) = split_turn_usage(&serde_json::json!({
            "input_tokens": 32161,
            "cached_input_tokens": 1920,
            "output_tokens": 47
        }));
        assert_eq!(uncached, 32161 - 1920);
        assert_eq!(cached, 1920);
        assert_eq!(output, 47);
    }

    /// A turn with no cache breakdown at all (older Codex, or a model that
    /// never hit the cache) must not underflow -- `cached_input_tokens`
    /// absent reads as 0, leaving `input_tokens` untouched.
    #[test]
    fn split_turn_usage_defaults_missing_cache_field_to_zero() {
        let (uncached, cached, output) = split_turn_usage(&serde_json::json!({
            "input_tokens": 100,
            "output_tokens": 10
        }));
        assert_eq!(uncached, 100);
        assert_eq!(cached, 0);
        assert_eq!(output, 10);
    }

    #[test]
    fn context_limit_args_empty_when_neither_is_set() {
        assert!(context_limit_args(&RunOptions::default()).is_empty());
    }

    // ── codex_infers_compaction (RAL-339) ──────────────────────────────────

    #[test]
    fn codex_infers_compaction_is_false_with_no_previous_turn() {
        assert!(!codex_infers_compaction(None, 500));
    }

    #[test]
    fn codex_infers_compaction_ignores_a_drop_below_the_minimum_token_floor() {
        // A 10x-or-more drop from a tiny previous turn (below the floor)
        // must not trivially count as a compaction.
        assert!(!codex_infers_compaction(Some(50), 1));
    }

    #[test]
    fn codex_infers_compaction_detects_an_order_of_magnitude_drop() {
        assert!(codex_infers_compaction(
            Some(CODEX_COMPACTION_INFERENCE_MIN_TOKENS * 10),
            CODEX_COMPACTION_INFERENCE_MIN_TOKENS
        ));
    }

    #[test]
    fn codex_infers_compaction_requires_at_least_a_10x_drop() {
        // Exactly 9x is not "an order of magnitude" -- must not infer.
        let prev = CODEX_COMPACTION_INFERENCE_MIN_TOKENS * 10;
        assert!(!codex_infers_compaction(Some(prev), prev / 9));
    }

    #[test]
    fn codex_infers_compaction_a_stable_or_growing_input_never_infers() {
        assert!(!codex_infers_compaction(Some(5000), 5200));
    }

    #[test]
    fn context_limit_args_maps_both_fields_as_config_overrides() {
        let options = RunOptions {
            maximum_context: Some(100_000),
            auto_compact_threshold: Some(80_000),
            ..Default::default()
        };
        let args = context_limit_args(&options);
        assert!(
            args.windows(2)
                .any(|w| w == ["-c", "model_context_window=100000"])
        );
        assert!(
            args.windows(2)
                .any(|w| w == ["-c", "model_auto_compact_token_limit=80000"])
        );
    }

    #[test]
    fn context_limit_args_maps_tool_output_max_tokens_as_a_config_override() {
        let options = RunOptions {
            tool_output_max_tokens: Some(20_000),
            ..Default::default()
        };
        let args = context_limit_args(&options);
        assert!(
            args.windows(2)
                .any(|w| w == ["-c", "tool_output_token_limit=20000"])
        );
    }

    // ── RAL-336 agent isolation ────────────────────────────────────────────

    fn test_workspace() -> Workspace {
        Workspace::create(std::env::temp_dir()).unwrap()
    }

    #[test]
    fn isolated_codex_home_is_none_when_both_opt_ins_are_allowed() {
        let options = RunOptions {
            allow_personal_settings: true,
            allow_personal_memory: true,
            ..Default::default()
        };
        let ws = test_workspace();
        assert_eq!(isolated_codex_home(&options, &ws), None);
    }

    #[test]
    fn isolated_codex_home_is_some_when_either_opt_in_is_off() {
        let ws = test_workspace();
        assert!(isolated_codex_home(&RunOptions::default(), &ws).is_some());
        let settings_only = RunOptions {
            allow_personal_settings: true,
            ..Default::default()
        };
        assert!(isolated_codex_home(&settings_only, &ws).is_some());
        let memory_only = RunOptions {
            allow_personal_memory: true,
            ..Default::default()
        };
        assert!(isolated_codex_home(&memory_only, &ws).is_some());
    }

    #[test]
    fn apply_codex_home_env_sets_the_var_when_some() {
        let dir = std::env::temp_dir().join("ralphus-codex-isolation-test");
        let mut cmd = Command::new("echo");
        apply_codex_home_env(&mut cmd, Some(&dir));
        let val = cmd
            .get_envs()
            .find(|(k, _)| *k == "CODEX_HOME")
            .and_then(|(_, v)| v);
        assert_eq!(val, Some(dir.as_os_str()));
    }

    #[test]
    fn apply_codex_home_env_is_a_noop_when_none() {
        let mut cmd = Command::new("echo");
        apply_codex_home_env(&mut cmd, None);
        assert!(cmd.get_envs().find(|(k, _)| *k == "CODEX_HOME").is_none());
    }
}
