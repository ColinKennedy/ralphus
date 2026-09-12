//! Turns a [`CellSpec`] into a [`CellResult`]. The single choke point that
//! dispatches `command` cells (no model), and wraps `prompt`/proof
//! cells with system-prompt composition, the still-working retry loop,
//! and verdict/ghost marker parsing.

use crate::agent_backend::AgentBackend;
use crate::backend::{BackendOutcome, ModelBackend, RunOptions};
use crate::claude_code_backend::ClaudeCodeBackend;
use crate::codex_backend::CodexBackend;
use crate::harness_backend::HarnessBackend;
use crate::pi_backend::PiBackend;
use crate::spec::{CellResult, CellSpec};
use crate::tools::Workspace;

// These five constants must stay byte-identical to `daemon/src/runner.rs`'s
// `PROOF_SYSTEM_PROMPT`/`GHOST_SYSTEM_PROMPT`/`ASYNC_SYSTEM_PROMPT`/
// `TOOLS_SYSTEM_PROMPT`/`NON_INTERACTIVE_SYSTEM_PROMPT` -- that file's own
// comment says the same about staying in sync with this one.
//
// The assembled prompt (caller-authored fragment first, then these in the
// order `combine_system_prompts` lists them) is organized as
// `## Background` -> `## Regarding Tools` -> `## Conclusion`: the
// non-interactive and tools fragments open their own sections, and the
// async fragment opens `## Conclusion`, so the proof/ghost fragment that
// follows it lands inside that section.
const PROOF_SYSTEM_PROMPT: &str = "This is a PROOF step, not a normal task. Investigate whether the \
     task holds, attempting to fix any problems you find so the check passes \
     if you can reasonably do so. When you are done, your FINAL line of \
     output must be exactly one of:\nRALPHUS_PROOF: PASS\nRALPHUS_PROOF: FAIL\nwith \
     nothing else on that line.";
const GHOST_SYSTEM_PROMPT: &str = "Operational logging note, not a request to change your behavior: this \
     ralphus task run keeps a short handoff record for whichever agent picks \
     up dependent work next. That agent will see your code changes but not \
     this conversation. After you finish the task above, add one final \
     section to your reply, starting on its own line with the exact marker \
     'RALPHUS_GHOST:', followed by up to 5 short bullet points. Only include \
     things a future agent could NOT already learn by reading `git log` or \
     the diff: places you struggled, workarounds you used, issues you noticed \
     but did not fix, and open questions. This is not a changelog. If there \
     is truly nothing worth flagging, write 'RALPHUS_GHOST: (nothing to report)'.";
const ASYNC_SYSTEM_PROMPT: &str = "## Conclusion\nThis is a single, non-interactive invocation — no \
     human will check back on you or answer follow-up questions, though \
     Ralphus may re-invoke you synchronously to continue. Never use an \
     asynchronous/background/'notify me later' tool for anything this session \
     depends on, and never launch the thing you are checking as a \
     background/detached process and end your turn while it is still running; \
     those require a persistent session this invocation does not have. If a \
     check genuinely takes a long time, block and wait for it synchronously \
     in the foreground within this same turn — it is fine for that to take a \
     long time. If, despite that, you truly cannot reach a definitive result \
     before you must stop, end your reply with 'RALPHUS_STILL_WORKING: \
     <one-line reason>' as the last line instead of trailing off — you will \
     be re-invoked shortly to continue synchronously from where you left \
     off, though only a bounded number of times, so prefer just finishing \
     the check yourself.";
const TOOLS_SYSTEM_PROMPT: &str = "## Regarding Tools\nPrefer `rg` for shell searches; \
     use `grep` only when `rg` is unavailable or you need grep-specific \
     behavior. In shell examples, use `rg \"pattern\" .`.";
const NON_INTERACTIVE_SYSTEM_PROMPT: &str = "## Background\nYou are running unattended in a non-interactive \
     cell — no human is available to answer questions or approve a plan. \
     Never ask a clarifying question, never stop to present a plan for \
     confirmation, and never pause waiting for input. Make the most \
     reasonable judgment call yourself and continue until the task is \
     complete. Your working directory for this cell is fixed for the entire \
     session — never `cd` to, read, or write any path outside it, even one \
     that looks related or more familiar (such as this repository's main \
     checkout); every file edit and git operation must happen inside the \
     working directory you were given.";

const MAX_ASYNC_ATTEMPTS: u32 = 3;
/// RAL-292: how many times a turn that ended with an unresolved backgrounded
/// job gets nudged (via [`ModelBackend::nudge`]) before the cell is failed
/// outright.
const MAX_BACKGROUND_JOB_NUDGE_ATTEMPTS: u32 = 3;
const GHOST_MAX_CHARS: usize = 4000;
const COMMAND_TAIL_CHARS: usize = 2000;

/// Resolves an agent name to its backend, mirroring the three-way dispatch
/// documented in `runner/__main__.py::_load_backend`: `claude-code`/`codex`/`pi`
/// drive an external CLI with stream parsing; `claude`/`anthropic`/`ollama`
/// use the hand-rolled tool loop; `raw` is the explicit generic harness.
fn load_backend(
    agent: &str,
    executable: Option<&str>,
    keep_temporary_files: bool,
) -> Result<Box<dyn ModelBackend>, String> {
    match agent {
        "claude-code" | "claude-cli" => Ok(Box::new(ClaudeCodeBackend {
            keep_temporary_files,
            program_override: executable.map(str::to_string),
        })),
        "codex" | "codex-cli" => Ok(Box::new(CodexBackend {
            keep_temporary_files,
            program_override: executable.map(str::to_string),
        })),
        "pi" => Ok(Box::new(PiBackend {
            keep_temporary_files,
            program_override: executable.map(str::to_string),
        })),
        "claude" | "anthropic" | "ollama" => Ok(Box::new(AgentBackend {
            agent: agent.to_string(),
        })),
        "raw" => Ok(Box::new(HarnessBackend {
            program: executable
                .ok_or_else(|| "backend \"raw\" requires an executable".to_string())?
                .to_string(),
        })),
        other => Err(format!(
            "unknown backend {other:?}; expected claude, anthropic, ollama, claude-code, codex, pi, or raw"
        )),
    }
}

/// Ask the selected backend whether its launcher is available without opening
/// an agent session.
pub fn preflight_agent(
    agent: &str,
    executable: Option<&str>,
    keep_temporary_files: bool,
) -> Result<(), String> {
    load_backend(agent, executable, keep_temporary_files)
        .and_then(|backend| backend.preflight().map_err(|error| error.to_string()))
}

/// Runs one cell end-to-end. Never panics on a bad workspace/backend --
/// every failure mode becomes a `"failed"` [`CellResult`], always
/// producing a result rather than letting an
/// exception escape to a nonzero process exit with no JSON on stdout.
#[must_use]
pub fn run_cell(spec: &CellSpec, keep_temporary_files: bool) -> CellResult {
    let workspace = match Workspace::create(&spec.cwd) {
        Ok(w) => w,
        Err(e) => return CellResult::failed(e.to_string(), ""),
    };

    if let Some(command) = &spec.command {
        return run_command(&workspace, command, spec.timeout_sec);
    }

    let Some(prompt) = &spec.prompt else {
        return CellResult::failed("spec has neither prompt nor command", "");
    };

    run_prompt(spec, prompt, &workspace, keep_temporary_files)
}

fn run_command(workspace: &Workspace, command: &str, timeout_sec: Option<u64>) -> CellResult {
    match workspace.run_bash(command, timeout_sec) {
        Ok(out) if out.ok() => CellResult::done(tail(&out.stdout, COMMAND_TAIL_CHARS)),
        Ok(out) => {
            let detail = tail(&out.stderr, COMMAND_TAIL_CHARS);
            let detail = if detail.is_empty() {
                tail(&out.stdout, COMMAND_TAIL_CHARS)
            } else {
                detail
            };
            CellResult::failed(format!("command exited {}", out.exit_code), detail)
        }
        Err(e) => CellResult::failed(e.to_string(), ""),
    }
}

fn run_prompt(
    spec: &CellSpec,
    original_prompt: &str,
    workspace: &Workspace,
    keep_temporary_files: bool,
) -> CellResult {
    log_llm_start(spec, original_prompt);
    let result = run_prompt_inner(spec, original_prompt, workspace, keep_temporary_files);
    log_llm_done(spec, &result);
    result
}

/// First 8 hex chars of the SHA-256 digest -- matches Python's
/// `hashlib.sha256(prompt.encode()).hexdigest()[:8]`.
fn short_prompt_hash(data: &[u8]) -> String {
    use sha2::{Digest as _, Sha256};
    Sha256::digest(data)[..4]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>()
}

/// RAL-288 Stage 5: emitted via Cartographer only, not a plain `eprintln!` —
/// this information had no Cartographer counterpart before Stage 5 (an
/// `eprintln!` alone only ever existed in the live tmux pane, gone the
/// moment the pane closes), so simply removing the print to clean up the
/// pane would have silently lost it rather than just relocating it. Uses
/// the same `RALPHUS_EVENT:` marker/context shape every other runner
/// diagnostic does, so the daemon-side pane-cleaning strip
/// (`daemon/src/server.rs::capture_pane_reply`) covers it identically.
fn log_llm_start(spec: &CellSpec, prompt: &str) {
    let kind = if spec.proof { "proof-" } else { "" };
    crate::cartographer::emit(
        "runner",
        &format!("{kind}llm start"),
        "info",
        event_context(spec),
        serde_json::json!({
            "agent": spec.agent,
            "model": spec.model,
            "prompt_len": prompt.len(),
            "prompt_hash": short_prompt_hash(prompt.as_bytes()),
        }),
    );
}

fn log_llm_done(spec: &CellSpec, result: &CellResult) {
    let kind = if spec.proof { "proof-" } else { "" };
    if result.ok() {
        crate::cartographer::emit(
            "runner",
            &format!("{kind}llm done"),
            "info",
            event_context(spec),
            serde_json::json!({
                "tokens_in": result.tokens_in,
                "tokens_out": result.tokens_out,
                "cache_creation_tokens": result.cache_creation_tokens,
                "cache_read_tokens": result.cache_read_tokens,
                "compaction_input_tokens": result.compaction_input_tokens,
                "compaction_count": result.compaction_count,
                "cost_usd": result.cost_usd,
            }),
        );
    } else {
        crate::cartographer::emit(
            "runner",
            &format!("{kind}llm error"),
            "warning",
            event_context(spec),
            serde_json::json!({"error": result.error.as_deref().unwrap_or("unknown error")}),
        );
    }
}

fn event_context(spec: &CellSpec) -> crate::cartographer::EventContext<'_> {
    crate::cartographer::EventContext {
        squad_id: Some(&spec.squad_id),
        cell_id: Some(&spec.cell_id),
        task: Some(&spec.task),
    }
}

fn run_prompt_inner(
    spec: &CellSpec,
    original_prompt: &str,
    workspace: &Workspace,
    keep_temporary_files: bool,
) -> CellResult {
    let backend = match load_backend(
        &spec.agent,
        spec.executable.as_deref(),
        keep_temporary_files,
    ) {
        Ok(backend) => backend,
        Err(e) => return CellResult::failed(e, ""),
    };
    if let Err(e) = check_context_limit_support(spec, backend.as_ref()) {
        return CellResult::failed(e, "");
    }
    run_with_backend(spec, original_prompt, workspace, backend.as_ref())
}

/// RAL-304: defense in depth -- `core::validate`'s
/// `agent_supports_maximum_context`/`agent_supports_auto_compact_threshold`
/// gates already reject these combinations at submit time, so this should be
/// unreachable in practice, but fail closed rather than silently ignoring the
/// cap if it's ever reached (e.g. a spec built by something other than the
/// daemon's own submit path). Checked independently, not as a pair --
/// claude-code supports `auto_compact_threshold` but not `maximum_context`.
/// Split out from [`run_prompt_inner`] so it can be exercised against a stub
/// [`ModelBackend`] in tests, the same reason [`run_with_backend`] is split
/// out.
fn check_context_limit_support(spec: &CellSpec, backend: &dyn ModelBackend) -> Result<(), String> {
    if spec.maximum_context.is_some() && !backend.supports_maximum_context() {
        return Err(format!(
            "agent {:?} does not support maximum_context",
            spec.agent
        ));
    }
    if spec.auto_compact_threshold.is_some() && !backend.supports_auto_compact_threshold() {
        return Err(format!(
            "agent {:?} does not support auto_compact_threshold",
            spec.agent
        ));
    }
    if spec.maximum_tool_output_tokens.is_some() && !backend.supports_maximum_tool_output_tokens() {
        return Err(format!(
            "agent {:?} does not support maximum_tool_output_tokens",
            spec.agent
        ));
    }
    Ok(())
}

/// The backend-agnostic half of [`run_prompt_inner`], split out so the
/// still-working retry loop and the RAL-292 background-job nudge loop can be
/// exercised in tests against a stub [`ModelBackend`] instead of a real CLI
/// subprocess.
fn run_with_backend(
    spec: &CellSpec,
    original_prompt: &str,
    workspace: &Workspace,
    backend: &dyn ModelBackend,
) -> CellResult {
    let system_prompt = combine_system_prompts(&[
        spec.system_prompt.as_deref(),
        Some(NON_INTERACTIVE_SYSTEM_PROMPT),
        Some(TOOLS_SYSTEM_PROMPT),
        Some(ASYNC_SYSTEM_PROMPT),
        Some(if spec.proof {
            PROOF_SYSTEM_PROMPT
        } else {
            GHOST_SYSTEM_PROMPT
        }),
    ]);
    if let Some(sp) = &system_prompt {
        crate::cartographer::emit(
            "runner",
            "system-prompt applied",
            "info",
            event_context(spec),
            serde_json::json!({"len": sp.len(), "position": spec.system_prompt_position}),
        );
    }

    let mut prompt = original_prompt.to_string();
    let mut resume_id = spec.resume_agent_session_id.clone();
    let mut total_tokens_in = 0i64;
    let mut total_tokens_out = 0i64;
    let mut total_cache_creation_tokens = 0i64;
    let mut total_cache_read_tokens = 0i64;
    let mut total_compaction_input_tokens = 0i64;
    let mut total_compaction_count = 0i64;
    let mut total_cost_usd = 0.0f64;
    let mut agent_session_id: Option<String> = None;

    for attempt in 0..MAX_ASYNC_ATTEMPTS {
        let options = RunOptions {
            model: spec.model.as_deref(),
            append_system_prompt: system_prompt.as_deref(),
            resume_agent_session_id: resume_id.as_deref(),
            assigned_agent_session_id: spec.assigned_agent_session_id.as_deref(),
            timeout_sec: spec.timeout_sec,
            maximum_context: spec.maximum_context,
            auto_compact_threshold: spec.auto_compact_threshold,
            tool_arg_truncate_chars: spec.tool_arg_truncate_chars,
            thrash_max_compactions: spec.thrash_max_compactions,
            thrash_min_turn_gap: spec.thrash_min_turn_gap,
            maximum_tool_output_tokens: spec.maximum_tool_output_tokens,
            allow_personal_settings: spec.allow_personal_settings,
            allow_personal_memory: spec.allow_personal_memory,
        };
        let mut outcome: BackendOutcome = match backend.run(&prompt, workspace, &options) {
            Ok(o) => o,
            Err(e) => return CellResult::failed(e.to_string(), ""),
        };

        total_tokens_in += outcome.tokens_in;
        total_tokens_out += outcome.tokens_out;
        total_cache_creation_tokens += outcome.cache_creation_tokens;
        total_cache_read_tokens += outcome.cache_read_tokens;
        total_compaction_input_tokens += outcome.compaction_input_tokens;
        total_compaction_count += outcome.compaction_count;
        total_cost_usd += outcome.cost_usd;
        agent_session_id = outcome.agent_session_id.clone().or(agent_session_id);

        // RAL-339: the backend already killed its child process at the
        // compaction boundary itself (a safe stop point) rather than let a
        // thrashing run continue -- fail the cell now, before any bg-job
        // nudge/still-working/budget/proof logic below gets a chance to run,
        // so this never gets mistaken for a normal completion and no proof
        // step ever starts.
        if let Some(detail) = outcome.compaction_thrash {
            return thrash_cell_result(
                spec,
                &detail,
                outcome.summary,
                total_tokens_in,
                total_tokens_out,
                total_cache_creation_tokens,
                total_cache_read_tokens,
                total_compaction_input_tokens,
                total_compaction_count,
                total_cost_usd,
                agent_session_id,
            );
        }

        // RAL-292: a turn that ended with an unresolved backgrounded job
        // (launched but never checked) is a silent-completion bug, not a
        // sanctioned `RALPHUS_STILL_WORKING:` escape hatch -- give the same
        // session a bounded number of nudges to check the job's real result
        // and finish, before giving up on the cell entirely.
        let mut bg_nudge_attempt = 0u32;
        while outcome.abandoned_background_job.is_some()
            && parse_still_working(&outcome.summary).is_none()
        {
            let job = outcome.abandoned_background_job.clone().unwrap_or_default();
            if bg_nudge_attempt >= MAX_BACKGROUND_JOB_NUDGE_ATTEMPTS {
                crate::cartographer::emit_scoped(
                    "runner",
                    "background-job nudge exhausted",
                    "warning",
                    Some("cell"),
                    event_context(spec),
                    serde_json::json!({"attempts": bg_nudge_attempt, "job": job}),
                );
                return CellResult {
                    status: "failed".to_string(),
                    tokens_in: total_tokens_in,
                    tokens_out: total_tokens_out,
                    cache_creation_tokens: total_cache_creation_tokens,
                    cache_read_tokens: total_cache_read_tokens,
                    compaction_input_tokens: total_compaction_input_tokens,
                    compaction_count: total_compaction_count,
                    cost_usd: total_cost_usd,
                    cost_is_estimated: false,
                    summary: outcome.summary,
                    error: Some(format!(
                        "abandoned background job: agent ended its turn without checking the result of a backgrounded job (\"{job}\") after {bg_nudge_attempt} nudge attempts"
                    )),
                    proofed: None,
                    agent_session_id,
                    ghost: None,
                };
            }
            bg_nudge_attempt += 1;
            crate::cartographer::emit_scoped(
                "runner",
                "background-job nudge sent",
                "warning",
                Some("cell"),
                event_context(spec),
                serde_json::json!({
                    "attempt": bg_nudge_attempt,
                    "max_attempts": MAX_BACKGROUND_JOB_NUDGE_ATTEMPTS,
                    "job": job,
                }),
            );
            let nudge_options = RunOptions {
                model: spec.model.as_deref(),
                append_system_prompt: system_prompt.as_deref(),
                resume_agent_session_id: agent_session_id.as_deref(),
                assigned_agent_session_id: None,
                timeout_sec: spec.timeout_sec,
                maximum_context: spec.maximum_context,
                auto_compact_threshold: spec.auto_compact_threshold,
                tool_arg_truncate_chars: spec.tool_arg_truncate_chars,
                thrash_max_compactions: spec.thrash_max_compactions,
                thrash_min_turn_gap: spec.thrash_min_turn_gap,
                maximum_tool_output_tokens: spec.maximum_tool_output_tokens,
                allow_personal_settings: spec.allow_personal_settings,
                allow_personal_memory: spec.allow_personal_memory,
            };
            outcome = match backend.nudge(workspace, &nudge_options) {
                Ok(Some(o)) => o,
                Ok(None) => {
                    crate::cartographer::emit_scoped(
                        "runner",
                        "background-job nudge unsupported",
                        "warning",
                        Some("cell"),
                        event_context(spec),
                        serde_json::json!({"job": job}),
                    );
                    return CellResult {
                        status: "failed".to_string(),
                        tokens_in: total_tokens_in,
                        tokens_out: total_tokens_out,
                        cache_creation_tokens: total_cache_creation_tokens,
                        cache_read_tokens: total_cache_read_tokens,
                        compaction_input_tokens: total_compaction_input_tokens,
                        compaction_count: total_compaction_count,
                        cost_usd: total_cost_usd,
                        cost_is_estimated: false,
                        summary: outcome.summary,
                        error: Some(format!(
                            "abandoned background job: agent ended its turn without checking the result of a backgrounded job (\"{job}\"); this backend does not support nudging"
                        )),
                        proofed: None,
                        agent_session_id,
                        ghost: None,
                    };
                }
                Err(e) => return CellResult::failed(e.to_string(), ""),
            };
            total_tokens_in += outcome.tokens_in;
            total_tokens_out += outcome.tokens_out;
            total_cache_creation_tokens += outcome.cache_creation_tokens;
            total_cache_read_tokens += outcome.cache_read_tokens;
            total_compaction_input_tokens += outcome.compaction_input_tokens;
            total_compaction_count += outcome.compaction_count;
            total_cost_usd += outcome.cost_usd;
            agent_session_id = outcome.agent_session_id.clone().or(agent_session_id);
            if let Some(detail) = outcome.compaction_thrash {
                return thrash_cell_result(
                    spec,
                    &detail,
                    outcome.summary,
                    total_tokens_in,
                    total_tokens_out,
                    total_cache_creation_tokens,
                    total_cache_read_tokens,
                    total_compaction_input_tokens,
                    total_compaction_count,
                    total_cost_usd,
                    agent_session_id,
                );
            }
            crate::cartographer::emit_scoped(
                "runner",
                "background-job nudge outcome",
                "info",
                Some("cell"),
                event_context(spec),
                serde_json::json!({
                    "attempt": bg_nudge_attempt,
                    "still_abandoned": outcome.abandoned_background_job.is_some(),
                }),
            );
        }

        if let Some(reason) = parse_still_working(&outcome.summary) {
            if attempt + 1 < MAX_ASYNC_ATTEMPTS {
                resume_id = outcome.agent_session_id.or(resume_id);
                prompt = still_working_followup(&reason, &outcome.summary, original_prompt);
                continue;
            }
            // Exhausted retries while still reporting in-progress: hard
            // failure for a normal cell; fail-closed (proofed=false)
            // for a proof step -- same "budget exceeded" shape below.
            if spec.proof {
                return CellResult {
                    status: "done".to_string(),
                    tokens_in: total_tokens_in,
                    tokens_out: total_tokens_out,
                    cache_creation_tokens: total_cache_creation_tokens,
                    cache_read_tokens: total_cache_read_tokens,
                    compaction_input_tokens: total_compaction_input_tokens,
                    compaction_count: total_compaction_count,
                    cost_usd: total_cost_usd,
                    cost_is_estimated: false,
                    summary: outcome.summary,
                    error: None,
                    proofed: Some(false),
                    agent_session_id,
                    ghost: None,
                };
            }
            return CellResult {
                status: "failed".to_string(),
                tokens_in: total_tokens_in,
                tokens_out: total_tokens_out,
                cache_creation_tokens: total_cache_creation_tokens,
                cache_read_tokens: total_cache_read_tokens,
                compaction_input_tokens: total_compaction_input_tokens,
                compaction_count: total_compaction_count,
                cost_usd: total_cost_usd,
                cost_is_estimated: false,
                summary: outcome.summary,
                error: Some(format!(
                    "still working after {MAX_ASYNC_ATTEMPTS} attempts: {reason}"
                )),
                proofed: None,
                agent_session_id,
                ghost: None,
            };
        }

        let budget_exceeded =
            budget_exceeded(total_tokens_in, total_tokens_out, spec.budget_tokens);

        if spec.proof {
            let verdict = if budget_exceeded {
                Some(false)
            } else {
                parse_verdict(&outcome.summary)
            };
            let summary = if verdict.is_none() {
                format!("{}\n(no marker found; treated as FAIL)", outcome.summary)
            } else {
                outcome.summary
            };
            return CellResult {
                status: "done".to_string(),
                tokens_in: total_tokens_in,
                tokens_out: total_tokens_out,
                cache_creation_tokens: total_cache_creation_tokens,
                cache_read_tokens: total_cache_read_tokens,
                compaction_input_tokens: total_compaction_input_tokens,
                compaction_count: total_compaction_count,
                cost_usd: total_cost_usd,
                cost_is_estimated: false,
                summary,
                error: None,
                proofed: Some(verdict.unwrap_or(false)),
                agent_session_id,
                ghost: None,
            };
        }

        if budget_exceeded {
            return CellResult {
                status: "failed".to_string(),
                tokens_in: total_tokens_in,
                tokens_out: total_tokens_out,
                cache_creation_tokens: total_cache_creation_tokens,
                cache_read_tokens: total_cache_read_tokens,
                compaction_input_tokens: total_compaction_input_tokens,
                compaction_count: total_compaction_count,
                cost_usd: total_cost_usd,
                cost_is_estimated: false,
                summary: outcome.summary,
                error: Some(format!(
                    "token budget exceeded: used {} tokens, budget was {}",
                    total_tokens_in + total_tokens_out,
                    spec.budget_tokens.unwrap_or(0)
                )),
                proofed: None,
                agent_session_id,
                ghost: None,
            };
        }

        let ghost = match parse_ghost(&outcome.summary) {
            GhostParse::Published(text) => Some(text),
            GhostParse::NoMarker | GhostParse::NothingReported => None,
            GhostParse::Malformed => {
                crate::cartographer::emit(
                    "runner",
                    "ghost rejected: malformed",
                    "warning",
                    event_context(spec),
                    serde_json::json!({
                        "reason": "RALPHUS_GHOST: marker found but the text after it \
                                    didn't parse as a short bullet list",
                    }),
                );
                None
            }
        };

        return CellResult {
            status: "done".to_string(),
            tokens_in: total_tokens_in,
            tokens_out: total_tokens_out,
            cache_creation_tokens: total_cache_creation_tokens,
            cache_read_tokens: total_cache_read_tokens,
            compaction_input_tokens: total_compaction_input_tokens,
            compaction_count: total_compaction_count,
            cost_usd: total_cost_usd,
            cost_is_estimated: false,
            summary: outcome.summary.clone(),
            error: None,
            proofed: None,
            agent_session_id,
            ghost,
        };
    }

    CellResult::failed("unreachable: retry loop exited without returning", "")
}

/// RAL-339: builds the failed `CellResult` for a detected autocompaction
/// thrash condition and emits its Cartographer event, shared by both call
/// sites in [`run_with_backend`] (the main run and the RAL-292 background-job
/// nudge loop) so the event shape and error message stay in exactly one
/// place.
#[allow(clippy::too_many_arguments)]
fn thrash_cell_result(
    spec: &CellSpec,
    detail: &crate::thrash::ThrashDetail,
    summary: String,
    tokens_in: i64,
    tokens_out: i64,
    cache_creation_tokens: i64,
    cache_read_tokens: i64,
    compaction_input_tokens: i64,
    compaction_count: i64,
    cost_usd: f64,
    agent_session_id: Option<String>,
) -> CellResult {
    crate::cartographer::emit(
        "runner",
        "cell failed: autocompaction thrashing",
        "error",
        event_context(spec),
        serde_json::json!({
            "agent": spec.agent,
            "compaction_count": detail.compaction_count,
            "turns_since_previous_compaction": detail.turns_since_previous_compaction,
            "max_compactions": detail.max_compactions,
            "min_turn_gap": detail.min_turn_gap,
        }),
    );
    CellResult {
        status: "failed".to_string(),
        tokens_in,
        tokens_out,
        cache_creation_tokens,
        cache_read_tokens,
        compaction_input_tokens,
        compaction_count,
        cost_usd,
        cost_is_estimated: false,
        summary,
        error: Some(crate::thrash::thrash_error_message(&spec.agent, detail)),
        proofed: None,
        agent_session_id,
        ghost: None,
    }
}

fn combine_system_prompts(parts: &[Option<&str>]) -> Option<String> {
    let combined = parts
        .iter()
        .filter_map(|p| *p)
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    if combined.is_empty() {
        None
    } else {
        Some(combined)
    }
}

fn still_working_followup(reason: &str, previous_summary: &str, original_prompt: &str) -> String {
    format!(
        "You previously reported you were still working, with this reason: {reason}\n\n\
         Tail of your previous output for context:\n{}\n\n\
         Continue and finish the original task:\n{original_prompt}",
        tail(previous_summary, COMMAND_TAIL_CHARS)
    )
}

fn budget_exceeded(tokens_in: i64, tokens_out: i64, budget_tokens: Option<u64>) -> bool {
    match budget_tokens {
        Some(cap) if cap > 0 => (tokens_in + tokens_out).max(0) as u64 > cap,
        _ => false,
    }
}

/// Finds the last occurrence of a `RALPHUS_PROOF: PASS`/`FAIL` marker
/// anywhere in `text` (not requiring it to be alone on its own line -- local
/// models sometimes embed it mid-sentence).
fn parse_verdict(text: &str) -> Option<bool> {
    let pass_idx = text.rfind("RALPHUS_PROOF: PASS");
    let fail_idx = text.rfind("RALPHUS_PROOF: FAIL");
    match (pass_idx, fail_idx) {
        (Some(p), Some(f)) => Some(p > f),
        (Some(_), None) => Some(true),
        (None, Some(_)) => Some(false),
        (None, None) => None,
    }
}

/// Finds the last `RALPHUS_STILL_WORKING: <reason>` marker's reason text
/// (rest of that line).
fn parse_still_working(text: &str) -> Option<String> {
    let idx = text.rfind("RALPHUS_STILL_WORKING:")?;
    let rest = &text[idx + "RALPHUS_STILL_WORKING:".len()..];
    let line = rest.lines().next().unwrap_or("").trim();
    Some(line.to_string())
}

/// Outcome of scanning an agent's final reply for a `RALPHUS_GHOST:` marker.
#[derive(Debug, PartialEq, Eq)]
enum GhostParse {
    /// No marker anywhere in the reply -- the agent didn't attempt to report.
    NoMarker,
    /// The agent explicitly reported nothing to hand off.
    NothingReported,
    /// A marker was found, but what followed it didn't parse as the short
    /// bullet list `GHOST_SYSTEM_PROMPT` asks for -- rejected rather than
    /// carried forward as a partial, unverified fragment.
    Malformed,
    /// Marker found, followed by well-formed bullet lines.
    Published(String),
}

/// Finds the last `RALPHUS_GHOST:` marker and validates that what follows is
/// a short bullet list -- the shape `GHOST_SYSTEM_PROMPT` asks for -- rather
/// than trusting arbitrary trailing text. A model that goes off the rails
/// right after writing the marker (drifting into an unrelated tangent
/// instead of stopping) must not have that tangent carried into the next
/// attempt's prompt: parsing stops at the first line that isn't a bullet,
/// rather than consuming to the end of the reply or [`GHOST_MAX_CHARS`]. An
/// attempt whose marker is followed by no valid bullets at all contributes
/// nothing ([`GhostParse::Malformed`]) -- the same as if it had never
/// published a ghost, since a partial, unverified fragment is not safer than
/// carrying forward nothing.
fn parse_ghost(text: &str) -> GhostParse {
    const MAX_GHOST_BULLETS: usize = 5;
    let Some(idx) = text.rfind("RALPHUS_GHOST:") else {
        return GhostParse::NoMarker;
    };
    let rest = text[idx + "RALPHUS_GHOST:".len()..].trim_start();
    if rest.is_empty() || rest.to_lowercase().starts_with("(nothing to report)") {
        return GhostParse::NothingReported;
    }
    let mut bullets: Vec<&str> = Vec::new();
    for line in rest.lines() {
        let line = line.trim();
        let is_bullet = line.starts_with('-') || line.starts_with('*') || line.starts_with('•');
        if line.is_empty() || !is_bullet {
            break;
        }
        bullets.push(line);
        if bullets.len() >= MAX_GHOST_BULLETS {
            break;
        }
    }
    if bullets.is_empty() {
        return GhostParse::Malformed;
    }
    let joined: String = bullets.join("\n");
    GhostParse::Published(joined.chars().take(GHOST_MAX_CHARS).collect())
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::BackendError;

    #[test]
    fn short_prompt_hash_matches_known_vector_prefix() {
        assert_eq!(short_prompt_hash(b"abc"), "ba7816bf");
    }

    #[test]
    fn parse_verdict_trusts_last_occurrence() {
        assert_eq!(
            parse_verdict("first RALPHUS_PROOF: FAIL then RALPHUS_PROOF: PASS"),
            Some(true)
        );
        assert_eq!(parse_verdict("RALPHUS_PROOF: FAIL"), Some(false));
        assert_eq!(parse_verdict("no marker here"), None);
    }

    #[test]
    fn parse_still_working_extracts_reason() {
        assert_eq!(
            parse_still_working("working...\nRALPHUS_STILL_WORKING: waiting on build\n"),
            Some("waiting on build".to_string())
        );
        assert_eq!(parse_still_working("all done"), None);
    }

    #[test]
    fn parse_ghost_extracts_single_bullet() {
        assert_eq!(
            parse_ghost("done.\nRALPHUS_GHOST: - noted a workaround"),
            GhostParse::Published("- noted a workaround".to_string())
        );
    }

    #[test]
    fn parse_ghost_extracts_multiple_bullets_and_caps_at_five() {
        let reply = "done.\nRALPHUS_GHOST:\n- one\n- two\n- three\n- four\n- five\n- six (dropped)";
        assert_eq!(
            parse_ghost(reply),
            GhostParse::Published("- one\n- two\n- three\n- four\n- five".to_string())
        );
    }

    #[test]
    fn parse_ghost_nothing_reported_is_not_malformed() {
        assert_eq!(
            parse_ghost("done.\nRALPHUS_GHOST: (nothing to report)"),
            GhostParse::NothingReported
        );
    }

    #[test]
    fn parse_ghost_no_marker_is_no_marker() {
        assert_eq!(parse_ghost("no marker"), GhostParse::NoMarker);
    }

    /// Reproduces the real failure this validation exists for: the agent
    /// wrote one legitimate bullet, then drifted into an unrelated tangent
    /// instead of stopping. The tangent must not survive, but the bullet
    /// that came before it is genuine and worth keeping -- parsing stops the
    /// instant the shape breaks, publishing only the clean prefix rather
    /// than either the whole (partly-garbage) blob or nothing at all.
    #[test]
    fn parse_ghost_truncates_at_the_tangent_that_follows_a_real_bullet() {
        let reply = "RALPHUS_GHOST: - noted retire_stale_worktrees has no scheduler caller\n\
                      Wait, I think the user typed \"yourself\" -- let me reconsider...";
        assert_eq!(
            parse_ghost(reply),
            GhostParse::Published(
                "- noted retire_stale_worktrees has no scheduler caller".to_string()
            )
        );
    }

    #[test]
    fn parse_ghost_rejects_free_prose_with_no_bullets() {
        assert_eq!(
            parse_ghost("RALPHUS_GHOST: nothing much happened, all good"),
            GhostParse::Malformed
        );
    }

    #[test]
    fn budget_exceeded_respects_unlimited_and_cap() {
        assert!(!budget_exceeded(100, 100, None));
        assert!(!budget_exceeded(100, 100, Some(0)));
        assert!(budget_exceeded(600, 500, Some(1000)));
        assert!(!budget_exceeded(400, 500, Some(1000)));
    }

    #[test]
    fn combine_system_prompts_joins_non_empty_with_blank_line() {
        assert_eq!(
            combine_system_prompts(&[Some("a"), None, Some("b")]),
            Some("a\n\nb".to_string())
        );
        assert_eq!(combine_system_prompts(&[None, None]), None);
    }

    #[test]
    fn run_command_reports_exit_code_and_output() {
        let dir = std::env::temp_dir().join(format!("ralphus-execute-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let ws = Workspace::create(&dir).unwrap();
        let result = run_command(&ws, "exit 0", None);
        assert!(result.ok());
        let result = run_command(&ws, "exit 7", None);
        assert!(!result.ok());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Asserts the structure of the fully assembled unattended-cell system
    /// prompt (caller-authored fragment + ralphus fragments joined with blank
    /// lines), the form the backend actually receives -- not just the
    /// individual constants.
    fn assert_assembled_prompt_sections(sp: &str, caller_first_line: &str) {
        assert!(
            sp.starts_with(caller_first_line),
            "caller-authored fragment must come first: {sp}"
        );
        let background = sp.find("## Background\n").expect("Background section");
        let tools = sp
            .find("## Regarding Tools\n")
            .expect("Regarding Tools section");
        let conclusion = sp.find("## Conclusion\n").expect("Conclusion section");
        assert!(background < tools && tools < conclusion);
        // Tool guidance: prefer rg, allow grep as fallback, example form.
        assert!(sp.contains("Prefer `rg` for shell searches"), "{sp}");
        assert!(
            sp.contains("use `grep` only when `rg` is unavailable"),
            "{sp}"
        );
        assert!(sp.contains("use `rg \"pattern\" .`"), "{sp}");
        // Synchronous-execution constraints with the exact escape marker.
        assert!(
            sp.contains("never launch the thing you are checking as a background"),
            "{sp}"
        );
        assert!(
            sp.contains("'RALPHUS_STILL_WORKING: <one-line reason>'"),
            "{sp}"
        );
    }

    #[test]
    fn assembled_unattended_cell_prompt_is_sectioned_and_gated_on_markers() {
        let mut spec = test_spec(false);
        spec.system_prompt = Some("Do NOT commit and do NOT push under any circumstances.".into());
        let ws = Workspace::create(std::env::temp_dir()).unwrap();
        let backend = ScriptedBackend::new(
            vec![Ok(BackendOutcome {
                summary: "all done".to_string(),
                ..Default::default()
            })],
            vec![],
        );

        let result = run_with_backend(&spec, "do the thing", &ws, &backend);

        assert!(result.ok());
        let sp = backend
            .last_system_prompt
            .borrow()
            .clone()
            .expect("run() should have received the assembled system prompt");
        assert_assembled_prompt_sections(
            &sp,
            "Do NOT commit and do NOT push under any circumstances.",
        );
        // Ghost handoff contract (this is a non-proof cell) with the exact
        // marker and the 5-bullet cap, and no proof marker text.
        assert!(
            sp.contains("exact marker 'RALPHUS_GHOST:', followed by up to 5 short"),
            "{sp}"
        );
        assert!(sp.contains("'RALPHUS_GHOST: (nothing to report)'"), "{sp}");
        assert!(!sp.contains("RALPHUS_PROOF:"), "{sp}");
        // No human follow-up, but Ralphus itself may re-invoke synchronously.
        assert!(sp.contains("no human will check back on you"), "{sp}");
        assert!(
            sp.contains("Ralphus may re-invoke you synchronously"),
            "{sp}"
        );
    }

    #[test]
    fn assembled_proof_prompt_is_sectioned_and_gated_on_verdict_markers() {
        let mut spec = test_spec(true);
        spec.system_prompt = Some("Do NOT commit and do NOT push under any circumstances.".into());
        let ws = Workspace::create(std::env::temp_dir()).unwrap();
        let backend = ScriptedBackend::new(
            vec![Ok(BackendOutcome {
                summary: "checked\nRALPHUS_PROOF: PASS".to_string(),
                ..Default::default()
            })],
            vec![],
        );

        let result = run_with_backend(&spec, "do the thing", &ws, &backend);

        assert!(result.ok());
        assert_eq!(result.proofed, Some(true));
        let sp = backend
            .last_system_prompt
            .borrow()
            .clone()
            .expect("run() should have received the assembled system prompt");
        assert_assembled_prompt_sections(
            &sp,
            "Do NOT commit and do NOT push under any circumstances.",
        );
        // Proof steps get the verdict contract, not the ghost handoff.
        assert!(sp.contains("RALPHUS_PROOF: PASS"), "{sp}");
        assert!(sp.contains("RALPHUS_PROOF: FAIL"), "{sp}");
        assert!(!sp.contains("RALPHUS_GHOST:"), "{sp}");
    }

    /// A [`ModelBackend`] stub whose `run`/`nudge` outcomes are scripted in
    /// advance, so the RAL-292 background-job nudge loop in
    /// [`run_with_backend`] can be exercised without a real CLI subprocess.
    struct ScriptedBackend {
        run_outcomes:
            std::cell::RefCell<std::collections::VecDeque<Result<BackendOutcome, BackendError>>>,
        nudge_outcomes: std::cell::RefCell<
            std::collections::VecDeque<Result<Option<BackendOutcome>, BackendError>>,
        >,
        nudge_calls: std::cell::Cell<u32>,
        last_system_prompt: std::cell::RefCell<Option<String>>,
    }

    impl ScriptedBackend {
        fn new(
            run_outcomes: Vec<Result<BackendOutcome, BackendError>>,
            nudge_outcomes: Vec<Result<Option<BackendOutcome>, BackendError>>,
        ) -> Self {
            Self {
                run_outcomes: std::cell::RefCell::new(run_outcomes.into()),
                nudge_outcomes: std::cell::RefCell::new(nudge_outcomes.into()),
                nudge_calls: std::cell::Cell::new(0),
                last_system_prompt: std::cell::RefCell::new(None),
            }
        }
    }

    impl ModelBackend for ScriptedBackend {
        fn run(
            &self,
            _prompt: &str,
            _workspace: &Workspace,
            options: &RunOptions<'_>,
        ) -> Result<BackendOutcome, BackendError> {
            *self.last_system_prompt.borrow_mut() =
                options.append_system_prompt.map(str::to_string);
            self.run_outcomes
                .borrow_mut()
                .pop_front()
                .expect("run() called more times than scripted")
        }

        fn nudge(
            &self,
            _workspace: &Workspace,
            _options: &RunOptions<'_>,
        ) -> Result<Option<BackendOutcome>, BackendError> {
            self.nudge_calls.set(self.nudge_calls.get() + 1);
            self.nudge_outcomes
                .borrow_mut()
                .pop_front()
                .expect("nudge() called more times than scripted")
        }
    }

    fn abandoned_outcome(job: &str) -> BackendOutcome {
        BackendOutcome {
            summary: "did some work".to_string(),
            abandoned_background_job: Some(job.to_string()),
            ..Default::default()
        }
    }

    fn test_spec(proof: bool) -> CellSpec {
        CellSpec {
            squad_id: "sq".into(),
            task: "t".into(),
            cell_id: "c".into(),
            cwd: std::env::temp_dir().display().to_string(),
            prompt: Some("do the thing".into()),
            command: None,
            agent: "claude".into(),
            executable: None,
            model: None,
            system_prompt: None,
            system_prompt_position: None,
            args: vec![],
            budget_tokens: None,
            maximum_context: None,
            auto_compact_threshold: None,
            timeout_sec: None,
            proof,
            trace_context: None,
            resume_agent_session_id: None,
            assigned_agent_session_id: None,
            tool_arg_truncate_chars: None,
            thrash_max_compactions: None,
            thrash_min_turn_gap: None,
            maximum_tool_output_tokens: None,
            allow_personal_settings: false,
            allow_personal_memory: false,
        }
    }

    #[test]
    fn background_job_nudge_loop_fails_the_cell_after_the_per_cell_try_cap() {
        let spec = test_spec(false);
        let ws = Workspace::create(std::env::temp_dir()).unwrap();
        let backend = ScriptedBackend::new(
            vec![Ok(abandoned_outcome("bash job"))],
            vec![
                Ok(Some(abandoned_outcome("bash job"))),
                Ok(Some(abandoned_outcome("bash job"))),
                Ok(Some(abandoned_outcome("bash job"))),
            ],
        );

        let result = run_with_backend(&spec, "do the thing", &ws, &backend);

        assert_eq!(backend.nudge_calls.get(), MAX_BACKGROUND_JOB_NUDGE_ATTEMPTS);
        assert!(!result.ok());
        let error = result.error.unwrap();
        assert!(error.contains("abandoned background job"), "{error}");
        assert!(error.contains("bash job"), "{error}");
        assert!(error.contains("3 nudge attempts"), "{error}");
    }

    /// RAL-339: a backend that detected autocompaction thrashing must fail
    /// the cell immediately with everything it captured live, and must never
    /// reach the bg-job-nudge/still-working/budget/proof logic below it --
    /// the `nudge_calls` assertion below is what proves that.
    #[test]
    fn run_with_backend_fails_the_cell_immediately_on_compaction_thrash() {
        let spec = test_spec(false);
        let ws = Workspace::create(std::env::temp_dir()).unwrap();
        let detail = crate::thrash::ThrashDetail {
            compaction_count: 3,
            turns_since_previous_compaction: 0,
            max_compactions: 3,
            min_turn_gap: 2,
        };
        let backend = ScriptedBackend::new(
            vec![Ok(BackendOutcome {
                summary: "partial work before the thrashing compaction".to_string(),
                tokens_in: 111,
                tokens_out: 222,
                cost_usd: 0.5,
                agent_session_id: Some("sess-thrash".to_string()),
                compaction_thrash: Some(detail),
                ..Default::default()
            })],
            vec![],
        );

        let result = run_with_backend(&spec, "do the thing", &ws, &backend);

        assert_eq!(backend.nudge_calls.get(), 0);
        assert!(!result.ok());
        assert_eq!(result.tokens_in, 111);
        assert_eq!(result.tokens_out, 222);
        assert!((result.cost_usd - 0.5).abs() < f64::EPSILON);
        assert_eq!(
            result.summary,
            "partial work before the thrashing compaction"
        );
        assert_eq!(result.agent_session_id.as_deref(), Some("sess-thrash"));
        let error = result.error.unwrap();
        assert!(error.contains("autocompaction thrashing"), "{error}");
        assert!(error.contains("claude"), "{error}"); // spec.agent from test_spec
    }

    /// RAL-339: a healthy run (no thrash) must be entirely unaffected by the
    /// new check -- same shape as before this change.
    #[test]
    fn run_with_backend_succeeds_normally_when_no_thrash_is_reported() {
        let spec = test_spec(false);
        let ws = Workspace::create(std::env::temp_dir()).unwrap();
        let backend = ScriptedBackend::new(
            vec![Ok(BackendOutcome {
                summary: "all done".to_string(),
                compaction_thrash: None,
                ..Default::default()
            })],
            vec![],
        );

        let result = run_with_backend(&spec, "do the thing", &ws, &backend);

        assert!(result.ok());
        assert_eq!(result.summary, "all done");
    }

    #[test]
    fn background_job_nudge_loop_recovers_when_a_later_nudge_resolves_the_job() {
        let spec = test_spec(false);
        let ws = Workspace::create(std::env::temp_dir()).unwrap();
        let backend = ScriptedBackend::new(
            vec![Ok(abandoned_outcome("bash job"))],
            vec![Ok(Some(BackendOutcome {
                summary: "checked the job, all good".to_string(),
                abandoned_background_job: None,
                ..Default::default()
            }))],
        );

        let result = run_with_backend(&spec, "do the thing", &ws, &backend);

        assert_eq!(backend.nudge_calls.get(), 1);
        assert!(result.ok());
        assert_eq!(result.summary, "checked the job, all good");
    }

    #[test]
    fn background_job_nudge_loop_fails_immediately_when_the_backend_cannot_nudge() {
        let spec = test_spec(false);
        let ws = Workspace::create(std::env::temp_dir()).unwrap();
        let backend = ScriptedBackend::new(vec![Ok(abandoned_outcome("bash job"))], vec![Ok(None)]);

        let result = run_with_backend(&spec, "do the thing", &ws, &backend);

        assert_eq!(backend.nudge_calls.get(), 1);
        assert!(!result.ok());
        let error = result.error.unwrap();
        assert!(error.contains("does not support nudging"), "{error}");
    }

    /// A minimal [`ModelBackend`] whose `supports_maximum_context`/
    /// `supports_auto_compact_threshold` are independently toggleable, so
    /// [`check_context_limit_support`]'s field-by-field gate can be exercised
    /// without a real CLI subprocess. `run`/`nudge` are never called by these
    /// tests -- the check fails closed before either would run.
    struct LimitsStubBackend {
        max_context: bool,
        auto_compact: bool,
    }

    impl ModelBackend for LimitsStubBackend {
        fn run(
            &self,
            _prompt: &str,
            _workspace: &Workspace,
            _options: &RunOptions<'_>,
        ) -> Result<BackendOutcome, BackendError> {
            unreachable!("check_context_limit_support tests never call run()")
        }

        fn supports_maximum_context(&self) -> bool {
            self.max_context
        }

        fn supports_auto_compact_threshold(&self) -> bool {
            self.auto_compact
        }
    }

    #[test]
    fn check_context_limit_support_rejects_maximum_context_when_unsupported() {
        let mut spec = test_spec(false);
        spec.maximum_context = Some(100_000);
        let backend = LimitsStubBackend {
            max_context: false,
            auto_compact: false,
        };
        let err = check_context_limit_support(&spec, &backend).unwrap_err();
        assert!(err.contains("does not support maximum_context"), "{err}");
    }

    #[test]
    fn check_context_limit_support_rejects_auto_compact_threshold_when_unsupported() {
        let mut spec = test_spec(false);
        spec.auto_compact_threshold = Some(80_000);
        let backend = LimitsStubBackend {
            max_context: false,
            auto_compact: false,
        };
        let err = check_context_limit_support(&spec, &backend).unwrap_err();
        assert!(
            err.contains("does not support auto_compact_threshold"),
            "{err}"
        );
    }

    #[test]
    fn check_context_limit_support_allows_auto_compact_threshold_without_maximum_context_support() {
        // Mirrors the claude-code backend's real shape: it supports
        // auto_compact_threshold but not maximum_context, and the two fields
        // must be checked independently rather than as a pair.
        let mut spec = test_spec(false);
        spec.auto_compact_threshold = Some(80_000);
        let backend = LimitsStubBackend {
            max_context: false,
            auto_compact: true,
        };
        assert!(check_context_limit_support(&spec, &backend).is_ok());
    }

    #[test]
    fn run_cell_fails_closed_when_neither_prompt_nor_command() {
        let dir = std::env::temp_dir().join(format!(
            "ralphus-execute-test-neither-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let spec = CellSpec {
            squad_id: "r".into(),
            task: "t".into(),
            cell_id: "s".into(),
            cwd: dir.display().to_string(),
            prompt: None,
            command: None,
            agent: "claude".into(),
            executable: None,
            model: None,
            system_prompt: None,
            system_prompt_position: None,
            args: vec![],
            budget_tokens: None,
            maximum_context: None,
            auto_compact_threshold: None,
            timeout_sec: None,
            proof: false,
            trace_context: None,
            resume_agent_session_id: None,
            assigned_agent_session_id: None,
            tool_arg_truncate_chars: None,
            thrash_max_compactions: None,
            thrash_min_turn_gap: None,
            maximum_tool_output_tokens: None,
            allow_personal_settings: false,
            allow_personal_memory: false,
        };
        let result = run_cell(&spec, false);
        assert!(!result.ok());
        std::fs::remove_dir_all(&dir).ok();
    }
}
