//! Remote session execution via a registered machine provider (RAL-185
//! Phase 2).
//!
//! The seam this hangs off already existed: [`crate::runner::Runner`] is the
//! trait the scheduler hands a [`RunnerSpec`] to and gets a [`RunnerResult`]
//! back, with [`crate::runner::SubprocessRunner`] as the local implementation
//! and an in-process fake for tests. A remote machine is simply *another
//! `Runner`* — reusing that boundary inherits the existing timeout, budget,
//! Cartographer and event-forwarding plumbing rather than duplicating it.
//!
//! Two types live here:
//!
//! - [`ProviderRunner`] speaks the contract in `docs/machine-providers.md` to
//!   one registered provider program.
//! - [`MachineRouter`] implements `Runner` by dispatching on the spec's
//!   [`RunnerSpec::machine`]: `None`/local goes to the wrapped local runner,
//!   anything else is resolved against the registry and handed to a
//!   `ProviderRunner`.
//!
//! The router is what the scheduler holds, so session dispatch stays a single
//! `&dyn Runner` call and nothing above it needs to know a machine exists.

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::cancel::CancelToken;
use crate::machines::{PROTOCOL_VERSION, ResolvedMachine};
use crate::runner::{EVENT_MARKER, Runner, RunnerResult, RunnerSpec};
use crate::store::Store;

/// The `exec` verb: run one session/verify in a provisioned workspace.
pub const VERB_EXEC: &str = "exec";

/// The `provision` verb: ensure a workspace exists on the machine, and report
/// the path to use as the session's `cwd`.
pub const VERB_PROVISION: &str = "provision";

/// The `run` verb: execute one VCS command in a workspace and return its
/// output. The transport underneath is entirely the provider's choice — a
/// one-shot SSH invocation, a multiplexed connection, or a persistent worker
/// channel — which is why the contract says nothing about it (RAL-185 D7).
///
/// "Channel" rather than "session" deliberately: a *session* is already a
/// first-class ralphus concept (task → session → verify), and reusing the word
/// for connection reuse would make both harder to read.
pub const VERB_RUN: &str = "run";

/// The `read-file` verb: return a file's contents from a workspace.
pub const VERB_READ_FILE: &str = "read-file";

/// The `write-file` verb: write a file into a workspace.
pub const VERB_WRITE_FILE: &str = "write-file";

/// The `remove-path` verb: delete a file or directory tree in a workspace.
pub const VERB_REMOVE_PATH: &str = "remove-path";

/// The `ping` verb: confirm the machine is reachable and ready, without doing
/// any work. Cheap by contract — it is called to surface reachability in the
/// board, not as part of running anything.
pub const VERB_PING: &str = "ping";

/// The `status` verb: report whether an async `exec` handle is still running.
pub const VERB_STATUS: &str = "status";

/// The `stream` verb: fetch output produced since a cursor, for Live View.
pub const VERB_STREAM: &str = "stream";

/// The `cancel` verb: stop the work behind an `exec` handle.
pub const VERB_CANCEL: &str = "cancel";

/// How often an async `exec` handle is polled for status/output.
///
/// Fast enough that Live View feels live, slow enough not to hammer a provider
/// that shells out over SSH on every call.
const POLL_INTERVAL: Duration = Duration::from_secs(1);

/// How a workspace's source should be obtained, sent to a provider's
/// `provision` verb.
///
/// Deliberately **not** git-shaped: `kind` names the VCS (from the project
/// registry's own `vcs` column) and the remaining fields are advisory. A
/// provider backing a Perforce or plain-directory project reads `kind`,
/// ignores `url`/`branch`, and does whatever that source system needs. This is
/// the seam that keeps the daemon from re-acquiring the git assumption that
/// RAL-175/184 baked in (`require_git_binary` on the generic path).
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct WorkspaceSource {
    /// VCS kind, e.g. `"git"`. Taken verbatim from the registered project.
    pub kind: String,
    /// Clone/fetch URL, when the VCS has one. `None` for kinds that don't.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Branch (or equivalent named revision) to check out, when meaningful.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
}

/// The payload sent to a provider's file verbs (`read-file`, `write-file`,
/// `remove-path`).
///
/// A merge does not only run git — it hand-writes `.git` worktree link files,
/// reads conflict markers back out of files, and tears directories down. Those
/// are as machine-bound as the git commands and need the same treatment.
#[derive(Debug, Clone, Serialize)]
pub struct FileRequest {
    /// Absolute path, **on the provider's machine**, to the file or directory.
    pub path: String,
    /// `write-file` only: the contents to write. Text, because everything a
    /// merge writes is text; a binary file would need a different verb rather
    /// than silently mangling this one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// `remove-path` only: whether to remove a directory tree rather than a
    /// single file.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub recursive: bool,
}

/// The payload sent to a provider's `run` verb.
#[derive(Debug, Clone, Serialize)]
pub struct RunRequest {
    /// Absolute path, **on the provider's machine**, to run the command in.
    pub cwd: String,
    /// The program to run. Always `"git"` today; named explicitly so a
    /// non-git VCS can be added without reshaping the request.
    pub program: String,
    /// Arguments, already split — never a shell string, so nothing has to be
    /// quoted or escaped correctly on the far side.
    pub args: Vec<String>,
}

/// The payload sent to a provider's `provision` verb.
#[derive(Debug, Clone, Serialize)]
pub struct ProvisionRequest {
    /// Registered project name this workspace belongs to.
    pub project: String,
    /// Where the workspace's contents come from.
    pub source: WorkspaceSource,
    /// Owning run, for a provider that wants to namespace its workspaces.
    pub run_id: String,
    /// Owning session id, same purpose.
    pub session_id: String,
}

/// One provider's JSON response envelope (`docs/machine-providers.md`).
///
/// `result` is only present for `exec`. It is deliberately a nested object
/// rather than flattened into the envelope so a provider can report
/// `ok: false` (the *invocation* failed) distinctly from
/// `ok: true, result.status = "failed"` (the session ran and failed) — the
/// former is an infrastructure problem, the latter is a normal task outcome,
/// and collapsing them would make a broken provider look like failing work.
#[derive(Debug, Deserialize)]
struct ProviderResponse {
    ok: bool,
    #[serde(default)]
    protocol_version: Option<i64>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    result: Option<RunnerResult>,
    /// `provision` only: the absolute path, **on the provider's machine**, that
    /// the session should use as its `cwd`.
    #[serde(default)]
    workspace: Option<String>,
    /// `exec` only: an opaque handle naming the started work, when the provider
    /// runs it asynchronously. A provider that returns `result` instead ran the
    /// work synchronously and needs no polling — both shapes are supported so a
    /// trivial provider stays trivial while a capable one gets live output and
    /// mid-run cancellation.
    #[serde(default)]
    handle: Option<String>,
    /// `status` only: `"running"`, `"done"` or `"failed"`.
    #[serde(default)]
    state: Option<String>,
    /// `stream` only: output produced since the requested cursor.
    #[serde(default)]
    output: Option<String>,
    /// `stream` only: the cursor to pass as `--since` on the next call.
    #[serde(default)]
    next: Option<i64>,
    /// `ping` only: an optional human-readable note about the machine (its
    /// hostname, queue depth, whatever the provider finds worth surfacing).
    #[serde(default)]
    detail: Option<String>,
    /// `run` only: the command's standard output.
    #[serde(default)]
    stdout: Option<String>,
    /// `run` only: the command's exit status. `Some(0)` is success; anything
    /// else is the command itself failing, which is distinct from the provider
    /// failing to run it (`ok: false`).
    #[serde(default)]
    exit_code: Option<i64>,
}

/// Runs sessions on one machine by invoking a registered provider program.
pub struct ProviderRunner {
    /// Provider program path.
    program: String,
    /// Arguments always prepended, before the verb.
    args: Vec<String>,
    /// The scheme this provider was registered under, for diagnostics.
    scheme: String,
    /// The opaque machine uri, passed through verbatim.
    uri: String,
    /// Whether this provider implements the `channel` verb (RAL-185 D7). When
    /// true, `run` commands reuse one long-lived process instead of spawning
    /// per command.
    supports_channel: bool,
    /// When set, `RALPHUS_EVENT:` marker lines on the provider's stderr are
    /// forwarded into Cartographer, exactly as `SubprocessRunner` does for a
    /// local run — without this a remote session is invisible to Cartographer
    /// and, worse, its live cost cap silently stops being enforced.
    cartographer: Option<Arc<Mutex<Store>>>,
}

impl ProviderRunner {
    /// Build a runner for one registered provider + machine uri.
    #[must_use]
    pub fn new(
        program: impl Into<String>,
        args: Vec<String>,
        scheme: impl Into<String>,
        uri: impl Into<String>,
    ) -> Self {
        Self {
            program: program.into(),
            args,
            scheme: scheme.into(),
            uri: uri.into(),
            supports_channel: false,
            cartographer: None,
        }
    }

    /// Declare that this provider implements the `channel` verb.
    #[must_use]
    pub fn with_channel(mut self, supports: bool) -> Self {
        self.supports_channel = supports;
        self
    }

    /// Forward the provider's `RALPHUS_EVENT:` stderr lines into Cartographer.
    #[must_use]
    pub fn with_cartographer(mut self, store: Arc<Mutex<Store>>) -> Self {
        self.cartographer = Some(store);
        self
    }

    /// Invoke `verb` against this provider, writing `payload` to its stdin and
    /// parsing one JSON envelope from its stdout.
    fn invoke(
        &self,
        verb: &str,
        payload: &str,
        spec: &RunnerSpec,
    ) -> Result<ProviderResponse, String> {
        self.invoke_with(verb, payload, spec, &[])
    }

    /// [`Self::invoke`] with additional verb-specific flags appended after
    /// `--uri` (e.g. `--handle`, `--since`).
    fn invoke_with(
        &self,
        verb: &str,
        payload: &str,
        spec: &RunnerSpec,
        extra: &[String],
    ) -> Result<ProviderResponse, String> {
        let mut cmd = Command::new(&self.program);
        cmd.args(&self.args)
            .arg(verb)
            .arg("--uri")
            .arg(&self.uri)
            .args(extra)
            .envs(&spec.env_overrides)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().map_err(|e| {
            format!(
                "could not run machine provider {:?} ({}): {e}",
                self.scheme, self.program
            )
        })?;
        crate::rlog!(
            INFO,
            "ralphus [remote] provider {} verb={verb} uri={} run={} session={}",
            self.scheme,
            self.uri,
            spec.run_id,
            spec.session_id
        );

        if let Some(mut stdin) = child.stdin.take() {
            // A provider that never reads stdin would otherwise deadlock us on
            // a full pipe; treat the write failing as non-fatal and let the
            // exit status/stdout decide.
            let _ = stdin.write_all(payload.as_bytes());
        }

        // Read stderr on its own thread so a chatty provider can't fill the
        // pipe and block its own stdout write, and so `RALPHUS_EVENT:` lines
        // reach Cartographer live rather than only at exit.
        let stderr_handle = child.stderr.take().map(|stderr| {
            let cartographer = self.cartographer.clone();
            let run_id = spec.run_id.clone();
            let session_id = spec.session_id.clone();
            let task = spec.task.clone();
            std::thread::spawn(move || {
                let mut tail = String::new();
                for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                    if let Some(json) = line.trim_end().strip_prefix(EVENT_MARKER) {
                        crate::runner::forward_runner_event(
                            cartographer.as_ref(),
                            &run_id,
                            &session_id,
                            &task,
                            json,
                        );
                    } else {
                        // Keep a bounded tail purely so a provider that dies
                        // without valid JSON can still explain itself.
                        if tail.len() < 4096 {
                            tail.push_str(&line);
                            tail.push('\n');
                        }
                    }
                }
                tail
            })
        });

        let out = child.wait_with_output().map_err(|e| {
            format!(
                "machine provider {:?} could not be waited on: {e}",
                self.scheme
            )
        })?;
        let stderr_tail = stderr_handle
            .and_then(|h| h.join().ok())
            .unwrap_or_default();
        let stdout = String::from_utf8_lossy(&out.stdout);
        let trimmed = stdout.trim();
        if trimmed.is_empty() {
            return Err(format!(
                "machine provider {:?} produced no JSON on stdout (exit {}){}",
                self.scheme,
                out.status.code().unwrap_or(-1),
                if stderr_tail.trim().is_empty() {
                    String::new()
                } else {
                    format!("; stderr: {}", stderr_tail.trim())
                }
            ));
        }
        let resp: ProviderResponse = serde_json::from_str(trimmed).map_err(|e| {
            format!(
                "machine provider {:?} returned unparseable JSON: {e} ({})",
                self.scheme,
                truncate(trimmed, 300)
            )
        })?;
        // A provider declaring the wrong contract version is refused rather
        // than trusted -- the whole point of versioning it (1.13).
        if let Some(v) = resp.protocol_version {
            if v != PROTOCOL_VERSION {
                return Err(format!(
                    "machine provider {:?} replied with contract version {v}, but this daemon implements {PROTOCOL_VERSION}",
                    self.scheme
                ));
            }
        }
        if !resp.ok {
            return Err(format!(
                "machine provider {:?} rejected the {verb}: {}",
                self.scheme,
                resp.error.as_deref().unwrap_or("no reason given")
            ));
        }
        Ok(resp)
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    format!("{}…", &s[..max])
}

impl ProviderRunner {
    /// Ask the provider to ensure a workspace exists and report its path on
    /// that machine.
    ///
    /// Idempotent by contract: a re-run after a daemon restart must reuse the
    /// existing workspace rather than recreating it — the same restart-safety
    /// property [`crate::worktrees::ensure_worktree`] has locally.
    ///
    /// # Errors
    /// Returns the provider's own message when it refuses, or a description of
    /// how it violated the contract (no workspace path, bad JSON, wrong
    /// protocol version).
    pub fn provision(&self, req: &ProvisionRequest, spec: &RunnerSpec) -> Result<String, String> {
        let payload = serde_json::to_string(req)
            .map_err(|e| format!("could not serialize provision request: {e}"))?;
        let resp = self.invoke(VERB_PROVISION, &payload, spec)?;
        resp.workspace
            .map(|w| w.trim().to_string())
            .filter(|w| !w.is_empty())
            .ok_or_else(|| {
                format!(
                    "machine provider {:?} accepted the provision but returned no \"workspace\" path",
                    self.scheme
                )
            })
    }
}

impl ProviderRunner {
    /// Invoke a handle-scoped verb (`status`/`stream`/`cancel`).
    fn invoke_handle(
        &self,
        verb: &str,
        handle: &str,
        since: Option<i64>,
        spec: &RunnerSpec,
    ) -> Result<ProviderResponse, String> {
        let mut extra = vec!["--handle".to_string(), handle.to_string()];
        if let Some(n) = since {
            extra.push("--since".to_string());
            extra.push(n.to_string());
        }
        self.invoke_with(verb, "", spec, &extra)
    }

    /// Drive an async `exec` handle to completion: poll `status`, pump `stream`
    /// into the session's Live View snapshot, and `cancel` if the token trips.
    fn poll_to_completion(
        &self,
        handle: &str,
        spec: &RunnerSpec,
        cancel: Option<&CancelToken>,
    ) -> RunnerResult {
        let session_name = crate::tmux::session_name(&spec.run_id, &spec.task, &spec.session_id);
        let started = Instant::now();
        let budget = spec.timeout_sec.map(Duration::from_secs);
        let mut transcript = String::new();
        let mut cursor: Option<i64> = None;
        loop {
            if cancel.is_some_and(CancelToken::is_cancelled) {
                // Best-effort: a provider that can't cancel still leaves us
                // reporting the session cancelled, which matches how a local
                // kill that races the process exit behaves.
                if let Err(e) = self.invoke_handle(VERB_CANCEL, handle, None, spec) {
                    crate::rlog!(
                        WARNING,
                        "ralphus [remote] provider {} could not cancel handle {handle}: {e}",
                        self.scheme
                    );
                }
                crate::tmux::write_pane_snapshot(&session_name, &transcript);
                return RunnerResult::failure("cancelled");
            }
            if let Some(budget) = budget {
                if started.elapsed() >= budget {
                    let _ = self.invoke_handle(VERB_CANCEL, handle, None, spec);
                    crate::tmux::write_pane_snapshot(&session_name, &transcript);
                    return RunnerResult::failure(format!(
                        "timed out after {}s on machine provider {:?}",
                        budget.as_secs(),
                        self.scheme
                    ));
                }
            }
            // Pump output first so a session that finishes between polls still
            // gets its final chunk recorded.
            match self.invoke_handle(VERB_STREAM, handle, cursor, spec) {
                Ok(resp) => {
                    if let Some(chunk) = resp.output.filter(|c| !c.is_empty()) {
                        transcript.push_str(&chunk);
                        crate::tmux::write_pane_snapshot(&session_name, &transcript);
                    }
                    if resp.next.is_some() {
                        cursor = resp.next;
                    }
                }
                Err(e) => {
                    // Streaming is a convenience, not the result -- a provider
                    // that doesn't implement it must not fail the session.
                    crate::rlog!(
                        DEBUG,
                        "ralphus [remote] provider {} stream unavailable for {handle}: {e}",
                        self.scheme
                    );
                }
            }
            let status = match self.invoke_handle(VERB_STATUS, handle, None, spec) {
                Ok(s) => s,
                Err(e) => {
                    crate::tmux::write_pane_snapshot(&session_name, &transcript);
                    return RunnerResult::failure(e);
                }
            };
            match status.state.as_deref() {
                Some("running") | None => {}
                Some(_) => {
                    crate::tmux::write_pane_snapshot(&session_name, &transcript);
                    return status.result.unwrap_or_else(|| {
                        RunnerResult::failure(format!(
                            "machine provider {:?} reported handle {handle} finished but returned no \"result\" object",
                            self.scheme
                        ))
                    });
                }
            }
            std::thread::sleep(POLL_INTERVAL);
        }
    }

    /// Shared body of [`Runner::run`] and [`Runner::run_cancellable`].
    fn exec(&self, spec: &RunnerSpec, cancel: Option<&CancelToken>) -> RunnerResult {
        if cancel.is_some_and(CancelToken::is_cancelled) {
            return RunnerResult::failure("cancelled before dispatch to the machine provider");
        }
        let payload = match serde_json::to_string(spec) {
            Ok(p) => p,
            Err(e) => {
                return RunnerResult::failure(format!("could not serialize session spec: {e}"));
            }
        };
        let resp = match self.invoke(VERB_EXEC, &payload, spec) {
            Ok(r) => r,
            Err(e) => {
                crate::rlog!(WARNING, "ralphus [remote] {e}");
                return RunnerResult::failure(e);
            }
        };
        // A synchronous provider returns the result outright; an async one
        // returns a handle we then poll for status/output/cancellation.
        if let Some(result) = resp.result {
            return result;
        }
        match resp.handle.filter(|h| !h.trim().is_empty()) {
            Some(handle) => self.poll_to_completion(&handle, spec, cancel),
            None => RunnerResult::failure(format!(
                "machine provider {:?} accepted the exec but returned neither a \"result\" object nor a \"handle\"",
                self.scheme
            )),
        }
    }
}

impl ProviderRunner {
    /// Ask the machine to confirm it is reachable and ready (RAL-185 Phase 3,
    /// Q3). Returns the provider's own detail line, if it offered one.
    ///
    /// Deliberately separate from every other verb: a machine being *down* is a
    /// different fact from work failing on it, and conflating them means a user
    /// debugging a failed run cannot tell "the build broke" from "the build box
    /// is unplugged". Surfacing it once, up front, is what lets the board mark
    /// a machine unreachable before anyone submits against it.
    ///
    /// # Errors
    /// Any failure to reach the machine, verbatim, for display.
    pub fn ping(&self, spec: &RunnerSpec) -> Result<Option<String>, String> {
        let resp = self.invoke(VERB_PING, "", spec)?;
        Ok(resp.detail)
    }
}

impl ProviderRunner {
    /// Run one VCS command in a workspace on this machine and return its
    /// stdout (RAL-185 D7).
    ///
    /// Arguments are sent already split rather than as a shell string: the far
    /// side never has to quote or escape correctly, which is a whole class of
    /// bug — a branch name with a space, a path with a quote — that simply
    /// cannot occur.
    ///
    /// # Errors
    /// Either the provider failing to run the command at all, or the command
    /// running and exiting non-zero. Both are surfaced with the command in the
    /// message, since a caller reading a merge failure needs to know which.
    /// Try this provider's channel, returning `None` when it is unavailable or
    /// misbehaving so the caller falls back to a one-shot spawn.
    ///
    /// A transport problem must never fail work that would otherwise succeed:
    /// the fallback is semantically identical, only slower. A *command* that
    /// runs and fails still fails, because that arrives as a normal reply.
    fn run_via_channel(&self, payload: &str) -> Option<ProviderResponse> {
        if !self.supports_channel {
            return None;
        }
        match crate::channel::request(&self.scheme, &self.uri, &self.program, &self.args, payload) {
            Ok(line) => match serde_json::from_str::<ProviderResponse>(&line) {
                Ok(resp) => Some(resp),
                Err(e) => {
                    crate::rlog!(
                        WARNING,
                        "ralphus [remote] provider {} channel returned unparseable JSON ({e}); falling back to a one-shot spawn",
                        self.scheme
                    );
                    None
                }
            },
            Err(e) => {
                crate::rlog!(
                    WARNING,
                    "ralphus [remote] provider {} channel unavailable ({e}); falling back to a one-shot spawn",
                    self.scheme
                );
                None
            }
        }
    }

    /// Read a file from a workspace on this machine.
    ///
    /// # Errors
    /// When the provider cannot read it — including because it does not exist,
    /// which callers often treat as "absent" rather than a failure.
    pub fn read_file(&self, path: &str, spec: &RunnerSpec) -> Result<String, String> {
        let req = FileRequest {
            path: path.to_string(),
            content: None,
            recursive: false,
        };
        let payload = serde_json::to_string(&req)
            .map_err(|e| format!("could not serialize read-file request: {e}"))?;
        let resp = self.invoke(VERB_READ_FILE, &payload, spec)?;
        Ok(resp.stdout.unwrap_or_default())
    }

    /// Write a file into a workspace on this machine, creating parent
    /// directories as needed.
    ///
    /// # Errors
    /// Any provider-side failure.
    pub fn write_file(&self, path: &str, content: &str, spec: &RunnerSpec) -> Result<(), String> {
        let req = FileRequest {
            path: path.to_string(),
            content: Some(content.to_string()),
            recursive: false,
        };
        let payload = serde_json::to_string(&req)
            .map_err(|e| format!("could not serialize write-file request: {e}"))?;
        self.invoke(VERB_WRITE_FILE, &payload, spec).map(|_| ())
    }

    /// Delete a file, or a directory tree when `recursive`.
    ///
    /// # Errors
    /// Any provider-side failure. A path that does not exist is **not** an
    /// error — every caller here is cleaning up and does not care.
    pub fn remove_path(
        &self,
        path: &str,
        recursive: bool,
        spec: &RunnerSpec,
    ) -> Result<(), String> {
        let req = FileRequest {
            path: path.to_string(),
            content: None,
            recursive,
        };
        let payload = serde_json::to_string(&req)
            .map_err(|e| format!("could not serialize remove-path request: {e}"))?;
        self.invoke(VERB_REMOVE_PATH, &payload, spec).map(|_| ())
    }

    pub fn run_vcs(&self, req: &RunRequest, spec: &RunnerSpec) -> Result<String, String> {
        let payload = serde_json::to_string(req)
            .map_err(|e| format!("could not serialize run request: {e}"))?;
        let resp = self
            .run_via_channel(&payload)
            .map_or_else(|| self.invoke(VERB_RUN, &payload, spec), Ok)?;
        match resp.exit_code {
            Some(0) | None => Ok(resp.stdout.unwrap_or_default()),
            Some(code) => Err(format!(
                "{} {} failed on machine {:?} (exit {code}): {}",
                req.program,
                req.args.join(" "),
                self.scheme,
                resp.stdout.unwrap_or_default().trim()
            )),
        }
    }
}

impl Runner for ProviderRunner {
    fn run(&self, spec: &RunnerSpec) -> RunnerResult {
        self.exec(spec, None)
    }

    fn run_cancellable(&self, spec: &RunnerSpec, cancel: &CancelToken) -> RunnerResult {
        self.exec(spec, Some(cancel))
    }
}

/// Build the provider for `machine` from an already-borrowed [`Store`], or
/// `None` when it resolves local.
///
/// Takes `&Store` rather than the `Arc<Mutex<Store>>` a [`MachineRouter`]
/// holds so callers already inside the store lock can use it without
/// deadlocking on a second acquisition.
///
/// # Errors
/// Returns a description when the machine cannot be resolved, names an
/// unregistered provider, or names a built-in that cannot run sessions.
pub fn provider_from_store(store: &Store, machine: &str) -> Result<Option<ProviderRunner>, String> {
    let resolved = store
        .resolve_machine(Some(machine))
        .map_err(|e| e.to_string())?;
    let (scheme, uri) = match resolved {
        ResolvedMachine::Local => return Ok(None),
        ResolvedMachine::Provider { scheme, uri } => (scheme, uri),
    };
    let Some(provider) = store
        .get_machine_provider(&scheme)
        .map_err(|e| e.to_string())?
    else {
        return Err(format!(
            "machine provider {scheme:?} is no longer registered"
        ));
    };
    let supports_channel = provider.supports_channel;
    Ok(Some(
        ProviderRunner::new(provider.program, provider.args, scheme, uri)
            .with_channel(supports_channel),
    ))
}

/// Routes each spec to the local runner or a machine provider, based on the
/// spec's resolved [`RunnerSpec::machine`].
///
/// The scheduler holds one of these instead of a bare `SubprocessRunner`, so
/// every existing dispatch site keeps working unchanged and local runs take
/// exactly the path they always did.
pub struct MachineRouter {
    local: Arc<dyn Runner>,
    store: Arc<Mutex<Store>>,
}

impl MachineRouter {
    /// Wrap `local` (normally a [`crate::runner::SubprocessRunner`]), using
    /// `store` to resolve machine values against the provider registry.
    #[must_use]
    pub fn new(local: Arc<dyn Runner>, store: Arc<Mutex<Store>>) -> Self {
        Self { local, store }
    }

    /// Build the provider runner for `machine`, or `None` when it resolves
    /// local. `Err` when the machine cannot be resolved or dispatched.
    fn provider_for(&self, machine: &str) -> Result<Option<ProviderRunner>, String> {
        let built = {
            let guard = self.store.lock().expect("store mutex poisoned");
            provider_from_store(&guard, machine)?
        };
        Ok(built.map(|p| p.with_cartographer(Arc::clone(&self.store))))
    }

    /// Provision a workspace for a remote session, returning its path on that
    /// machine. `Ok(None)` when `machine` resolves local — the caller keeps
    /// its existing local behavior ([`crate::worktrees::ensure_worktree`]).
    ///
    /// # Errors
    /// Propagates a resolution failure or the provider's own refusal.
    pub fn provision_for(
        &self,
        machine: &str,
        req: &ProvisionRequest,
        spec: &RunnerSpec,
    ) -> Result<Option<String>, String> {
        let Some(provider) = self.provider_for(machine)? else {
            return Ok(None);
        };
        provider.provision(req, spec).map(Some)
    }

    /// Resolve `spec` to the runner that should execute it.
    fn route(&self, spec: &RunnerSpec) -> Result<Option<ProviderRunner>, String> {
        let Some(machine) = spec
            .machine
            .as_deref()
            .map(str::trim)
            .filter(|m| !m.is_empty())
        else {
            return Ok(None);
        };
        self.provider_for(machine)
    }
}

impl Runner for MachineRouter {
    fn run(&self, spec: &RunnerSpec) -> RunnerResult {
        match self.route(spec) {
            Ok(None) => self.local.run(spec),
            Ok(Some(remote)) => remote.run(spec),
            Err(e) => RunnerResult::failure(e),
        }
    }

    fn run_cancellable(&self, spec: &RunnerSpec, cancel: &CancelToken) -> RunnerResult {
        match self.route(spec) {
            Ok(None) => self.local.run_cancellable(spec, cancel),
            Ok(Some(remote)) => remote.run_cancellable(spec, cancel),
            Err(e) => RunnerResult::failure(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// A local runner that records what it was handed.
    struct RecordingLocal {
        seen: Mutex<Vec<String>>,
    }

    impl Runner for RecordingLocal {
        fn run(&self, spec: &RunnerSpec) -> RunnerResult {
            self.seen.lock().unwrap().push(spec.session_id.clone());
            RunnerResult {
                status: "done".to_string(),
                tokens_in: 7,
                tokens_out: 8,
                cost_usd: 0.25,
                summary: "local".to_string(),
                error: None,
                verified: None,
                agent_session_id: None,
                ghost: None,
            }
        }
    }

    fn spec(machine: Option<&str>) -> RunnerSpec {
        RunnerSpec {
            run_id: "run-1".to_string(),
            task: "t".to_string(),
            session_id: "s0".to_string(),
            cwd: ".".to_string(),
            prompt: Some("do work".to_string()),
            command: None,
            agent: "claude".to_string(),
            model: None,
            system_prompt: None,
            system_prompt_position: None,
            timeout_sec: None,
            budget_tokens: None,
            maximum_budget_usd: None,
            verify: false,
            trace_context: None,
            resume_agent_session_id: None,
            env_overrides: BTreeMap::new(),
            machine: machine.map(str::to_string),
        }
    }

    fn router(store: Arc<Mutex<Store>>) -> (MachineRouter, Arc<RecordingLocal>) {
        let local = Arc::new(RecordingLocal {
            seen: Mutex::new(vec![]),
        });
        let router = MachineRouter::new(Arc::clone(&local) as Arc<dyn Runner>, store);
        (router, local)
    }

    fn store() -> Arc<Mutex<Store>> {
        Arc::new(Mutex::new(Store::open_in_memory().unwrap()))
    }

    #[test]
    fn an_unset_machine_routes_to_the_local_runner() {
        let (router, local) = router(store());
        let r = router.run(&spec(None));
        assert_eq!(r.status, "done");
        assert_eq!(r.summary, "local");
        assert_eq!(local.seen.lock().unwrap().as_slice(), ["s0"]);
    }

    #[test]
    fn an_explicitly_local_machine_routes_to_the_local_runner() {
        let (router, local) = router(store());
        let r = router.run(&spec(Some("local")));
        assert_eq!(r.summary, "local");
        assert_eq!(local.seen.lock().unwrap().len(), 1);
    }

    #[test]
    fn an_unregistered_machine_fails_the_session_without_running_it_locally() {
        // The dangerous failure mode this guards: silently falling back to the
        // local runner would run the work on the wrong host while the user
        // believes it went to the build farm.
        let (router, local) = router(store());
        let r = router.run(&spec(Some("ghostfarm:A")));
        assert_eq!(r.status, "failed");
        assert!(
            r.error.as_deref().unwrap_or_default().contains("ghostfarm"),
            "{r:?}"
        );
        assert!(
            local.seen.lock().unwrap().is_empty(),
            "must NOT silently fall back to local execution"
        );
    }

    #[test]
    fn a_deregistered_provider_fails_rather_than_running_locally() {
        let s = store();
        s.lock()
            .unwrap()
            .register_machine_provider("ib", "", "/opt/ib.sh", &[], PROTOCOL_VERSION, false)
            .unwrap();
        let (router, local) = router(Arc::clone(&s));
        s.lock().unwrap().deregister_machine_provider("ib").unwrap();
        let r = router.run(&spec(Some("ib:A")));
        assert_eq!(r.status, "failed");
        assert!(local.seen.lock().unwrap().is_empty());
    }

    #[test]
    fn cancelling_before_dispatch_does_not_reach_the_provider() {
        let s = store();
        s.lock()
            .unwrap()
            .register_machine_provider(
                "ib",
                "",
                "/nonexistent/provider.sh",
                &[],
                PROTOCOL_VERSION,
                false,
            )
            .unwrap();
        let (router, _local) = router(s);
        let cancel = CancelToken::new();
        cancel.cancel();
        let r = router.run_cancellable(&spec(Some("ib:A")), &cancel);
        assert_eq!(r.status, "failed");
        assert!(
            r.error.as_deref().unwrap_or_default().contains("cancelled"),
            "{r:?}"
        );
    }

    /// Write a throwaway provider script that echoes `stdout_json` verbatim,
    /// plus any `stderr_lines`, then exits 0. Returns its path.
    ///
    /// This is what makes the exec path testable end to end without a real
    /// remote machine: the contract is "a program that reads stdin and writes
    /// one JSON envelope to stdout", which a two-line script satisfies exactly
    /// as well as a build farm does.
    fn fake_provider(tag: &str, stdout_json: &str, stderr_lines: &[&str]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("ral185-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        // An empty `stdout_json` must produce genuinely no stdout. A bare
        // `echo` in cmd prints "ECHO is off." rather than nothing, which would
        // make the silent-provider case look like a malformed-JSON case.
        let (path, body) = if cfg!(windows) {
            let mut b = String::from("@echo off\r\n");
            for line in stderr_lines {
                b.push_str(&format!("echo {line}>&2\r\n"));
            }
            if !stdout_json.is_empty() {
                b.push_str(&format!("echo {stdout_json}\r\n"));
            }
            (dir.join("provider.cmd"), b)
        } else {
            let mut b = String::from("#!/bin/sh\n");
            for line in stderr_lines {
                b.push_str(&format!("echo '{line}' >&2\n"));
            }
            if !stdout_json.is_empty() {
                b.push_str(&format!("echo '{stdout_json}'\n"));
            }
            (dir.join("provider.sh"), b)
        };
        std::fs::write(&path, body).expect("write provider script");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        }
        path
    }

    #[test]
    fn a_real_provider_script_executes_a_session_and_its_result_is_returned() {
        let script = fake_provider(
            "exec-ok",
            r#"{"ok":true,"protocol_version":1,"result":{"status":"done","tokens_in":3,"tokens_out":4,"cost_usd":0.5,"summary":"remote ok"}}"#,
            &[],
        );
        let s = store();
        s.lock()
            .unwrap()
            .register_machine_provider(
                "ib",
                "",
                &script.to_string_lossy(),
                &[],
                PROTOCOL_VERSION,
                false,
            )
            .unwrap();
        let (router, local) = router(Arc::clone(&s));
        let r = router.run(&spec(Some("ib:slot-A")));
        assert_eq!(r.status, "done", "{r:?}");
        assert_eq!(r.summary, "remote ok");
        assert_eq!(r.tokens_in, 3);
        assert_eq!(r.tokens_out, 4);
        assert!((r.cost_usd - 0.5).abs() < f64::EPSILON);
        assert!(
            local.seen.lock().unwrap().is_empty(),
            "the session must have run remotely, not locally"
        );
    }

    #[test]
    fn a_provider_reporting_ok_false_is_an_infrastructure_failure_not_a_task_result() {
        // The distinction matters: `ok:false` means the provider could not run
        // the work at all, which is a different thing from work that ran and
        // failed. Collapsing them would make a broken build farm look like
        // legitimately failing tasks.
        let script = fake_provider(
            "exec-reject",
            r#"{"ok":false,"protocol_version":1,"error":"slot A is offline"}"#,
            &[],
        );
        let s = store();
        s.lock()
            .unwrap()
            .register_machine_provider(
                "ib",
                "",
                &script.to_string_lossy(),
                &[],
                PROTOCOL_VERSION,
                false,
            )
            .unwrap();
        let (router, _local) = router(s);
        let r = router.run(&spec(Some("ib:A")));
        assert_eq!(r.status, "failed");
        let err = r.error.unwrap_or_default();
        assert!(err.contains("slot A is offline"), "{err}");
    }

    #[test]
    fn a_provider_declaring_the_wrong_contract_version_is_refused() {
        let script = fake_provider(
            "exec-badver",
            r#"{"ok":true,"protocol_version":99,"result":{"status":"done","summary":"x"}}"#,
            &[],
        );
        let s = store();
        s.lock()
            .unwrap()
            .register_machine_provider(
                "ib",
                "",
                &script.to_string_lossy(),
                &[],
                PROTOCOL_VERSION,
                false,
            )
            .unwrap();
        let (router, _local) = router(s);
        let r = router.run(&spec(Some("ib:A")));
        assert_eq!(r.status, "failed");
        assert!(
            r.error
                .as_deref()
                .unwrap_or_default()
                .contains("version 99"),
            "{r:?}"
        );
    }

    #[test]
    fn a_provider_that_writes_nothing_to_stdout_says_so_and_quotes_its_stderr() {
        let script = fake_provider("exec-silent", "", &["boom: no ssh key"]);
        let s = store();
        s.lock()
            .unwrap()
            .register_machine_provider(
                "ib",
                "",
                &script.to_string_lossy(),
                &[],
                PROTOCOL_VERSION,
                false,
            )
            .unwrap();
        let (router, _local) = router(s);
        let r = router.run(&spec(Some("ib:A")));
        assert_eq!(r.status, "failed");
        let err = r.error.unwrap_or_default();
        assert!(err.contains("no JSON on stdout"), "{err}");
        assert!(
            err.contains("no ssh key"),
            "stderr must be quoted so the failure is diagnosable: {err}"
        );
    }

    #[test]
    fn provider_stderr_ralphus_events_reach_cartographer() {
        // Without this a remote session is invisible in Cartographer AND its
        // live cost cap silently stops being enforced -- both fail quietly.
        let event = r#"{"source":"llm-invoke","message":"done","payload":{}}"#;
        let line = format!("{EVENT_MARKER}{event}");
        let script = fake_provider(
            "exec-events",
            r#"{"ok":true,"protocol_version":1,"result":{"status":"done","summary":"ok"}}"#,
            &[&line],
        );
        let s = store();
        s.lock()
            .unwrap()
            .register_machine_provider(
                "ib",
                "",
                &script.to_string_lossy(),
                &[],
                PROTOCOL_VERSION,
                false,
            )
            .unwrap();
        let (router, _local) = router(Arc::clone(&s));
        let r = router.run(&spec(Some("ib:A")));
        assert_eq!(r.status, "done", "{r:?}");
        let page = s
            .lock()
            .unwrap()
            .cartographer_query(&crate::cartographer::CartographerFilter {
                run_id: Some("run-1".to_string()),
                ..crate::cartographer::CartographerFilter::recent(50)
            })
            .unwrap();
        assert!(
            page.rows.iter().any(|row| row.source == "llm-invoke"),
            "the provider's RALPHUS_EVENT line must reach Cartographer; got {:?}",
            page.rows.iter().map(|r| &r.source).collect::<Vec<_>>()
        );
    }

    /// A provider script that dispatches on the verb it was handed, so the
    /// async `exec` → `status`/`stream`/`cancel` flow can be exercised for
    /// real. `$1` is the verb (the args are `<verb> --uri <uri> ...`).
    fn fake_async_provider(tag: &str, arms: &[(&str, &str)]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("ral185-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        if cfg!(windows) {
            let mut b = String::from("@echo off\r\n");
            for (verb, json) in arms {
                b.push_str(&format!("if \"%1\"==\"{verb}\" echo {json}\r\n"));
            }
            let path = dir.join("provider.cmd");
            std::fs::write(&path, b).expect("write");
            path
        } else {
            let mut b = String::from("#!/bin/sh\ncase \"$1\" in\n");
            for (verb, json) in arms {
                b.push_str(&format!("  {verb}) echo '{json}' ;;\n"));
            }
            b.push_str("esac\n");
            let path = dir.join("provider.sh");
            std::fs::write(&path, b).expect("write");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
            path
        }
    }

    fn register(s: &Arc<Mutex<Store>>, scheme: &str, script: &std::path::Path) {
        s.lock()
            .unwrap()
            .register_machine_provider(
                scheme,
                "",
                &script.to_string_lossy(),
                &[],
                PROTOCOL_VERSION,
                false,
            )
            .unwrap();
    }

    #[test]
    fn an_async_exec_handle_is_polled_to_completion_and_its_result_returned() {
        // The handle shape is what makes Live View and mid-run cancellation
        // possible at all -- a synchronous exec has nothing to poll or cancel.
        let script = fake_async_provider(
            "async-ok",
            &[
                ("exec", r#"{"ok":true,"protocol_version":1,"handle":"h1"}"#),
                (
                    "stream",
                    r#"{"ok":true,"protocol_version":1,"output":"building...","next":1}"#,
                ),
                (
                    "status",
                    r#"{"ok":true,"protocol_version":1,"state":"done","result":{"status":"done","summary":"remote async ok","tokens_in":11}}"#,
                ),
            ],
        );
        let s = store();
        register(&s, "ib", &script);
        let (router, local) = router(Arc::clone(&s));
        let r = router.run(&spec(Some("ib:A")));
        assert_eq!(r.status, "done", "{r:?}");
        assert_eq!(r.summary, "remote async ok");
        assert_eq!(r.tokens_in, 11);
        assert!(local.seen.lock().unwrap().is_empty());
    }

    #[test]
    fn a_streamed_remote_session_is_readable_through_the_existing_live_view_snapshot() {
        // Reusing the pane-snapshot file the board already reads means remote
        // Live View needs no UI change at all.
        let script = fake_async_provider(
            "async-stream",
            &[
                ("exec", r#"{"ok":true,"protocol_version":1,"handle":"h1"}"#),
                (
                    "stream",
                    r#"{"ok":true,"protocol_version":1,"output":"hello from the farm","next":1}"#,
                ),
                (
                    "status",
                    r#"{"ok":true,"protocol_version":1,"state":"done","result":{"status":"done","summary":"ok"}}"#,
                ),
            ],
        );
        let s = store();
        register(&s, "ib", &script);
        let (router, _local) = router(Arc::clone(&s));
        let sp = spec(Some("ib:A"));
        let r = router.run(&sp);
        assert_eq!(r.status, "done", "{r:?}");
        let name = crate::tmux::session_name(&sp.run_id, &sp.task, &sp.session_id);
        let snapshot = crate::tmux::read_pane_snapshot(&name).unwrap_or_default();
        assert!(
            snapshot.contains("hello from the farm"),
            "streamed output must land in the pane snapshot the board reads; got {snapshot:?}"
        );
        let _ = std::fs::remove_file(crate::tmux::pane_snapshot_path(&name));
    }

    #[test]
    fn a_provider_that_does_not_implement_stream_still_completes_the_session() {
        // Streaming is a convenience. A provider that omits it loses Live View,
        // not the ability to run work.
        let script = fake_async_provider(
            "async-nostream",
            &[
                ("exec", r#"{"ok":true,"protocol_version":1,"handle":"h1"}"#),
                (
                    "status",
                    r#"{"ok":true,"protocol_version":1,"state":"done","result":{"status":"done","summary":"no stream"}}"#,
                ),
            ],
        );
        let s = store();
        register(&s, "ib", &script);
        let (router, _local) = router(Arc::clone(&s));
        let r = router.run(&spec(Some("ib:A")));
        assert_eq!(r.status, "done", "{r:?}");
        assert_eq!(r.summary, "no stream");
    }

    #[test]
    fn cancelling_mid_run_stops_polling_and_reports_cancelled() {
        // Before the handle refactor a cancelled remote session kept running on
        // the remote machine with no way to stop it -- budget burning with no
        // off switch. `status` here never leaves "running", so the only way
        // this test terminates is via the cancel path.
        let script = fake_async_provider(
            "async-cancel",
            &[
                ("exec", r#"{"ok":true,"protocol_version":1,"handle":"h1"}"#),
                (
                    "status",
                    r#"{"ok":true,"protocol_version":1,"state":"running"}"#,
                ),
                ("cancel", r#"{"ok":true,"protocol_version":1}"#),
            ],
        );
        let s = store();
        register(&s, "ib", &script);
        let (router, _local) = router(Arc::clone(&s));
        let cancel = CancelToken::new();
        let flag = cancel.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            flag.cancel();
        });
        let r = router.run_cancellable(&spec(Some("ib:A")), &cancel);
        assert_eq!(r.status, "failed");
        assert_eq!(r.error.as_deref(), Some("cancelled"), "{r:?}");
    }

    #[test]
    fn an_exec_returning_neither_result_nor_handle_is_a_contract_violation() {
        let script = fake_async_provider(
            "async-empty",
            &[("exec", r#"{"ok":true,"protocol_version":1}"#)],
        );
        let s = store();
        register(&s, "ib", &script);
        let (router, _local) = router(s);
        let r = router.run(&spec(Some("ib:A")));
        assert_eq!(r.status, "failed");
        assert!(
            r.error
                .as_deref()
                .unwrap_or_default()
                .contains("neither a \"result\" object nor a \"handle\""),
            "{r:?}"
        );
    }

    #[test]
    fn a_channel_capable_provider_serves_many_run_commands_from_one_process() {
        // The point of the channel: N commands, one spawn. Each reply carries
        // the serving process's pid, so re-spawning would show up as a change.
        let dir = std::env::temp_dir().join(format!("ral185-rrchan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let py = dir.join("chan.py");
        // Built line-by-line rather than as one continued string literal: Rust's
        // `\`-at-end-of-line strips the following indentation, which silently
        // corrupts Python.
        std::fs::write(
            &py,
            [
                "import sys, os",
                "pid = os.getpid()",
                "for line in sys.stdin:",
                "    if not line.strip():",
                "        continue",
                r#"    print('{"ok":true,"protocol_version":1,"exit_code":0,"stdout":"pid=%d"}' % pid, flush=True)"#,
            ]
            .join("
"),
        )
        .unwrap();
        let provider = ProviderRunner::new(
            "python",
            vec![py.to_string_lossy().into_owned()],
            "chantest",
            "A",
        )
        .with_channel(true);
        let req = RunRequest {
            cwd: ".".to_string(),
            program: "git".to_string(),
            args: vec!["status".to_string()],
        };
        let sp = spec(Some("chantest:A"));
        let a = provider.run_vcs(&req, &sp).expect("first");
        let b = provider.run_vcs(&req, &sp).expect("second");
        let c = provider.run_vcs(&req, &sp).expect("third");
        assert_eq!(a, b);
        assert_eq!(b, c);
        assert!(a.starts_with("pid="), "{a}");
        crate::channel::close("chantest", "A");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_broken_channel_falls_back_to_a_one_shot_spawn_instead_of_failing_the_command() {
        // A transport problem must never fail work that would otherwise
        // succeed. This provider exits immediately in channel mode but answers
        // a normal `run` invocation, so the command must still complete.
        let dir = std::env::temp_dir().join(format!("ral185-rrfall-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let ok = r#"{"ok":true,"protocol_version":1,"exit_code":0,"stdout":"fell back"}"#;
        let py = dir.join("half.py");
        std::fs::write(
            &py,
            [
                "import sys",
                "if 'channel' in sys.argv:",
                "    sys.exit(0)",
                "sys.stdin.read()",
                &format!("print('{ok}')"),
            ]
            .join(
                "
",
            ),
        )
        .unwrap();
        let provider = ProviderRunner::new(
            "python",
            vec![py.to_string_lossy().into_owned()],
            "falltest",
            "A",
        )
        .with_channel(true);
        let req = RunRequest {
            cwd: ".".to_string(),
            program: "git".to_string(),
            args: vec!["status".to_string()],
        };
        let out = provider
            .run_vcs(&req, &spec(Some("falltest:A")))
            .expect("must fall back rather than fail");
        assert_eq!(out.trim(), "fell back");
        crate::channel::close("falltest", "A");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_provider_program_that_cannot_be_spawned_reports_which_provider_failed() {
        let s = store();
        s.lock()
            .unwrap()
            .register_machine_provider(
                "ib",
                "",
                "/definitely/not/a/real/provider",
                &[],
                PROTOCOL_VERSION,
                false,
            )
            .unwrap();
        let (router, _local) = router(s);
        let r = router.run(&spec(Some("ib:A")));
        assert_eq!(r.status, "failed");
        let err = r.error.unwrap_or_default();
        assert!(err.contains("ib"), "error must name the provider: {err}");
    }
}
