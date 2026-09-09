//! The `pi` `ModelBackend`. Drives `pi` in JSON mode non-interactively,
//! parsing its JSONL event stream on stdout.

use std::io::{BufRead as _, BufReader, Write as _};
use std::path::Path;
use std::process::{Child, Command, Stdio};

use serde_json::Value;

use crate::backend::{BackendError, BackendOutcome, ModelBackend, RunOptions};
use crate::cli_agent_common::{live_session_path, write_live_session_id};
use crate::shellcmd::{self, Env};
use crate::tools::Workspace;

const DEFAULT_PROGRAM: &str = "pi";
const SUMMARY_TAIL_CHARS: usize = 2000;

pub struct PiBackend {
    pub keep_temporary_files: bool,
    pub program_override: Option<String>,
}

impl PiBackend {
    fn program(&self) -> String {
        self.program_override.clone().unwrap_or_else(|| {
            std::env::var("RALPHUS_PI_COMMAND").unwrap_or_else(|_| DEFAULT_PROGRAM.to_string())
        })
    }

    /// Resolve Pi's direct launcher before spawning it. On Windows npm ships a
    /// POSIX shim named `pi` beside the executable `pi.cmd`; the shell chooses
    /// the latter through `PATHEXT`, but `Command::new("pi")` need not. Keeping
    /// the resolution here makes every Pi invocation use the same launcher
    /// selection while leaving compound user commands for the shell.
    fn launch_program(&self) -> String {
        let program = self.program();
        if crate::cli_agent_common::launcher_requires_shell(&program) {
            return program;
        }
        shellcmd::find_program(&program).unwrap_or(program)
    }
}

fn real_config_dir(
    ambient_dir: Option<std::path::PathBuf>,
    home_dir: Option<std::path::PathBuf>,
) -> Option<std::path::PathBuf> {
    ambient_dir.or_else(|| home_dir.map(|home| home.join(".pi").join("agent")))
}

impl ModelBackend for PiBackend {
    fn preflight(&self) -> Result<(), BackendError> {
        crate::cli_agent_common::preflight_default_program(
            &self.program(),
            self.program_override.is_some() || std::env::var_os("RALPHUS_PI_COMMAND").is_some(),
        )
    }

    fn run(
        &self,
        prompt: &str,
        workspace: &Workspace,
        options: &RunOptions<'_>,
    ) -> Result<BackendOutcome, BackendError> {
        // RAL-336: pi exposes only one config-directory lever
        // (`PI_CODING_AGENT_DIR`) that can't cleanly separate personal
        // settings from personal memory, so isolation engages whenever
        // either opt-in is off, favoring over-isolation. `known_config_dir`
        // resolves once here and feeds both `apply_context_settings` (so it
        // never touches the operator's real directory once isolated) and
        // `spawn`'s own env override.
        let isolate = !options.allow_personal_settings || !options.allow_personal_memory;
        let ambient_dir = std::env::var_os("PI_CODING_AGENT_DIR").map(std::path::PathBuf::from);
        // Pi's documented default when PI_CODING_AGENT_DIR is absent is
        // ~/.pi/agent.  Isolation must preserve the login from that real
        // location too; otherwise a normal interactive Pi installation gets
        // an empty isolated auth.json and can end a JSON-mode run without an
        // assistant turn.
        let real_config_dir =
            real_config_dir(ambient_dir.clone(), crate::agent_isolation::home_dir());
        let isolated_dir = if isolate {
            let dir = crate::agent_isolation::isolated_config_dir(workspace.root(), "pi");
            // Best-effort: preserve a stored login so isolation doesn't force
            // a re-auth. Preserved regardless of the two opt-ins -- this is
            // about *authentication*, not personal settings/memory.
            crate::agent_isolation::preserve_auth_file(
                real_config_dir.as_deref(),
                &dir,
                "auth.json",
            );
            // Pi resolves an explicit provider/model through this downloaded
            // catalog. It contains model metadata rather than personal memory
            // or settings, so carry it into the isolated directory whenever
            // it exists; otherwise a valid `openrouter/...` model is unknown
            // only to the isolated runner process.
            crate::agent_isolation::preserve_auth_file(
                real_config_dir.as_deref(),
                &dir,
                "models-store.json",
            );
            // `models.json` carries provider-specific routing and model
            // overrides. In particular, an OpenRouter override can pin a
            // model to an approved cheaper upstream and disable fallbacks;
            // preserve that execution policy even while memory is isolated.
            crate::agent_isolation::preserve_auth_file(
                real_config_dir.as_deref(),
                &dir,
                "models.json",
            );
            if options.allow_personal_settings {
                // Settings opted back in but memory did not -- copy settings
                // (not any memory file) into the isolated dir.
                crate::agent_isolation::preserve_auth_file(
                    real_config_dir.as_deref(),
                    &dir,
                    "settings.json",
                );
            }
            Some(dir)
        } else {
            None
        };
        let known_config_dir = isolated_dir.clone().or_else(|| ambient_dir.clone());

        apply_context_settings(
            known_config_dir.as_deref(),
            options.model,
            options.maximum_context,
            options.auto_compact_threshold,
            options.maximum_tool_output_tokens,
        )?;

        let program = self.launch_program();
        let compound = crate::cli_agent_common::launcher_requires_shell(&program);

        // A shell-routed launcher (npm's `pi.cmd` shim included) cannot carry
        // a multiline argument, so hand the system prompt over as a file --
        // the same trade claude-code makes for the same reason.
        let system_prompt_file = match options.append_system_prompt {
            Some(sp) if compound => Some(
                crate::cli_agent_common::write_prompt_file(sp)
                    .map_err(|e| BackendError(e.to_string()))?,
            ),
            _ => None,
        };

        // Always send the cell's own prompt, even on resume -- matches
        // claude-code/codex (RAL-248 AC3): cross-cell session sharing needs
        // the new cell's task text, not a generic "continue".
        let args = build_args(options, system_prompt_file.as_deref());

        let mut child = spawn(
            &program,
            compound,
            &args,
            workspace,
            isolated_dir.as_deref(),
        )
        .map_err(|e| BackendError(format!("could not spawn {program}: {e}")))?;

        // Written on its own thread for the same reason as Codex's: writing
        // then closing stdin inline could deadlock against Pi filling its own
        // stdout/stderr pipes before it starts reading. Dropping the handle at
        // the end of the closure closes stdin, which is what ends Pi's
        // `readPipedStdin`.
        if let Some(mut stdin) = child.stdin.take() {
            let prompt = prompt.to_string();
            std::thread::spawn(move || {
                let _ = stdin.write_all(prompt.as_bytes());
            });
        }

        print_header(options.model, workspace);
        let thrash_thresholds = crate::thrash::ThrashThresholds {
            max_compactions: options
                .thrash_max_compactions
                .unwrap_or(crate::thrash::DEFAULT_MAX_COMPACTIONS),
            min_turn_gap: options
                .thrash_min_turn_gap
                .unwrap_or(crate::thrash::DEFAULT_MIN_TURN_GAP),
        };
        let outcome = drive_json_events(&mut child, workspace, thrash_thresholds)?;

        if !self.keep_temporary_files {
            let _ = std::fs::remove_file(live_session_path(workspace.root()));
        }

        Ok(outcome)
    }

    fn supports_maximum_context(&self) -> bool {
        true
    }

    fn supports_auto_compact_threshold(&self) -> bool {
        true
    }

    fn supports_maximum_tool_output_tokens(&self) -> bool {
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
/// RAL-333 reuses the same `modelOverrides.<model-id>` entry for
/// `maximum_tool_output_tokens`, writing it as `maxTokens` -- the model catalog's
/// own generation-budget field, and the closest thing `pi` has to a
/// per-tool-output cap. When `maximum_tool_output_tokens` is unset but
/// `maximum_context` is set, `maxTokens` defaults to 75% of `maximum_context`
/// (per the ticket's confirmed scope) rather than being left unwritten -- an
/// explicit `maximum_tool_output_tokens` always overrides that default.
///
/// A no-op when none of the three fields is set, so a `pi` cell that never
/// touches them never requires `PI_CODING_AGENT_DIR` at all.
///
/// `dir` is the resolved directory to operate against -- the caller
/// (`run()`) resolves it once, before this is called, from either the
/// ambient `PI_CODING_AGENT_DIR` or (RAL-336) an isolated per-worktree
/// directory when isolation is on, so this always writes wherever `pi`
/// itself will actually be pointed for the same invocation rather than
/// reading process env directly (this workspace forbids `unsafe_code`, and
/// `std::env::set_var` is the only way to make an in-process
/// `PI_CODING_AGENT_DIR` read see an isolated path).
fn apply_context_settings(
    dir: Option<&Path>,
    model: Option<&str>,
    maximum_context: Option<u64>,
    auto_compact_threshold: Option<u64>,
    maximum_tool_output_tokens: Option<u64>,
) -> Result<(), BackendError> {
    if maximum_context.is_none()
        && auto_compact_threshold.is_none()
        && maximum_tool_output_tokens.is_none()
    {
        return Ok(());
    }
    let dir = dir.ok_or_else(|| {
        BackendError(
            "pi: maximum_context/auto_compact_threshold/maximum_tool_output_tokens require \
             PI_CODING_AGENT_DIR to be set"
                .to_string(),
        )
    })?;
    apply_context_settings_in(
        dir,
        model,
        maximum_context,
        auto_compact_threshold,
        maximum_tool_output_tokens,
    )
}

/// Hermetic core of [`apply_context_settings`], split out the same way
/// `config.rs::load`/`load_with` split env-reading from the logic -- this
/// workspace forbids `unsafe_code`, so a test can't use `std::env::set_var`
/// to exercise the `PI_CODING_AGENT_DIR` lookup in-process.
///
/// Validates every field (provider-qualified model for `maximum_context`
/// and/or `maximum_tool_output_tokens`; `auto_compact_threshold <
/// maximum_context` when both are set) before writing anything, so a
/// rejected cell never leaves one file updated and the other not.
fn apply_context_settings_in(
    dir: &Path,
    model: Option<&str>,
    maximum_context: Option<u64>,
    auto_compact_threshold: Option<u64>,
    maximum_tool_output_tokens: Option<u64>,
) -> Result<(), BackendError> {
    let provider_model = (maximum_context.is_some() || maximum_tool_output_tokens.is_some())
        .then(|| {
            split_provider_model(model).ok_or_else(|| {
                BackendError(
                    "pi: maximum_context/maximum_tool_output_tokens require the cell's `model` to be \
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

    // RAL-333: an explicit maximum_tool_output_tokens always wins; otherwise
    // default to 75% of maximum_context (pi's own maxTokens/contextWindow
    // ratio in its example model catalog), written only when
    // maximum_context is itself set.
    let effective_max_tokens =
        maximum_tool_output_tokens.or_else(|| maximum_context.map(|ctx| ctx * 3 / 4));

    if let Some((provider, model_id)) = provider_model {
        if let Some(v) = maximum_context {
            write_model_context_window(dir, provider, model_id, v)?;
        }
        if let Some(v) = effective_max_tokens {
            write_model_max_tokens(dir, provider, model_id, v)?;
        }
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
/// set on this same model's override, e.g. a user's own `cost`). Note that
/// `maxTokens` is not among the preserved fields once `maximum_context` is
/// set -- see [`write_model_max_tokens`]'s 75%-default behavior (RAL-333).
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

/// Merges `providers.<provider>.modelOverrides.<model_id>.maxTokens` into
/// `dir`'s `models.json`, preserving every other key the same way
/// [`write_model_context_window`] does (RAL-333).
fn write_model_max_tokens(
    dir: &Path,
    provider: &str,
    model_id: &str,
    max_tokens: u64,
) -> Result<(), BackendError> {
    let path = dir.join("models.json");
    let mut root = read_json_object(&path)?;
    let model_entry = nested_object(
        &mut root,
        &path,
        &["providers", provider, "modelOverrides", model_id],
    )?;
    model_entry.insert("maxTokens".to_string(), Value::from(max_tokens));
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

/// Pi's own argv, with the two potentially-multiline values kept out of it.
///
/// The prompt is not an argument at all -- it is piped to stdin, which Pi
/// folds into the initial message (its `buildInitialMessage` puts piped stdin
/// content first). `--append-system-prompt` stays an argument but carries a
/// *path* when the launcher is shell-routed: Pi resolves that value as a file
/// when one exists at it, and falls back to treating it as literal text.
///
/// Both matter because Windows resolves `pi` to npm's `pi.cmd` shim, and a
/// batch launcher cannot carry a newline in any argument -- cmd truncates the
/// line at it, silently dropping everything after (RAL-385).
fn build_args(options: &RunOptions<'_>, system_prompt_file: Option<&Path>) -> Vec<String> {
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
    if let Some(path) = system_prompt_file {
        args.push("--append-system-prompt".to_string());
        args.push(path.display().to_string());
    } else if let Some(sp) = options.append_system_prompt {
        args.push("--append-system-prompt".to_string());
        args.push(sp.to_string());
    }
    // Bare flag: Pi's `-p`/`--print` takes no value. The prompt arrives on
    // stdin instead.
    args.push("-p".to_string());
    args
}

/// `isolated_config_dir` is `Some` only when RAL-336 isolation is engaged for
/// this run -- it sets `PI_CODING_AGENT_DIR` on the child to redirect it away
/// from the operator's real directory. `None` leaves the child's inherited
/// `PI_CODING_AGENT_DIR` (if any) untouched, preserving today's behavior when
/// both isolation opt-ins are on.
fn spawn(
    program: &str,
    compound: bool,
    args: &[String],
    workspace: &Workspace,
    isolated_config_dir: Option<&Path>,
) -> std::io::Result<Child> {
    if compound {
        // npm's `pi.cmd` is a batch file, not a generic user command. It
        // must be interpreted by cmd.exe; routing it through the daemon's
        // parent shell can change the one prompt argument after `-p`.
        let shell = if crate::cli_agent_common::is_windows_batch_launcher(program) {
            "cmd".to_string()
        } else {
            shellcmd::resolve_shell(None)
        };
        let _ = shellcmd::detect_parent_shell(&Env::from_process());
        let line = crate::cli_agent_common::shell_command_line(&shell, program, args);
        let mut cmd =
            shellcmd::command_for_spawn_args(shellcmd::shell_spawn_args(&shell, &line), args)?;
        cmd.current_dir(workspace.root())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        apply_isolated_config_dir_env(&mut cmd, isolated_config_dir);
        cmd.spawn()
    } else {
        let mut cmd = Command::new(program);
        cmd.args(args)
            .current_dir(workspace.root())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        apply_isolated_config_dir_env(&mut cmd, isolated_config_dir);
        cmd.spawn()
    }
}

/// Sets `PI_CODING_AGENT_DIR` on `cmd` when isolation resolved a redirect
/// directory -- a no-op otherwise, mirroring
/// `claude_code_backend.rs::apply_auto_compact_env`'s shape.
fn apply_isolated_config_dir_env(cmd: &mut Command, isolated_config_dir: Option<&Path>) {
    if let Some(dir) = isolated_config_dir {
        cmd.env("PI_CODING_AGENT_DIR", dir);
    }
}

fn drive_json_events(
    child: &mut Child,
    workspace: &Workspace,
    thrash_thresholds: crate::thrash::ThrashThresholds,
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

    let mut state = ParseState {
        thrash: crate::thrash::ThrashTracker::new(thrash_thresholds),
        ..ParseState::default()
    };
    for line in reader.lines().map_while(Result::ok) {
        let Ok(event) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        process_event(&event, &mut state, workspace.root());
        // RAL-339: stop reading at the compaction boundary itself -- a safe
        // stop point -- instead of waiting for pi's own stdout to reach EOF,
        // which could be arbitrarily many further (thrashing) turns away.
        if state.compaction_thrash.is_some() {
            break;
        }
    }

    if let Some(detail) = state.compaction_thrash {
        // RAL-339: the run is thrashing -- kill the child now rather than
        // wait for a natural exit that may be arbitrarily far off, then fail
        // the cell with whatever was captured live so far.
        let _ = child.kill();
        let _ = child.wait();
        if let Some(t) = stderr_thread {
            let _ = t.join();
        }
        return Ok(BackendOutcome {
            summary: tail(&state.latest_assistant_message, SUMMARY_TAIL_CHARS),
            tokens_in: state.tokens_in,
            tokens_out: state.tokens_out,
            cache_creation_tokens: state.cache_creation_tokens,
            cache_read_tokens: state.cache_read_tokens,
            cost_usd: state.cost_usd,
            agent_session_id: state.agent_session_id,
            abandoned_background_job: None,
            compaction_thrash: Some(detail),
            // RAL-373: this backend reports no compaction data (it has no
            // `compact_boundary`-equivalent event), not "never compacts".
            compaction_input_tokens: 0,
            compaction_count: 0,
        });
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
        cache_creation_tokens: state.cache_creation_tokens,
        cache_read_tokens: state.cache_read_tokens,
        cost_usd: state.cost_usd,
        agent_session_id: state.agent_session_id,
        abandoned_background_job: None,
        compaction_thrash: None,
        // RAL-373: this backend reports no compaction data (it has no
        // `compact_boundary`-equivalent event), not "never compacts".
        compaction_input_tokens: 0,
        compaction_count: 0,
    })
}

#[derive(Default)]
struct ParseState {
    agent_session_id: Option<String>,
    latest_assistant_message: String,
    tokens_in: i64,
    tokens_out: i64,
    cache_creation_tokens: i64,
    cache_read_tokens: i64,
    cost_usd: f64,
    saw_terminal_event: bool,
    printed_text_delta: bool,
    /// RAL-339: shared compaction-thrash counter for this run (see
    /// `crate::thrash`).
    thrash: crate::thrash::ThrashTracker,
    /// RAL-339: set the moment [`Self::thrash`] reports the run has crossed
    /// into thrash -- the outer read loop in [`drive_json_events`] checks
    /// this after every event and stops (kills the child) rather than
    /// waiting for stdout EOF.
    compaction_thrash: Option<crate::thrash::ThrashDetail>,
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
            // RAL-326: pi's own `Usage` splits prompt-cache tokens out of
            // `input` into `cacheWrite`/`cacheRead`, so `input` alone is only
            // the uncached slice -- the same shape claude-code and codex use.
            let cache_creation_tokens = usage["cacheWrite"].as_i64().unwrap_or(0);
            let cache_read_tokens = usage["cacheRead"].as_i64().unwrap_or(0);
            let cost_usd = usage["cost"]["total"].as_f64().unwrap_or(0.0);
            if tokens_in > 0
                || tokens_out > 0
                || cache_creation_tokens > 0
                || cache_read_tokens > 0
                || cost_usd > 0.0
            {
                state.tokens_in = tokens_in;
                state.tokens_out = tokens_out;
                state.cache_creation_tokens = cache_creation_tokens;
                state.cache_read_tokens = cache_read_tokens;
                state.cost_usd = cost_usd;
                crate::cartographer::emit(
                    "pi",
                    crate::cartographer::LIVE_USAGE_MESSAGE,
                    "info",
                    crate::cartographer::EventContext::default(),
                    serde_json::json!({
                        "tokens_in": tokens_in,
                        "tokens_out": tokens_out,
                        "cache_creation_tokens": cache_creation_tokens,
                        "cache_read_tokens": cache_read_tokens,
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
                // RAL-339: this fires once per assistant turn -- the natural
                // "turns since previous compaction" tick.
                state.thrash.record_assistant_turn();
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
            // RAL-339: track compaction cadence and fail closed the moment
            // it crosses into thrash -- see `crate::thrash` for the rule.
            if let Some(detail) = state.thrash.record_compaction() {
                eprintln!(
                    "[thrash] autocompaction thrashing detected: {} compactions, \
                     most recently {} turn(s) after the previous one",
                    detail.compaction_count, detail.turns_since_previous_compaction
                );
                crate::thrash::emit_thrash_event("pi", &detail);
                state.compaction_thrash = Some(detail);
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
    fn real_config_dir_uses_pis_default_when_the_env_override_is_unset() {
        assert_eq!(
            real_config_dir(None, Some(std::path::PathBuf::from("C:/Users/tester"))),
            Some(std::path::PathBuf::from("C:/Users/tester/.pi/agent"))
        );
    }

    #[test]
    fn real_config_dir_prefers_the_explicit_env_override() {
        assert_eq!(
            real_config_dir(
                Some(std::path::PathBuf::from("D:/pi-config")),
                Some(std::path::PathBuf::from("C:/Users/tester")),
            ),
            Some(std::path::PathBuf::from("D:/pi-config"))
        );
    }

    #[test]
    fn build_args_includes_resume_model_and_system_prompt() {
        let args = build_args(
            &RunOptions {
                model: Some("openrouter/deepseek"),
                append_system_prompt: Some("be terse"),
                resume_agent_session_id: Some("sess-123"),
                ..Default::default()
            },
            None,
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
    }

    /// RAL-385: the prompt is piped to stdin, never passed as an argument --
    /// npm's `pi.cmd` shim cannot carry a multiline argv token.
    #[test]
    fn build_args_never_carries_the_prompt() {
        let args = build_args(&RunOptions::default(), None);
        assert_eq!(args.last().map(String::as_str), Some("-p"));
        assert!(!args.iter().any(|a| a.contains('\n')));
    }

    /// RAL-385: a shell-routed launcher gets the system prompt as a file path,
    /// so no argument carries the multiline text itself.
    #[test]
    fn build_args_passes_system_prompt_as_a_file_when_given_one() {
        let path = Path::new("C:/state/task_prompts/abc123.md");
        let args = build_args(
            &RunOptions {
                append_system_prompt: Some("line one\n\nline two"),
                ..Default::default()
            },
            Some(path),
        );
        let value = path.display().to_string();
        assert!(
            args.windows(2)
                .any(|w| w[0] == "--append-system-prompt" && w[1] == value)
        );
        assert!(!args.iter().any(|a| a.contains('\n')));
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

    /// RAL-326: pi's own `Usage` splits prompt-cache tokens out of `input`
    /// into `cacheWrite`/`cacheRead`, so reading `input` alone captures only
    /// the uncached slice of what was billed.
    #[test]
    fn process_event_captures_pis_prompt_cache_token_split() {
        let mut state = ParseState::default();
        let root = Path::new(".");
        process_event(
            &serde_json::json!({
                "type":"message_update",
                "usage":{
                    "input":12,
                    "output":34,
                    "cacheWrite":900,
                    "cacheRead":41_000,
                    "cost":{"total":0.56}
                }
            }),
            &mut state,
            root,
        );
        assert_eq!(state.tokens_in, 12, "uncached input keeps its old meaning");
        assert_eq!(state.cache_creation_tokens, 900);
        assert_eq!(state.cache_read_tokens, 41_000);
    }

    /// A turn served entirely from pi's cache must still be recorded -- the
    /// pre-RAL-326 guard keyed on input/output/cost alone and dropped it.
    #[test]
    fn process_event_records_a_pi_turn_billed_entirely_to_the_cache() {
        let mut state = ParseState::default();
        let root = Path::new(".");
        process_event(
            &serde_json::json!({
                "type":"message_update",
                "usage":{"input":0,"output":0,"cacheRead":41_000,"cost":{"total":0.0}}
            }),
            &mut state,
            root,
        );
        assert_eq!(state.cache_read_tokens, 41_000);
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

    /// RAL-339: the default thresholds (N=3, M=2) -- three compactions with
    /// zero assistant turns between the second and third must fail closed at
    /// the third compaction's own event, not later.
    #[test]
    fn process_event_flags_thrash_on_the_default_thresholds() {
        let mut state = ParseState::default();
        let root = Path::new(".");
        let compaction_end = serde_json::json!({
            "type":"compaction_end",
            "reason":"threshold",
            "aborted":false,
            "result":{"tokensBefore":164975,"estimatedTokensAfter":20000}
        });
        process_event(&compaction_end, &mut state, root);
        assert_eq!(state.compaction_thrash, None);
        process_event(&compaction_end, &mut state, root);
        assert_eq!(state.compaction_thrash, None);
        process_event(&compaction_end, &mut state, root);
        let detail = state
            .compaction_thrash
            .expect("third rapid compaction should thrash");
        assert_eq!(detail.compaction_count, 3);
        assert_eq!(detail.turns_since_previous_compaction, 0);
    }

    /// RAL-339: the same three compactions, but with enough assistant turns
    /// between each, must never thrash.
    #[test]
    fn process_event_does_not_flag_thrash_when_turns_separate_compactions_healthily() {
        let mut state = ParseState::default();
        let root = Path::new(".");
        let compaction_end = serde_json::json!({
            "type":"compaction_end",
            "reason":"threshold",
            "aborted":false,
            "result":{"tokensBefore":164975,"estimatedTokensAfter":20000}
        });
        let assistant_turn = serde_json::json!({
            "type":"message_end",
            "message":{"role":"assistant","content":[{"type":"text","text":"working"}]}
        });
        for _ in 0..3 {
            process_event(&compaction_end, &mut state, root);
            assert_eq!(state.compaction_thrash, None);
            process_event(&assistant_turn, &mut state, root);
            process_event(&assistant_turn, &mut state, root);
        }
        assert_eq!(state.compaction_thrash, None);
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
        apply_context_settings_in(&dir, Some("openrouter/deepseek"), Some(100_000), None, None)
            .unwrap();

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
            None,
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
            None,
        )
        .unwrap();

        let models: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("models.json")).unwrap())
                .unwrap();
        // RAL-333: no explicit maximum_tool_output_tokens was given, so the
        // pre-existing maxTokens is overwritten with 75% of maximum_context
        // rather than preserved.
        assert_eq!(
            models["providers"]["openrouter"]["modelOverrides"]["deepseek"]["maxTokens"],
            serde_json::json!(75_000)
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
        let err = apply_context_settings_in(&dir, Some("deepseek"), Some(100_000), None, None)
            .unwrap_err();
        assert!(err.0.contains("provider"), "unexpected error: {}", err.0);
        assert!(!dir.join("models.json").exists());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn apply_context_settings_errors_when_threshold_set_without_maximum_context() {
        let dir = temp_settings_dir("no-max");
        let err =
            apply_context_settings_in(&dir, Some("openrouter/deepseek"), None, Some(80_000), None)
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
            None,
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
        // never sets any of the three fields must never require it.
        assert!(apply_context_settings(None, None, None, None, None).is_ok());
    }

    #[test]
    fn apply_context_settings_writes_models_json_max_tokens() {
        let dir = temp_settings_dir("max-tokens-create");
        apply_context_settings_in(&dir, Some("openrouter/deepseek"), None, None, Some(20_000))
            .unwrap();

        let text = std::fs::read_to_string(dir.join("models.json")).unwrap();
        let json: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(
            json["providers"]["openrouter"]["modelOverrides"]["deepseek"]["maxTokens"],
            serde_json::json!(20_000)
        );
        assert!(
            json["providers"]["openrouter"]["modelOverrides"]["deepseek"]["contextWindow"]
                .is_null()
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn apply_context_settings_writes_both_context_window_and_max_tokens() {
        let dir = temp_settings_dir("max-tokens-with-context");
        apply_context_settings_in(
            &dir,
            Some("openrouter/deepseek"),
            Some(100_000),
            None,
            Some(20_000),
        )
        .unwrap();

        let text = std::fs::read_to_string(dir.join("models.json")).unwrap();
        let json: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(
            json["providers"]["openrouter"]["modelOverrides"]["deepseek"]["contextWindow"],
            serde_json::json!(100_000)
        );
        assert_eq!(
            json["providers"]["openrouter"]["modelOverrides"]["deepseek"]["maxTokens"],
            serde_json::json!(20_000)
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn apply_context_settings_defaults_max_tokens_to_75_percent_of_maximum_context() {
        // maximum_context alone (no explicit maximum_tool_output_tokens) must
        // still synthesize a maxTokens default of 75% of maximum_context,
        // per the ticket's confirmed scope (RAL-333).
        let dir = temp_settings_dir("synthesized-default");
        apply_context_settings_in(&dir, Some("openrouter/deepseek"), Some(100_000), None, None)
            .unwrap();

        let text = std::fs::read_to_string(dir.join("models.json")).unwrap();
        let json: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(
            json["providers"]["openrouter"]["modelOverrides"]["deepseek"]["maxTokens"],
            serde_json::json!(75_000)
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn apply_context_settings_explicit_max_tokens_overrides_the_75_percent_default() {
        let dir = temp_settings_dir("explicit-overrides-default");
        apply_context_settings_in(
            &dir,
            Some("openrouter/deepseek"),
            Some(100_000),
            None,
            Some(20_000),
        )
        .unwrap();

        let text = std::fs::read_to_string(dir.join("models.json")).unwrap();
        let json: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(
            json["providers"]["openrouter"]["modelOverrides"]["deepseek"]["maxTokens"],
            serde_json::json!(20_000)
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn apply_context_settings_errors_without_provider_qualified_model_for_max_tokens_alone() {
        let dir = temp_settings_dir("no-provider-max-tokens");
        let err = apply_context_settings_in(&dir, Some("deepseek"), None, None, Some(20_000))
            .unwrap_err();
        assert!(err.0.contains("provider"), "unexpected error: {}", err.0);
        assert!(!dir.join("models.json").exists());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn apply_context_settings_errors_when_dir_is_none_but_a_field_is_set() {
        let err =
            apply_context_settings(None, Some("openrouter/deepseek"), Some(100_000), None, None)
                .unwrap_err();
        assert!(err.0.contains("PI_CODING_AGENT_DIR"), "{}", err.0);
    }

    #[test]
    fn apply_context_settings_writes_into_the_given_dir_not_the_ambient_env_var() {
        // RAL-336: `apply_context_settings` must operate against whatever
        // `dir` the caller resolved (the isolated directory, when isolation
        // is on) -- never fall back to reading `PI_CODING_AGENT_DIR` itself.
        let dir = temp_settings_dir("explicit-dir");
        apply_context_settings(
            Some(&dir),
            Some("openrouter/deepseek"),
            Some(100_000),
            None,
            None,
        )
        .unwrap();
        let text = std::fs::read_to_string(dir.join("models.json")).unwrap();
        let json: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(
            json["providers"]["openrouter"]["modelOverrides"]["deepseek"]["contextWindow"],
            serde_json::json!(100_000)
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    // ── RAL-336 agent isolation ────────────────────────────────────────────

    #[test]
    fn apply_isolated_config_dir_env_sets_the_var_when_some() {
        let dir = std::env::temp_dir().join("ralphus-pi-isolation-test");
        let mut cmd = Command::new("echo");
        apply_isolated_config_dir_env(&mut cmd, Some(&dir));
        let val = cmd
            .get_envs()
            .find(|(k, _)| *k == "PI_CODING_AGENT_DIR")
            .and_then(|(_, v)| v);
        assert_eq!(val, Some(dir.as_os_str()));
    }

    #[test]
    fn apply_isolated_config_dir_env_is_a_noop_when_none() {
        let mut cmd = Command::new("echo");
        apply_isolated_config_dir_env(&mut cmd, None);
        assert!(!cmd.get_envs().any(|(k, _)| k == "PI_CODING_AGENT_DIR"));
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
