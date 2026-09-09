//! Remote cell execution via a registered machine provider (RAL-185
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
//! The router is what the scheduler holds, so cell dispatch stays a single
//! `&dyn Runner` call and nothing above it needs to know a machine exists.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::cancel::CancelToken;
use crate::machines::{PROTOCOL_VERSION, ResolvedMachine};
use crate::runner::{EVENT_MARKER, LiveUsage, Runner, RunnerResult, RunnerSpec};
use crate::store::Store;

/// The `exec` verb: run one cell/proof in a provisioned workspace.
pub const VERB_EXEC: &str = "exec";

/// The `provision` verb: ensure a workspace exists on the machine, and report
/// the path to use as the cell's `cwd`.
pub const VERB_PROVISION: &str = "provision";

/// The `run` verb: execute one VCS command in a workspace and return its
/// output. The transport underneath is entirely the provider's choice — a
/// one-shot SSH invocation, a multiplexed connection, or a persistent worker
/// channel — which is why the contract says nothing about it (RAL-185 D7).
///
/// "Channel" rather than "cell" deliberately: a *cell* is already a
/// first-class ralphus concept (task → cell → proof), and reusing the word
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

/// The `capabilities` verb (RAL-355 Phase 0 remainder): report what this
/// provider/machine pairing actually supports, so the daemon (and Phase 9's
/// `check health --all-remotes`) can display it and refuse work that needs a
/// capability the machine lacks, instead of discovering the gap mid-Cell.
/// Optional — a provider that does not implement it is not a hard failure;
/// [`ProviderRunner::capabilities`] treats "unimplemented" the same as "no
/// capability information available" rather than refusing to work with an
/// otherwise-functional provider.
pub const VERB_CAPABILITIES: &str = "capabilities";

/// The `status` verb: report whether an async `exec` handle is still running.
pub const VERB_STATUS: &str = "status";

/// The `stream` verb: fetch output produced since a cursor, for Live View.
pub const VERB_STREAM: &str = "stream";

/// The `cancel` verb: stop the work behind an `exec` handle.
pub const VERB_CANCEL: &str = "cancel";

/// The `cleanup` verb: tear a provisioned workspace down (RAL-201).
///
/// Never called automatically — see [`ProviderRunner::cleanup`]'s doc for the
/// retention policy. Documented in `docs/machine-providers.md`'s verb table
/// since RAL-185 Phase 1, but had no daemon-side dispatch until now.
pub const VERB_CLEANUP: &str = "cleanup";

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
    /// The already-resolved (RAL-100/RAL-258: no `?upstream=<<sentinel>>` or
    /// literal `<remote>/<branch>` ambiguity left) reference `branch` should
    /// be created from/track, when meaningful. Mirrors what
    /// `worktrees::ensure_worktree`'s own `upstream` parameter carries for a
    /// local worktree (RAL-355 Phase 4) -- until this was added, a
    /// `provision` request had no way to say what a brand-new remote branch
    /// should be based on at all.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upstream: Option<String>,
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
    /// Owning squad, for a provider that wants to namespace its workspaces.
    pub squad_id: String,
    /// Owning cell id, same purpose.
    pub cell_id: String,
    /// This machine's configured durable remote storage root (RAL-355
    /// Phase 2: `[machine.targets.<name>].remote_root`), when one is
    /// configured. A provider that manages persistent, deterministic
    /// storage (unlike this provider's own pre-Phase-4 behavior, which
    /// derived an ephemeral-shaped workspace under
    /// `RALPHUS_SSH_REMOTE_BASE`) needs this to know *where* to put
    /// anything durable — `None` when no target is configured for this
    /// machine, which a provider that requires durable storage should
    /// refuse clearly rather than silently falling back to a guess.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote_root: Option<String>,
    /// Target-specific runner installation policy. Providers that do not
    /// manage runner deployment may ignore this field.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runner: Option<TargetRunnerConfig>,
}

/// The payload sent to a provider's `cleanup` verb (RAL-355 Phase 2
/// remainder). Carries enough identity to target exactly one workspace under
/// the Phase 4 durable, multi-workspace-per-machine layout — the verb's
/// original `{"machine": "<scheme>:<uri>"}`-only contract predates that
/// layout and could only ever have meant "the one workspace this machine
/// has", which stopped being true the moment a machine could hold many
/// projects and many branches per project.
#[derive(Debug, Clone, Serialize)]
pub struct CleanupRequest {
    /// Registered project name, for the same deterministic directory
    /// derivation `provision` uses (`layout::project_dir`).
    pub project: String,
    /// The project's registered clone URL — part of that same derivation,
    /// so a cleanup request can never accidentally resolve to a different
    /// project's directory that happens to share a name.
    pub clone_url: String,
    /// Remove one worktree's directory when set; the whole project
    /// directory (repository plus every worktree) when omitted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// This machine's configured durable remote storage root, mirroring
    /// [`ProvisionRequest::remote_root`]. `None` in legacy (pre-Phase-2)
    /// mode, where a provider has no root to scope the removal under.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote_root: Option<String>,
}

/// Runner policy resolved from `[machine.targets.*]` for provider use.
#[derive(Debug, Clone, Serialize)]
pub struct TargetRunnerConfig {
    pub mode: String,
    pub command: String,
    pub artifacts: BTreeMap<String, String>,
    pub remote_root: String,
}

/// What a provider/machine pairing reports supporting (RAL-355 Phase 0
/// remainder), via the optional `capabilities` verb. Every field is
/// best-effort: a provider may omit anything it cannot cheaply determine
/// (e.g. `os`/`arch` when it has no remote-probe mechanism), and `None`
/// means "unknown", never "no"/"false" by omission.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct Capabilities {
    /// Remote operating system family, e.g. `"linux"`. Lowercase by
    /// convention so callers never have to case-fold it themselves.
    #[serde(default)]
    pub os: Option<String>,
    /// Remote CPU architecture, e.g. `"x86_64"`.
    #[serde(default)]
    pub arch: Option<String>,
    /// Verbs this provider actually implements against this machine —
    /// distinct from the daemon/provider protocol's *documented* verb set,
    /// since a real provider may implement a subset (`docs/machine-providers.md`).
    #[serde(default)]
    pub supported_ops: Vec<String>,
    /// Whether `exec` can return a durable async handle (`status`/`stream`/
    /// `cancel`-capable) rather than only blocking synchronously.
    #[serde(default)]
    pub async_exec: bool,
    /// Whether this machine supports an interactive terminal relay (Open
    /// Agent, RAL-355 Phase 10).
    #[serde(default)]
    pub terminal: bool,
    /// The remote `ralphus-runner` executable's own version string
    /// (`ralphus-runner --version`'s output), when the provider could
    /// determine it without side effects (e.g. without triggering an
    /// upload). `None` when unknown or not yet installed.
    #[serde(default)]
    pub runner_version: Option<String>,
}

/// One provider's JSON response envelope (`docs/machine-providers.md`).
///
/// `result` is only present for `exec`. It is deliberately a nested object
/// rather than flattened into the envelope so a provider can report
/// `ok: false` (the *invocation* failed) distinctly from
/// `ok: true, result.status = "failed"` (the cell ran and failed) — the
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
    /// the cell should use as its `cwd`.
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
    /// `cleanup` only: the absolute path, on the provider's machine, that was
    /// removed — so an operator can see exactly what a cleanup call did
    /// rather than trusting a bare `ok: true`.
    #[serde(default)]
    removed: Option<String>,
    /// `capabilities` only.
    #[serde(default)]
    capabilities: Option<Capabilities>,
}

/// Runs cells on one machine by invoking a registered provider program.
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
    /// local run — without this a remote cell is invisible to Cartographer
    /// and, worse, its live cost cap silently stops being enforced.
    cartographer: Option<Arc<Mutex<Store>>>,
    /// The most recent usage snapshot seen on any invocation's
    /// stderr (RAL-161/RAL-201), so [`Self::poll_to_completion`] can enforce
    /// `maximum_budget_usd` the same way [`crate::runner::SubprocessRunner`]
    /// does locally. Shared via `Arc` because each provider invocation reads
    /// its stderr on its own short-lived thread (see [`Self::invoke_with`]),
    /// not the thread that later checks the cap.
    live_usage: Arc<Mutex<Option<LiveUsage>>>,
    /// Target-specific runner policy forwarded to providers through a
    /// protected process environment value, never command-line arguments.
    target_runner: Option<TargetRunnerConfig>,
    /// Per-agent executable overrides for this target (RAL-355 Phase 7
    /// remainder), keyed by agent name. Consulted, never forwarded to the
    /// provider directly — see [`Self::resolve_remote_executable`].
    agent_executables: BTreeMap<String, String>,
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
            live_usage: Arc::new(Mutex::new(None)),
            target_runner: None,
            agent_executables: BTreeMap::new(),
        }
    }

    /// Attach this target's per-agent executable overrides
    /// (`[machine.targets.<name>.agents]`).
    #[must_use]
    pub fn with_agent_executables(mut self, agent_executables: BTreeMap<String, String>) -> Self {
        self.agent_executables = agent_executables;
        self
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

    /// Attach the runner policy for the concrete configured target.
    #[must_use]
    pub fn with_target_runner(mut self, runner: Option<TargetRunnerConfig>) -> Self {
        self.target_runner = runner;
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
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(runner) = &self.target_runner {
            let encoded = serde_json::to_string(runner)
                .map_err(|e| format!("could not serialize target runner policy: {e}"))?;
            cmd.env("RALPHUS_TARGET_RUNNER_CONFIG", encoded);
        }
        let mut child = cmd.spawn().map_err(|e| {
            format!(
                "could not run machine provider {:?} ({}): {e}",
                self.scheme, self.program
            )
        })?;
        // ralphus[ignore-rlog-pair]: this low-level helper has no Store; its Store-owning caller records the structured workflow outcome
        crate::rlog!(
            INFO,
            "ralphus [remote] provider {} verb={verb} uri={} squad={} cell={}",
            self.scheme,
            self.uri,
            spec.squad_id,
            spec.cell_id
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
            let squad_id = spec.squad_id.clone();
            let cell_id = spec.cell_id.clone();
            let task = spec.task.clone();
            let live_usage = Arc::clone(&self.live_usage);
            std::thread::spawn(move || {
                let mut tail = String::new();
                for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                    if let Some(json) = line.trim_end().strip_prefix(EVENT_MARKER) {
                        let fwd = crate::runner::forward_runner_event(
                            cartographer.as_ref(),
                            &squad_id,
                            &cell_id,
                            &task,
                            json,
                        );
                        // RAL-161/RAL-201: keep the latest snapshot so a
                        // caller polling this handle can enforce
                        // `maximum_budget_usd` the same way a local cell
                        // does -- without this a remote cell's live cost
                        // cap is silently unenforced (usage still lands in
                        // the DB via `forward_runner_event`, but nothing acts
                        // on it mid-run).
                        if let Some(usage) = fwd.live_usage {
                            *live_usage.lock().expect("poisoned") = Some(usage);
                        }
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
        let mut resp: ProviderResponse = serde_json::from_str(trimmed).map_err(|e| {
            format!(
                "machine provider {:?} returned unparseable JSON: {e} ({})",
                self.scheme,
                redact_remote_text(&truncate(trimmed, 300))
            )
        })?;
        redact_provider_response(&mut resp);
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

impl ProviderRunner {
    /// Emit a Cartographer record for a lifecycle event on this provider, when
    /// a store handle is attached (see [`Self::with_cartographer`]). A no-op
    /// in a test/context that never wired one, matching every other
    /// best-effort Cartographer call site in this codebase.
    fn note(
        &self,
        spec: &RunnerSpec,
        level: crate::logging::LogLevel,
        message: &str,
        payload: serde_json::Value,
    ) {
        let Some(store) = &self.cartographer else {
            return;
        };
        let Ok(guard) = store.lock() else {
            return;
        };
        crate::cartographer::Note::new("remote")
            .level(level)
            .scope("cell")
            .squad(&spec.squad_id)
            .cell(&spec.cell_id)
            .task(&spec.task)
            .emit(&guard, message, payload);
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    format!("{}…", &s[..max])
}

fn redact_remote_text(text: &str) -> String {
    let registered = crate::redact::redact_all(text);
    ralphus_core::redact::redact_secrets(&registered).into_owned()
}

fn redact_provider_response(response: &mut ProviderResponse) {
    for text in [
        &mut response.error,
        &mut response.output,
        &mut response.detail,
        &mut response.stdout,
    ]
    .into_iter()
    .flatten()
    {
        *text = redact_remote_text(text);
    }
    if let Some(result) = &mut response.result {
        result.summary = redact_remote_text(&result.summary);
        if let Some(error) = &mut result.error {
            *error = redact_remote_text(error);
        }
        if let Some(ghost) = &mut result.ghost {
            *ghost = redact_remote_text(ghost);
        }
    }
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

    /// Stop the work behind an async `exec` handle, without polling it to
    /// completion first (RAL-201). Used by
    /// [`reconcile_remote_exec_handles`] to stop a stale attempt on a
    /// provider after a daemon restart -- unlike [`Self::exec`]'s own
    /// cancel-on-drop inside `poll_to_completion`, this is a one-shot call
    /// with no owning poll loop.
    ///
    /// # Errors
    /// The provider's own refusal reason, verbatim.
    pub fn cancel_handle(&self, handle: &str, spec: &RunnerSpec) -> Result<(), String> {
        self.invoke_handle(VERB_CANCEL, handle, None, spec)
            .map(|_| ())
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
    /// into the cell's Live View snapshot, and `cancel` if the token trips.
    fn poll_to_completion(
        &self,
        handle: &str,
        spec: &RunnerSpec,
        cancel: Option<&CancelToken>,
    ) -> RunnerResult {
        let session_name = crate::tmux::session_name(&spec.squad_id, &spec.task, &spec.cell_id);
        let started = Instant::now();
        let budget = spec.timeout_sec.map(Duration::from_secs);
        let mut transcript = String::new();
        let mut cursor: Option<i64> = None;
        loop {
            if cancel.is_some_and(CancelToken::is_cancelled) {
                // Best-effort: a provider that can't cancel still leaves us
                // reporting the cell cancelled, which matches how a local
                // kill that races the process exit behaves.
                if let Err(e) = self.invoke_handle(VERB_CANCEL, handle, None, spec) {
                    crate::rlog!(
                        WARNING,
                        "ralphus [remote] provider {} could not cancel handle {handle}: {e}",
                        self.scheme
                    );
                }
                crate::tmux::write_pane_snapshot(&session_name, &transcript);
                self.note(
                    spec,
                    crate::logging::LogLevel::INFO,
                    "remote cell cancelled",
                    serde_json::json!({"handle": handle, "scheme": self.scheme}),
                );
                return RunnerResult::failure("cancelled");
            }
            if let Some(budget) = budget {
                if started.elapsed() >= budget {
                    let _ = self.invoke_handle(VERB_CANCEL, handle, None, spec);
                    crate::tmux::write_pane_snapshot(&session_name, &transcript);
                    self.note(
                        spec,
                        crate::logging::LogLevel::WARNING,
                        "remote cell timed out",
                        serde_json::json!({
                            "handle": handle,
                            "scheme": self.scheme,
                            "timeout_sec": budget.as_secs(),
                        }),
                    );
                    return RunnerResult::failure(format!(
                        "timed out after {}s on machine provider {:?}",
                        budget.as_secs(),
                        self.scheme
                    ));
                }
            }
            // RAL-161/RAL-201: mirror `SubprocessRunner`'s live cost-cap kill.
            // `live_usage` is populated by the stderr-reading thread of every
            // `invoke_with` call this handle has made so far (see
            // `Self::invoke_with`) -- without this check a remote cell's
            // `maximum_budget_usd` is silently unenforced, even though the
            // usage itself is still persisted to the DB.
            if let Some(cap) = spec.maximum_budget_usd {
                let usage = self
                    .live_usage
                    .lock()
                    .expect("poisoned")
                    .unwrap_or_default();
                if usage.cost_usd > cap {
                    if let Err(e) = self.invoke_handle(VERB_CANCEL, handle, None, spec) {
                        crate::rlog!(
                            WARNING,
                            "ralphus [remote] provider {} could not cancel over-budget handle {handle}: {e}",
                            self.scheme
                        );
                    }
                    crate::tmux::write_pane_snapshot(&session_name, &transcript);
                    crate::rlog!(
                        WARNING,
                        "ralphus [remote] cost ${:.4} exceeded maximum_budget_usd cap ${cap:.4} on machine provider {:?}, cancelling handle {handle}",
                        usage.cost_usd,
                        self.scheme
                    );
                    self.note(
                        spec,
                        crate::logging::LogLevel::WARNING,
                        "remote cell cost cap exceeded",
                        serde_json::json!({
                            "handle": handle,
                            "scheme": self.scheme,
                            "cost_usd": usage.cost_usd,
                            "cap": cap,
                        }),
                    );
                    return RunnerResult::cost_exceeded(usage, cap);
                }
            }
            // Pump output first so a cell that finishes between polls still
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
                    // that doesn't implement it must not fail the cell.
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
                Some("starting" | "running") | None => {}
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
        let mut spec = spec.clone();
        match self.resolve_remote_executable(&spec) {
            Ok(resolved) => spec.executable = resolved,
            Err(e) => return RunnerResult::failure(e),
        }
        let payload = match provider_exec_payload(&spec) {
            Ok(p) => p,
            Err(e) => {
                return RunnerResult::failure(format!("could not serialize cell spec: {e}"));
            }
        };
        let resp = match self.invoke(VERB_EXEC, &payload, &spec) {
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
            Some(handle) => {
                // RAL-201: persist the handle before polling so a daemon
                // restart mid-poll can reconcile it (`reconcile_remote_exec_handles`)
                // instead of silently leaving this work running unattended on
                // the provider while the daemon starts a fresh attempt from
                // scratch. Best-effort: a store failure here must not block
                // dispatch, the same way every other Cartographer write in
                // this module is best-effort.
                if let Some(store) = &self.cartographer {
                    if let Ok(guard) = store.lock() {
                        let _ = guard.save_remote_exec_handle(
                            &spec.squad_id,
                            &spec.cell_id,
                            &self.scheme,
                            &self.uri,
                            &handle,
                        );
                    }
                }
                let result = self.poll_to_completion(&handle, &spec, cancel);
                if let Some(store) = &self.cartographer {
                    if let Ok(guard) = store.lock() {
                        let _ = guard.clear_remote_exec_handle(&spec.squad_id, &spec.cell_id);
                    }
                }
                result
            }
            None => RunnerResult::failure(format!(
                "machine provider {:?} accepted the exec but returned neither a \"result\" object nor a \"handle\"",
                self.scheme
            )),
        }
    }

    /// Resolve which executable a remote cell's agent backend should launch
    /// with (RAL-355 Phase 7 remainder).
    ///
    /// A target-specific override
    /// (`[machine.targets.<name>.agents].<agent>`) always wins when present
    /// — that is exactly the "deliberately mapped" escape hatch the plan
    /// calls for. Otherwise, `spec.executable` (an agent profile's explicit
    /// override, resolved for the *daemon's own* filesystem) is forwarded
    /// unchanged only if it is a bare command name; a path-shaped value
    /// (contains `/` or `\`) is refused before ever reaching the provider,
    /// since a daemon-local path is essentially never valid on a different
    /// machine and running it anyway would fail confusingly deep inside the
    /// remote runner instead of with an actionable message here. `None`
    /// (no override at all) passes through unchanged, letting the remote
    /// runner fall back to its own `RALPHUS_CLAUDE_COMMAND`-style
    /// environment default or bare command on the remote account's `PATH`.
    ///
    /// # Errors
    /// A daemon-local absolute/path-shaped executable override with no
    /// target-specific mapping to replace it.
    fn resolve_remote_executable(&self, spec: &RunnerSpec) -> Result<Option<String>, String> {
        if let Some(mapped) = self.agent_executables.get(&spec.agent) {
            return Ok(Some(mapped.clone()));
        }
        match &spec.executable {
            Some(path) if path.contains('/') || path.contains('\\') => Err(format!(
                "cell '{}' agent {:?} has a daemon-local executable override {path:?}, which \
                 cannot run on machine {:?} — configure [machine.targets.<name>.agents].{:?} to \
                 map it, or leave the agent profile's executable unset to use the remote \
                 account's PATH",
                spec.cell_id, spec.agent, self.scheme, spec.agent
            )),
            other => Ok(other.clone()),
        }
    }
}

fn provider_exec_payload(spec: &RunnerSpec) -> Result<String, serde_json::Error> {
    let mut value = serde_json::to_value(spec)?;
    if let Some(object) = value.as_object_mut() {
        object.insert(
            "execution_environment".to_string(),
            serde_json::to_value(&spec.env_overrides)?,
        );
    }
    serde_json::to_string(&value)
}

impl ProviderRunner {
    /// Spawn this provider's `terminal` verb (RAL-355 Phase 10) as a
    /// long-lived child process with piped stdin/stdout, and return the
    /// [`std::process::Child`] handle directly instead of invoking it
    /// through [`Self::invoke`] -- `terminal` is the one verb in the
    /// contract that is not JSON request/response (see
    /// `docs/machine-providers.md`), so there is no envelope to parse here;
    /// the caller relays raw bytes to/from this child's stdio itself for as
    /// long as the terminal session lives.
    ///
    /// # Errors
    /// The provider program failing to spawn at all.
    pub fn spawn_terminal(
        &self,
        command: &str,
        cols: u16,
        lines: u16,
    ) -> Result<std::process::Child, String> {
        let mut cmd = Command::new(&self.program);
        cmd.args(&self.args)
            .arg("terminal")
            .arg("--uri")
            .arg(&self.uri)
            .arg("--command")
            .arg(command)
            .arg("--cols")
            .arg(cols.to_string())
            .arg("--lines")
            .arg(lines.to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        cmd.spawn().map_err(|e| {
            format!(
                "could not run machine provider {:?} ({}) for a terminal session: {e}",
                self.scheme, self.program
            )
        })
    }

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

    /// Ask the machine what it supports (RAL-355 Phase 0 remainder).
    /// `Ok(None)` covers both "the provider does not implement this verb"
    /// and "it implemented it but returned nothing" — deliberately treated
    /// the same as "no information available" rather than an error, since a
    /// provider is fully usable without ever answering this.
    ///
    /// # Errors
    /// Only a genuine transport/invocation failure (the machine could not
    /// be reached at all) — an unimplemented verb is reported as `Ok(None)`,
    /// not an `Err`, via the same `UNIMPLEMENTED_VERBS` reply shape every
    /// other optional verb (`channel`) already uses.
    pub fn capabilities(&self, spec: &RunnerSpec) -> Result<Option<Capabilities>, String> {
        match self.invoke(VERB_CAPABILITIES, "", spec) {
            Ok(resp) => Ok(resp.capabilities),
            Err(e) if e.contains("does not implement") => Ok(None),
            Err(e) => Err(e),
        }
    }
}

impl ProviderRunner {
    /// Tear a provisioned workspace down (RAL-201).
    ///
    /// **Never called automatically.** Local worktrees (`worktrees.rs`,
    /// `ensure_worktree`) are never auto-deleted either — a cell's
    /// workspace stays on disk after it finishes so a human can inspect it,
    /// and remote workspaces keep that same property rather than being
    /// reclaimed the moment a run ends. `cleanup` exists so an operator can
    /// explicitly reclaim a workspace once they are actually done with it
    /// (`POST /api/machines/cleanup`, `ralphus machine cleanup`), not as a
    /// lifecycle hook the daemon invokes on its own.
    ///
    /// **Retention policy on failure: nothing is discarded.** This call does
    /// not touch any daemon-side record of the workspace — there is none to
    /// touch (`provision` is idempotent and re-derives the same workspace
    /// from `squad_id`/`cell_id`/the placeholder branch every time, see
    /// [`Self::provision`]) — so a failed cleanup simply leaves the remote
    /// workspace exactly as it was, and the error is returned to the caller
    /// verbatim rather than swallowed. A human can inspect why cleanup
    /// failed (permissions, a still-running process holding the directory
    /// open, a dead machine) and retry, instead of the daemon silently
    /// giving up and forgetting the workspace ever existed.
    ///
    /// # Errors
    /// The provider's own refusal reason, verbatim.
    pub fn cleanup(
        &self,
        req: &CleanupRequest,
        spec: &RunnerSpec,
    ) -> Result<Option<String>, String> {
        let payload = serde_json::to_string(req)
            .map_err(|e| format!("could not serialize cleanup request: {e}"))?;
        let resp = self.invoke(VERB_CLEANUP, &payload, spec)?;
        Ok(resp.removed)
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
                    // ralphus[ignore-rlog-pair]: this low-level helper has no Store; its Store-owning caller records the structured workflow outcome
                    crate::rlog!(
                        WARNING,
                        "ralphus [remote] provider {} channel returned unparseable JSON ({e}); falling back to a one-shot spawn",
                        self.scheme
                    );
                    None
                }
            },
            Err(e) => {
                // ralphus[ignore-rlog-pair]: this low-level helper has no Store; its Store-owning caller records the structured workflow outcome
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

    /// Prove `remote_root` is actually usable on the machine: create a small
    /// probe file, read it back, rename it (approximated as write-under-a-
    /// new-name then remove-the-old-name, since the provider contract has no
    /// direct rename verb), then delete it (RAL-355 Phase 2 remainder).
    ///
    /// This is the "expand and validate on the remote machine, not the
    /// daemon machine" check `[machine.targets.*]` config parsing can only
    /// approximate textually (`machine_targets::looks_absolute`) — round-
    /// tripping a real file through the real machine is the only way to
    /// know a configured root is genuinely writable there. Reuses the
    /// `read-file`/`write-file`/`remove-path` verbs rather than adding a
    /// dedicated probe verb, per the Phase 2 design interview
    /// (`REMOTE_IMPROVEMENTS.local.md`).
    ///
    /// Best-effort cleanup on failure: if a later step fails, this still
    /// attempts to remove whatever it already created, so a failed probe
    /// doesn't litter the remote root with orphaned probe files — but a
    /// cleanup failure itself is never allowed to mask the original error.
    ///
    /// # Errors
    /// Names which of create/read-back/rename/delete failed and why.
    pub fn probe_remote_root(&self, remote_root: &str, spec: &RunnerSpec) -> Result<(), String> {
        const CONTENT: &str = "ralphus-probe";
        let root = remote_root.trim_end_matches('/');
        let mut suffix = [0_u8; 8];
        getrandom::getrandom(&mut suffix)
            .map_err(|e| format!("could not generate a probe file name: {e}"))?;
        let suffix = suffix
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        let created = format!("{root}/.ralphus-probe-{suffix}");
        let renamed = format!("{root}/.ralphus-probe-{suffix}-renamed");

        self.write_file(&created, CONTENT, spec).map_err(|e| {
            format!("remote_root {root:?} probe: could not create a file there: {e}")
        })?;

        let readback = self.read_file(&created, spec).map_err(|e| {
            let _ = self.remove_path(&created, false, spec);
            format!("remote_root {root:?} probe: could not read the file back: {e}")
        })?;
        if readback.trim() != CONTENT {
            let _ = self.remove_path(&created, false, spec);
            return Err(format!(
                "remote_root {root:?} probe: file round-tripped with unexpected content {readback:?}"
            ));
        }

        if let Err(e) = self.write_file(&renamed, CONTENT, spec) {
            let _ = self.remove_path(&created, false, spec);
            return Err(format!(
                "remote_root {root:?} probe: could not create the renamed file: {e}"
            ));
        }
        if let Err(e) = self.remove_path(&created, false, spec) {
            let _ = self.remove_path(&renamed, false, spec);
            return Err(format!(
                "remote_root {root:?} probe: could not remove the pre-rename file: {e}"
            ));
        }

        self.remove_path(&renamed, false, spec).map_err(|e| {
            format!("remote_root {root:?} probe: could not delete the probe file: {e}")
        })
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
/// unregistered provider, or names a built-in that cannot run cells.
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
    let targets = crate::machine_targets::load_machine_targets()?;
    let resolved_target = crate::machine_targets::find_by_machine(&targets, machine);
    let target_runner = resolved_target.map(|target| TargetRunnerConfig {
        mode: match target.runner_mode {
            crate::machine_targets::RunnerMode::Installed => "installed",
            crate::machine_targets::RunnerMode::Upload => "upload",
        }
        .to_string(),
        command: target.runner_command.clone(),
        artifacts: target.runner_artifacts.clone(),
        remote_root: target.remote_root.clone(),
    });
    let agent_executables = resolved_target
        .map(|target| target.agent_executables.clone())
        .unwrap_or_default();
    Ok(Some(
        ProviderRunner::new(provider.program, provider.args, scheme, uri)
            .with_channel(supports_channel)
            .with_target_runner(target_runner)
            .with_agent_executables(agent_executables),
    ))
}

/// Reconcile persisted remote `exec` handles against a fresh daemon start
/// (RAL-201). Call once at `serve()` startup, immediately alongside
/// [`Store::recover_orphaned_squads`] — both rest on the same invariant
/// ("nothing is executing yet, so any `running` row/persisted handle is
/// orphaned"), so they belong together, not one without the other.
///
/// A cell that was mid-poll when the daemon died has no in-process record
/// surviving the crash — only the row `ProviderRunner::exec` wrote before
/// entering its poll loop. `recover_orphaned_squads` has already reset that
/// cell to `Pending`, so the scheduler will re-provision and re-`exec` it
/// as a brand new attempt; this function's job is to stop the *old* attempt
/// on the provider first, so a restart never leaves two copies of the same
/// work running remotely at once.
///
/// Best-effort throughout: a provider that cannot be reached, no longer
/// exists, or refuses the cancel is logged and the row is cleared anyway —
/// there is nothing else productive to do with a handle whose owning cell
/// no longer considers itself running, and leaving a stale row behind would
/// just make the next restart re-cancel the same (by then meaningless)
/// handle forever.
pub fn reconcile_remote_exec_handles(store: &Store) {
    let handles = match store.all_remote_exec_handles() {
        Ok(h) => h,
        Err(e) => {
            crate::rlog!(
                ERROR,
                "ralphus [recovery] could not read remote exec handles: {e}"
            );
            return;
        }
    };
    for h in &handles {
        let machine = format!("{}:{}", h.scheme, h.uri);
        let outcome = match provider_from_store(store, &machine) {
            Ok(Some(provider)) => {
                let spec = crate::runner::RunnerSpec::for_command_proof(
                    &h.squad_id,
                    &h.cell_id,
                    &machine,
                    ".",
                    "",
                    "claude",
                    Some(30),
                );
                provider.cancel_handle(&h.handle, &spec)
            }
            Ok(None) => Err(format!("machine {machine:?} unexpectedly resolved local")),
            Err(e) => Err(e),
        };
        if let Err(e) = &outcome {
            crate::rlog!(
                WARNING,
                "ralphus [recovery] could not cancel stale remote handle {} for squad={} \
                 cell={} on machine {machine:?}: {e}",
                h.handle,
                h.squad_id,
                h.cell_id
            );
        }
        crate::cartographer::Note::new("recovery")
            .level(if outcome.is_ok() {
                crate::logging::LogLevel::WARNING
            } else {
                crate::logging::LogLevel::ERROR
            })
            .scope("cell")
            .squad(&h.squad_id)
            .cell(&h.cell_id)
            .emit(
                store,
                "stale remote exec handle reconciled on startup",
                serde_json::json!({
                    "machine": machine,
                    "handle": h.handle,
                    "cancelled_ok": outcome.is_ok(),
                    "error": outcome.err(),
                }),
            );
        let _ = store.clear_remote_exec_handle(&h.squad_id, &h.cell_id);
    }
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

    /// Provision a workspace for a remote cell, returning its path on that
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

    fn preflight_agent(
        &self,
        agent: &str,
        executable: Option<&str>,
        machine: Option<&str>,
    ) -> Result<(), String> {
        let Some(machine) = machine.map(str::trim).filter(|machine| !machine.is_empty()) else {
            return self.local.preflight_agent(agent, executable, None);
        };
        match self.provider_for(machine)? {
            Some(_) => {
                // A provider executes on a different host, so its PATH and
                // custom agent mappings cannot be inspected from this daemon.
                Ok(())
            }
            None => self.local.preflight_agent(agent, executable, Some(machine)),
        }
    }

    fn preflight_runner_executable(&self, machine: Option<&str>) -> Result<(), String> {
        let Some(machine) = machine.map(str::trim).filter(|machine| !machine.is_empty()) else {
            return self.local.preflight_runner_executable(None);
        };
        match self.provider_for(machine)? {
            // A provider executes on a different host, so its PATH cannot be
            // inspected from this daemon -- same rationale as
            // `preflight_agent` above.
            Some(_) => Ok(()),
            None => self.local.preflight_runner_executable(Some(machine)),
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
            self.seen.lock().unwrap().push(spec.cell_id.clone());
            RunnerResult {
                status: "done".to_string(),
                tokens_in: 7,
                tokens_out: 8,
                cache_creation_tokens: 0,
                cache_read_tokens: 0,
                compaction_input_tokens: 0,
                compaction_count: 0,
                cost_usd: 0.25,
                cost_is_estimated: false,
                summary: "local".to_string(),
                error: None,
                proofed: None,
                agent_session_id: None,
                ghost: None,
            }
        }
    }

    fn spec(machine: Option<&str>) -> RunnerSpec {
        RunnerSpec {
            squad_id: "squad-1".to_string(),
            task: "t".to_string(),
            cell_id: "s0".to_string(),
            cwd: ".".to_string(),
            prompt: Some("do work".to_string()),
            command: None,
            agent: "claude".to_string(),
            executable: None,
            model: None,
            system_prompt: None,
            system_prompt_position: None,
            timeout_sec: None,
            budget_tokens: None,
            maximum_budget_usd: None,
            maximum_context: None,
            auto_compact_threshold: None,
            maximum_tool_output_tokens: None,
            proof: false,
            trace_context: None,
            resume_agent_session_id: None,
            assigned_agent_session_id: None,
            env_overrides: BTreeMap::new(),
            machine: machine.map(str::to_string),
            tool_arg_truncate_chars: None,
            thrash_max_compactions: None,
            thrash_min_turn_gap: None,
            allow_personal_settings: false,
            allow_personal_memory: false,
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
    fn an_unregistered_machine_fails_the_cell_without_running_it_locally() {
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

    /// A Python provider that fakes a tiny remote filesystem under `store_in`
    /// (a real local directory the test controls) so `read-file`/
    /// `write-file`/`remove-path` behave like a genuine round-trip across
    /// process spawns, unlike [`fake_provider`]'s single canned reply. When
    /// `writes_fail` is set, every `write-file` call reports `ok: false`
    /// instead of succeeding, to test the probe's failure path.
    fn fake_filesystem_provider(
        store_in: &std::path::Path,
        writes_fail: bool,
    ) -> std::path::PathBuf {
        std::fs::create_dir_all(store_in).expect("mkdir");
        let py = store_in.join("provider.py");
        let fail_writes = if writes_fail { "True" } else { "False" };
        std::fs::write(
            &py,
            format!(
                r#"import json, os, sys
verb = sys.argv[1]
base = {store_in:?}
fail_writes = {fail_writes}
def path_for(p):
    return os.path.join(base, p.lstrip("/").replace("/", "_"))
if verb == "write-file":
    if fail_writes:
        print(json.dumps({{"ok": False, "protocol_version": 1, "error": "disk full"}}))
    else:
        req = json.load(sys.stdin)
        with open(path_for(req["path"]), "w") as f:
            f.write(req["content"])
        print(json.dumps({{"ok": True, "protocol_version": 1}}))
elif verb == "read-file":
    req = json.load(sys.stdin)
    fp = path_for(req["path"])
    if os.path.exists(fp):
        print(json.dumps({{"ok": True, "protocol_version": 1, "stdout": open(fp).read()}}))
    else:
        print(json.dumps({{"ok": False, "protocol_version": 1, "error": "not found"}}))
elif verb == "remove-path":
    req = json.load(sys.stdin)
    fp = path_for(req["path"])
    if os.path.exists(fp):
        os.remove(fp)
    print(json.dumps({{"ok": True, "protocol_version": 1}}))
else:
    print(json.dumps({{"ok": False, "protocol_version": 1, "error": "unexpected verb " + verb}}))
"#,
                store_in = store_in.to_string_lossy(),
            ),
        )
        .unwrap();
        py
    }

    #[test]
    fn probe_remote_root_round_trips_create_read_rename_delete() {
        let dir = std::env::temp_dir().join(format!("ral355-probe-ok-{}", std::process::id()));
        let py = fake_filesystem_provider(&dir, false);
        let provider = ProviderRunner::new(
            "python",
            vec![py.to_string_lossy().into_owned()],
            "probetest",
            "A",
        );
        provider
            .probe_remote_root("/srv/ralphus", &spec(Some("probetest:A")))
            .expect("a fully functional remote filesystem must pass the probe");
        // Nothing left behind: the fake filesystem directory holds only the
        // provider script itself once the probe's own cleanup has run.
        let leftover: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| name != "provider.py")
            .collect();
        assert!(leftover.is_empty(), "probe left files behind: {leftover:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn probe_remote_root_reports_which_step_failed() {
        let dir = std::env::temp_dir().join(format!("ral355-probe-fail-{}", std::process::id()));
        let py = fake_filesystem_provider(&dir, true);
        let provider = ProviderRunner::new(
            "python",
            vec![py.to_string_lossy().into_owned()],
            "probefail",
            "A",
        );
        let err = provider
            .probe_remote_root("/srv/ralphus", &spec(Some("probefail:A")))
            .expect_err("an unwritable remote_root must fail the probe");
        assert!(err.contains("could not create a file there"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_real_provider_script_executes_a_cell_and_its_result_is_returned() {
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
            "the cell must have run remotely, not locally"
        );
    }

    #[test]
    fn remote_exec_forwards_the_full_runner_result_shape_unchanged() {
        // RAL-355 Phase 7 result/accounting parity: `ProviderRunner::exec`
        // performs no field-level transformation on `result` at all --
        // `resp.result` (already a parsed `RunnerResult`) is returned
        // verbatim (see `exec`'s `if let Some(result) = resp.result { return
        // result; }`). So the strongest true parity claim is exactly what
        // this test proves: feeding the identical CellResult content through
        // the wire contract's `result` object round-trips every field
        // (summary, tokens, cost, error, proof verdict, session id, ghost)
        // byte-for-byte, the same way any local caller parsing that same
        // JSON directly would see it -- there is no separate "local
        // formatting" step in this codebase for a remote path to diverge
        // from.
        let full_result = serde_json::json!({
            "status": "failed",
            "tokens_in": 42,
            "tokens_out": 84,
            "cost_usd": 1.23,
            "summary": "did the thing, then it broke",
            "error": "agent exited non-zero",
            "proofed": false,
            "agent_session_id": "sess-abc-123",
            "ghost": "handoff: retry with a smaller diff"
        });
        let expected: RunnerResult = serde_json::from_value(full_result.clone()).unwrap();

        let script = fake_provider(
            "parity",
            &format!(r#"{{"ok":true,"protocol_version":1,"result":{full_result}}}"#),
            &[],
        );
        let provider =
            ProviderRunner::new(script.to_string_lossy().into_owned(), vec![], "ct", "A");
        let remote = provider.exec(&spec(Some("ct:A")), None);

        assert_eq!(remote.status, expected.status);
        assert_eq!(remote.tokens_in, expected.tokens_in);
        assert_eq!(remote.tokens_out, expected.tokens_out);
        assert!((remote.cost_usd - expected.cost_usd).abs() < f64::EPSILON);
        assert_eq!(remote.summary, expected.summary);
        assert_eq!(remote.error, expected.error);
        assert_eq!(remote.proofed, expected.proofed);
        assert_eq!(remote.agent_session_id, expected.agent_session_id);
        assert_eq!(remote.ghost, expected.ghost);
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
        // Without this a remote cell is invisible in Cartographer AND its
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
                squad_id: Some("squad-1".to_string()),
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
    fn execution_environment_is_payload_not_local_provider_environment() {
        let dir = std::env::temp_dir().join(format!("ral185-provider-env-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let py = dir.join("provider.py");
        std::fs::write(
            &py,
            [
                "import json, os, sys",
                "payload = json.load(sys.stdin)",
                "remote = payload.get('execution_environment', {}).get('REMOTE_ONLY')",
                "local = os.environ.get('REMOTE_ONLY')",
                "ok = remote == 'payload-secret' and local is None",
                "result = {'status': 'done' if ok else 'failed', 'summary': 'separated' if ok else 'environment leaked into provider'}",
                "print(json.dumps({'ok': True, 'protocol_version': 1, 'result': result}))",
            ]
            .join("\n"),
        )
        .unwrap();
        let provider =
            ProviderRunner::new("python", vec![py.to_string_lossy().into_owned()], "ct", "A");
        let mut remote = spec(Some("ct:A"));
        remote
            .env_overrides
            .insert("REMOTE_ONLY".to_string(), "payload-secret".to_string());
        let result = provider.run(&remote);
        assert_eq!(result.status, "done", "{result:?}");
        assert_eq!(result.summary, "separated");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn provider_results_and_stream_output_redact_registered_secrets() {
        crate::redact::with_registry_lock(|| {
            crate::redact::clear_for_tests();
            crate::redact::register("phase7-secret-value");
            let mut response: ProviderResponse = serde_json::from_value(serde_json::json!({
                "ok": true,
                "output": "stream phase7-secret-value",
                "detail": "detail phase7-secret-value",
                "stdout": "stdout phase7-secret-value",
                "result": {
                    "status": "failed",
                    "summary": "summary phase7-secret-value",
                    "error": "error phase7-secret-value",
                    "ghost": "ghost phase7-secret-value"
                }
            }))
            .unwrap();
            redact_provider_response(&mut response);
            let rendered = format!("{response:?}");
            assert!(!rendered.contains("phase7-secret-value"), "{rendered}");
            assert!(rendered.contains(crate::redact::REDACTED), "{rendered}");
            crate::redact::clear_for_tests();
        });
    }

    #[test]
    fn an_async_starting_state_is_polled_instead_of_treated_as_terminal() {
        let dir =
            std::env::temp_dir().join(format!("ral185-async-starting-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let py = dir.join("provider.py");
        std::fs::write(
            &py,
            [
                "import json, pathlib, sys",
                "verb = sys.argv[1]",
                "counter = pathlib.Path(__file__).with_suffix('.count')",
                "if verb == 'exec':",
                "    print(json.dumps({'ok': True, 'protocol_version': 1, 'handle': 'h1'}))",
                "elif verb == 'stream':",
                "    print(json.dumps({'ok': True, 'protocol_version': 1, 'output': '', 'next': 0}))",
                "elif verb == 'status':",
                "    seen = counter.exists()",
                "    counter.write_text('1')",
                "    if seen:",
                "        print(json.dumps({'ok': True, 'protocol_version': 1, 'state': 'done', 'result': {'status': 'done', 'summary': 'started safely'}}))",
                "    else:",
                "        print(json.dumps({'ok': True, 'protocol_version': 1, 'state': 'starting'}))",
            ]
            .join("\n"),
        )
        .unwrap();
        let provider =
            ProviderRunner::new("python", vec![py.to_string_lossy().into_owned()], "ct", "A");
        let result = provider.run(&spec(Some("ct:A")));
        assert_eq!(result.status, "done", "{result:?}");
        assert_eq!(result.summary, "started safely");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn an_async_handle_is_cleared_from_the_store_once_the_cell_finishes() {
        // RAL-201: the persisted handle exists so a daemon restart mid-poll
        // can reconcile it -- once the cell actually finishes normally
        // there is nothing left to reconcile, so the row must not linger.
        let script = fake_async_provider(
            "async-clear",
            &[
                ("exec", r#"{"ok":true,"protocol_version":1,"handle":"h1"}"#),
                (
                    "status",
                    r#"{"ok":true,"protocol_version":1,"state":"done","result":{"status":"done","summary":"ok"}}"#,
                ),
            ],
        );
        let s = store();
        register(&s, "ib", &script);
        let (router, _local) = router(Arc::clone(&s));
        let r = router.run(&spec(Some("ib:A")));
        assert_eq!(r.status, "done", "{r:?}");
        assert!(
            s.lock()
                .unwrap()
                .all_remote_exec_handles()
                .unwrap()
                .is_empty(),
            "the handle row must be cleared once the cell finishes"
        );
    }

    #[test]
    fn reconcile_cancels_a_stale_handle_and_clears_its_row() {
        // RAL-201: simulates a daemon restart mid-poll -- a handle row
        // survives in the DB (no in-process poll loop does, since that died
        // with the old process) and `reconcile_remote_exec_handles` must
        // stop the old attempt on the provider before the scheduler starts a
        // fresh one, so a restart never leaves two copies of the same work
        // running remotely at once.
        let script = fake_async_provider(
            "async-reconcile",
            &[("cancel", r#"{"ok":true,"protocol_version":1}"#)],
        );
        let s = store();
        register(&s, "ib", &script);
        s.lock()
            .unwrap()
            .save_remote_exec_handle("squad-1", "s0", "ib", "A", "stale-handle")
            .unwrap();

        reconcile_remote_exec_handles(&s.lock().unwrap());

        assert!(
            s.lock()
                .unwrap()
                .all_remote_exec_handles()
                .unwrap()
                .is_empty(),
            "the stale row must be cleared after reconciliation"
        );
        let page = s
            .lock()
            .unwrap()
            .cartographer_query(&crate::cartographer::CartographerFilter {
                squad_id: Some("squad-1".to_string()),
                ..crate::cartographer::CartographerFilter::recent(50)
            })
            .unwrap();
        assert!(
            page.rows
                .iter()
                .any(|row| row.message.contains("reconciled")),
            "reconciliation must leave a Cartographer record; got {:?}",
            page.rows.iter().map(|r| &r.message).collect::<Vec<_>>()
        );
    }

    #[test]
    fn reconcile_of_an_unregistered_provider_still_clears_the_row() {
        // The provider that owned this handle may itself have been
        // deregistered between the crash and the restart -- reconciliation
        // must not leave the row stuck forever in that case.
        let s = store();
        s.lock()
            .unwrap()
            .save_remote_exec_handle("squad-1", "s0", "ghostscheme", "A", "h1")
            .unwrap();
        reconcile_remote_exec_handles(&s.lock().unwrap());
        assert!(
            s.lock()
                .unwrap()
                .all_remote_exec_handles()
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn a_streamed_remote_cell_is_readable_through_the_existing_live_view_snapshot() {
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
        // Use a cell id unique to this test: the pane snapshot is keyed by
        // `session_name(squad, task, cell)`, and every sibling test in this
        // module reuses `spec()`'s default `squad-1/t/s0` — writing to that
        // shared file concurrently would let a sibling's start-state snapshot
        // ("building...") clobber this test's streamed output before it reads
        // it back. A distinct cell id keeps this read/write pair private.
        let mut sp = spec(Some("ib:A"));
        sp.cell_id = "s0-farm-stream".to_string();
        let r = router.run(&sp);
        assert_eq!(r.status, "done", "{r:?}");
        let name = crate::tmux::session_name(&sp.squad_id, &sp.task, &sp.cell_id);
        let snapshot = crate::tmux::read_pane_snapshot(&name).unwrap_or_default();
        assert!(
            snapshot.contains("hello from the farm"),
            "streamed output must land in the pane snapshot the board reads; got {snapshot:?}"
        );
        let _ = std::fs::remove_file(crate::tmux::pane_snapshot_path(&name));
    }

    #[test]
    fn a_provider_that_does_not_implement_stream_still_completes_the_cell() {
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
        // Before the handle refactor a cancelled remote cell kept running on
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
    fn a_remote_cell_over_its_cost_cap_is_cancelled_mid_poll() {
        // RAL-201: mirrors `SubprocessRunner`'s live cost-cap kill (RAL-161).
        // Without tracking `llm-invoke` usage across `status`/`stream` polls,
        // a remote cell's `maximum_budget_usd` was silently unenforced
        // even though the usage itself still reached the DB via
        // `forward_runner_event` -- this asserts the in-flight kill itself,
        // not just that the usage was recorded.
        let dir = std::env::temp_dir().join(format!("ral185-rrcost-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let py = dir.join("cost.py");
        std::fs::write(
            &py,
            [
                "import sys, json",
                "verb = sys.argv[1]",
                "if verb == 'exec':",
                "    print(json.dumps({'ok': True, 'protocol_version': 1, 'handle': 'h1'}))",
                "elif verb == 'status':",
                "    event = json.dumps({'source': 'llm-invoke', 'message': 'usage', \
                 'payload': {'cost_usd': 5.0, 'tokens_in': 10, 'tokens_out': 20}})",
                "    print('RALPHUS_EVENT: ' + event, file=sys.stderr)",
                "    print(json.dumps({'ok': True, 'protocol_version': 1, 'state': 'running'}))",
                "elif verb == 'stream':",
                "    print(json.dumps({'ok': True, 'protocol_version': 1, 'output': '', 'next': 0}))",
                "elif verb == 'cancel':",
                "    print(json.dumps({'ok': True, 'protocol_version': 1}))",
            ]
            .join("\n"),
        )
        .unwrap();
        // `with_cartographer` is required here, not just cosmetic: the
        // stderr-reading thread's `RALPHUS_EVENT:` parsing is a no-op
        // without a store to persist to (see `forward_runner_event`), so
        // `live_usage` would never populate and this test would hang
        // forever waiting for a cap trip that can't happen -- exactly what
        // `MachineRouter::provider_for` always wires up in production.
        let provider =
            ProviderRunner::new("python", vec![py.to_string_lossy().into_owned()], "ct", "A")
                .with_cartographer(Arc::new(Mutex::new(Store::open_in_memory().unwrap())));
        let mut over_budget = spec(Some("ct:A"));
        over_budget.maximum_budget_usd = Some(1.0);
        let r = provider.run(&over_budget);
        assert_eq!(r.status, "failed", "{r:?}");
        assert!(
            r.error.as_deref().unwrap_or_default().contains("cost"),
            "{r:?}"
        );
        assert_eq!(r.tokens_in, 10);
        assert_eq!(r.tokens_out, 20);
        assert!((r.cost_usd - 5.0).abs() < f64::EPSILON, "{r:?}");
        let _ = std::fs::remove_dir_all(&dir);
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

    fn cleanup_req(project: &str) -> CleanupRequest {
        CleanupRequest {
            project: project.to_string(),
            clone_url: "git@host:org/repo.git".to_string(),
            branch: None,
            remote_root: None,
        }
    }

    #[test]
    fn a_target_specific_agent_override_always_wins() {
        let ok = fake_provider(
            "agentexec-ok",
            r#"{"ok":true,"protocol_version":1,"result":{"status":"done","tokens_in":1,"tokens_out":1,"cost_usd":0.0,"summary":"ok"}}"#,
            &[],
        );
        let provider = ProviderRunner::new(ok.to_string_lossy().into_owned(), vec![], "ct", "A")
            .with_agent_executables(BTreeMap::from([(
                "claude".to_string(),
                "claude-remote".to_string(),
            )]));
        let mut cell = spec(Some("ct:A"));
        // The daemon-local path would normally be refused (see below) --
        // proving the target override still wins even against a value that
        // would otherwise be rejected shows the override is checked first,
        // not merely as a fallback for the None case.
        cell.executable = Some("/usr/local/bin/claude".to_string());
        assert_eq!(
            provider.resolve_remote_executable(&cell).unwrap(),
            Some("claude-remote".to_string())
        );
    }

    #[test]
    fn a_bare_command_name_forwards_unchanged() {
        let provider = ProviderRunner::new("unused", vec![], "ct", "A");
        let mut cell = spec(Some("ct:A"));
        cell.executable = Some("claude".to_string());
        assert_eq!(
            provider.resolve_remote_executable(&cell).unwrap(),
            Some("claude".to_string())
        );
    }

    #[test]
    fn no_override_at_all_passes_through_as_none() {
        let provider = ProviderRunner::new("unused", vec![], "ct", "A");
        assert_eq!(
            provider
                .resolve_remote_executable(&spec(Some("ct:A")))
                .unwrap(),
            None
        );
    }

    #[test]
    fn a_daemon_local_path_shaped_override_is_refused_without_a_target_mapping() {
        let provider = ProviderRunner::new("unused", vec![], "ct", "A");
        let mut cell = spec(Some("ct:A"));
        cell.executable = Some("/usr/local/bin/claude".to_string());
        let err = provider
            .resolve_remote_executable(&cell)
            .expect_err("a path-shaped override with no target mapping must be refused");
        assert!(err.contains("daemon-local executable override"), "{err}");
        assert!(err.contains("[machine.targets.<name>.agents]"), "{err}");

        let mut windows_path = spec(Some("ct:A"));
        windows_path.executable = Some(r"C:\Users\me\claude.exe".to_string());
        assert!(provider.resolve_remote_executable(&windows_path).is_err());
    }

    #[test]
    fn a_refused_executable_override_never_reaches_the_provider() {
        // The provider program does not even exist -- if `exec` dispatched
        // anyway, the error would say "could not run machine provider", not
        // name the executable override. Proves the check happens strictly
        // before dispatch.
        let provider = ProviderRunner::new("/definitely/not/a/real/provider", vec![], "ct", "A");
        let mut cell = spec(Some("ct:A"));
        cell.executable = Some("/usr/local/bin/claude".to_string());
        let result = provider.exec(&cell, None);
        assert_eq!(result.status, "failed");
        let err = result.error.unwrap_or_default();
        assert!(err.contains("daemon-local executable override"), "{err}");
        assert!(!err.contains("could not run machine provider"), "{err}");
    }

    /// Write a throwaway provider script that echoes its own argv, one
    /// argument per line, to stdout and exits 0 -- used to confirm
    /// [`ProviderRunner::spawn_terminal`] actually builds the command line
    /// it claims to (`terminal --uri ... --command ... --cols ... --lines
    /// ...`), since `terminal` has no JSON reply to assert against the way
    /// every other verb's tests do.
    fn argv_echoing_provider(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("ral355-argv-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let (path, body) = if cfg!(windows) {
            (
                dir.join("provider.cmd"),
                "@echo off\r\nfor %%a in (%*) do echo %%~a\r\n".to_string(),
            )
        } else {
            (
                dir.join("provider.sh"),
                "#!/bin/sh\nfor a in \"$@\"; do echo \"$a\"; done\n".to_string(),
            )
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
    fn spawn_terminal_builds_the_expected_argv_and_pipes_stdio() {
        let script = argv_echoing_provider("spawn-terminal");
        let provider =
            ProviderRunner::new(script.to_string_lossy().into_owned(), vec![], "ct", "A");
        let child = provider
            .spawn_terminal(
                "claude --resume sess-1 --dangerously-skip-permissions",
                100,
                40,
            )
            .expect("must spawn");
        let output = child.wait_with_output().expect("must run to completion");
        let argv = String::from_utf8_lossy(&output.stdout);
        assert!(argv.contains("terminal"), "{argv}");
        assert!(argv.contains("--uri"), "{argv}");
        assert!(argv.contains('A'), "{argv}");
        assert!(argv.contains("--command"), "{argv}");
        assert!(
            argv.contains("claude --resume sess-1 --dangerously-skip-permissions"),
            "{argv}"
        );
        assert!(argv.contains("--cols"), "{argv}");
        assert!(argv.contains("100"), "{argv}");
        assert!(argv.contains("--lines"), "{argv}");
        assert!(argv.contains("40"), "{argv}");
    }

    #[test]
    fn spawn_terminal_reports_a_clear_error_when_the_provider_program_does_not_exist() {
        let provider = ProviderRunner::new("/definitely/not/a/real/provider", vec![], "ct", "A");
        let err = provider
            .spawn_terminal("claude --resume sess-1", 80, 24)
            .expect_err("a nonexistent provider program must fail to spawn");
        assert!(err.contains("terminal session"), "{err}");
    }

    #[test]
    fn capabilities_reports_what_the_provider_returns() {
        let ok = fake_provider(
            "capabilities-ok",
            r#"{"ok":true,"protocol_version":1,"capabilities":{"os":"linux","arch":"x86_64","supported_ops":["run","exec"],"async_exec":true,"terminal":false,"runner_version":"ralphus-runner 0.1.0"}}"#,
            &[],
        );
        let provider = ProviderRunner::new(ok.to_string_lossy().into_owned(), vec![], "ct", "A");
        let caps = provider
            .capabilities(&spec(Some("ct:A")))
            .expect("must succeed")
            .expect("must report capabilities");
        assert_eq!(caps.os.as_deref(), Some("linux"));
        assert_eq!(caps.arch.as_deref(), Some("x86_64"));
        assert!(caps.async_exec);
        assert!(!caps.terminal);
        assert_eq!(caps.runner_version.as_deref(), Some("ralphus-runner 0.1.0"));
        assert_eq!(
            caps.supported_ops,
            vec!["run".to_string(), "exec".to_string()]
        );
    }

    #[test]
    fn capabilities_from_a_provider_that_does_not_implement_the_verb_is_none_not_an_error() {
        let unimplemented = fake_provider(
            "capabilities-unimplemented",
            r#"{"ok":false,"protocol_version":1,"error":"some-provider does not implement \"capabilities\"; see docs/machine-providers.md for its supported verbs"}"#,
            &[],
        );
        let provider = ProviderRunner::new(
            unimplemented.to_string_lossy().into_owned(),
            vec![],
            "ct",
            "A",
        );
        let caps = provider
            .capabilities(&spec(Some("ct:A")))
            .expect("an unimplemented verb must not be an error");
        assert!(caps.is_none());
    }

    #[test]
    fn capabilities_from_an_unreachable_machine_is_a_real_error() {
        let provider = ProviderRunner::new("/definitely/not/a/real/provider", vec![], "ct", "A");
        let err = provider
            .capabilities(&spec(Some("ct:A")))
            .expect_err("a genuinely unreachable provider must still be an error");
        assert!(err.contains("ct"), "{err}");
    }

    #[test]
    fn cleanup_dispatches_the_cleanup_verb_and_reports_provider_failure() {
        let ok = fake_provider(
            "cleanup-ok",
            r#"{"ok":true,"protocol_version":1,"removed":"/srv/ralphus/projects/proj-1234"}"#,
            &[],
        );
        let provider = ProviderRunner::new(ok.to_string_lossy().into_owned(), vec![], "ct", "A");
        let removed = provider
            .cleanup(&cleanup_req("proj"), &spec(Some("ct:A")))
            .expect("a provider replying ok:true must succeed");
        assert_eq!(removed.as_deref(), Some("/srv/ralphus/projects/proj-1234"));

        let fail = fake_provider(
            "cleanup-fail",
            r#"{"ok":false,"protocol_version":1,"error":"workspace busy"}"#,
            &[],
        );
        let failing = ProviderRunner::new(fail.to_string_lossy().into_owned(), vec![], "ct", "B");
        let err = failing
            .cleanup(&cleanup_req("proj"), &spec(Some("ct:B")))
            .expect_err("a provider replying ok:false must surface its reason");
        // RAL-201: retention-on-failure -- the caller must see the provider's
        // own reason verbatim, not a swallowed/generic failure, since nothing
        // on the daemon side tracks the workspace to retry against.
        assert!(err.contains("workspace busy"), "{err}");
    }

    #[test]
    fn dispatch_is_generic_across_two_independently_configured_providers() {
        // RAL-201: the core design requirement is genericity -- one TOML
        // shape, any provider, with zero daemon-side branching on which
        // scheme is in play. Register two schemes with distinct executables,
        // distinct response shapes (one synchronous, one an async
        // handle-based provider), and distinct behavior, then dispatch the
        // full exec/status/stream/cancel/cleanup verb set to both through the
        // exact same `MachineRouter`/`ProviderRunner` code path (provision is
        // covered separately by
        // `worktrees::tests::the_same_placeholder_on_two_machines_resolves_to_two_workspaces`)
        // and confirm each gets its own correct, independent answer.
        let sync_script = fake_provider(
            "generic-sync",
            r#"{"ok":true,"protocol_version":1,"result":{"status":"done","tokens_in":1,"tokens_out":2,"cost_usd":0.1,"summary":"from sync provider"}}"#,
            &[],
        );
        let async_script = fake_async_provider(
            "generic-async",
            &[
                (
                    "exec",
                    r#"{"ok":true,"protocol_version":1,"handle":"h-generic"}"#,
                ),
                (
                    "status",
                    r#"{"ok":true,"protocol_version":1,"state":"done","result":{"status":"done","tokens_in":5,"tokens_out":6,"cost_usd":0.2,"summary":"from async provider"}}"#,
                ),
                (
                    "stream",
                    r#"{"ok":true,"protocol_version":1,"output":"","next":0}"#,
                ),
                ("cancel", r#"{"ok":true,"protocol_version":1}"#),
                ("cleanup", r#"{"ok":true,"protocol_version":1}"#),
            ],
        );

        let s = store();
        register(&s, "syncscheme", &sync_script);
        register(&s, "asyncscheme", &async_script);
        let (router, local) = router(s);

        let sync_result = router.run(&spec(Some("syncscheme:X")));
        let async_result = router.run(&spec(Some("asyncscheme:Y")));

        assert_eq!(sync_result.summary, "from sync provider");
        assert_eq!(sync_result.tokens_in, 1);
        assert_eq!(async_result.summary, "from async provider");
        assert_eq!(async_result.tokens_in, 5);
        // Neither dispatch fell through to the local runner -- genericity
        // means the router resolved each purely from the registry, with no
        // code path anywhere that special-cases "syncscheme" or
        // "asyncscheme" by name.
        assert!(local.seen.lock().unwrap().is_empty());

        // cancel and cleanup are handle-scoped/no-payload verbs that `run()`
        // never exercises above -- dispatch both through the same
        // `MachineRouter::provider_for` resolution path `route()` uses
        // internally, so this is the real registry lookup, not a
        // hand-built `ProviderRunner`.
        let sync_provider = router
            .provider_for("syncscheme:X")
            .expect("syncscheme must resolve without error")
            .expect("syncscheme must resolve to a provider, not local");
        let async_provider = router
            .provider_for("asyncscheme:Y")
            .expect("asyncscheme must resolve without error")
            .expect("asyncscheme must resolve to a provider, not local");
        sync_provider
            .cancel_handle("irrelevant-handle", &spec(Some("syncscheme:X")))
            .expect("sync provider's cancel must dispatch generically");
        async_provider
            .cancel_handle("h-generic", &spec(Some("asyncscheme:Y")))
            .expect("async provider's cancel must dispatch generically");
        sync_provider
            .cleanup(&cleanup_req("proj"), &spec(Some("syncscheme:X")))
            .expect("sync provider's cleanup must dispatch generically");
        async_provider
            .cleanup(&cleanup_req("proj"), &spec(Some("asyncscheme:Y")))
            .expect("async provider's cleanup must dispatch generically");
    }
}
