//! Invoking the Python session runner.
//!
//! The daemon builds a [`RunnerSpec`] for each session, hands it to a [`Runner`],
//! and gets back a [`RunnerResult`]. The real implementation
//! ([`SubprocessRunner`]) spawns the `ralphus-runner` process and speaks the JSON
//! contract in `cli/src/ralphus/runner/spec.py`. The trait keeps the scheduler
//! testable with an in-process fake.

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use opentelemetry::trace::{SpanKind, Status};
use serde::{Deserialize, Serialize};

use crate::cancel::CancelToken;
use crate::procreg::ProcRegistry;
use crate::store::{NodeState, SessionRow, Store};
use crate::tmux::Tmux;

/// Prefix the runner subprocess writes to stderr before a JSON-encoded
/// [`RunnerEvent`], so its structured events reach Cartographer without
/// touching stdout (reserved for the `SessionSpec`/`SessionResult` contract).
/// Mirrors the existing `RALPHUS_VERIFY: PASS/FAIL` marker-parsing pattern.
pub const EVENT_MARKER: &str = "RALPHUS_EVENT: ";

/// One structured event forwarded from the runner subprocess over the
/// `RALPHUS_EVENT:` stderr marker (RAL-98). `run_id`/`session_id`/`task` fall
/// back to the owning [`RunnerSpec`] when the event itself omits them.
#[derive(Debug, Deserialize)]
struct RunnerEvent {
    source: String,
    message: String,
    #[serde(default)]
    level: Option<String>,
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    run_id: Option<String>,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    task: Option<String>,
    #[serde(default)]
    payload: serde_json::Value,
}

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
    /// W3C `traceparent` of the OpenTelemetry span this session/verify run is
    /// a child of (RAL-96), so the Python runner's own spans continue the
    /// same trace instead of starting a disconnected one. `None` when tracing
    /// is not configured (see `daemon/src/otel.rs`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trace_context: Option<String>,
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
            trace_context: None,
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
            trace_context: None,
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
    /// RAL-136: the agent's self-summarized handoff note ("ghost"), extracted
    /// from a `RALPHUS_GHOST:` marker in a normal (non-verify) prompt
    /// session's final response. `None` for command sessions, verify steps,
    /// or when the agent had nothing to hand off.
    #[serde(default)]
    pub ghost: Option<String>,
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
            ghost: None,
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
    /// When set, `RALPHUS_EVENT:` marker lines on the child's stderr are
    /// parsed and forwarded into Cartographer (RAL-98).
    cartographer: Option<Arc<Mutex<Store>>>,
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
            cartographer: None,
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

    /// Attach a store handle so `RALPHUS_EVENT:` marker lines on the child's
    /// stderr are forwarded into Cartographer (RAL-98).
    #[must_use]
    pub fn with_cartographer(mut self, store: Arc<Mutex<Store>>) -> Self {
        self.cartographer = Some(store);
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

/// How often a tmux-wrapped runner invocation's pane is polled for the
/// completion sentinel (RAL-102). Coarser than [`POLL_CHILD_INTERVAL`] since
/// each tick costs a real `tmux capture-pane` subprocess spawn, not just a
/// syscall.
const TMUX_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Sentinel prefix the runner prints to its own stdout (which lands in the
/// tmux pane, not a pipe the daemon reads) once it has written its
/// `SessionResult` to the `--result-file` it was given. Mirrors the
/// `RALPHUS_VERIFY:`/`RALPHUS_EVENT:` marker idiom, just polled from pane
/// content instead of a stderr pipe (RAL-102 Q2/Q3/Q5).
const TMUX_DONE_MARKER: &str = "RALPHUS_TMUX_DONE";

impl Runner for SubprocessRunner {
    fn run(&self, spec: &RunnerSpec) -> RunnerResult {
        self.run_cancellable(spec, &CancelToken::never())
    }

    /// `prompt`-kind specs (agent invocations — normal task sessions, `agent`-
    /// kind verify steps, and Guardian merge/resolver/synthesizer sessions,
    /// all of which set `prompt`) run tmux-wrapped so the board can show a
    /// live view (RAL-102). `command`-kind specs (deterministic shell
    /// sessions — no LLM involved, nothing to watch live) keep running as a
    /// raw child process exactly as before.
    fn run_cancellable(&self, spec: &RunnerSpec, cancel: &CancelToken) -> RunnerResult {
        if spec.prompt.is_some() {
            self.run_via_tmux(spec, cancel)
        } else {
            self.run_raw_subprocess(spec, cancel)
        }
    }
}

impl SubprocessRunner {
    /// Run `spec` inside a detached tmux session and collect its result over
    /// a file-based side channel, since the tmux pane — not a pipe the daemon
    /// reads — is where the child's stdout/stderr actually go.
    ///
    /// 1. Write `spec` to a temp file and pick a temp path for the result.
    /// 2. Start `<program> <args...> <spec-file> --result-file <result-file>`
    ///    in a new tmux session named deterministically from
    ///    `(run_id, task, session_id)` (see `crate::tmux::session_name`).
    /// 3. Poll `capture-pane` until the `RALPHUS_TMUX_DONE` sentinel appears
    ///    (or cancellation/timeout fires), forwarding any `RALPHUS_EVENT:`
    ///    lines seen along the way into Cartographer — the pane merges what
    ///    would otherwise be separate stdout/stderr streams, so this scans
    ///    every new line rather than a dedicated stderr reader thread.
    /// 4. Read the result file and kill the session.
    fn run_via_tmux(&self, spec: &RunnerSpec, cancel: &CancelToken) -> RunnerResult {
        let tmux = match Tmux::resolve() {
            Ok(t) => t,
            Err(e) => return RunnerResult::failure(format!("could not resolve tmux: {e}")),
        };

        let io_dir = std::env::temp_dir().join("ralphus-runner-io");
        if let Err(e) = std::fs::create_dir_all(&io_dir) {
            return RunnerResult::failure(format!("could not create runner IO dir: {e}"));
        }
        let key = crate::tmux::session_name(&spec.run_id, &spec.task, &spec.session_id);
        let spec_path = io_dir.join(format!("{key}.spec.json"));
        let result_path = io_dir.join(format!("{key}.result.json"));
        // A stale file from a prior crashed/killed run of the same
        // (run_id, session_id) must never be mistaken for this run's result.
        let _ = std::fs::remove_file(&result_path);

        let payload = match serde_json::to_string(spec) {
            Ok(p) => p,
            Err(e) => return RunnerResult::failure(format!("could not serialize spec: {e}")),
        };
        if let Err(e) = std::fs::write(&spec_path, payload) {
            return RunnerResult::failure(format!("could not write spec file: {e}"));
        }

        let mut all_args = self.args.clone();
        all_args.push(spec_path.to_string_lossy().into_owned());
        all_args.push("--result-file".to_string());
        all_args.push(result_path.to_string_lossy().into_owned());
        let command = crate::tmux::build_command_line(&self.program, &all_args);

        // `key` (from `tmux::session_name`) is already the fully-formed,
        // "ralphus_"-prefixed session name — do not re-prefix it here. A
        // prior double-prefix bug meant the tmux session actually created
        // never matched the name `capture_pane_reply`/`attach_tmux_terminal`
        // recomputed via a bare `tmux::session_name` call, so the live
        // "peek"/"open terminal" endpoints always reported the session as
        // inactive even while it was running.
        let session_name = key;
        // A tmux session with this exact deterministic name can already exist
        // and still be alive: tmux sessions are owned by the tmux server, not
        // the daemon, so they survive a daemon crash/restart. Resuming this
        // (run_id, task, session_id) slot after such a restart would
        // otherwise collide with that orphaned session (`new-session` errors
        // on a duplicate name), failing this attempt immediately. Kill it
        // first so the resume can proceed.
        //
        // Known caveat (not yet implemented — see TMUX.local.md): this
        // discards the orphaned session's pane content without saving it.
        if tmux.has_session(&session_name) {
            crate::rlog!(
                WARNING,
                "ralphus [runner] killing stale tmux session {session_name} (orphaned, likely by a prior daemon restart) before starting a fresh one run={} session={}",
                spec.run_id,
                spec.session_id,
            );
            let _ = tmux.kill_session(&session_name);
        }
        if let Err(e) = tmux.new_detached_session_with_command(&session_name, &spec.cwd, &command) {
            let _ = std::fs::remove_file(&spec_path);
            return RunnerResult::failure(format!("could not start tmux session: {e}"));
        }
        crate::rlog!(
            INFO,
            "ralphus [runner] tmux session started {session_name} run={} session={} agent={} model={}",
            spec.run_id,
            spec.session_id,
            spec.agent,
            spec.model.as_deref().unwrap_or("default"),
        );
        self.emit_tmux_note(spec, "tmux session started", &session_name);
        // Best-effort: if the session dies with no explanation (the ongoing
        // mystery -- see PSMUX_CRASH_NOTES.local.md), this is the one way to
        // learn whether its process actually crashed (a real Windows
        // exception code) or exited cleanly, without needing admin rights.
        // `None` (PID lookup failed, or non-Windows) just means no exit-code
        // detail is available later -- never fatal to the session itself.
        let exit_watch =
            crate::tmux::find_server_pid(&session_name).map(crate::tmux::watch_for_exit);

        let started = Instant::now();
        let deadline = spec.timeout_sec.map(Duration::from_secs);
        let mut lines_seen: usize = 0;
        let mut last_pane: Option<String> = None;
        let mut missing_session_strikes: u32 = 0;
        // A session that appears gone must be confirmed gone across a couple
        // of consecutive polls (not acted on the first miss) — root cause not
        // yet pinned down, but this build's tmux alternative (psmux) has
        // shown transient `capture-pane`/`has-session` failures under real
        // load that clear on the very next poll. Requiring 3 consecutive
        // misses (~1.5s at the current poll interval) filters those out
        // without meaningfully delaying a genuine session death.
        const MISSING_SESSION_STRIKE_LIMIT: u32 = 3;
        // Set only when the loop breaks because the session vanished on its
        // own (the mystery this file's exit-code watcher exists for) — never
        // for an intentional `cancel()`/timeout kill, which also produce a
        // non-done `RunnerResult` but have a perfectly well-known cause and
        // should never get "tmux server process exit code: ..." appended to
        // their message. A test caught this: `live_tmux_run_via_tmux_is_cancellable`
        // asserts the error is exactly `"cancelled"`.
        let mut session_died_unexpectedly = false;
        let result = loop {
            if cancel.is_cancelled() {
                let _ = tmux.kill_session(&session_name);
                crate::rlog!(
                    INFO,
                    "ralphus [runner] cancelled run={} session={}",
                    spec.run_id,
                    spec.session_id
                );
                break RunnerResult::failure("cancelled");
            }
            if timed_out(started.elapsed(), deadline) {
                let _ = tmux.kill_session(&session_name);
                let secs = deadline.map(|d| d.as_secs()).unwrap_or(0);
                crate::rlog!(
                    WARNING,
                    "ralphus [runner] timed out after {secs}s run={} session={}",
                    spec.run_id,
                    spec.session_id
                );
                break RunnerResult::failure(format!("timed out after {secs}s"));
            }
            match tmux.capture_pane(&session_name, 10_000) {
                Ok(pane) => {
                    missing_session_strikes = 0;
                    let all_lines: Vec<&str> = pane.lines().collect();
                    if all_lines.len() > lines_seen {
                        for line in &all_lines[lines_seen..] {
                            if let Some(json) = line.trim_end().strip_prefix(EVENT_MARKER) {
                                forward_runner_event(
                                    self.cartographer.as_ref(),
                                    &spec.run_id,
                                    &spec.session_id,
                                    &spec.task,
                                    json,
                                );
                            }
                        }
                        lines_seen = all_lines.len();
                    }
                    let done = pane.contains(TMUX_DONE_MARKER);
                    last_pane = Some(pane);
                    if done {
                        break Self::read_tmux_result(&result_path, last_pane.as_deref());
                    }
                }
                Err(_) => {
                    // The session may have died before printing the sentinel
                    // (e.g. the runner process crashed hard enough to tear
                    // down the pane) — or `has-session` itself may just be
                    // having a transient hiccup, so this isn't trusted on the
                    // first miss (see `MISSING_SESSION_STRIKE_LIMIT`).
                    if !tmux.has_session(&session_name) {
                        missing_session_strikes += 1;
                        if missing_session_strikes >= MISSING_SESSION_STRIKE_LIMIT {
                            session_died_unexpectedly = true;
                            break Self::read_tmux_result(&result_path, last_pane.as_deref());
                        }
                    } else {
                        missing_session_strikes = 0;
                    }
                }
            }
            std::thread::sleep(TMUX_POLL_INTERVAL);
        };

        let _ = tmux.kill_session(&session_name);
        let _ = std::fs::remove_file(&spec_path);
        let _ = std::fs::remove_file(&result_path);
        self.emit_tmux_note(spec, "tmux session ended", &session_name);

        // Only worth the (bounded) wait on a failure -- a clean completion
        // doesn't need the "was this a crash?" diagnostic. If the tracked
        // process is really gone (which it should be, since the loop above
        // just concluded the session ended), the watcher's own blocking wait
        // should already be finishing, so this rarely actually waits long.
        let mut result = result;
        if session_died_unexpectedly && !result.is_done() {
            if let Some(observation) =
                exit_watch.and_then(|rx| rx.recv_timeout(Duration::from_secs(2)).ok())
            {
                let detail = match observation {
                    crate::tmux::ProcessExit::Exited(code) => {
                        format!("tmux server process exit code: {code}")
                    }
                    crate::tmux::ProcessExit::Unknown(reason) => {
                        format!("tmux server process exit code: unknown ({reason})")
                    }
                };
                crate::rlog!(
                    WARNING,
                    "ralphus [runner] {detail} run={} session={}",
                    spec.run_id,
                    spec.session_id
                );
                result.error = Some(match result.error.take() {
                    Some(existing) => format!("{existing}\n{detail}"),
                    None => detail,
                });
            }
        }

        crate::rlog!(
            INFO,
            "ralphus [runner] tmux done run={} session={} status={} tokens_in={} tokens_out={} cost_usd={:.4}",
            spec.run_id,
            spec.session_id,
            result.status,
            result.tokens_in,
            result.tokens_out,
            result.cost_usd,
        );
        result
    }

    /// Emit a tmux session-lifecycle Cartographer note. Per RAL-102, only
    /// these start/end transitions are logged — the `capture-pane` polling
    /// itself never is, so a long-running session doesn't flood
    /// `cartographer_events`.
    fn emit_tmux_note(&self, spec: &RunnerSpec, message: &str, session_name: &str) {
        let Some(store) = &self.cartographer else {
            return;
        };
        let Ok(guard) = store.lock() else { return };
        crate::cartographer::Note::new("runner")
            .run(&spec.run_id)
            .session(&spec.session_id)
            .task(&spec.task)
            .scope("tmux")
            .emit(
                &guard,
                format!("{message} ({session_name})"),
                serde_json::json!({"session_name": session_name}),
            );
    }

    /// Read and parse the `SessionResult` a tmux-wrapped runner wrote to
    /// `path`. A missing or malformed file (the runner crashed before
    /// writing it, or the session was killed before it finished) is
    /// reported as a failure, never a panic.
    ///
    /// `last_pane` — the tail of the most recent successful `capture-pane`
    /// read before this call, if any — is folded into a "no result file"
    /// failure so the surfaced error shows what the agent was doing/printing
    /// right before its tmux session disappeared. Without this, a session
    /// that vanishes mid-run (observed on the Windows psmux build this
    /// project targets, root cause not yet pinned down) produces only the
    /// bare OS error and no clue what the agent was actually doing at the
    /// time — nothing to diagnose from if it happens again.
    fn read_tmux_result(path: &std::path::Path, last_pane: Option<&str>) -> RunnerResult {
        match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
                RunnerResult::failure(format!("runner result file was not valid JSON: {e}"))
            }),
            Err(e) => {
                let tail = last_pane.map(|pane| tail_lines(pane, 60));
                match tail {
                    Some(tail) if !tail.is_empty() => RunnerResult::failure(format!(
                        "runner produced no result file: {e}\nlast pane output:\n{tail}"
                    )),
                    _ => RunnerResult::failure(format!("runner produced no result file: {e}")),
                }
            }
        }
    }

    /// The original raw-subprocess path: spawn the runner directly and speak
    /// the stdin/stdout JSON contract over pipes. Still used for
    /// `command`-kind specs (see [`Runner::run_cancellable`] above).
    fn run_raw_subprocess(&self, spec: &RunnerSpec, cancel: &CancelToken) -> RunnerResult {
        // The daemon-side boundary of the reentrant Rust → Python call (RAL-96).
        // Held for the whole subprocess lifetime (RAII ends it on every return
        // path below, including the early-failure ones). Its own traceparent —
        // not `spec.trace_context` verbatim — is what gets sent to the runner,
        // so the Python spans nest under *this* span rather than becoming its
        // sibling.
        let cx = crate::otel::context_from_traceparent(spec.trace_context.as_deref());
        let _span = crate::otel::start_span("runner.subprocess", &cx, SpanKind::Client);
        _span.set_attribute("run_id", spec.run_id.clone());
        _span.set_attribute("session_id", spec.session_id.clone());
        _span.set_attribute("agent", spec.agent.clone());

        let mut wire_spec = spec.clone();
        wire_spec.trace_context = crate::otel::traceparent_from_context(&_span.cx);
        let payload = match serde_json::to_string(&wire_spec) {
            Ok(p) => p,
            Err(e) => {
                _span.set_status(Status::error("could not serialize spec"));
                return RunnerResult::failure(format!("could not serialize spec: {e}"));
            }
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
                _span.set_status(Status::error("could not spawn runner"));
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
                _span.set_status(Status::error("could not write spec to runner"));
                return RunnerResult::failure(format!("could not write spec to runner: {e}"));
            }
            // stdin dropped here → the child sees EOF on its input.
        }

        // Drain stdout/stderr on their own threads so a chatty child never
        // blocks on a full pipe while we poll for exit/cancellation. stderr is
        // read line-by-line (not buffered to EOF) so `RALPHUS_EVENT:` marker
        // lines reach Cartographer as they're emitted, not just at exit.
        let out_reader = child.stdout.take().map(spawn_reader);
        let err_reader = child
            .stderr
            .take()
            .map(|pipe| spawn_stderr_reader(pipe, self.cartographer.clone(), spec));

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
                _span.set_status(Status::error("cancelled"));
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
                _span.set_status(Status::error("timed out"));
                return RunnerResult::failure(format!("timed out after {secs}s"));
            }
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) => std::thread::sleep(POLL_CHILD_INTERVAL),
                Err(e) => {
                    let _ = child.kill();
                    _span.set_status(Status::error("runner wait failed"));
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
        _span.set_status(if result.is_done() {
            Status::Ok
        } else {
            Status::error(result.error.clone().unwrap_or_default())
        });
        result
    }
}

/// Spawn a thread that reads the child's stderr line-by-line, forwarding any
/// `RALPHUS_EVENT: {json}` lines into Cartographer as they arrive (RAL-98)
/// and always accumulating the raw bytes for the existing failure-message
/// fallback. A malformed marker line is logged as a warning and otherwise
/// ignored — one bad line must never lose the rest of the stream.
fn spawn_stderr_reader<R: Read + Send + 'static>(
    pipe: R,
    cartographer: Option<Arc<Mutex<Store>>>,
    spec: &RunnerSpec,
) -> std::thread::JoinHandle<Vec<u8>> {
    let run_id = spec.run_id.clone();
    let session_id = spec.session_id.clone();
    let task = spec.task.clone();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let mut reader = BufReader::new(pipe);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {
                    buf.extend_from_slice(line.as_bytes());
                    if let Some(json) = line.trim_end().strip_prefix(EVENT_MARKER) {
                        forward_runner_event(
                            cartographer.as_ref(),
                            &run_id,
                            &session_id,
                            &task,
                            json,
                        );
                    }
                }
                Err(_) => break,
            }
        }
        buf
    })
}

/// Parse and persist one `RALPHUS_EVENT:` JSON payload from the runner
/// subprocess. Missing `run_id`/`session_id`/`task` fall back to the owning
/// session's spec.
fn forward_runner_event(
    cartographer: Option<&Arc<Mutex<Store>>>,
    run_id: &str,
    session_id: &str,
    task: &str,
    json: &str,
) {
    let Some(store) = cartographer else { return };
    let event: RunnerEvent = match serde_json::from_str(json) {
        Ok(e) => e,
        Err(e) => {
            crate::rlog!(
                WARNING,
                "ralphus [runner] malformed RALPHUS_EVENT: {e} ({json:?})"
            );
            return;
        }
    };
    let level = event
        .level
        .as_deref()
        .and_then(|s| s.parse().ok())
        .unwrap_or(crate::logging::LogLevel::INFO);
    let Ok(guard) = store.lock() else { return };
    // RAL-102 follow-up: as soon as the runner reports the Claude Code session
    // id (from `claude_code_backend.py`'s `stream-json` init event), persist
    // it immediately rather than waiting for the whole session to finish, so
    // the board's "Open Agent" action activates right away. A no-op for any
    // event whose (run_id, task, session_id) isn't a session row — verify
    // steps and Guardian resolver invocations share this same forwarding path
    // but aren't rows in the `sessions` table.
    if event.source == "llm-invoke" {
        if let Some(sid) = event
            .payload
            .get("claude_session_id")
            .and_then(|v| v.as_str())
        {
            let _ = guard.set_session_claude_session_id_live(
                event.run_id.as_deref().unwrap_or(run_id),
                event.task.as_deref().unwrap_or(task),
                event.session_id.as_deref().unwrap_or(session_id),
                sid,
            );
        }
    }
    let _ = guard.cartographer_log(crate::cartographer::CartographerEntry {
        level,
        source: &event.source,
        message: &event.message,
        scope: event.scope.as_deref(),
        run_id: Some(event.run_id.as_deref().unwrap_or(run_id)),
        guardian_id: None,
        session_id: Some(event.session_id.as_deref().unwrap_or(session_id)),
        task: Some(event.task.as_deref().unwrap_or(task)),
        payload: event.payload,
    });
}

/// Whether `elapsed` has reached the optional `deadline`. `None` = no limit,
/// so never times out. Factored out so the timeout rule is unit-testable
/// without spawning a real subprocess (RAL-15).
fn timed_out(elapsed: Duration, deadline: Option<Duration>) -> bool {
    matches!(deadline, Some(d) if elapsed >= d)
}

/// The last `n` non-empty lines of `text`, joined back with newlines — used
/// to fold a bit of tmux pane context into a "no result file" failure
/// message. Empty when `text` has no non-empty lines.
fn tail_lines(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
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
            ghost: None,
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
    fn stderr_reader_forwards_event_marker_lines_to_cartographer() {
        let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
        let spec = RunnerSpec::for_verify(
            "run-1", "build", "s0", "/repo", "check", "claude", None, None, None,
        );
        let stderr = std::io::Cursor::new(
            b"ralphus [llm] session start\n\
              RALPHUS_EVENT: {\"source\":\"llm\",\"message\":\"session start\",\"level\":\"info\",\"scope\":\"session\",\"payload\":{\"prompt_len\":42}}\n\
              some other noise\n\
              RALPHUS_EVENT: not json at all\n"
                .to_vec(),
        );
        let handle = spawn_stderr_reader(stderr, Some(Arc::clone(&store)), &spec);
        let raw = handle.join().unwrap();
        assert!(String::from_utf8_lossy(&raw).contains("some other noise"));

        let page = store
            .lock()
            .unwrap()
            .cartographer_query(&crate::cartographer::CartographerFilter::recent(10))
            .unwrap();
        assert_eq!(
            page.total, 1,
            "the malformed marker line must not be recorded"
        );
        let row = &page.rows[0];
        assert_eq!(row.source, "llm");
        assert_eq!(row.message, "session start");
        assert_eq!(row.scope.as_deref(), Some("session"));
        assert_eq!(row.run_id.as_deref(), Some("run-1"));
        assert_eq!(row.session_id.as_deref(), Some("s0"));
        assert_eq!(row.payload, serde_json::json!({"prompt_len": 42}));
    }

    #[test]
    fn stderr_reader_without_cartographer_handle_is_a_noop() {
        let spec = RunnerSpec::for_verify(
            "run-1", "build", "s0", "/repo", "check", "claude", None, None, None,
        );
        let stderr = std::io::Cursor::new(
            b"RALPHUS_EVENT: {\"source\":\"llm\",\"message\":\"hi\"}\n".to_vec(),
        );
        let handle = spawn_stderr_reader(stderr, None, &spec);
        // Must not panic when no store handle is attached (e.g. test fakes).
        handle.join().unwrap();
    }

    #[test]
    fn tail_lines_keeps_only_the_last_n_non_empty_lines() {
        let text = "a\n\nb\nc\nd\n";
        assert_eq!(tail_lines(text, 2), "c\nd");
        assert_eq!(tail_lines(text, 10), "a\nb\nc\nd");
        assert_eq!(tail_lines("", 5), "");
        assert_eq!(tail_lines("\n\n", 5), "");
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
            cartographer: None,
        };
        #[cfg(not(target_os = "windows"))]
        let runner = SubprocessRunner {
            program: "sleep".to_string(),
            args: vec!["1".to_string()],
            registry: Some(reg.clone()),
            cartographer: None,
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

    // ── tmux-wrapped path (RAL-102) ─────────────────────────────────────────
    //
    // These exercise `run_via_tmux` against the real `tmux` binary with a
    // tiny fake "runner" program (a one-line Python script) standing in for
    // `ralphus-runner`, so no dependency on pydantic-ai/Claude is needed.
    // Skips (rather than fails) when `tmux` or `python` isn't on PATH,
    // mirroring the project's live-Ollama-integration-test skip idiom.

    fn tmux_and_python_available() -> bool {
        let has_tmux = std::env::var_os("PATH").is_some_and(|path| {
            std::env::split_paths(&path).any(|dir| {
                dir.join("tmux").is_file()
                    || dir.join("tmux.exe").is_file()
                    || dir.join("tmux.cmd").is_file()
            })
        });
        let has_python = std::process::Command::new("python")
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success());
        has_tmux && has_python
    }

    /// A fake `ralphus-runner`: reads no real spec, just writes a canned
    /// `SessionResult` to whatever `--result-file` path it was given and
    /// prints the `RALPHUS_TMUX_DONE` sentinel — mirroring
    /// `cli/src/ralphus/runner/__main__.py`'s `--result-file` contract
    /// exactly (RAL-102).
    // A single logical line (no embedded newlines): the runner's command is
    // delivered by typing it into an interactive pane via `send-keys` on
    // Windows, and an embedded newline there is indistinguishable from a
    // literal Enter keypress, splitting the command into separate lines the
    // shell executes independently instead of one `python -c ...` call.
    //
    // The sentinel string is built via concatenation (`"RALPHUS_TMUX" +
    // "_DONE: done"`) rather than written out whole: `capture-pane` sees the
    // *typed command itself* (this script's own source) echoed into the pane
    // before it ever runs, so a literal `RALPHUS_TMUX_DONE` in the source
    // would false-positive the sentinel scan on the echoed input, not just
    // the eventual real output. Real `ralphus-runner` invocations never hit
    // this — their command line is just `<program> <args>`, never Python
    // source containing the marker — so this is a test-fake-only concern.
    const FAKE_RUNNER_SCRIPT: &str = "import sys,json; \
        rp=sys.argv[sys.argv.index(\"--result-file\")+1]; \
        open(rp,\"w\").write(json.dumps({\"status\":\"done\",\"tokens_in\":1,\"tokens_out\":2,\"cost_usd\":0.01,\"summary\":\"fake\",\"error\":None,\"verified\":None,\"claude_session_id\":None})); \
        print(\"RALPHUS_TMUX\" + \"_DONE: done\")";

    /// A fake runner that never finishes, to exercise the timeout path.
    const HANGING_RUNNER_SCRIPT: &str = "import time; time.sleep(30)";

    #[test]
    fn live_tmux_run_via_tmux_full_roundtrip() {
        if !tmux_and_python_available() {
            println!("SKIP: tmux and/or python not found on PATH");
            return;
        }
        let runner = SubprocessRunner {
            program: "python".to_string(),
            args: vec!["-c".to_string(), FAKE_RUNNER_SCRIPT.to_string()],
            registry: None,
            cartographer: None,
        };
        let cwd = std::env::temp_dir().to_string_lossy().into_owned();
        let spec = RunnerSpec::for_verify(
            "run-tmux-1",
            "build",
            "fake-session",
            &cwd,
            "do something",
            "claude",
            None,
            Some(60),
            None,
        );
        let result = runner.run(&spec);
        assert!(result.is_done(), "expected done, got: {result:?}");
        assert_eq!(result.tokens_in, 1);
        assert_eq!(result.tokens_out, 2);
        assert_eq!(result.summary, "fake");
    }

    #[test]
    fn live_tmux_run_via_tmux_times_out() {
        if !tmux_and_python_available() {
            println!("SKIP: tmux and/or python not found on PATH");
            return;
        }
        let runner = SubprocessRunner {
            program: "python".to_string(),
            args: vec!["-c".to_string(), HANGING_RUNNER_SCRIPT.to_string()],
            registry: None,
            cartographer: None,
        };
        let cwd = std::env::temp_dir().to_string_lossy().into_owned();
        let spec = RunnerSpec::for_verify(
            "run-tmux-2",
            "build",
            "hanging-session",
            &cwd,
            "do something",
            "claude",
            None,
            Some(1),
            None,
        );
        let started = Instant::now();
        let result = runner.run(&spec);
        assert!(!result.is_done());
        assert!(result.error.unwrap().contains("timed out"));
        assert!(
            started.elapsed() < Duration::from_secs(15),
            "timeout enforcement should fire promptly, took {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn live_tmux_run_via_tmux_is_cancellable() {
        if !tmux_and_python_available() {
            println!("SKIP: tmux and/or python not found on PATH");
            return;
        }
        let runner = SubprocessRunner {
            program: "python".to_string(),
            args: vec!["-c".to_string(), HANGING_RUNNER_SCRIPT.to_string()],
            registry: None,
            cartographer: None,
        };
        let cwd = std::env::temp_dir().to_string_lossy().into_owned();
        let spec = RunnerSpec::for_verify(
            "run-tmux-3",
            "build",
            "cancel-session",
            &cwd,
            "do something",
            "claude",
            None,
            None,
            None,
        );
        let cancel = CancelToken::new();
        let cancel_clone = cancel.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            cancel_clone.cancel();
        });
        let result = runner.run_cancellable(&spec, &cancel);
        assert!(!result.is_done());
        assert_eq!(result.error.as_deref(), Some("cancelled"));
    }
}
