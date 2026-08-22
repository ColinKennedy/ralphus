//! Turns a [`SessionSpec`] into a [`SessionResult`], ported from
//! `cli/src/ralphus/runner/execute.py`. The single choke point that
//! dispatches `command` sessions (no model), and wraps `prompt`/verify
//! sessions with system-prompt composition, the still-working retry loop,
//! and verdict/ghost marker parsing.

use crate::agent_backend::AgentBackend;
use crate::backend::{BackendOutcome, ModelBackend, RunOptions};
use crate::claude_code_backend::ClaudeCodeBackend;
use crate::codex_backend::CodexBackend;
use crate::harness_backend::HarnessBackend;
use crate::spec::{SessionResult, SessionSpec};
use crate::tools::Workspace;

// These four constants must stay byte-identical to
// `daemon/src/runner.rs`'s `VERIFY_SYSTEM_PROMPT`/`GHOST_SYSTEM_PROMPT`/
// `ASYNC_SYSTEM_PROMPT`/`NON_INTERACTIVE_SYSTEM_PROMPT` -- that file's own
// comment says the same about staying in sync with this one.
const VERIFY_SYSTEM_PROMPT: &str = "This is a VERIFICATION step, not a normal task. Investigate whether the \
     task holds, attempting to fix any problems you find so the check passes \
     if you can reasonably do so. When you are done, your FINAL line of \
     output must be exactly one of:\nRALPHUS_VERIFY: PASS\nRALPHUS_VERIFY: FAIL\nwith \
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
const ASYNC_SYSTEM_PROMPT: &str = "This is a single, non-interactive invocation with no later turn — \
     nothing will check back on you. Never use an asynchronous/background/\
     'notify me later' tool for anything this session depends on, and never \
     launch the thing you are checking as a background/detached process and \
     end your turn while it is still running; those require a persistent \
     session this invocation does not have. If a check genuinely takes a long \
     time, block and wait for it synchronously in the foreground within this \
     same turn — it is fine for that to take a long time. If, despite that, \
     you truly cannot reach a definitive result before you must stop, end \
     your reply with 'RALPHUS_STILL_WORKING: <one-line reason>' as the last \
     line instead of trailing off — you will be re-invoked shortly to \
     continue synchronously from where you left off, though only a bounded \
     number of times, so prefer just finishing the check yourself.";
const NON_INTERACTIVE_SYSTEM_PROMPT: &str = "You are running unattended in a non-interactive session — no human is \
     available to answer questions or approve a plan. Never ask a clarifying \
     question, never stop to present a plan for confirmation, and never pause \
     waiting for input. Make the most reasonable judgment call yourself and \
     continue until the task is complete.";

const MAX_ASYNC_ATTEMPTS: u32 = 3;
const GHOST_MAX_CHARS: usize = 4000;
const COMMAND_TAIL_CHARS: usize = 2000;

/// Resolves an agent name to its backend, mirroring the three-way dispatch
/// documented in `runner/__main__.py::_load_backend`: `claude-code`/`codex`
/// drive an external CLI with stream parsing; `claude`/`anthropic`/`ollama`
/// use the hand-rolled tool loop; anything else is treated as a generic
/// external harness program.
fn load_backend(agent: &str, keep_temporary_files: bool) -> Box<dyn ModelBackend> {
    match agent {
        "claude-code" | "claude-cli" => Box::new(ClaudeCodeBackend {
            keep_temporary_files,
        }),
        "codex" | "codex-cli" => Box::new(CodexBackend {
            keep_temporary_files,
        }),
        "claude" | "anthropic" | "ollama" => Box::new(AgentBackend {
            agent: agent.to_string(),
        }),
        other => Box::new(HarnessBackend {
            program: other.to_string(),
        }),
    }
}

/// Runs one session end-to-end. Never panics on a bad workspace/backend --
/// every failure mode becomes a `"failed"` [`SessionResult`], matching
/// Python's own behavior of always producing a result rather than letting an
/// exception escape to a nonzero process exit with no JSON on stdout.
#[must_use]
pub fn run_session(spec: &SessionSpec, keep_temporary_files: bool) -> SessionResult {
    let workspace = match Workspace::create(&spec.cwd) {
        Ok(w) => w,
        Err(e) => return SessionResult::failed(e.to_string(), ""),
    };

    if let Some(command) = &spec.command {
        return run_command(&workspace, command, spec.timeout_sec);
    }

    let Some(prompt) = &spec.prompt else {
        return SessionResult::failed("spec has neither prompt nor command", "");
    };

    run_prompt(spec, prompt, &workspace, keep_temporary_files)
}

fn run_command(workspace: &Workspace, command: &str, timeout_sec: Option<u64>) -> SessionResult {
    match workspace.run_bash(command, timeout_sec) {
        Ok(out) if out.ok() => SessionResult::done(tail(&out.stdout, COMMAND_TAIL_CHARS)),
        Ok(out) => {
            let detail = tail(&out.stderr, COMMAND_TAIL_CHARS);
            let detail = if detail.is_empty() {
                tail(&out.stdout, COMMAND_TAIL_CHARS)
            } else {
                detail
            };
            SessionResult::failed(format!("command exited {}", out.exit_code), detail)
        }
        Err(e) => SessionResult::failed(e.to_string(), ""),
    }
}

fn run_prompt(
    spec: &SessionSpec,
    original_prompt: &str,
    workspace: &Workspace,
    keep_temporary_files: bool,
) -> SessionResult {
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

fn log_llm_start(spec: &SessionSpec, prompt: &str) {
    let kind = if spec.verify { "verify " } else { "" };
    eprintln!(
        "ralphus [llm] {kind}start run={} session={} agent={:?} model={:?} prompt_len={} prompt_hash={}",
        spec.run_id,
        spec.session_id,
        spec.agent,
        spec.model,
        prompt.len(),
        short_prompt_hash(prompt.as_bytes())
    );
}

fn log_llm_done(spec: &SessionSpec, result: &SessionResult) {
    let kind = if spec.verify { "verify " } else { "" };
    if result.ok() {
        eprintln!(
            "ralphus [llm] {kind}done run={} session={} tokens_in={} tokens_out={} cost_usd={:.4}",
            spec.run_id, spec.session_id, result.tokens_in, result.tokens_out, result.cost_usd
        );
    } else {
        eprintln!(
            "ralphus [llm] {kind}error run={} session={}: {}",
            spec.run_id,
            spec.session_id,
            result.error.as_deref().unwrap_or("unknown error")
        );
    }
}

fn run_prompt_inner(
    spec: &SessionSpec,
    original_prompt: &str,
    workspace: &Workspace,
    keep_temporary_files: bool,
) -> SessionResult {
    let backend = load_backend(&spec.agent, keep_temporary_files);
    let system_prompt = combine_system_prompts(&[
        spec.system_prompt.as_deref(),
        Some(NON_INTERACTIVE_SYSTEM_PROMPT),
        Some(ASYNC_SYSTEM_PROMPT),
        Some(if spec.verify {
            VERIFY_SYSTEM_PROMPT
        } else {
            GHOST_SYSTEM_PROMPT
        }),
    ]);
    if let Some(sp) = &system_prompt {
        eprintln!(
            "ralphus [llm] system-prompt applied len={} position={:?}",
            sp.len(),
            spec.system_prompt_position
        );
    }

    let mut prompt = original_prompt.to_string();
    let mut resume_id = spec.resume_agent_session_id.clone();
    let mut total_tokens_in = 0i64;
    let mut total_tokens_out = 0i64;
    let mut total_cost_usd = 0.0f64;
    let mut agent_session_id: Option<String> = None;

    for attempt in 0..MAX_ASYNC_ATTEMPTS {
        let options = RunOptions {
            model: spec.model.as_deref(),
            append_system_prompt: system_prompt.as_deref(),
            resume_agent_session_id: resume_id.as_deref(),
            timeout_sec: spec.timeout_sec,
        };
        let outcome: BackendOutcome = match backend.run(&prompt, workspace, &options) {
            Ok(o) => o,
            Err(e) => return SessionResult::failed(e.to_string(), ""),
        };

        total_tokens_in += outcome.tokens_in;
        total_tokens_out += outcome.tokens_out;
        total_cost_usd += outcome.cost_usd;
        agent_session_id = outcome.agent_session_id.clone().or(agent_session_id);

        if let Some(reason) = parse_still_working(&outcome.summary) {
            if attempt + 1 < MAX_ASYNC_ATTEMPTS {
                resume_id = outcome.agent_session_id.or(resume_id);
                prompt = still_working_followup(&reason, &outcome.summary, original_prompt);
                continue;
            }
            // Exhausted retries while still reporting in-progress: hard
            // failure for a normal session; fail-closed (verified=false)
            // for a verify step -- same "budget exceeded" shape below.
            if spec.verify {
                return SessionResult {
                    status: "done".to_string(),
                    tokens_in: total_tokens_in,
                    tokens_out: total_tokens_out,
                    cost_usd: total_cost_usd,
                    summary: outcome.summary,
                    error: None,
                    verified: Some(false),
                    agent_session_id,
                    ghost: None,
                };
            }
            return SessionResult {
                status: "failed".to_string(),
                tokens_in: total_tokens_in,
                tokens_out: total_tokens_out,
                cost_usd: total_cost_usd,
                summary: outcome.summary,
                error: Some(format!(
                    "still working after {MAX_ASYNC_ATTEMPTS} attempts: {reason}"
                )),
                verified: None,
                agent_session_id,
                ghost: None,
            };
        }

        let budget_exceeded =
            budget_exceeded(total_tokens_in, total_tokens_out, spec.budget_tokens);

        if spec.verify {
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
            return SessionResult {
                status: "done".to_string(),
                tokens_in: total_tokens_in,
                tokens_out: total_tokens_out,
                cost_usd: total_cost_usd,
                summary,
                error: None,
                verified: Some(verdict.unwrap_or(false)),
                agent_session_id,
                ghost: None,
            };
        }

        if budget_exceeded {
            return SessionResult {
                status: "failed".to_string(),
                tokens_in: total_tokens_in,
                tokens_out: total_tokens_out,
                cost_usd: total_cost_usd,
                summary: outcome.summary,
                error: Some(format!(
                    "token budget exceeded: used {} tokens, budget was {}",
                    total_tokens_in + total_tokens_out,
                    spec.budget_tokens.unwrap_or(0)
                )),
                verified: None,
                agent_session_id,
                ghost: None,
            };
        }

        return SessionResult {
            status: "done".to_string(),
            tokens_in: total_tokens_in,
            tokens_out: total_tokens_out,
            cost_usd: total_cost_usd,
            summary: outcome.summary.clone(),
            error: None,
            verified: None,
            agent_session_id,
            ghost: parse_ghost(&outcome.summary),
        };
    }

    SessionResult::failed("unreachable: retry loop exited without returning", "")
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

/// Finds the last occurrence of a `RALPHUS_VERIFY: PASS`/`FAIL` marker
/// anywhere in `text` (not requiring it to be alone on its own line -- local
/// models sometimes embed it mid-sentence).
fn parse_verdict(text: &str) -> Option<bool> {
    let pass_idx = text.rfind("RALPHUS_VERIFY: PASS");
    let fail_idx = text.rfind("RALPHUS_VERIFY: FAIL");
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

/// Finds the last `RALPHUS_GHOST:` marker and returns everything after it
/// (to end of text), capped at [`GHOST_MAX_CHARS`]. `None` if empty or the
/// agent explicitly reported nothing to hand off.
fn parse_ghost(text: &str) -> Option<String> {
    let idx = text.rfind("RALPHUS_GHOST:")?;
    let rest = text[idx + "RALPHUS_GHOST:".len()..].trim();
    if rest.is_empty() || rest.to_lowercase().starts_with("(nothing to report)") {
        return None;
    }
    Some(rest.chars().take(GHOST_MAX_CHARS).collect())
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

    #[test]
    fn short_prompt_hash_matches_known_vector_prefix() {
        assert_eq!(short_prompt_hash(b"abc"), "ba7816bf");
    }

    #[test]
    fn parse_verdict_trusts_last_occurrence() {
        assert_eq!(
            parse_verdict("first RALPHUS_VERIFY: FAIL then RALPHUS_VERIFY: PASS"),
            Some(true)
        );
        assert_eq!(parse_verdict("RALPHUS_VERIFY: FAIL"), Some(false));
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
    fn parse_ghost_extracts_and_caps() {
        assert_eq!(
            parse_ghost("done.\nRALPHUS_GHOST: - noted a workaround"),
            Some("- noted a workaround".to_string())
        );
        assert_eq!(
            parse_ghost("done.\nRALPHUS_GHOST: (nothing to report)"),
            None
        );
        assert_eq!(parse_ghost("no marker"), None);
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

    #[test]
    fn run_session_fails_closed_when_neither_prompt_nor_command() {
        let dir = std::env::temp_dir().join(format!(
            "ralphus-execute-test-neither-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let spec = SessionSpec {
            run_id: "r".into(),
            task: "t".into(),
            session_id: "s".into(),
            cwd: dir.display().to_string(),
            prompt: None,
            command: None,
            agent: "claude".into(),
            model: None,
            system_prompt: None,
            system_prompt_position: None,
            args: vec![],
            budget_tokens: None,
            timeout_sec: None,
            verify: false,
            trace_context: None,
            resume_agent_session_id: None,
        };
        let result = run_session(&spec, false);
        assert!(!result.ok());
        std::fs::remove_dir_all(&dir).ok();
    }
}
