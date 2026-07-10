//! Invoking the Python session runner.
//!
//! The daemon builds a [`RunnerSpec`] for each session, hands it to a [`Runner`],
//! and gets back a [`RunnerResult`]. The real implementation
//! ([`SubprocessRunner`]) spawns the `ralphus-runner` process and speaks the JSON
//! contract in `cli/src/ralphus/runner/spec.py`. The trait keeps the scheduler
//! testable with an in-process fake.

use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::cancel::CancelToken;
use crate::procreg::ProcRegistry;
use crate::store::{NodeState, SessionRow};

/// The JSON spec sent to the runner on stdin (mirrors Python `SessionSpec`).
#[derive(Debug, Clone, Serialize)]
pub struct RunnerSpec {
    /// Owning run id.
    pub run_id: String,
    /// Owning task name.
    pub task: String,
    /// Session id.
    pub session_id: String,
    /// Working directory.
    pub cwd: String,
    /// AI prompt, if this is a prompt session.
    pub prompt: Option<String>,
    /// Shell command, if this is a command session.
    pub command: Option<String>,
    /// Agent program.
    pub agent: String,
    /// Model, if any.
    pub model: Option<String>,
    /// System-prompt text to deliver as an *appended* system prompt (via the
    /// backend's own flag, e.g. Claude Code's `--append-system-prompt`) rather
    /// than concatenated into `prompt`. `None` when unset (RAL-5).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    /// Placement of `system_prompt` — only `"append"` today. `None` when unset.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_prompt_position: Option<String>,
    /// Wall-clock timeout in seconds; the daemon kills the runner subprocess if
    /// it exceeds this. `None` means no limit (RAL-15).
    pub timeout_sec: Option<u64>,
    /// Total-token budget; the runner fails the session if usage exceeds it.
    /// `None` means no cap (RAL-15).
    pub budget_tokens: Option<u64>,
    /// True when this spec is an `agent`-kind verify step rather than a
    /// normal session: the runner wraps `prompt` with verdict-reporting
    /// instructions and returns a `verified` result instead of just "ran".
    pub verify: bool,
}

impl RunnerSpec {
    /// Build a spec from a stored session row.
    ///
    /// When the session declares `subprojects`, a system-prompt addendum is
    /// injected to tell the agent to confine its edits to those subdirectories
    /// (RAL-23). The addendum is appended after any user-supplied system prompt.
    #[must_use]
    pub fn from_row(run_id: &str, row: &SessionRow) -> Self {
        let subproject_addendum = if row.subprojects.is_empty() {
            None
        } else {
            let list = row
                .subprojects
                .iter()
                .map(|p| format!("`{p}`"))
                .collect::<Vec<_>>()
                .join(", ");
            Some(format!(
                "This repository is a monorepo. Your task is scoped to the \
                 following package(s): {list}. Focus your edits on those \
                 subdirectories; the entire repository is accessible but your \
                 changes must stay within those paths unless the task \
                 explicitly requires otherwise."
            ))
        };
        let system_prompt = match (row.system_prompt.clone(), subproject_addendum) {
            (Some(existing), Some(addendum)) => {
                let combined = format!("{existing}\n\n{addendum}");
                crate::rlog!(
                    DEBUG,
                    "ralphus [spec] session {} system-prompt: user-supplied ({} chars) + subproject addendum → combined ({} chars)",
                    row.session_id,
                    existing.len(),
                    combined.len()
                );
                Some(combined)
            }
            (Some(existing), None) => {
                crate::rlog!(
                    DEBUG,
                    "ralphus [spec] session {} system-prompt: borrowed from session config ({} chars)",
                    row.session_id,
                    existing.len()
                );
                Some(existing)
            }
            (None, Some(addendum)) => {
                crate::rlog!(
                    DEBUG,
                    "ralphus [spec] session {} system-prompt: synthesised from subprojects ({} chars, {:?})",
                    row.session_id,
                    addendum.len(),
                    row.subprojects
                );
                Some(addendum)
            }
            (None, None) => None,
        };
        // Carry the stored position through; when the subproject injection
        // creates a system prompt where none existed before, record "append" so
        // the wire contract is self-consistent.
        let system_prompt_position = row
            .system_prompt_position
            .clone()
            .or_else(|| system_prompt.as_ref().map(|_| "append".to_string()));
        Self {
            run_id: run_id.to_string(),
            task: row.task_name.clone(),
            session_id: row.session_id.clone(),
            cwd: row.cwd.clone().unwrap_or_default(),
            prompt: row.prompt.clone(),
            command: row.command.clone(),
            agent: row.agent.clone(),
            model: row.model.clone(),
            system_prompt,
            system_prompt_position,
            timeout_sec: row.timeout_sec.and_then(|s| u64::try_from(s).ok()),
            budget_tokens: row.budget_tokens.and_then(|b| u64::try_from(b).ok()),
            verify: false,
        }
    }

    /// Build a spec for an `agent`-kind verify step. `agent`/`model` are the
    /// owning session's resolved backend, with `model` overridden by the
    /// verify step's own `model` field when set.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn for_verify(
        run_id: &str,
        task: &str,
        session_id: &str,
        cwd: &str,
        prompt: &str,
        agent: &str,
        model: Option<&str>,
        timeout_sec: Option<u64>,
        budget_tokens: Option<u64>,
    ) -> Self {
        Self {
            run_id: run_id.to_string(),
            task: task.to_string(),
            session_id: session_id.to_string(),
            cwd: cwd.to_string(),
            prompt: Some(prompt.to_string()),
            command: None,
            agent: agent.to_string(),
            model: model.map(str::to_string),
            // Verify steps do not carry a session's appended system prompt.
            system_prompt: None,
            system_prompt_position: None,
            timeout_sec,
            budget_tokens,
            verify: true,
        }
    }
}

/// The JSON result read from the runner's stdout (mirrors Python `SessionResult`).
#[derive(Debug, Clone, Deserialize)]
pub struct RunnerResult {
    /// `"done"` or `"failed"`.
    pub status: String,
    /// Input tokens used.
    #[serde(default)]
    pub tokens_in: i64,
    /// Output tokens used.
    #[serde(default)]
    pub tokens_out: i64,
    /// Cost in USD.
    #[serde(default)]
    pub cost_usd: f64,
    /// Short summary of what happened.
    #[serde(default)]
    pub summary: String,
    /// Error detail when failed.
    #[serde(default)]
    pub error: Option<String>,
    /// For an `agent`-kind verify run: the parsed PASS/FAIL verdict. `None`
    /// for a normal session, or when the verifier ran but produced no
    /// parseable verdict (the runner treats that case as a fail already, so
    /// this is `Some(false)` far more often than `None` in practice).
    #[serde(default)]
    pub verified: Option<bool>,
    /// The Claude Code session UUID emitted by the claude-code backend, for
    /// `claude --resume`. `None` for non-claude-code sessions or when the
    /// runner could not extract it.
    #[serde(default)]
    pub claude_session_id: Option<String>,
}

impl RunnerResult {
    /// A synthetic failure (e.g. the runner process could not be spawned).
    #[must_use]
    pub fn failure(error: impl Into<String>) -> Self {
        Self {
            status: "failed".to_string(),
            tokens_in: 0,
            tokens_out: 0,
            cost_usd: 0.0,
            summary: String::new(),
            error: Some(error.into()),
            verified: None,
            claude_session_id: None,
        }
    }

    /// Whether the session succeeded.
    #[must_use]
    pub fn is_done(&self) -> bool {
        self.status == "done"
    }

    /// Map to a node state.
    #[must_use]
    pub fn node_state(&self) -> NodeState {
        if self.is_done() {
            NodeState::Done
        } else {
            NodeState::Failed
        }
    }

    /// Whether an `agent`-kind verify step passed: the runner itself must
    /// have completed (not crashed) *and* reported a true verdict. A verifier
    /// that ran but could not be parsed for a verdict, or that crashed
    /// outright, counts as not-passed — fail closed.
    #[must_use]
    pub fn verify_passed(&self) -> bool {
        self.is_done() && self.verified.unwrap_or(false)
    }
}

/// Something that can execute a session. Send + Sync so worker threads can share it.
pub trait Runner: Send + Sync {
    /// Execute a session and report its result.
    fn run(&self, spec: &RunnerSpec) -> RunnerResult;

    /// Like [`run`](Runner::run), but aborts — killing any spawned
    /// subprocess — as soon as `cancel` trips. The default ignores
    /// cancellation and just calls [`run`](Runner::run); the real
    /// [`SubprocessRunner`] overrides it to poll the token and kill its child.
    fn run_cancellable(&self, spec: &RunnerSpec, _cancel: &CancelToken) -> RunnerResult {
        self.run(spec)
    }
}

/// Runs a session by spawning the configured runner program and speaking JSON
/// over stdin/stdout.
pub struct SubprocessRunner {
    program: String,
    args: Vec<String>,
    /// When set, each spawned child's PID is registered here for the lifetime of
    /// the session so the resource view can attribute OS metrics to it (RAL-11).
    registry: Option<ProcRegistry>,
}

impl SubprocessRunner {
    /// Build from a command line like `"ralphus-runner"` or `"python -m ralphus.runner"`.
    #[must_use]
    pub fn new(command_line: &str) -> Self {
        let mut parts = command_line.split_whitespace().map(str::to_string);
        let program = parts.next().unwrap_or_else(|| "ralphus-runner".to_string());
        Self {
            program,
            args: parts.collect(),
            registry: None,
        }
    }

    /// Resolve the runner command from `RALPHUS_RUNNER_CMD`, defaulting to
    /// `ralphus-runner`.
    #[must_use]
    pub fn from_env() -> Self {
        let cmd =
            std::env::var("RALPHUS_RUNNER_CMD").unwrap_or_else(|_| "ralphus-runner".to_string());
        Self::new(&cmd)
    }

    /// Attach a process registry so spawned children are tracked for the
    /// resource-usage endpoint (RAL-11).
    #[must_use]
    pub fn with_registry(mut self, registry: ProcRegistry) -> Self {
        self.registry = Some(registry);
        self
    }
}

/// Registers a session's subprocess PID on construction and unregisters it on
/// drop, so the resource view never sees a PID after the process has exited —
/// whatever exit path the runner takes (success, timeout, cancel, error).
struct PidGuard<'a> {
    registry: &'a ProcRegistry,
    run_id: &'a str,
    session_id: &'a str,
}

impl Drop for PidGuard<'_> {
    fn drop(&mut self) {
        self.registry.unregister(self.run_id, self.session_id);
    }
}

/// How often a running child is polled for exit / cancellation.
const POLL_CHILD_INTERVAL: Duration = Duration::from_millis(100);

impl Runner for SubprocessRunner {
    fn run(&self, spec: &RunnerSpec) -> RunnerResult {
        self.run_cancellable(spec, &CancelToken::never())
    }

    fn run_cancellable(&self, spec: &RunnerSpec, cancel: &CancelToken) -> RunnerResult {
        let payload = match serde_json::to_string(spec) {
            Ok(p) => p,
            Err(e) => return RunnerResult::failure(format!("could not serialize spec: {e}")),
        };

        let mut child = match Command::new(&self.program)
            .args(&self.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                return RunnerResult::failure(format!(
                    "could not spawn runner '{}': {e}",
                    self.program
                ));
            }
        };
        crate::rlog!(
            DEBUG,
            "ralphus [runner] spawning {} pid={} run={} session={} agent={} model={} timeout={:?}s",
            self.program,
            child.id(),
            spec.run_id,
            spec.session_id,
            spec.agent,
            spec.model.as_deref().unwrap_or("default"),
            spec.timeout_sec,
        );

        // Track this session's PID for the resource view until the child exits
        // (the guard unregisters on every return path below).
        let _pid_guard = self.registry.as_ref().map(|registry| {
            registry.register(&spec.run_id, &spec.session_id, child.id());
            PidGuard {
                registry,
                run_id: &spec.run_id,
                session_id: &spec.session_id,
            }
        });

        if let Some(mut stdin) = child.stdin.take() {
            if let Err(e) = stdin.write_all(payload.as_bytes()) {
                let _ = child.kill();
                return RunnerResult::failure(format!("could not write spec to runner: {e}"));
            }
            // stdin dropped here → the child sees EOF on its input.
        }

        // Drain stdout/stderr on their own threads so a chatty child never
        // blocks on a full pipe while we poll for exit/cancellation.
        let out_reader = child.stdout.take().map(spawn_reader);
        let err_reader = child.stderr.take().map(spawn_reader);

        // Enforce the session's wall-clock timeout here (RAL-15): the daemon
        // owns the kill so a runaway/hung agent is guaranteed to be stopped,
        // regardless of what the runner itself does.
        let started = Instant::now();
        let deadline = spec.timeout_sec.map(Duration::from_secs);

        // Poll until the child exits, the run is cancelled, or the timeout fires.
        loop {
            if cancel.is_cancelled() {
                let _ = child.kill();
                let _ = child.wait();
                join_reader(out_reader);
                join_reader(err_reader);
                crate::rlog!(
                    INFO,
                    "ralphus [runner] cancelled run={} session={}",
                    spec.run_id,
                    spec.session_id
                );
                return RunnerResult::failure("cancelled");
            }
            if timed_out(started.elapsed(), deadline) {
                let _ = child.kill();
                let _ = child.wait();
                join_reader(out_reader);
                join_reader(err_reader);
                let secs = deadline.map(|d| d.as_secs()).unwrap_or(0);
                crate::rlog!(
                    WARNING,
                    "ralphus [runner] timed out after {secs}s run={} session={}",
                    spec.run_id,
                    spec.session_id
                );
                return RunnerResult::failure(format!("timed out after {secs}s"));
            }
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) => std::thread::sleep(POLL_CHILD_INTERVAL),
                Err(e) => {
                    let _ = child.kill();
                    return RunnerResult::failure(format!("runner wait failed: {e}"));
                }
            }
        }

        let stdout_bytes = join_reader(out_reader);
        let stderr_bytes = join_reader(err_reader);
        let stdout = String::from_utf8_lossy(&stdout_bytes);
        let result = parse_result(&stdout).unwrap_or_else(|| {
            let stderr = String::from_utf8_lossy(&stderr_bytes);
            RunnerResult::failure(format!(
                "runner produced no valid result (stderr: {})",
                stderr.trim()
            ))
        });
        crate::rlog!(
            INFO,
            "ralphus [runner] done run={} session={} status={} tokens_in={} tokens_out={} cost_usd={:.4}",
            spec.run_id,
            spec.session_id,
            result.status,
            result.tokens_in,
            result.tokens_out,
            result.cost_usd,
        );
        result
    }
}

/// Whether `elapsed` has reached the optional `deadline`. `None` = no limit,
/// so never times out. Factored out so the timeout rule is unit-testable
/// without spawning a real subprocess (RAL-15).
fn timed_out(elapsed: Duration, deadline: Option<Duration>) -> bool {
    matches!(deadline, Some(d) if elapsed >= d)
}

/// Spawn a thread that reads a child pipe to EOF, yielding its bytes on join.
fn spawn_reader<R: Read + Send + 'static>(mut pipe: R) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = pipe.read_to_end(&mut buf);
        buf
    })
}

/// Join a reader thread, returning its collected bytes (empty if absent/panicked).
fn join_reader(handle: Option<std::thread::JoinHandle<Vec<u8>>>) -> Vec<u8> {
    handle.and_then(|h| h.join().ok()).unwrap_or_default()
}

/// Parse the last JSON object line the runner printed.
fn parse_result(stdout: &str) -> Option<RunnerResult> {
    stdout
        .lines()
        .rev()
        .map(str::trim)
        .filter(|l| l.starts_with('{'))
        .find_map(|l| serde_json::from_str::<RunnerResult>(l).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_serializes_expected_fields() {
        let row = SessionRow {
            task_idx: 0,
            idx: 0,
            task_name: "build".to_string(),
            session_id: "s0".to_string(),
            cwd: Some("/repo".to_string()),
            subprojects: vec![],
            prompt: None,
            command: Some("cargo build".to_string()),
            agent: "claude".to_string(),
            model: None,
            system_prompt: None,
            system_prompt_position: None,
            depends_on: vec![],
            timeout_sec: None,
            budget_tokens: None,
            upstream: None,
        };
        let spec = RunnerSpec::from_row("run-1", &row);
        let json = serde_json::to_string(&spec).unwrap();
        assert!(json.contains("\"run_id\":\"run-1\""));
        assert!(json.contains("\"command\":\"cargo build\""));
        assert!(json.contains("\"cwd\":\"/repo\""));
    }

    #[test]
    fn spec_carries_system_prompt_from_row() {
        let row = SessionRow {
            task_idx: 0,
            idx: 0,
            task_name: "build".to_string(),
            session_id: "s0".to_string(),
            cwd: Some("/repo".to_string()),
            subprojects: vec![],
            prompt: Some("do work".to_string()),
            command: None,
            agent: "claude-code".to_string(),
            model: None,
            system_prompt: Some("Follow the house style.".to_string()),
            system_prompt_position: Some("append".to_string()),
            depends_on: vec![],
            timeout_sec: None,
            budget_tokens: None,
            upstream: None,
        };
        let spec = RunnerSpec::from_row("run-1", &row);
        assert_eq!(
            spec.system_prompt.as_deref(),
            Some("Follow the house style.")
        );
        assert_eq!(spec.system_prompt_position.as_deref(), Some("append"));
        let json = serde_json::to_string(&spec).unwrap();
        assert!(json.contains("\"system_prompt\":\"Follow the house style.\""));
        assert!(json.contains("\"system_prompt_position\":\"append\""));
    }

    #[test]
    fn spec_omits_system_prompt_when_unset() {
        let row = SessionRow {
            task_idx: 0,
            idx: 0,
            task_name: "build".to_string(),
            session_id: "s0".to_string(),
            cwd: Some("/repo".to_string()),
            subprojects: vec![],
            prompt: None,
            command: Some("cargo build".to_string()),
            agent: "claude".to_string(),
            model: None,
            system_prompt: None,
            system_prompt_position: None,
            depends_on: vec![],
            timeout_sec: None,
            budget_tokens: None,
            upstream: None,
        };
        let json = serde_json::to_string(&RunnerSpec::from_row("run-1", &row)).unwrap();
        assert!(!json.contains("system_prompt"));
    }

    #[test]
    fn for_verify_builds_a_prompt_spec_with_verify_set() {
        let spec = RunnerSpec::for_verify(
            "run-1",
            "build",
            "verify-session-0",
            "/repo",
            "check something",
            "ollama",
            Some("qwen3:8b"),
            Some(300),
            Some(10000),
        );
        assert!(spec.verify);
        assert_eq!(spec.timeout_sec, Some(300));
        assert_eq!(spec.budget_tokens, Some(10000));
        assert_eq!(spec.command, None);
        assert_eq!(spec.prompt.as_deref(), Some("check something"));
        assert_eq!(spec.agent, "ollama");
        assert_eq!(spec.model.as_deref(), Some("qwen3:8b"));
    }

    #[test]
    fn verify_passed_requires_done_and_true_verdict() {
        let mut r = RunnerResult {
            status: "done".to_string(),
            tokens_in: 0,
            tokens_out: 0,
            cost_usd: 0.0,
            summary: String::new(),
            error: None,
            verified: Some(true),
            claude_session_id: None,
        };
        assert!(r.verify_passed());

        r.verified = Some(false);
        assert!(!r.verify_passed());

        r.verified = None;
        assert!(!r.verify_passed(), "no verdict should fail closed");

        r.status = "failed".to_string();
        r.verified = Some(true);
        assert!(
            !r.verify_passed(),
            "a crashed verifier can't have passed even with a stray verdict"
        );
    }

    #[test]
    fn parses_result_json() {
        let r = parse_result("noise\n{\"status\":\"done\",\"tokens_in\":5,\"cost_usd\":0.1}\n")
            .unwrap();
        assert!(r.is_done());
        assert_eq!(r.tokens_in, 5);
        assert_eq!(r.node_state(), NodeState::Done);
    }

    #[test]
    fn parses_failed_result() {
        let r = parse_result("{\"status\":\"failed\",\"error\":\"boom\"}").unwrap();
        assert!(!r.is_done());
        assert_eq!(r.node_state(), NodeState::Failed);
        assert_eq!(r.error.as_deref(), Some("boom"));
    }

    #[test]
    fn parses_verified_field() {
        let r = parse_result("{\"status\":\"done\",\"verified\":true,\"summary\":\"ok\"}").unwrap();
        assert_eq!(r.verified, Some(true));
        assert!(r.verify_passed());
    }

    #[test]
    fn missing_verified_field_defaults_to_none() {
        let r = parse_result("{\"status\":\"done\"}").unwrap();
        assert_eq!(r.verified, None);
        assert!(!r.verify_passed());
    }

    #[test]
    fn no_json_yields_none() {
        assert!(parse_result("just some logs\nno json here").is_none());
    }

    #[test]
    fn subprojects_single_injects_system_prompt() {
        let row = SessionRow {
            task_idx: 0,
            idx: 0,
            task_name: "t".to_string(),
            session_id: "s0".to_string(),
            cwd: Some("/mono".to_string()),
            subprojects: vec!["packages/foo".to_string()],
            prompt: Some("do work".to_string()),
            command: None,
            agent: "ollama".to_string(),
            model: None,
            system_prompt: None,
            system_prompt_position: None,
            depends_on: vec![],
            timeout_sec: None,
            budget_tokens: None,
            upstream: None,
        };
        let spec = RunnerSpec::from_row("run-1", &row);
        let sp = spec
            .system_prompt
            .as_deref()
            .expect("system_prompt should be injected");
        assert!(
            sp.contains("packages/foo"),
            "addendum must name the subproject: {sp}"
        );
        assert!(
            sp.contains("monorepo"),
            "addendum should mention monorepo: {sp}"
        );
        assert_eq!(spec.system_prompt_position.as_deref(), Some("append"));
    }

    #[test]
    fn subprojects_multiple_all_listed() {
        let row = SessionRow {
            task_idx: 0,
            idx: 0,
            task_name: "t".to_string(),
            session_id: "s0".to_string(),
            cwd: Some("/mono".to_string()),
            subprojects: vec!["packages/alpha".to_string(), "packages/beta".to_string()],
            prompt: Some("do work".to_string()),
            command: None,
            agent: "ollama".to_string(),
            model: None,
            system_prompt: None,
            system_prompt_position: None,
            depends_on: vec![],
            timeout_sec: None,
            budget_tokens: None,
            upstream: None,
        };
        let spec = RunnerSpec::from_row("run-1", &row);
        let sp = spec
            .system_prompt
            .as_deref()
            .expect("system_prompt injected");
        assert!(sp.contains("packages/alpha"), "alpha in addendum: {sp}");
        assert!(sp.contains("packages/beta"), "beta in addendum: {sp}");
        assert_eq!(spec.system_prompt_position.as_deref(), Some("append"));
    }

    #[test]
    fn subprojects_merges_with_existing_system_prompt() {
        let row = SessionRow {
            task_idx: 0,
            idx: 0,
            task_name: "t".to_string(),
            session_id: "s0".to_string(),
            cwd: Some("/mono".to_string()),
            subprojects: vec!["services/auth".to_string()],
            prompt: Some("do work".to_string()),
            command: None,
            agent: "claude-code".to_string(),
            model: None,
            system_prompt: Some("Follow the style guide.".to_string()),
            system_prompt_position: Some("append".to_string()),
            depends_on: vec![],
            timeout_sec: None,
            budget_tokens: None,
            upstream: None,
        };
        let spec = RunnerSpec::from_row("run-1", &row);
        let sp = spec
            .system_prompt
            .as_deref()
            .expect("system_prompt should be set");
        assert!(
            sp.contains("Follow the style guide."),
            "user prompt preserved: {sp}"
        );
        assert!(
            sp.contains("services/auth"),
            "subprojects addendum appended: {sp}"
        );
        assert_eq!(spec.system_prompt_position.as_deref(), Some("append"));
    }

    #[test]
    fn no_subproject_preserves_nil_system_prompt() {
        let row = SessionRow {
            task_idx: 0,
            idx: 0,
            task_name: "t".to_string(),
            session_id: "s0".to_string(),
            cwd: Some("/repo".to_string()),
            subprojects: vec![],
            prompt: Some("do work".to_string()),
            command: None,
            agent: "ollama".to_string(),
            model: None,
            system_prompt: None,
            system_prompt_position: None,
            depends_on: vec![],
            timeout_sec: None,
            budget_tokens: None,
            upstream: None,
        };
        let spec = RunnerSpec::from_row("run-1", &row);
        assert!(
            spec.system_prompt.is_none(),
            "no injection when subproject is absent"
        );
        assert!(spec.system_prompt_position.is_none());
    }

    #[test]
    fn command_line_splits_program_and_args() {
        let r = SubprocessRunner::new("python -m ralphus.runner");
        assert_eq!(r.program, "python");
        assert_eq!(r.args, vec!["-m", "ralphus.runner"]);
    }

    #[test]
    fn timed_out_respects_the_deadline() {
        // No deadline: never times out.
        assert!(!timed_out(Duration::from_secs(9999), None));
        // Below the deadline: still running.
        assert!(!timed_out(
            Duration::from_secs(29),
            Some(Duration::from_secs(30))
        ));
        // At or past the deadline: timed out.
        assert!(timed_out(
            Duration::from_secs(30),
            Some(Duration::from_secs(30))
        ));
        assert!(timed_out(
            Duration::from_secs(31),
            Some(Duration::from_secs(30))
        ));
    }

    #[test]
    fn registers_pid_while_the_subprocess_is_alive_and_clears_it_after() {
        // A real, briefly-lived subprocess: the runner must register its PID for
        // the resource view while it runs, and drop it once it exits (RAL-11).
        let reg = crate::procreg::ProcRegistry::new();
        #[cfg(target_os = "windows")]
        let runner = SubprocessRunner {
            program: "powershell".to_string(),
            args: vec![
                "-NoProfile".to_string(),
                "-Command".to_string(),
                "Start-Sleep -Milliseconds 700".to_string(),
            ],
            registry: Some(reg.clone()),
        };
        #[cfg(not(target_os = "windows"))]
        let runner = SubprocessRunner {
            program: "sleep".to_string(),
            args: vec!["1".to_string()],
            registry: Some(reg.clone()),
        };
        let row = SessionRow {
            task_idx: 0,
            idx: 0,
            task_name: "t".to_string(),
            session_id: "s0".to_string(),
            cwd: Some(".".to_string()),
            subprojects: vec![],
            prompt: None,
            command: Some("noop".to_string()),
            agent: "claude".to_string(),
            model: None,
            system_prompt: None,
            system_prompt_position: None,
            depends_on: vec![],
            timeout_sec: None,
            budget_tokens: None,
            upstream: None,
        };
        let spec = RunnerSpec::from_row("run-x", &row);
        let worker = std::thread::spawn(move || runner.run(&spec));

        // While the child is alive the PID is registered.
        let mut seen = None;
        for _ in 0..300 {
            if let Some(pid) = reg.pid_of("run-x", "s0") {
                seen = Some(pid);
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(seen.is_some(), "PID should be registered during the run");

        worker.join().unwrap();
        assert_eq!(
            reg.pid_of("run-x", "s0"),
            None,
            "PID should be cleared once the subprocess exits"
        );
    }

    #[test]
    fn missing_program_fails_gracefully() {
        let runner = SubprocessRunner::new("definitely-not-a-real-program-xyz");
        let row = SessionRow {
            task_idx: 0,
            idx: 0,
            task_name: "t".to_string(),
            session_id: "s".to_string(),
            cwd: Some(".".to_string()),
            subprojects: vec![],
            prompt: None,
            command: Some("echo hi".to_string()),
            agent: "claude".to_string(),
            model: None,
            system_prompt: None,
            system_prompt_position: None,
            depends_on: vec![],
            timeout_sec: None,
            budget_tokens: None,
            upstream: None,
        };
        let result = runner.run(&RunnerSpec::from_row("run-1", &row));
        assert!(!result.is_done());
        assert!(result.error.unwrap().contains("could not spawn"));
    }
}
