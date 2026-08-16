//! Task-file schema: the serde types a submitted TOML batch deserializes into.
//!
//! Ported from the predecessor project (`old:src/tasks/schema.rs`) with one
//! deliberate fix: a session's `prompt` is now `Option<String>` and pairs with
//! `command`. In the old project `prompt` was a required `String` even though
//! the validator and UI allowed `command`-only sessions, so a valid
//! `command`-only session would pass validation and then fail to deserialize in
//! the runner (see `FINDINGS.local.md` §2.3). Making both optional and enforcing
//! "exactly one" in the validator keeps the type and the rules in agreement.

use std::collections::BTreeMap;

use serde::Deserialize;

/// Settings that apply to an entire submission (task file).
///
/// Declared via `[[default]]` blocks in TOML; only the first block is used.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct DefaultBlock {
    /// Other run IDs (or `run-id/task/session` paths) that must be Done before
    /// this submission is allowed to start.
    #[serde(default)]
    pub depends_on: Vec<String>,
}

/// A whole submitted task file.
#[derive(Debug, Clone, Deserialize)]
pub struct TaskFile {
    /// `[[default]]` blocks. Only the first is significant.
    #[serde(default, rename = "default")]
    pub defaults: Vec<DefaultBlock>,
    /// The tasks in this submission.
    #[serde(default)]
    pub task: Vec<TaskDef>,
    /// Top-level review (guardian) declarations. Sessions opt in by setting
    /// `review = "<id>"` to match a review's `id` field.
    #[serde(default)]
    pub review: Vec<ReviewDef>,
}

/// One task: a unit of work made of one or more agent sessions plus verify steps.
#[derive(Debug, Clone, Deserialize)]
pub struct TaskDef {
    /// Task name / identifier (unique within a submission).
    pub name: String,
    /// Namespace label for a plain-path task; purely cosmetic and left unset if
    /// the caller doesn't care to name it (the UI shows "--"). When any of this
    /// task's sessions uses a `<project>:worktree/<branch>` placeholder `cwd`
    /// (RAL-100), this field's meaning changes: it becomes required and must
    /// name a project registered via `ralphus project git` -- checked
    /// structurally here (`project` set whenever a placeholder is present, see
    /// [`crate::validate`]) and against the registry at submit time (the
    /// registry lives in the daemon's store, which `core` cannot see).
    #[serde(default)]
    pub project: Option<String>,
    /// Agent binary for all sessions (e.g. `"claude"`). Sessions inherit unless overridden.
    #[serde(default)]
    pub agent: Option<String>,
    /// Default model for all sessions. Sessions may override.
    #[serde(default)]
    pub model: Option<String>,
    /// The machine every session (and verify step) under this task runs on
    /// (RAL-185), written `scheme:uri` — e.g. `"incredibuild:A"`. `scheme`
    /// names a provider registered with the daemon; `uri` is opaque and
    /// handed to that provider verbatim. Unset means [`LOCAL_MACHINE`].
    ///
    /// Sessions and verify steps inherit this and may override it, exactly
    /// like `agent`/`model` — but note that the affinity rules reject a
    /// submission whose sessions within one task disagree, so an override is
    /// only meaningful for restating the same value.
    #[serde(default)]
    pub machine: Option<String>,
    /// Extra CLI args passed to the agent for every session; task-level first.
    #[serde(default)]
    pub args: Vec<String>,
    /// Task budget cap in total tokens (input + output). A session exceeding it
    /// is failed. Sessions/verifies inherit this unless they set their own.
    #[serde(default)]
    pub budget_tokens: Option<u64>,
    /// Task-level max spend cap in USD. A session exceeding it is killed
    /// mid-run and failed. Sessions inherit this unless they set their own
    /// `maximum_budget_usd`.
    #[serde(default)]
    pub maximum_budget_usd: Option<f64>,
    /// Retry count.
    #[serde(default)]
    pub max_retries: Option<u32>,
    /// Initial queue-priority hint (lower value = higher priority = runs sooner).
    /// Seeds the live queue rank at submit time; the Queue view/CLI own ordering
    /// thereafter, so this is only a starting position, not an authoritative cap.
    #[serde(default)]
    pub priority: Option<u32>,
    /// Default wall-clock timeout in minutes for the task's sessions and verify
    /// steps; each may override with its own `timeout_minutes`.
    #[serde(default)]
    pub timeout_minutes: Option<u32>,
    /// Other tasks/sessions this whole task waits on.
    #[serde(default)]
    pub depends_on: Vec<String>,
    /// Environment variables for every session (and session verify step)
    /// under this task's spawned subprocess (RAL-172). A session sets the
    /// same key to override it just for itself, mirroring `agent`/`model`
    /// inheritance. At submission time these seed the task's row in the
    /// daemon's existing hierarchical env-override store (`env_overrides`
    /// column; see `daemon/src/store.rs`'s `run < task < session` layering,
    /// RAL-150) rather than being a separate delivery mechanism -- from
    /// there on they're indistinguishable from an override set later via
    /// `POST /api/runs/{id}/tasks/{ti}/env`.
    #[serde(default)]
    pub environment: BTreeMap<String, String>,
    /// The agent sessions.
    #[serde(default)]
    pub session: Vec<SessionDef>,
    /// Verify steps that run after ALL sessions complete.
    #[serde(default)]
    pub verify: Vec<VerifyStep>,
}

/// One agent session within a task.
#[derive(Debug, Clone, Deserialize)]
pub struct SessionDef {
    /// Session ID (for dependency references). Must not contain `/`.
    #[serde(default)]
    pub id: Option<String>,
    /// Human-readable display label. Shown in the board wherever sessions are
    /// listed; falls back to `id` when unset. Has no structural meaning.
    #[serde(default)]
    pub name: Option<String>,
    /// Agent-bus role.
    #[serde(default)]
    pub role: Option<String>,
    /// Working directory the agent operates in (required at validation time).
    #[serde(default)]
    pub cwd: Option<String>,
    /// AI-driven prompt. Mutually exclusive with `command`; exactly one is required.
    #[serde(default)]
    pub prompt: Option<String>,
    /// Deterministic shell command. Mutually exclusive with `prompt`.
    #[serde(default)]
    pub command: Option<String>,
    /// Sessions that must complete before this one starts. Within-task session
    /// ID (`"session-a"`) or cross-task `"<task-name>/<session-id>"`.
    #[serde(default)]
    pub depends_on: Vec<String>,
    /// Override the task-level agent for this session.
    #[serde(default)]
    pub agent: Option<String>,
    /// Override the task-level model for this session.
    #[serde(default)]
    pub model: Option<String>,
    /// Override the task-level `machine` for this session (RAL-185). See
    /// [`TaskDef::machine`] — the affinity rules require every session within
    /// one task to resolve to the same machine, so this may only restate the
    /// task's value, never diverge from it.
    #[serde(default)]
    pub machine: Option<String>,
    /// Subdirectories of a monorepo this session is scoped to (e.g.
    /// `["packages/foo", "packages/bar"]`). At run time the daemon injects a
    /// system-prompt addendum instructing the agent to confine its edits to
    /// those paths. The `cwd` itself always points to the repo root (RAL-23).
    #[serde(default)]
    pub subprojects: Vec<String>,
    /// System-prompt text delivered to the agent as an *appended* system prompt
    /// (via the backend's own mechanism, e.g. the Claude Code CLI's
    /// `--append-system-prompt`) rather than concatenated into the user prompt.
    /// Session-level only; validation restricts it to the `claude-code` backend
    /// until the other backends' support is complete (RAL-5).
    #[serde(default)]
    pub system_prompt: Option<String>,
    /// Where [`system_prompt`](Self::system_prompt) is placed. The only accepted
    /// value today is [`SYSTEM_PROMPT_POSITION_APPEND`] (`"append"`); validation
    /// rejects any other value.
    #[serde(default)]
    pub system_prompt_position: Option<String>,
    /// Extra args appended after any task-level args for this session only.
    #[serde(default)]
    pub args: Vec<String>,
    /// Per-session budget in total tokens (input + output). Falls back to the
    /// task-level `budget_tokens` when unset. Exceeding it fails the session.
    #[serde(default)]
    pub budget_tokens: Option<u64>,
    /// Per-session max spend cap in USD. Falls back to the task-level
    /// `maximum_budget_usd` when unset. Once the session's live running cost
    /// exceeds this, the daemon kills it mid-run and fails it.
    #[serde(default)]
    pub maximum_budget_usd: Option<f64>,
    /// Per-session wall-clock timeout in minutes. Falls back to the task-level
    /// `timeout_minutes` when unset.
    #[serde(default)]
    pub timeout_minutes: Option<u32>,
    /// Initial queue-priority hint for this session (lower value = higher
    /// priority = runs sooner). Seeds the live queue rank at submit time; the
    /// Queue view/CLI own ordering thereafter.
    #[serde(default)]
    pub priority: Option<u32>,
    /// Session-level verify steps.
    #[serde(default)]
    pub verify: Vec<VerifyStep>,
    /// Review (guardian) opt-in. Set to the `id` of a top-level `[[review]]`
    /// block to include this session's worktree branch in that review.
    #[serde(default)]
    pub review: Option<String>,
    /// Environment variables for this session's spawned subprocess
    /// (RAL-172). Merges with (and wins over on a shared key) the owning
    /// task's `environment`; see [`TaskDef::environment`] for how these
    /// values reach the runner.
    #[serde(default)]
    pub environment: BTreeMap<String, String>,
    /// Upstream branch source for this session's branch. When set to
    /// `"<<task:task-name>>"` (or `"<<task:task-name/session-id>>"`), the daemon
    /// rebases this session's branch onto the named dependency's current branch
    /// tip immediately before starting the runner. Both worktrees must be in the
    /// same git repository; cross-repo upstreams are skipped with a warning.
    #[serde(default)]
    pub upstream: Option<String>,
}

/// Sentinel prefix for `upstream = "<<task:task-name>>"` or
/// `"<<task:task-name/session-id>>"`: rebase this session's branch onto the
/// named dependency's current branch tip before the session starts. The daemon
/// resolves this at run time, immediately before launching the runner.
pub const UPSTREAM_TASK_REF_PREFIX: &str = "<<task:";

/// Parse the task/session ref from an `upstream = "<<task:...>>"` sentinel.
/// Returns the inner ref string (e.g. `"task-a"` or `"task-a/session-1"`)
/// when the sentinel matches, `None` otherwise (a plain branch-name string,
/// or any other non-matching value).
#[must_use]
pub fn parse_upstream_task_ref(upstream: &str) -> Option<&str> {
    upstream
        .strip_prefix(UPSTREAM_TASK_REF_PREFIX)
        .and_then(|s| s.strip_suffix(">>"))
}

/// Scheme prefix for a placeholder session `cwd` that names a branch to
/// materialize a fresh (or reused) worktree for, e.g.
/// `"ralphus:new-worktree/RAL-100-feature"` (RAL-100). Rather than a real
/// filesystem path, this names a branch; the owning task's `project` field
/// (REQUIRED whenever any of its sessions uses this placeholder) names which
/// project registered via `ralphus project git` to materialize it under. The
/// daemon resolves that project, materializes (or reuses) a git worktree for
/// the branch under `.git/.ralphus_worktrees/<branch>`, and rewrites the
/// session's `cwd` to that real path before the session runs.
pub const WORKTREE_PLACEHOLDER_PREFIX: &str = "ralphus:new-worktree/";

/// Parse a session `cwd` as a `ralphus:new-worktree/<branch>` placeholder
/// (RAL-100). Returns the branch name when non-empty; `None` for a plain
/// filesystem path or a malformed placeholder-shaped string (empty branch). A
/// real absolute path never starts with the literal `ralphus:new-worktree/`
/// prefix, so this is unambiguous. The project to materialize the branch
/// under is NOT embedded here -- it comes from the owning task's `project`
/// field (see [`crate::validate`]).
#[must_use]
pub fn parse_worktree_placeholder(cwd: &str) -> Option<&str> {
    let branch = cwd.strip_prefix(WORKTREE_PLACEHOLDER_PREFIX)?;
    if branch.is_empty() {
        return None;
    }
    Some(branch)
}

/// The scheme prefix for a new-review placeholder id. A review whose `id` is
/// `ralphus:new-review/<key>` is a *submission-local placeholder* for a review id
/// that does not exist yet: within a single submission, every session that names
/// the same `<key>` collapses into one freshly-minted guardian, and distinct
/// `<key>`s mint distinct guardians. The placeholder ALWAYS creates a new review —
/// a later submission that happens to reuse the same `<key>` string gets its own
/// brand-new guardian and never attaches to a previous submission's review. The
/// `<key>` is only a grouping alias, not a stable cross-submission link.
pub const REVIEW_LINK_PREFIX: &str = "ralphus:new-review/";

/// If `id` is a new-review placeholder (`ralphus:new-review/<key>`), return its
/// `<key>` trimmed of surrounding whitespace. Returns `None` for a plain id or a
/// non-matching scheme, or when the key is empty.
#[must_use]
pub fn review_link_key(id: &str) -> Option<&str> {
    id.strip_prefix(REVIEW_LINK_PREFIX)
        .map(str::trim)
        .filter(|k| !k.is_empty())
}

/// The reserved `machine` value naming the daemon's own host. Also the
/// implicit default when `machine` is unset anywhere in the inheritance chain,
/// so an existing task file that never mentions `machine` keeps running
/// exactly where it always did.
pub const LOCAL_MACHINE: &str = "local";

/// The minimum length of a `machine` URI scheme (RAL-185).
///
/// A one-character scheme is rejected purely to keep a Windows drive-letter
/// path (`C:\build\wt`) from silently parsing as scheme `C` with opaque
/// `\build\wt`. `machine` is not a path field, so nothing legitimate is lost,
/// and the error a user gets for pasting a path is far clearer than a
/// mysterious "provider C is not registered" at submit time.
pub const MIN_MACHINE_SCHEME_LEN: usize = 2;

/// A parsed `machine` value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MachineRef<'a> {
    /// The daemon's own host — `machine` unset, or the literal `"local"`.
    Local,
    /// A provider-resolved machine, written `scheme:uri`. The daemon never
    /// interprets `uri`; it is handed to the registered provider verbatim.
    Provider {
        /// Registered provider name, e.g. `"incredibuild"`.
        scheme: &'a str,
        /// Opaque, provider-defined remainder, e.g. `"A"` or
        /// `"https://useful.com/some/website"`.
        uri: &'a str,
    },
}

/// Why a `machine` value could not be parsed. Rendered by
/// [`crate::validate`] into a user-facing message with a line number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MachineParseError {
    /// The value was empty or all whitespace.
    Empty,
    /// No `:` separator, so there is no scheme (e.g. `"incredibuild"`).
    MissingScheme,
    /// The part before `:` was empty (e.g. `":A"`).
    EmptyScheme,
    /// The part after `:` was empty (e.g. `"incredibuild:"`).
    EmptyUri,
    /// The scheme was shorter than [`MIN_MACHINE_SCHEME_LEN`] — most often a
    /// Windows drive letter pasted in by mistake.
    SchemeTooShort,
    /// The scheme contained a character outside `[A-Za-z0-9_-]`.
    InvalidSchemeChar(char),
}

/// Parse a `machine` value into a [`MachineRef`].
///
/// Accepts the literal `"local"` (case-insensitive) or `scheme:uri`. The
/// `uri` half is deliberately unvalidated beyond "non-empty" — it is opaque
/// provider input and may be a bare token (`"A"`), a URL, a hostname, or
/// anything else the provider defines.
///
/// This is a *syntactic* check only. Whether `scheme` names a registered
/// provider cannot be answered here: the registry lives in the daemon's
/// store, which `core` deliberately cannot see (same split as `project`).
///
/// # Errors
/// Returns a [`MachineParseError`] describing the first problem found.
pub fn parse_machine(machine: &str) -> Result<MachineRef<'_>, MachineParseError> {
    let raw = machine.trim();
    if raw.is_empty() {
        return Err(MachineParseError::Empty);
    }
    if raw.eq_ignore_ascii_case(LOCAL_MACHINE) {
        return Ok(MachineRef::Local);
    }
    let Some((scheme, uri)) = raw.split_once(':') else {
        return Err(MachineParseError::MissingScheme);
    };
    if scheme.is_empty() {
        return Err(MachineParseError::EmptyScheme);
    }
    if let Some(bad) = scheme
        .chars()
        .find(|c| !c.is_ascii_alphanumeric() && *c != '_' && *c != '-')
    {
        return Err(MachineParseError::InvalidSchemeChar(bad));
    }
    if scheme.len() < MIN_MACHINE_SCHEME_LEN {
        return Err(MachineParseError::SchemeTooShort);
    }
    if uri.trim().is_empty() {
        return Err(MachineParseError::EmptyUri);
    }
    Ok(MachineRef::Provider { scheme, uri })
}

/// The effective `machine` for a session: its own value, else the owning
/// task's, else `None` (meaning [`LOCAL_MACHINE`]). Mirrors how `agent` and
/// `model` inherit in [`ResolvedAgent::resolve`].
#[must_use]
pub fn resolve_session_machine(task: &TaskDef, session: &SessionDef) -> Option<String> {
    session
        .machine
        .clone()
        .or_else(|| task.machine.clone())
        .map(|m| m.trim().to_string())
        .filter(|m| !m.is_empty())
}

/// The effective `machine` for a task-scope verify step (one with no owning
/// session): the step's own value, else the task's.
#[must_use]
pub fn resolve_task_verify_machine(task: &TaskDef, verify: &VerifyStep) -> Option<String> {
    verify
        .machine
        .clone()
        .or_else(|| task.machine.clone())
        .map(|m| m.trim().to_string())
        .filter(|m| !m.is_empty())
}

/// The effective `machine` for a session-scope verify step: the step's own
/// value, else the owning session's resolved machine (which itself falls back
/// to the task's).
#[must_use]
pub fn resolve_session_verify_machine(
    task: &TaskDef,
    session: &SessionDef,
    verify: &VerifyStep,
) -> Option<String> {
    verify
        .machine
        .clone()
        .map(|m| m.trim().to_string())
        .filter(|m| !m.is_empty())
        .or_else(|| resolve_session_machine(task, session))
}

/// A top-level review (guardian) declaration via `[[review]]`.
///
/// Sessions opt in by declaring `review = "<id>"`. When several sessions resolve
/// to the same project they collapse into one guardian; across N projects the
/// daemon materialises N guardians and disambiguates their names. The base branch
/// is always resolved from each worktree's tracking upstream at submit time.
#[derive(Debug, Clone, Deserialize)]
pub struct ReviewDef {
    /// Human-readable review id (also the default label). May be a
    /// `ralphus:new-review/<key>` placeholder that mints a fresh guardian for this
    /// submission (see [`REVIEW_LINK_PREFIX`]).
    #[serde(default)]
    pub id: Option<String>,
    /// GUI label; falls back to `id` when unset.
    #[serde(default)]
    pub name: Option<String>,
    /// Backend that resolves merge conflicts (and applies reviewer feedback) for
    /// this review, e.g. `"claude"` or `"ollama"`. Unset falls back to the
    /// `RALPHUS_RESOLVER_AGENT` env override, then `ollama`.
    #[serde(default)]
    pub agent: Option<String>,
    /// Model the resolver `agent` runs, e.g. `"qwen3:8b"`. Unset falls back to
    /// the `RALPHUS_RESOLVER_MODEL` env override, then `qwen3:8b` for the ollama
    /// backend (other backends take their own default).
    #[serde(default)]
    pub model: Option<String>,
    /// The branch this review's stack rebases onto, declared rather than
    /// discovered (RAL-185).
    ///
    /// A local review normally infers its base from each contributing
    /// worktree's git upstream. That read only works on the machine holding
    /// the worktree, so a review fed by a **remote** session must state its
    /// base here — the daemon has no way to look it up across machines, and
    /// guessing the project's default branch would silently produce a review
    /// against the wrong base.
    ///
    /// Optional for an all-local review, where inference still applies and
    /// this simply overrides it.
    #[serde(default)]
    pub base: Option<String>,
    /// The machine this review's worktrees and merge live on (RAL-185),
    /// written `scheme:uri`. Independent of any task's machine — a review may
    /// run somewhere none of its contributing tasks did. Unset means
    /// [`LOCAL_MACHINE`].
    ///
    /// Every worktree feeding one review must be on this machine: the stacked
    /// linear rebase needs all of the branches in one repo on one filesystem.
    #[serde(default)]
    pub machine: Option<String>,
    /// User-declared test actions shown as labelled buttons in the board UI.
    #[serde(default)]
    pub action: Vec<ReviewActionDef>,
}

/// A user-declared manual-test action shown as a labelled button in the review UI.
///
/// Exactly one of `prompt` or `command` must be set. `command` is run directly
/// in a terminal; `prompt` is forwarded to the LLM to expand into a runnable
/// command before it is offered to the reviewer.
#[derive(Debug, Clone, Deserialize)]
pub struct ReviewActionDef {
    /// Button label shown in the UI.
    pub label: String,
    /// Human-readable test description forwarded to the LLM for command expansion.
    /// Mutually exclusive with `command`.
    #[serde(default)]
    pub prompt: Option<String>,
    /// Verbatim shell command run directly without LLM involvement.
    /// Mutually exclusive with `prompt`.
    #[serde(default)]
    pub command: Option<String>,
    /// Optional command run before `command`/the expanded `prompt`, e.g. to stop
    /// a stale process from a previous run. Opt-in at run time via a UI
    /// checkbox (RAL-164) -- coexists with either `prompt` or `command`, no
    /// XOR involved.
    #[serde(default)]
    pub cleanup_command: Option<String>,
    /// Named, defaulted values referenced in `command`/`cleanup_command` as
    /// `{name}` placeholders (RAL-164), e.g. a port number that would
    /// otherwise be hardcoded and collide across concurrent reviews.
    #[serde(default)]
    pub input: Vec<ReviewActionInputDef>,
}

/// A named, defaulted input referenced by a [`ReviewActionDef`]'s
/// `command`/`cleanup_command` as a `{name}` placeholder (RAL-164).
#[derive(Debug, Clone, Deserialize)]
pub struct ReviewActionInputDef {
    /// Placeholder key, e.g. `"port"` for a `{port}` placeholder.
    pub name: String,
    /// Shown to the user next to the input field, e.g. "Port for the daemon".
    pub message: String,
    /// Pre-filled default value, offered until the user (or the resolver
    /// agent, via "set it for me") submits a different one.
    #[serde(default)]
    pub default: String,
}

/// One verify step. Exactly one of `command` / `brain` / `prompt` must be set.
#[derive(Debug, Clone, Deserialize)]
pub struct VerifyStep {
    /// Verify step ID (for `restart_on` / verify-level dependencies).
    #[serde(default)]
    pub id: Option<String>,
    /// Shell command; exit code is the verdict.
    #[serde(default)]
    pub command: Option<String>,
    /// Prompt routed to the local brain (deferred in ralphus MVP).
    #[serde(default)]
    pub brain: Option<String>,
    /// Headless AI verifier prompt. Runs using the owning session's
    /// resolved backend program (its `agent`), with this step's own `model`
    /// as an override — a verify step has no separate backend selector.
    #[serde(default)]
    pub prompt: Option<String>,
    /// Model override for the `prompt` verify (falls back to the owning
    /// session's resolved model when unset).
    #[serde(default)]
    pub model: Option<String>,
    /// Override the inherited `machine` for this verify step (RAL-185). Falls
    /// back to the owning session's resolved machine for a session-scope step,
    /// or the task's for a task-scope one. See [`TaskDef::machine`] — the
    /// affinity rules require every step under one task to agree.
    #[serde(default)]
    pub machine: Option<String>,
    /// Extra CLI args for the prompt-verifier invocation (e.g. `--append-system-prompt`).
    #[serde(default)]
    pub arguments: Vec<String>,
    /// Budget for the verify prompt, in total tokens (input + output). Falls
    /// back to the task-level `budget_tokens` when unset.
    #[serde(default)]
    pub budget_tokens: Option<u64>,
    /// Per-verify wall-clock timeout in minutes. Falls back to the task-level
    /// `timeout_minutes` when unset.
    #[serde(default)]
    pub timeout_minutes: Option<u32>,
    /// Whether the step needs human approval.
    #[serde(default)]
    pub requires_approval: bool,
    /// Other verify steps that, when they fire, re-run this session's verify
    /// cursor from the start. Grammar: `task/session/verify?on=pass|fail|both`,
    /// with wildcards `task/*` and `task/session/*`.
    #[serde(default)]
    pub restart_on: Vec<String>,
}

/// Resolved agent config after task→session inheritance is applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedAgent {
    /// Binary name or path to invoke.
    pub program: String,
    /// Effective model, if set.
    pub model: Option<String>,
    /// Extra args, task-level first then session-level.
    pub args: Vec<String>,
}

/// The default agent program when neither session nor task specifies one.
pub const DEFAULT_AGENT: &str = "claude";

/// The [`SessionDef::system_prompt_position`] value meaning "append to the
/// agent's system prompt". Currently the only accepted position (RAL-5).
pub const SYSTEM_PROMPT_POSITION_APPEND: &str = "append";

/// Whether `agent` is a backend with complete appended-system-prompt support.
///
/// The Claude Code CLI (`claude-code`, and its `claude-cli` alias) maps the
/// appended system prompt to a real backend flag (`--append-system-prompt`);
/// the Codex CLI (`codex`, and its `codex-cli` alias) maps it to its own
/// config-override mechanism (`-c developer_instructions=...` -- there is no
/// dedicated flag). Support for other backends is best-effort but not
/// complete, so validation rejects `system_prompt`/`system_prompt_position`
/// for any other agent (RAL-5).
#[must_use]
pub fn agent_supports_system_prompt(agent: &str) -> bool {
    matches!(agent, "claude-code" | "claude-cli" | "codex" | "codex-cli")
}

impl ResolvedAgent {
    /// Merge task defaults with session overrides (session wins).
    #[must_use]
    pub fn resolve(task: &TaskDef, session: &SessionDef) -> Self {
        let program = session
            .agent
            .clone()
            .or_else(|| task.agent.clone())
            .unwrap_or_else(|| DEFAULT_AGENT.to_string());
        let model = session.model.clone().or_else(|| task.model.clone());
        let mut args = task.args.clone();
        args.extend_from_slice(&session.args);
        Self {
            program,
            model,
            args,
        }
    }

    /// Task-level defaults only (for task-level verify steps that have no session).
    #[must_use]
    pub fn from_task(task: &TaskDef) -> Self {
        Self {
            program: task
                .agent
                .clone()
                .unwrap_or_else(|| DEFAULT_AGENT.to_string()),
            model: task.model.clone(),
            args: task.args.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task_with(agent: Option<&str>, model: Option<&str>, args: &[&str]) -> TaskDef {
        TaskDef {
            name: "t".into(),
            project: None,
            agent: agent.map(str::to_string),
            model: model.map(str::to_string),
            machine: None,
            args: args.iter().map(|s| (*s).to_string()).collect(),
            budget_tokens: None,
            maximum_budget_usd: None,
            max_retries: None,
            priority: None,
            timeout_minutes: None,
            depends_on: vec![],
            environment: BTreeMap::new(),
            session: vec![],
            verify: vec![],
        }
    }

    fn session_with(agent: Option<&str>, model: Option<&str>, args: &[&str]) -> SessionDef {
        SessionDef {
            id: None,
            name: None,
            role: None,
            cwd: Some("/tmp".into()),
            subprojects: vec![],
            prompt: Some("hi".into()),
            command: None,
            depends_on: vec![],
            agent: agent.map(str::to_string),
            model: model.map(str::to_string),
            machine: None,
            system_prompt: None,
            system_prompt_position: None,
            args: args.iter().map(|s| (*s).to_string()).collect(),
            budget_tokens: None,
            maximum_budget_usd: None,
            timeout_minutes: None,
            priority: None,
            environment: BTreeMap::new(),
            verify: vec![],
            review: None,
            upstream: None,
        }
    }

    #[test]
    fn session_overrides_task_agent_and_model() {
        let task = task_with(Some("claude"), Some("task-model"), &["--task"]);
        let sess = session_with(Some("aider"), Some("sess-model"), &["--sess"]);
        let r = ResolvedAgent::resolve(&task, &sess);
        assert_eq!(r.program, "aider");
        assert_eq!(r.model.as_deref(), Some("sess-model"));
        assert_eq!(r.args, vec!["--task", "--sess"]);
    }

    #[test]
    fn session_inherits_task_when_unset() {
        let task = task_with(Some("codex"), Some("m"), &["--a"]);
        let sess = session_with(None, None, &[]);
        let r = ResolvedAgent::resolve(&task, &sess);
        assert_eq!(r.program, "codex");
        assert_eq!(r.model.as_deref(), Some("m"));
        assert_eq!(r.args, vec!["--a"]);
    }

    #[test]
    fn defaults_to_claude_when_nothing_set() {
        let task = task_with(None, None, &[]);
        let sess = session_with(None, None, &[]);
        assert_eq!(ResolvedAgent::resolve(&task, &sess).program, DEFAULT_AGENT);
    }

    #[test]
    fn toplevel_review_deserializes() {
        let toml = r#"
            [[task]]
            name = "t"
            [[task.session]]
            cwd = "/repo/.wt/feat"
            prompt = "do work"
            review = "backend"

            [[review]]
            id = "backend"
            name = "Backend review"
            agent = "claude"
            model = "claude-opus-4-8"

            [[review.action]]
            label = "Smoke test"
            command = "cargo test"

            [[review.action]]
            label = "Frontend check"
            prompt = "Open localhost:3000 and click through the new wizard"
        "#;
        let parsed: TaskFile = toml::from_str(toml).expect("should deserialize");
        assert_eq!(parsed.task[0].session[0].review.as_deref(), Some("backend"));
        let review = &parsed.review;
        assert_eq!(review.len(), 1);
        assert_eq!(review[0].id.as_deref(), Some("backend"));
        assert_eq!(review[0].name.as_deref(), Some("Backend review"));
        assert_eq!(review[0].agent.as_deref(), Some("claude"));
        assert_eq!(review[0].model.as_deref(), Some("claude-opus-4-8"));
        assert_eq!(review[0].action.len(), 2);
        assert_eq!(review[0].action[0].label, "Smoke test");
        assert_eq!(review[0].action[0].command.as_deref(), Some("cargo test"));
        assert!(review[0].action[0].prompt.is_none());
        assert_eq!(review[0].action[1].label, "Frontend check");
        assert!(review[0].action[1].command.is_none());
        assert!(review[0].action[1].prompt.is_some());
    }

    #[test]
    fn review_action_cleanup_command_and_input_deserialize() {
        let toml = r#"
            [[task]]
            name = "t"
            [[task.session]]
            cwd = "/repo/.wt/feat"
            prompt = "do work"
            review = "backend"

            [[review]]
            id = "backend"

            [[review.action]]
            label = "Serve locally"
            command = "ralphus-daemon serve --port {port}"
            cleanup_command = "ralphus-daemon stop --port {port}"

            [[review.action.input]]
            name = "port"
            message = "Port for the daemon"
            default = "7890"
        "#;
        let parsed: TaskFile = toml::from_str(toml).expect("should deserialize");
        let action = &parsed.review[0].action[0];
        assert_eq!(
            action.cleanup_command.as_deref(),
            Some("ralphus-daemon stop --port {port}")
        );
        assert_eq!(action.input.len(), 1);
        assert_eq!(action.input[0].name, "port");
        assert_eq!(action.input[0].message, "Port for the daemon");
        assert_eq!(action.input[0].default, "7890");
    }

    #[test]
    fn review_action_cleanup_command_and_input_default_to_empty() {
        let toml = r#"
            [[task]]
            name = "t"
            [[task.session]]
            cwd = "/repo/.wt/feat"
            prompt = "do work"
            review = "backend"

            [[review]]
            id = "backend"

            [[review.action]]
            label = "Smoke test"
            command = "cargo test"
        "#;
        let parsed: TaskFile = toml::from_str(toml).expect("should deserialize");
        let action = &parsed.review[0].action[0];
        assert!(action.cleanup_command.is_none());
        assert!(action.input.is_empty());
    }

    #[test]
    fn system_prompt_fields_deserialize() {
        let toml = r#"
            [[task]]
            name = "t"
            [[task.session]]
            cwd = "/repo"
            prompt = "do work"
            agent = "claude-code"
            system_prompt = "Follow the house style guide."
            system_prompt_position = "append"
        "#;
        let parsed: TaskFile = toml::from_str(toml).expect("should deserialize");
        let sess = &parsed.task[0].session[0];
        assert_eq!(
            sess.system_prompt.as_deref(),
            Some("Follow the house style guide.")
        );
        assert_eq!(
            sess.system_prompt_position.as_deref(),
            Some(SYSTEM_PROMPT_POSITION_APPEND)
        );
    }

    #[test]
    fn system_prompt_fields_default_to_none() {
        let toml = r#"
            [[task]]
            name = "t"
            [[task.session]]
            cwd = "/repo"
            prompt = "do work"
        "#;
        let parsed: TaskFile = toml::from_str(toml).expect("should deserialize");
        let sess = &parsed.task[0].session[0];
        assert!(sess.system_prompt.is_none());
        assert!(sess.system_prompt_position.is_none());
    }

    #[test]
    fn environment_deserializes_at_task_and_session_level() {
        let toml = r#"
            [[task]]
            name = "t"
            environment = { SHARED = "from-task", TASK_ONLY = "1" }
            [[task.session]]
            cwd = "/repo"
            prompt = "do work"
            environment = { SHARED = "from-session", SESSION_ONLY = "2" }
        "#;
        let parsed: TaskFile = toml::from_str(toml).expect("should deserialize");
        let task = &parsed.task[0];
        assert_eq!(
            task.environment.get("SHARED").map(String::as_str),
            Some("from-task")
        );
        assert_eq!(
            task.environment.get("TASK_ONLY").map(String::as_str),
            Some("1")
        );
        let sess = &task.session[0];
        assert_eq!(
            sess.environment.get("SHARED").map(String::as_str),
            Some("from-session")
        );
        assert_eq!(
            sess.environment.get("SESSION_ONLY").map(String::as_str),
            Some("2")
        );
    }

    #[test]
    fn environment_defaults_to_empty() {
        let toml = r#"
            [[task]]
            name = "t"
            [[task.session]]
            cwd = "/repo"
            prompt = "do work"
        "#;
        let parsed: TaskFile = toml::from_str(toml).expect("should deserialize");
        assert!(parsed.task[0].environment.is_empty());
        assert!(parsed.task[0].session[0].environment.is_empty());
    }

    #[test]
    fn upstream_task_ref_matches_simple_task() {
        assert_eq!(parse_upstream_task_ref("<<task:my-task>>"), Some("my-task"));
    }

    #[test]
    fn upstream_task_ref_matches_task_session() {
        assert_eq!(
            parse_upstream_task_ref("<<task:my-task/session-1>>"),
            Some("my-task/session-1")
        );
    }

    #[test]
    fn upstream_task_ref_none_for_plain_branch() {
        assert_eq!(parse_upstream_task_ref("main"), None);
        assert_eq!(parse_upstream_task_ref("<<upstream>>"), None);
        assert_eq!(parse_upstream_task_ref(""), None);
    }

    #[test]
    fn worktree_placeholder_parses() {
        assert_eq!(
            parse_worktree_placeholder("ralphus:new-worktree/RAL-100-feature"),
            Some("RAL-100-feature")
        );
    }

    #[test]
    fn worktree_placeholder_rejects_plain_paths() {
        assert_eq!(parse_worktree_placeholder("/home/me/repo"), None);
        assert_eq!(parse_worktree_placeholder("C:/Users/me/repo"), None);
        assert_eq!(
            parse_worktree_placeholder("C:/repo/new-worktree/branch"),
            None
        );
        // The old (RAL-100) `<project>:worktree/<branch>` scheme no longer
        // parses -- it never starts with the new `ralphus:new-worktree/` prefix.
        assert_eq!(parse_worktree_placeholder("my-project:worktree/feat"), None);
    }

    #[test]
    fn worktree_placeholder_rejects_empty_branch() {
        assert_eq!(parse_worktree_placeholder("ralphus:new-worktree/"), None);
        assert_eq!(parse_worktree_placeholder("ralphus:new-worktree"), None);
    }

    // ── machine URI parsing (RAL-185) ─────────────────────────────────────

    #[test]
    fn machine_parses_scheme_and_opaque_uri() {
        assert_eq!(
            parse_machine("incredibuild:A"),
            Ok(MachineRef::Provider {
                scheme: "incredibuild",
                uri: "A"
            })
        );
        // The uri half is opaque -- colons, slashes and URLs all pass through
        // to the provider untouched.
        assert_eq!(
            parse_machine("some_machine_provider:https://useful.com/some/website"),
            Ok(MachineRef::Provider {
                scheme: "some_machine_provider",
                uri: "https://useful.com/some/website"
            })
        );
    }

    #[test]
    fn machine_accepts_the_local_literal_case_insensitively() {
        assert_eq!(parse_machine("local"), Ok(MachineRef::Local));
        assert_eq!(parse_machine("Local"), Ok(MachineRef::Local));
        assert_eq!(parse_machine("  local  "), Ok(MachineRef::Local));
    }

    #[test]
    fn machine_rejects_malformed_values() {
        assert_eq!(parse_machine(""), Err(MachineParseError::Empty));
        assert_eq!(parse_machine("   "), Err(MachineParseError::Empty));
        assert_eq!(
            parse_machine("incredibuild"),
            Err(MachineParseError::MissingScheme)
        );
        assert_eq!(parse_machine(":A"), Err(MachineParseError::EmptyScheme));
        assert_eq!(
            parse_machine("incredibuild:"),
            Err(MachineParseError::EmptyUri)
        );
        assert_eq!(
            parse_machine("incredibuild:   "),
            Err(MachineParseError::EmptyUri)
        );
        assert_eq!(
            parse_machine("bad scheme:A"),
            Err(MachineParseError::InvalidSchemeChar(' '))
        );
    }

    #[test]
    fn machine_rejects_a_windows_drive_letter_rather_than_reading_it_as_a_scheme() {
        // A pasted path would otherwise parse as scheme `C` with opaque
        // `\build\wt` and only surface much later as "provider C is not
        // registered", which tells the user nothing useful.
        assert_eq!(
            parse_machine(r"C:\build\wt"),
            Err(MachineParseError::SchemeTooShort)
        );
    }

    #[test]
    fn session_machine_overrides_task_and_unset_falls_through_to_none() {
        let toml = r#"
            [[task]]
            name = "t"
            machine = "incredibuild:A"
            [[task.session]]
            cwd = "/tmp"
            command = "x"
            [[task.session]]
            cwd = "/tmp"
            command = "y"
            machine = "incredibuild:B"
        "#;
        let parsed: TaskFile = toml::from_str(toml).expect("should deserialize");
        let task = &parsed.task[0];
        assert_eq!(
            resolve_session_machine(task, &task.session[0]).as_deref(),
            Some("incredibuild:A"),
            "an unset session machine inherits the task's"
        );
        assert_eq!(
            resolve_session_machine(task, &task.session[1]).as_deref(),
            Some("incredibuild:B"),
            "an explicit session machine wins"
        );

        let bare = r#"
            [[task]]
            name = "t"
            [[task.session]]
            cwd = "/tmp"
            command = "x"
        "#;
        let parsed: TaskFile = toml::from_str(bare).expect("should deserialize");
        let task = &parsed.task[0];
        assert_eq!(
            resolve_session_machine(task, &task.session[0]),
            None,
            "no machine anywhere means local, represented as None"
        );
    }

    #[test]
    fn verify_machine_inherits_from_its_owner() {
        let toml = r#"
            [[task]]
            name = "t"
            machine = "incredibuild:A"
            [[task.session]]
            cwd = "/tmp"
            command = "x"
            machine = "incredibuild:S"
            [[task.session.verify]]
            command = "cargo test"
            [[task.verify]]
            command = "cargo fmt"
        "#;
        let parsed: TaskFile = toml::from_str(toml).expect("should deserialize");
        let task = &parsed.task[0];
        let session = &task.session[0];
        assert_eq!(
            resolve_session_verify_machine(task, session, &session.verify[0]).as_deref(),
            Some("incredibuild:S"),
            "a session-scope verify follows its session, not the task"
        );
        assert_eq!(
            resolve_task_verify_machine(task, &task.verify[0]).as_deref(),
            Some("incredibuild:A"),
            "a task-scope verify follows the task"
        );
    }

    #[test]
    fn command_only_session_deserializes() {
        // The old project's bug: this used to fail because `prompt` was required.
        let toml = r#"
            [[task]]
            name = "t"
            [[task.session]]
            cwd = "/tmp"
            command = "cargo build"
        "#;
        let parsed: TaskFile = toml::from_str(toml).expect("should deserialize");
        let sess = &parsed.task[0].session[0];
        assert!(sess.prompt.is_none());
        assert_eq!(sess.command.as_deref(), Some("cargo build"));
    }
}
