//! Task-file schema: the serde types a submitted TOML batch deserializes into.
//!
//! Ported from the predecessor project (`old:src/tasks/schema.rs`) with one
//! deliberate fix: a cell's `prompt` is now `Option<String>` and pairs with
//! `command`. In the old project `prompt` was a required `String` even though
//! the validator and UI allowed `command`-only cells, so a valid
//! `command`-only cell would pass validation and then fail to deserialize in
//! the runner (see `FINDINGS.local.md` §2.3). Making both optional and enforcing
//! "exactly one" in the validator keeps the type and the rules in agreement.

use std::collections::BTreeMap;

use serde::Deserialize;

/// Settings that apply to an entire submission (task file).
///
/// Declared via `[[default]]` blocks in TOML; only the first block is used.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct DefaultBlock {
    /// Other squad IDs (or `squad-id/task/cell` paths) that must be Done before
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
    /// Top-level review (guardian) declarations. Cells opt in by setting
    /// `review = "<id>"` to match a review's `id` field.
    #[serde(default)]
    pub review: Vec<ReviewDef>,
}

/// One task: a unit of work made of one or more agent cells plus proof steps.
#[derive(Debug, Clone, Deserialize)]
pub struct TaskDef {
    /// Task name / identifier (unique within a submission).
    pub name: String,
    /// Namespace label for a plain-path task; purely cosmetic and left unset if
    /// the caller doesn't care to name it (the UI shows "--"). When any of this
    /// task's cells uses a `<project>:worktree/<branch>` placeholder `cwd`
    /// (RAL-100), this field's meaning changes: it becomes required and must
    /// name a project registered via `ralphus project git` -- checked
    /// structurally here (`project` set whenever a placeholder is present, see
    /// [`crate::validate`]) and against the registry at submit time (the
    /// registry lives in the daemon's store, which `core` cannot see).
    #[serde(default)]
    pub project: Option<String>,
    /// Agent binary for all cells (e.g. `"claude"`). Cells inherit unless overridden.
    #[serde(default)]
    pub agent: Option<String>,
    /// Default model for all cells. Cells may override.
    #[serde(default)]
    pub model: Option<String>,
    /// The machine every cell (and proof step) under this task runs on
    /// (RAL-185), written `scheme:uri` — e.g. `"incredibuild:A"`. `scheme`
    /// names a provider registered with the daemon; `uri` is opaque and
    /// handed to that provider verbatim. Unset means [`LOCAL_MACHINE`].
    ///
    /// Cells and proof steps inherit this and may override it, exactly
    /// like `agent`/`model` — but note that the affinity rules reject a
    /// submission whose cells within one task disagree, so an override is
    /// only meaningful for restating the same value.
    #[serde(default)]
    pub machine: Option<String>,
    /// Extra CLI args passed to the agent for every cell; task-level first.
    #[serde(default)]
    pub args: Vec<String>,
    /// Task budget cap in total tokens (input + output). A cell exceeding it
    /// is failed. Cells/proofs inherit this unless they set their own.
    #[serde(default)]
    pub budget_tokens: Option<u64>,
    /// Task-level max spend cap in USD. A cell exceeding it is killed
    /// mid-run and failed. Cells inherit this unless they set their own
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
    /// Default wall-clock timeout in minutes for the task's cells and proof
    /// steps; each may override with its own `timeout_minutes`.
    #[serde(default)]
    pub timeout_minutes: Option<u32>,
    /// Other tasks/cells this whole task waits on.
    #[serde(default)]
    pub depends_on: Vec<String>,
    /// Environment variables for every cell (and cell proof step)
    /// under this task's spawned subprocess (RAL-172). A cell sets the
    /// same key to override it just for itself, mirroring `agent`/`model`
    /// inheritance. At submission time these seed the task's row in the
    /// daemon's existing hierarchical env-override store (`env_overrides`
    /// column; see `daemon/src/store.rs`'s `squad < task < cell` layering,
    /// RAL-150) rather than being a separate delivery mechanism -- from
    /// there on they're indistinguishable from an override set later via
    /// `POST /api/squads/{id}/tasks/{ti}/env`.
    #[serde(default)]
    pub environment: BTreeMap<String, String>,
    /// Opts a git-backed task out of the daemon's automatic no-new-commits
    /// guard (RAL-156): normally, when the task's `project` is registered as
    /// git, the finalizer fails the task if none of its cells produced a
    /// commit since the task started. Set this for tasks that are legitimately
    /// expected to produce no commits (e.g. a read-only investigation). Has no
    /// effect on non-git-backed tasks, and never affects the manual
    /// `set_status → Done` override (RAL-74), which always bypasses the guard.
    #[serde(default)]
    pub no_commit_required: bool,
    /// The agent cells.
    #[serde(default)]
    pub cell: Vec<CellDef>,
    /// Proof steps that run after ALL cells complete.
    #[serde(default)]
    pub proof: Vec<ProofStep>,
}

/// One agent cell within a task.
#[derive(Debug, Clone, Deserialize)]
pub struct CellDef {
    /// Cell ID (for dependency references). Must not contain `/`.
    #[serde(default)]
    pub id: Option<String>,
    /// Human-readable display label. Shown in the board wherever cells are
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
    /// Cells that must complete before this one starts. Within-task cell
    /// ID (`"cell-a"`) or cross-task `"<task-name>/<cell-id>"`.
    #[serde(default)]
    pub depends_on: Vec<String>,
    /// Override the task-level agent for this cell.
    #[serde(default)]
    pub agent: Option<String>,
    /// Override the task-level model for this cell.
    #[serde(default)]
    pub model: Option<String>,
    /// Override the task-level `machine` for this cell (RAL-185). See
    /// [`TaskDef::machine`] — the affinity rules require every cell within
    /// one task to resolve to the same machine, so this may only restate the
    /// task's value, never diverge from it.
    #[serde(default)]
    pub machine: Option<String>,
    /// Subdirectories of a monorepo this cell is scoped to (e.g.
    /// `["packages/foo", "packages/bar"]`). At run time the daemon injects a
    /// system-prompt addendum instructing the agent to confine its edits to
    /// those paths. The `cwd` itself always points to the repo root (RAL-23).
    #[serde(default)]
    pub subprojects: Vec<String>,
    /// System-prompt text delivered to the agent as an *appended* system prompt
    /// (via the backend's own mechanism, e.g. the Claude Code CLI's
    /// `--append-system-prompt`) rather than concatenated into the user prompt.
    /// Cell-level only; validation restricts it to backends with complete
    /// appended-system-prompt support (currently `claude-code`, `codex`, and
    /// `pi`) until the other backends' support is complete (RAL-5).
    #[serde(default)]
    pub system_prompt: Option<String>,
    /// Where [`system_prompt`](Self::system_prompt) is placed. The only accepted
    /// value today is [`SYSTEM_PROMPT_POSITION_APPEND`] (`"append"`); validation
    /// rejects any other value.
    #[serde(default)]
    pub system_prompt_position: Option<String>,
    /// Extra args appended after any task-level args for this cell only.
    #[serde(default)]
    pub args: Vec<String>,
    /// Per-cell budget in total tokens (input + output). Falls back to the
    /// task-level `budget_tokens` when unset. Exceeding it fails the cell.
    #[serde(default)]
    pub budget_tokens: Option<u64>,
    /// Per-cell max spend cap in USD. Falls back to the task-level
    /// `maximum_budget_usd` when unset. Once the cell's live running cost
    /// exceeds this, the daemon kills it mid-run and fails it.
    #[serde(default)]
    pub maximum_budget_usd: Option<f64>,
    /// Per-cell wall-clock timeout in minutes. Falls back to the task-level
    /// `timeout_minutes` when unset.
    #[serde(default)]
    pub timeout_minutes: Option<u32>,
    /// Initial queue-priority hint for this cell (lower value = higher
    /// priority = runs sooner). Seeds the live queue rank at submit time; the
    /// Queue view/CLI own ordering thereafter.
    #[serde(default)]
    pub priority: Option<u32>,
    /// Cell-level proof steps.
    #[serde(default)]
    pub proof: Vec<ProofStep>,
    /// Review (guardian) opt-in. Set to the `id` of a top-level `[[review]]`
    /// block to include this cell's worktree branch in that review.
    #[serde(default)]
    pub review: Option<String>,
    /// Environment variables for this cell's spawned subprocess
    /// (RAL-172). Merges with (and wins over on a shared key) the owning
    /// task's `environment`; see [`TaskDef::environment`] for how these
    /// values reach the runner.
    #[serde(default)]
    pub environment: BTreeMap<String, String>,
    /// Upstream branch source for this cell's branch. When set to
    /// `"<<task:task-name>>"` (or `"<<task:task-name/cell-id>>"`), the daemon
    /// rebases this cell's branch onto the named dependency's current branch
    /// tip immediately before starting the runner. Both worktrees must be in the
    /// same git repository; cross-repo upstreams are skipped with a warning.
    #[serde(default)]
    pub upstream: Option<String>,
}

/// Sentinel prefix for `upstream = "<<task:task-name>>"` or
/// `"<<task:task-name/cell-id>>"`: rebase this cell's branch onto the
/// named dependency's current branch tip before the cell starts. The daemon
/// resolves this at run time, immediately before launching the runner.
pub const UPSTREAM_TASK_REF_PREFIX: &str = "<<task:";

/// Parse the task/cell ref from an `upstream = "<<task:...>>"` sentinel.
/// Returns the inner ref string (e.g. `"task-a"` or `"task-a/cell-1"`)
/// when the sentinel matches, `None` otherwise (a plain branch-name string,
/// or any other non-matching value).
#[must_use]
pub fn parse_upstream_task_ref(upstream: &str) -> Option<&str> {
    upstream
        .strip_prefix(UPSTREAM_TASK_REF_PREFIX)
        .and_then(|s| s.strip_suffix(">>"))
}

/// Scheme prefix for a placeholder cell `cwd` that names a branch to
/// materialize a fresh (or reused) worktree for, e.g.
/// `"ralphus:new-worktree/RAL-100-feature?upstream=main"` (RAL-100). Rather
/// than a real filesystem path, this names a branch; the owning task's
/// `project` field (REQUIRED whenever any of its cells uses this placeholder)
/// names which project registered via `ralphus project git` to materialize it
/// under. The trailing `?upstream=<upstream>` is REQUIRED (see
/// [`parse_worktree_placeholder_upstream`]) so ralphus always knows what the
/// branch tracks, rather than guessing from whatever `HEAD` happens to be at
/// materialization time. The value may be a literal branch name (`main`,
/// `origin/main`) or one of the reserved sentinels [`WORKTREE_UPSTREAM_DEFAULT`]
/// / [`WORKTREE_UPSTREAM_CURRENT_BRANCH`], which the daemon expands against
/// the project before materialization. The daemon resolves that project,
/// materializes (or reuses) a git worktree for the branch under
/// `.git/.ralphus_worktrees/<branch>`, and rewrites the cell's `cwd` to that
/// real path before the cell runs.
pub const WORKTREE_PLACEHOLDER_PREFIX: &str = "ralphus:new-worktree/";

/// Parse a cell `cwd` as a `ralphus:new-worktree/<branch>[?upstream=<upstream>]`
/// placeholder (RAL-100). Returns the branch name when non-empty; `None` for a
/// plain filesystem path or a malformed placeholder-shaped string (empty
/// branch). A real absolute path never starts with the literal
/// `ralphus:new-worktree/` prefix, so this is unambiguous. The project to
/// materialize the branch under is NOT embedded here -- it comes from the
/// owning task's `project` field (see [`crate::validate`]). The parser does no
/// further segmentation beyond the optional `?upstream=...` query suffix (see
/// [`parse_worktree_placeholder_upstream`]): everything else after the
/// prefix, slashes included, is returned as one literal branch name. A value
/// like `ralphus:new-worktree/origin/feature/x` therefore parses as the
/// branch name `origin/feature/x`; any later interpretation of that shape as
/// "maybe a remote-tracking branch" happens in daemon-side worktree
/// materialization, not here.
#[must_use]
pub fn parse_worktree_placeholder(cwd: &str) -> Option<&str> {
    let rest = cwd.strip_prefix(WORKTREE_PLACEHOLDER_PREFIX)?;
    let branch = rest.split_once('?').map_or(rest, |(branch, _)| branch);
    if branch.is_empty() {
        return None;
    }
    Some(branch)
}

/// Parse the `?upstream=<upstream>` query suffix off a
/// `ralphus:new-worktree/<branch>?upstream=<upstream>` placeholder. Returns
/// `None` when the placeholder has no query suffix, an empty `upstream=`
/// value, or any other malformed/unrecognized query -- callers that require
/// an explicit upstream (submit-time validation in [`crate::validate`], and
/// the daemon's own defensive re-check at worktree-resolution time) treat
/// `None` as "no upstream given" and report it themselves; this function does
/// no error reporting of its own.
///
/// `?` is never valid inside a git branch name (`git check-ref-format`
/// rejects it), so splitting the placeholder on the first `?` unambiguously
/// separates the branch from the query string.
///
/// This is a *worktree tracking* upstream -- the `git branch
/// --set-upstream-to` target that `derive_reviews` needs to determine a
/// review's base and that drives resync-on-reuse (fetch + rebase) for a
/// remote-tracking branch. It is unrelated to a cell's own top-level
/// `upstream = "<<task:...>>"` field ([`CellDef::upstream`]), which rebases
/// this cell's branch onto another task/cell's tip immediately before the
/// cell runs -- a content operation, not a tracking-configuration one. A cell
/// can set both: the `cwd` placeholder's `?upstream=` for tracking, and its
/// own `upstream` field for a pre-run rebase.
#[must_use]
pub fn parse_worktree_placeholder_upstream(cwd: &str) -> Option<&str> {
    let rest = cwd.strip_prefix(WORKTREE_PLACEHOLDER_PREFIX)?;
    let (_, query) = rest.split_once('?')?;
    let value = query.strip_prefix("upstream=")?;
    let value = value.split_once('&').map_or(value, |(value, _)| value);
    if value.is_empty() {
        return None;
    }
    Some(value)
}

/// Find every `<<...>>` placeholder body embedded in `text`, in order.
///
/// Unterminated `<<...` runs are ignored and left to callers as literal text.
/// The returned slices exclude the surrounding `<<` / `>>` delimiters.
#[must_use]
pub fn text_placeholders(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut offset = 0usize;
    while let Some(open_rel) = text[offset..].find("<<") {
        let open = offset + open_rel;
        let body_start = open + 2;
        let Some(close_rel) = text[body_start..].find(">>") else {
            break;
        };
        let close = body_start + close_rel;
        out.push(&text[body_start..close]);
        offset = close + 2;
    }
    out
}

/// The first worktree placeholder found in `text`, either as the whole string
/// itself (`ralphus:new-worktree/...`) or wrapped inside `<<...>>`.
#[must_use]
pub fn first_worktree_placeholder_in_text(text: &str) -> Option<&str> {
    parse_worktree_placeholder(text).map(|_| text).or_else(|| {
        text_placeholders(text)
            .into_iter()
            .find(|body| parse_worktree_placeholder(body).is_some())
    })
}

/// Reserved `?upstream=` sentinel value meaning "the repository's default
/// branch" — expanded daemon-side against the project root (see
/// `ralphus_daemon::worktrees::resolve_upstream`). Authored as
/// `?upstream=<<default>>`. The recommended choice: it names a stable,
/// well-defined tracking target without the caller needing to know the
/// branch name up front.
pub const WORKTREE_UPSTREAM_DEFAULT: &str = "<<default>>";

/// Reserved `?upstream=` sentinel value meaning "whatever branch the project
/// currently has checked out in its registered root/primary worktree" —
/// expanded daemon-side (see `ralphus_daemon::worktrees::resolve_upstream`).
/// Authored as `?upstream=<<current_branch>>`. Flagged in the tutor as
/// riskier than `<<default>>`: the checked-out branch can silently change
/// between runs, so the tracking target is not stable across submissions.
pub const WORKTREE_UPSTREAM_CURRENT_BRANCH: &str = "<<current_branch>>";

/// The recognized reserved `?upstream=` sentinels, in alphabetical order
/// (used to build the actionable "choose one of these" message at submit-time
/// validation in [`crate::validate`]).
pub const WORKTREE_UPSTREAM_SENTINELS: [&str; 2] =
    [WORKTREE_UPSTREAM_CURRENT_BRANCH, WORKTREE_UPSTREAM_DEFAULT];

/// Is `upstream` one of the reserved `?upstream=` sentinels
/// ([`WORKTREE_UPSTREAM_DEFAULT`] / [`WORKTREE_UPSTREAM_CURRENT_BRANCH`])?
/// Callers use this to reject any other `<<...>>` value — a typo or an
/// unsupported sentinel — at validation time, so it fails fast at submit
/// rather than reaching the daemon. A literal branch name returns `false`.
#[must_use]
pub fn is_worktree_upstream_sentinel(upstream: &str) -> bool {
    WORKTREE_UPSTREAM_SENTINELS.contains(&upstream)
}

/// The scheme prefix for a new-review placeholder id. A review whose `id` is
/// `ralphus:new-review/<key>` is a *submission-local placeholder* for a review id
/// that does not exist yet: within a single submission, every cell that names
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

/// The effective `machine` for a cell: its own value, else the owning
/// task's, else `None` (meaning [`LOCAL_MACHINE`]). Mirrors how `agent` and
/// `model` inherit in [`ResolvedAgent::resolve`].
#[must_use]
pub fn resolve_cell_machine(task: &TaskDef, cell: &CellDef) -> Option<String> {
    cell.machine
        .clone()
        .or_else(|| task.machine.clone())
        .map(|m| m.trim().to_string())
        .filter(|m| !m.is_empty())
}

/// The effective `machine` for a task-scope proof step (one with no owning
/// cell): the step's own value, else the task's.
#[must_use]
pub fn resolve_task_proof_machine(task: &TaskDef, proof: &ProofStep) -> Option<String> {
    proof
        .machine
        .clone()
        .or_else(|| task.machine.clone())
        .map(|m| m.trim().to_string())
        .filter(|m| !m.is_empty())
}

/// The effective `machine` for a cell-scope proof step: the step's own
/// value, else the owning cell's resolved machine (which itself falls back
/// to the task's).
#[must_use]
pub fn resolve_cell_proof_machine(
    task: &TaskDef,
    cell: &CellDef,
    proof: &ProofStep,
) -> Option<String> {
    proof
        .machine
        .clone()
        .map(|m| m.trim().to_string())
        .filter(|m| !m.is_empty())
        .or_else(|| resolve_cell_machine(task, cell))
}

/// A top-level review (guardian) declaration via `[[review]]`.
///
/// Cells opt in by declaring `review = "<id>"`. When several cells resolve
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
    /// the worktree, so a review fed by a **remote** cell must state its
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
    /// Max spend cap in USD for this review's own agent cost -- conflict
    /// resolution and prover calls made by the guardian merge machinery,
    /// summed cumulatively across every rebase/re-merge attempt (RAL-193).
    /// Enforced the same way [`TaskDef::maximum_budget_usd`]/
    /// [`CellDef::maximum_budget_usd`] are: once exceeded, the daemon
    /// stops making further resolver/prover calls for this review and
    /// fails it.
    #[serde(default)]
    pub maximum_budget_usd: Option<f64>,
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

/// One proof step. Exactly one of `command` / `brain` / `prompt` must be set.
#[derive(Debug, Clone, Deserialize)]
pub struct ProofStep {
    /// Proof step ID (for `restart_on` / proof-level dependencies).
    #[serde(default)]
    pub id: Option<String>,
    /// Shell command; exit code is the verdict.
    #[serde(default)]
    pub command: Option<String>,
    /// Prompt routed to the local brain (deferred in ralphus MVP).
    #[serde(default)]
    pub brain: Option<String>,
    /// Headless AI proof-step prompt. Runs using the owning cell's
    /// resolved backend program (its `agent`), with this step's own `model`
    /// as an override — a proof step has no separate backend selector.
    #[serde(default)]
    pub prompt: Option<String>,
    /// Model override for the `prompt` proof (falls back to the owning
    /// cell's resolved model when unset).
    #[serde(default)]
    pub model: Option<String>,
    /// Override the inherited `machine` for this proof step (RAL-185). Falls
    /// back to the owning cell's resolved machine for a cell-scope step,
    /// or the task's for a task-scope one. See [`TaskDef::machine`] — the
    /// affinity rules require every step under one task to agree.
    #[serde(default)]
    pub machine: Option<String>,
    /// Extra CLI args for the proof-prompt invocation (e.g. `--append-system-prompt`).
    #[serde(default)]
    pub arguments: Vec<String>,
    /// Budget for the proof prompt, in total tokens (input + output). Falls
    /// back to the task-level `budget_tokens` when unset.
    #[serde(default)]
    pub budget_tokens: Option<u64>,
    /// Per-proof wall-clock timeout in minutes. Falls back to the task-level
    /// `timeout_minutes` when unset.
    #[serde(default)]
    pub timeout_minutes: Option<u32>,
    /// Whether the step needs human approval.
    #[serde(default)]
    pub requires_approval: bool,
    /// Other proof steps that, when they fire, re-run this cell's proof
    /// cursor from the start. Grammar: `task/cell/proof?on=pass|fail|both`,
    /// with wildcards `task/*` and `task/cell/*`.
    #[serde(default)]
    pub restart_on: Vec<String>,
    /// Environment variables for **this one proof step's** spawned subprocess
    /// (RAL-191). The narrowest layer in the hierarchy: merged on top of the
    /// owning scope's `proof` overrides, which themselves sit on top of the
    /// cell's/task's/squad's — see [`TaskDef::environment`].
    ///
    /// Kept per-step rather than folded into the owning task's/cell's
    /// single `proof_env_overrides` row precisely because `proof` is an
    /// array: two `[[task.proof]]` blocks setting the same key to different
    /// values must not collide. At submission time this seeds that step's own
    /// row in the daemon's override store (`proofs.env_overrides`), after
    /// which it is indistinguishable from one set later via
    /// `POST /api/squads/{id}/tasks/{ti}/proof/{vi}/env`.
    #[serde(default)]
    pub environment: BTreeMap<String, String>,
}

/// Resolved agent config after task→cell inheritance is applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedAgent {
    /// Binary name or path to invoke.
    pub program: String,
    /// Effective model, if set.
    pub model: Option<String>,
    /// Extra args, task-level first then cell-level.
    pub args: Vec<String>,
}

/// The default agent program when neither cell nor task specifies one.
pub const DEFAULT_AGENT: &str = "claude";

/// The [`CellDef::system_prompt_position`] value meaning "append to the
/// agent's system prompt". Currently the only accepted position (RAL-5).
pub const SYSTEM_PROMPT_POSITION_APPEND: &str = "append";

/// The built-in backend names and their aliases -- the only `agent` values
/// `core` can classify on its own. Any other name might be a custom
/// `[agent.profiles.*]` entry (see `daemon/src/agent_profiles.rs`), whose
/// backend `core` cannot see: the profile registry lives in the daemon's
/// config/store, not in this dependency-light crate. This is the single
/// source of truth for that reserved set -- `daemon::agent_profiles` reuses
/// it rather than keeping its own copy, so a profile can never be named
/// after a built-in.
///
/// UPDATE THIS whenever a new agent backend/harness is added (see also
/// `ralphus agent list` / `cli-rs/src/agents.rs` and `PROFILE_BACKENDS` in
/// `daemon/src/agent_profiles.rs`, which enumerate backends too) --
/// otherwise the new name stays eligible to collide with a future custom
/// profile, and `check_system_prompt` in `core/src/validate.rs` will
/// silently defer its system_prompt support check to the daemon instead of
/// answering it offline.
pub const RESERVED_AGENT_NAMES: &[&str] = &[
    "claude",
    "anthropic",
    "ollama",
    "claude-code",
    "claude-cli",
    "codex",
    "codex-cli",
    "pi",
    "raw",
];

/// Whether `agent` is a backend with complete appended-system-prompt support.
///
/// The Claude Code CLI (`claude-code`, and its `claude-cli` alias) maps the
/// appended system prompt to a real backend flag (`--append-system-prompt`);
/// the Codex CLI (`codex`, and its `codex-cli` alias) maps it to its own
/// config-override mechanism (`-c developer_instructions=...` -- there is no
/// dedicated flag); Pi maps it to `--append-system-prompt`. Support for other
/// backends is best-effort but not complete, so validation rejects
/// `system_prompt`/`system_prompt_position` for any other agent (RAL-5).
///
/// Also used, unmodified, to test a *resolved backend* string (e.g. a custom
/// agent profile's `backend = "claude-code"`) -- see
/// `daemon::agent_profiles::validate_task_file_profiles`, which is the only
/// place a custom-profile cell's `system_prompt` gets checked, since `core`
/// itself defers on any agent name outside [`RESERVED_AGENT_NAMES`].
#[must_use]
pub fn agent_supports_system_prompt(agent: &str) -> bool {
    matches!(
        agent,
        "claude-code" | "claude-cli" | "codex" | "codex-cli" | "pi"
    )
}

impl ResolvedAgent {
    /// Merge task defaults with cell overrides (cell wins).
    #[must_use]
    pub fn resolve(task: &TaskDef, cell: &CellDef) -> Self {
        let program = cell
            .agent
            .clone()
            .or_else(|| task.agent.clone())
            .unwrap_or_else(|| DEFAULT_AGENT.to_string());
        let model = cell.model.clone().or_else(|| task.model.clone());
        let mut args = task.args.clone();
        args.extend_from_slice(&cell.args);
        Self {
            program,
            model,
            args,
        }
    }

    /// Task-level defaults only (for task-level proof steps that have no cell).
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
            no_commit_required: false,
            cell: vec![],
            proof: vec![],
        }
    }

    fn cell_with(agent: Option<&str>, model: Option<&str>, args: &[&str]) -> CellDef {
        CellDef {
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
            proof: vec![],
            review: None,
            upstream: None,
        }
    }

    #[test]
    fn cell_overrides_task_agent_and_model() {
        let task = task_with(Some("claude"), Some("task-model"), &["--task"]);
        let sess = cell_with(Some("aider"), Some("sess-model"), &["--sess"]);
        let r = ResolvedAgent::resolve(&task, &sess);
        assert_eq!(r.program, "aider");
        assert_eq!(r.model.as_deref(), Some("sess-model"));
        assert_eq!(r.args, vec!["--task", "--sess"]);
    }

    #[test]
    fn cell_inherits_task_when_unset() {
        let task = task_with(Some("codex"), Some("m"), &["--a"]);
        let sess = cell_with(None, None, &[]);
        let r = ResolvedAgent::resolve(&task, &sess);
        assert_eq!(r.program, "codex");
        assert_eq!(r.model.as_deref(), Some("m"));
        assert_eq!(r.args, vec!["--a"]);
    }

    #[test]
    fn defaults_to_claude_when_nothing_set() {
        let task = task_with(None, None, &[]);
        let sess = cell_with(None, None, &[]);
        assert_eq!(ResolvedAgent::resolve(&task, &sess).program, DEFAULT_AGENT);
    }

    #[test]
    fn toplevel_review_deserializes() {
        let toml = r#"
            [[task]]
            name = "t"
            [[task.cell]]
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
        assert_eq!(parsed.task[0].cell[0].review.as_deref(), Some("backend"));
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
            [[task.cell]]
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
            [[task.cell]]
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
            [[task.cell]]
            cwd = "/repo"
            prompt = "do work"
            agent = "claude-code"
            system_prompt = "Follow the house style guide."
            system_prompt_position = "append"
        "#;
        let parsed: TaskFile = toml::from_str(toml).expect("should deserialize");
        let sess = &parsed.task[0].cell[0];
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
            [[task.cell]]
            cwd = "/repo"
            prompt = "do work"
        "#;
        let parsed: TaskFile = toml::from_str(toml).expect("should deserialize");
        let sess = &parsed.task[0].cell[0];
        assert!(sess.system_prompt.is_none());
        assert!(sess.system_prompt_position.is_none());
    }

    #[test]
    fn environment_deserializes_at_task_and_cell_level() {
        let toml = r#"
            [[task]]
            name = "t"
            environment = { SHARED = "from-task", TASK_ONLY = "1" }
            [[task.cell]]
            cwd = "/repo"
            prompt = "do work"
            environment = { SHARED = "from-cell", CELL_ONLY = "2" }
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
        let sess = &task.cell[0];
        assert_eq!(
            sess.environment.get("SHARED").map(String::as_str),
            Some("from-cell")
        );
        assert_eq!(
            sess.environment.get("CELL_ONLY").map(String::as_str),
            Some("2")
        );
    }

    #[test]
    fn environment_deserializes_on_task_and_cell_proof_steps() {
        // RAL-191: `environment` is per proof *step*, not per proof scope --
        // two steps under the same task must be able to set the same key to
        // different values without colliding.
        let toml = r#"
            [[task]]
            name = "t"
            [[task.proof]]
            command = "cargo test"
            environment = { RUST_LOG = "debug" }
            [[task.proof]]
            command = "cargo clippy"
            environment = { RUST_LOG = "warn" }
            [[task.cell]]
            cwd = "/repo"
            prompt = "do work"
            [[task.cell.proof]]
            command = "npm test"
            environment = { CI = "1" }
        "#;
        let parsed: TaskFile = toml::from_str(toml).expect("should deserialize");
        let task = &parsed.task[0];
        assert_eq!(
            task.proof[0]
                .environment
                .get("RUST_LOG")
                .map(String::as_str),
            Some("debug")
        );
        assert_eq!(
            task.proof[1]
                .environment
                .get("RUST_LOG")
                .map(String::as_str),
            Some("warn"),
            "each proof step keeps its own value for the same key"
        );
        assert_eq!(
            task.cell[0].proof[0]
                .environment
                .get("CI")
                .map(String::as_str),
            Some("1")
        );
    }

    #[test]
    fn environment_defaults_to_empty() {
        let toml = r#"
            [[task]]
            name = "t"
            [[task.cell]]
            cwd = "/repo"
            prompt = "do work"
        "#;
        let parsed: TaskFile = toml::from_str(toml).expect("should deserialize");
        assert!(parsed.task[0].environment.is_empty());
        assert!(parsed.task[0].cell[0].environment.is_empty());
    }

    #[test]
    fn upstream_task_ref_matches_simple_task() {
        assert_eq!(parse_upstream_task_ref("<<task:my-task>>"), Some("my-task"));
    }

    #[test]
    fn upstream_task_ref_matches_task_cell() {
        assert_eq!(
            parse_upstream_task_ref("<<task:my-task/cell-1>>"),
            Some("my-task/cell-1")
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

    #[test]
    fn worktree_placeholder_strips_the_upstream_query_from_the_branch() {
        assert_eq!(
            parse_worktree_placeholder("ralphus:new-worktree/feat?upstream=main"),
            Some("feat")
        );
        assert_eq!(
            parse_worktree_placeholder(
                "ralphus:new-worktree/origin/feature/x?upstream=origin/blah"
            ),
            Some("origin/feature/x")
        );
    }

    #[test]
    fn worktree_placeholder_upstream_parses() {
        assert_eq!(
            parse_worktree_placeholder_upstream("ralphus:new-worktree/feat?upstream=main"),
            Some("main")
        );
        assert_eq!(
            parse_worktree_placeholder_upstream(
                "ralphus:new-worktree/origin/feature/x?upstream=origin/blah"
            ),
            Some("origin/blah")
        );
        assert_eq!(
            parse_worktree_placeholder_upstream("ralphus:new-worktree/foo?upstream=bar"),
            Some("bar")
        );
    }

    #[test]
    fn worktree_placeholder_upstream_is_none_when_absent_or_malformed() {
        assert_eq!(
            parse_worktree_placeholder_upstream("ralphus:new-worktree/feat"),
            None
        );
        assert_eq!(
            parse_worktree_placeholder_upstream("ralphus:new-worktree/feat?upstream="),
            None
        );
        assert_eq!(
            parse_worktree_placeholder_upstream("ralphus:new-worktree/feat?"),
            None
        );
        assert_eq!(
            parse_worktree_placeholder_upstream("ralphus:new-worktree/feat?bogus=main"),
            None
        );
        assert_eq!(parse_worktree_placeholder_upstream("/home/me/repo"), None);
    }

    #[test]
    fn text_placeholders_extracts_embedded_marker_bodies() {
        assert_eq!(
            text_placeholders("prefix <<ralphus:new-worktree/feat?upstream=main>> suffix"),
            vec!["ralphus:new-worktree/feat?upstream=main"]
        );
        assert_eq!(text_placeholders("<<one>><<two>>"), vec!["one", "two"]);
        assert!(text_placeholders("<<unterminated").is_empty());
    }

    #[test]
    fn first_worktree_placeholder_in_text_finds_bare_and_wrapped_forms() {
        assert_eq!(
            first_worktree_placeholder_in_text("ralphus:new-worktree/feat?upstream=main"),
            Some("ralphus:new-worktree/feat?upstream=main")
        );
        assert_eq!(
            first_worktree_placeholder_in_text(
                "<<ralphus:new-worktree/feat?upstream=main>>/more/text"
            ),
            Some("ralphus:new-worktree/feat?upstream=main")
        );
        assert_eq!(first_worktree_placeholder_in_text("<<unknown>>"), None);
    }

    #[test]
    fn worktree_upstream_sentinel_recognition() {
        // Both reserved sentinels parse as ordinary upstream values and are
        // recognized as such.
        assert_eq!(
            parse_worktree_placeholder_upstream("ralphus:new-worktree/feat?upstream=<<default>>"),
            Some("<<default>>")
        );
        assert_eq!(
            parse_worktree_placeholder_upstream(
                "ralphus:new-worktree/feat?upstream=<<current_branch>>"
            ),
            Some("<<current_branch>>")
        );
        assert!(is_worktree_upstream_sentinel("<<default>>"));
        assert!(is_worktree_upstream_sentinel("<<current_branch>>"));

        // A plain branch name, or a `<<...>>` typo/unsupported sentinel, is
        // NOT a recognized sentinel -- the latter must fail validation fast.
        assert!(!is_worktree_upstream_sentinel("main"));
        assert!(!is_worktree_upstream_sentinel("<<does-not-exist>>"));
        assert!(!is_worktree_upstream_sentinel(""));
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
    fn cell_machine_overrides_task_and_unset_falls_through_to_none() {
        let toml = r#"
            [[task]]
            name = "t"
            machine = "incredibuild:A"
            [[task.cell]]
            cwd = "/tmp"
            command = "x"
            [[task.cell]]
            cwd = "/tmp"
            command = "y"
            machine = "incredibuild:B"
        "#;
        let parsed: TaskFile = toml::from_str(toml).expect("should deserialize");
        let task = &parsed.task[0];
        assert_eq!(
            resolve_cell_machine(task, &task.cell[0]).as_deref(),
            Some("incredibuild:A"),
            "an unset cell machine inherits the task's"
        );
        assert_eq!(
            resolve_cell_machine(task, &task.cell[1]).as_deref(),
            Some("incredibuild:B"),
            "an explicit cell machine wins"
        );

        let bare = r#"
            [[task]]
            name = "t"
            [[task.cell]]
            cwd = "/tmp"
            command = "x"
        "#;
        let parsed: TaskFile = toml::from_str(bare).expect("should deserialize");
        let task = &parsed.task[0];
        assert_eq!(
            resolve_cell_machine(task, &task.cell[0]),
            None,
            "no machine anywhere means local, represented as None"
        );
    }

    #[test]
    fn proof_machine_inherits_from_its_owner() {
        let toml = r#"
            [[task]]
            name = "t"
            machine = "incredibuild:A"
            [[task.cell]]
            cwd = "/tmp"
            command = "x"
            machine = "incredibuild:S"
            [[task.cell.proof]]
            command = "cargo test"
            [[task.proof]]
            command = "cargo fmt"
        "#;
        let parsed: TaskFile = toml::from_str(toml).expect("should deserialize");
        let task = &parsed.task[0];
        let cell = &task.cell[0];
        assert_eq!(
            resolve_cell_proof_machine(task, cell, &cell.proof[0]).as_deref(),
            Some("incredibuild:S"),
            "a cell-scope proof follows its cell, not the task"
        );
        assert_eq!(
            resolve_task_proof_machine(task, &task.proof[0]).as_deref(),
            Some("incredibuild:A"),
            "a task-scope proof follows the task"
        );
    }

    #[test]
    fn no_commit_required_defaults_to_false() {
        let toml = r#"
            [[task]]
            name = "t"
            [[task.cell]]
            cwd = "/tmp"
            command = "cargo build"
        "#;
        let parsed: TaskFile = toml::from_str(toml).expect("should deserialize");
        assert!(!parsed.task[0].no_commit_required);
    }

    #[test]
    fn no_commit_required_true_deserializes() {
        let toml = r#"
            [[task]]
            name = "t"
            no_commit_required = true
            [[task.cell]]
            cwd = "/tmp"
            command = "cargo build"
        "#;
        let parsed: TaskFile = toml::from_str(toml).expect("should deserialize");
        assert!(parsed.task[0].no_commit_required);
    }

    #[test]
    fn command_only_cell_deserializes() {
        // The old project's bug: this used to fail because `prompt` was required.
        let toml = r#"
            [[task]]
            name = "t"
            [[task.cell]]
            cwd = "/tmp"
            command = "cargo build"
        "#;
        let parsed: TaskFile = toml::from_str(toml).expect("should deserialize");
        let sess = &parsed.task[0].cell[0];
        assert!(sess.prompt.is_none());
        assert_eq!(sess.command.as_deref(), Some("cargo build"));
    }
}
