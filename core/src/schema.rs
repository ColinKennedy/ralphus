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
    /// `review = "<<review:<id>>>"` to match a review's `id` field (see
    /// [`parse_cell_review_sentinel`]).
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
    /// Task-level context-window token limit (RAL-304), delivered to the
    /// backend via its own mechanism (e.g. Codex's
    /// `-c model_context_window=...`). Cells inherit this unless they set
    /// their own `maximum_context`. Only accepted for a backend with a real
    /// delivery mechanism -- see
    /// [`agent_supports_maximum_context`] -- checked at validation time.
    /// Claude Code has no lever that caps the window itself (its
    /// `CLAUDE_CODE_MAX_OUTPUT_TOKENS` only reserves output-generation
    /// budget), so it does not accept this field -- see
    /// [`agent_supports_auto_compact_threshold`] for the field it does
    /// accept.
    #[serde(default)]
    pub maximum_context: Option<u64>,
    /// Task-level auto-compact trigger threshold in tokens (RAL-304),
    /// delivered to the backend via its own mechanism (e.g. Claude Code's
    /// `CLAUDE_CODE_AUTO_COMPACT_WINDOW` env var, Codex's
    /// `-c model_auto_compact_token_limit=...`). Cells inherit this unless
    /// they set their own `auto_compact_threshold`. Accepted for a wider set
    /// of backends than [`Self::maximum_context`] -- see
    /// [`agent_supports_auto_compact_threshold`].
    #[serde(default)]
    pub auto_compact_threshold: Option<u64>,
    /// Task-level cap on how many tokens a single tool-call output (e.g. a
    /// large file read) may inject into the agent's context (RAL-333),
    /// delivered to the backend via its own mechanism (e.g. Claude Code's
    /// `CLAUDE_CODE_FILE_READ_MAX_OUTPUT_TOKENS` env var, Codex's
    /// `-c tool_output_token_limit=...`, Pi's `models.json` `maxTokens`).
    /// Cells inherit this unless they set their own
    /// `maximum_tool_output_tokens`. Ralphus never truncates the output itself --
    /// it only configures the backend's own mechanism and defers entirely to
    /// however that backend behaves once the cap is set. Only accepted for a
    /// backend with a real delivery mechanism -- see
    /// [`agent_supports_maximum_tool_output_tokens`] -- checked at validation
    /// time.
    #[serde(default)]
    pub maximum_tool_output_tokens: Option<u64>,
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
    /// Whether a cell may resume a completed dependency's agent session
    /// (cross-cell session sharing) rather than start fresh, when the
    /// scheduler's own model-mismatch guard allows it -- see
    /// `daemon::scheduler::sharing_blocked_reason`. Off by default: sharing
    /// used to be implicit in any `depends_on` link with no way to opt out,
    /// which surprised a user running Codex under two different models on
    /// dependent cells. Applies to every cell in this task unless a cell sets
    /// its own [`CellDef::share_session`], which wins.
    #[serde(default)]
    pub share_session: Option<bool>,
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
    /// Per-cell context-window token limit (RAL-304). Falls back to the
    /// task-level `maximum_context` when unset. See
    /// [`TaskDef::maximum_context`] for the delivery mechanism and the
    /// backend-support restriction.
    #[serde(default)]
    pub maximum_context: Option<u64>,
    /// Per-cell auto-compact trigger threshold in tokens (RAL-304). Falls
    /// back to the task-level `auto_compact_threshold` when unset. See
    /// [`TaskDef::auto_compact_threshold`].
    #[serde(default)]
    pub auto_compact_threshold: Option<u64>,
    /// Per-cell cap on how many tokens a single tool-call output may inject
    /// into the agent's context (RAL-333). Falls back to the task-level
    /// `maximum_tool_output_tokens` when unset. See
    /// [`TaskDef::maximum_tool_output_tokens`] for the delivery mechanism and the
    /// backend-support restriction.
    #[serde(default)]
    pub maximum_tool_output_tokens: Option<u64>,
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
    /// Review (guardian) opt-in. Must be wrapped in `<<...>>` sentinel syntax
    /// (per `docs/glossary.md`'s sentinel definition -- this value is
    /// resolved/minted at squad time, not used literally): either
    /// `<<review:<id>>>` naming the `id` of a top-level `[[review]]` block to
    /// include this cell's worktree branch in that review, or
    /// `<<ralphus:new-review/<key>>>` to mint a fresh guardian for this
    /// submission (see [`REVIEW_LINK_PREFIX`]). See
    /// [`parse_cell_review_sentinel`].
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
    /// Override the task-level [`TaskDef::share_session`] for this cell only.
    #[serde(default)]
    pub share_session: Option<bool>,
    /// Opts this cell into Triage (RAL-318): rather than naming an explicit
    /// `[[review]]` block via [`Self::review`], the cell is pooled by the
    /// daemon's Arbiter subsystem, keyed by `(project, triage type)`, and a
    /// review is created automatically once that pool's count threshold or
    /// one of its cron schedules fires. Mutually independent of `review` --
    /// nothing stops a task file from setting both on different cells, but
    /// setting both on the *same* cell is meaningless (checked nowhere
    /// special; `review` simply wins since Triage pooling only ever looks at
    /// cells that set `triage = true`). Requires the owning task's `project`
    /// to be set (see `core::validate::validate_cells`), since a pool is
    /// keyed by project.
    #[serde(default)]
    pub triage: bool,
    /// Inline triage type name(s) for this cell's auto-review -- a bare
    /// string (`"security"`) for the common single-type case, or an array
    /// (`["bug", "investigation"]`) when a cell belongs to more than one
    /// pool at once (e.g. a fix that is simultaneously a bug fix and a
    /// piece of research). Each name is matched at `ralphus submit` time
    /// against the daemon's triage-type registry. Only meaningful when
    /// [`Self::triage`] is `true`. Unset means the Arbiter classifies this
    /// cell into type(s) itself, once, at submit time, against the
    /// registered types' label + description text -- a classification
    /// failure permanently assigns the built-in `unclassified` type rather
    /// than retrying. When set, this cell is pooled into every one of its
    /// named types' `(project, triage_type)` pools independently.
    #[serde(default, deserialize_with = "deserialize_triage_type")]
    pub triage_type: Option<Vec<String>>,
}

/// Deserializes `triage_type` as either a bare TOML string (sugar for a
/// single-element list) or an array of strings, so the common one-type case
/// (`triage_type = "security"`) stays as simple to write as before
/// multi-type support existed, while `triage_type = ["bug", "feature"]`
/// works too. Only invoked when the key is present -- `#[serde(default)]`
/// on the field handles the absent case.
fn deserialize_triage_type<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<Vec<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(String),
        Many(Vec<String>),
    }
    Ok(Some(match OneOrMany::deserialize(deserializer)? {
        OneOrMany::One(s) => vec![s],
        OneOrMany::Many(v) => v,
    }))
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
/// review's upstream and that drives resync-on-reuse (fetch + rebase) for a
/// remote-tracking branch. It is unrelated to a cell's own top-level
/// `upstream = "<<task:...>>"` field ([`CellDef::upstream`]), which rebases
/// this cell's branch onto another task/cell's tip immediately before the
/// cell runs -- a content operation, not a tracking-configuration one. It is
/// also unrelated to a `[[review]]` block's own `upstream` field
/// ([`ReviewDef::upstream`]), which declares the branch a review's stack
/// rebases onto rather than a worktree tracking target. A cell can set both
/// of the cell-level fields: the `cwd` placeholder's `?upstream=` for
/// tracking, and its own `upstream` field for a pre-run rebase.
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

/// Byte offset of the `>>` that closes the `<<` whose body starts at
/// `body_start`, or `None` when the run is unterminated.
///
/// Nested `<<...>>` pairs inside the body are matched and skipped, so
/// `<<ralphus:new-worktree/feat?upstream=<<default>>>>` yields a body of
/// `ralphus:new-worktree/feat?upstream=<<default>>` rather than stopping at
/// the inner sentinel's `>>`. When the body contains a `<<` that is never
/// balanced, the scan falls back to the first `>>` so that a lone stray `<<`
/// inside otherwise-literal text still terminates the placeholder where it
/// always did.
#[must_use]
pub fn placeholder_close(text: &str, body_start: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut cursor = body_start;
    let mut first_close = None;
    while cursor < text.len() {
        let rest = &text[cursor..];
        let open_at = rest.find("<<").map(|i| cursor + i);
        let Some(close_at) = rest.find(">>").map(|i| cursor + i) else {
            break;
        };
        if first_close.is_none() {
            first_close = Some(close_at);
        }
        match open_at {
            Some(open) if open < close_at => {
                depth += 1;
                cursor = open + 2;
            }
            _ => {
                if depth == 0 {
                    return Some(close_at);
                }
                depth -= 1;
                cursor = close_at + 2;
            }
        }
    }
    first_close
}

/// Find every `<<...>>` placeholder body embedded in `text`, in order.
///
/// Unterminated `<<...` runs are ignored and left to callers as literal text.
/// The returned slices exclude the surrounding `<<` / `>>` delimiters, and a
/// nested `<<...>>` (e.g. a `?upstream=<<default>>` sentinel inside a wrapped
/// worktree marker) stays part of the outer body -- see [`placeholder_close`].
#[must_use]
pub fn text_placeholders(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut offset = 0usize;
    while let Some(open_rel) = text[offset..].find("<<") {
        let open = offset + open_rel;
        let body_start = open + 2;
        let Some(close) = placeholder_close(text, body_start) else {
            break;
        };
        out.push(&text[body_start..close]);
        offset = close + 2;
    }
    out
}

/// The first worktree placeholder found in `text`, either as the whole string
/// itself (`ralphus:new-worktree/...`) or wrapped inside `<<...>>`.
///
/// This lenient (bare-or-wrapped) matching is for daemon-side resolution of
/// already-submitted data, which must keep resolving whatever shape a cell's
/// `cwd` was recorded in. Newly authored TOML is held to a stricter rule:
/// `core::validate` rejects a bare (unwrapped) placeholder cwd and requires
/// the wrapped `<<...>>` form, which is also the only form
/// `cli/src/tutor.rs` teaches -- it is the only shape that still composes
/// when the same marker needs to be repeated inside another field (e.g. an
/// `environment` value reusing a cell's worktree path).
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

/// Valid values for `[[review]] proof_scope` and the CLI's `ralphus review
/// settings --proof-scope`: which branches actually run their proof steps
/// during a Guardian merge. See [`ReviewDef::proof_scope`].
pub const PROOF_SCOPE_EACH_BRANCH: &str = "each_branch";
pub const PROOF_SCOPE_FINAL_BRANCH: &str = "final_branch";
pub const PROOF_SCOPE_NOTHING: &str = "nothing";

/// Every accepted `proof_scope` literal, in the order shown to a user.
pub const PROOF_SCOPE_VALUES: &[&str] = &[
    PROOF_SCOPE_EACH_BRANCH,
    PROOF_SCOPE_FINAL_BRANCH,
    PROOF_SCOPE_NOTHING,
];

/// If `id` is a new-review placeholder (`ralphus:new-review/<key>`), return its
/// `<key>` trimmed of surrounding whitespace. Returns `None` for a plain id or a
/// non-matching scheme, or when the key is empty.
#[must_use]
pub fn review_link_key(id: &str) -> Option<&str> {
    id.strip_prefix(REVIEW_LINK_PREFIX)
        .map(str::trim)
        .filter(|k| !k.is_empty())
}

/// Sentinel prefix for `review = "<<review:<id>>>"`: wraps a plain,
/// already-declared `[[review]].id` reference. Mirrors
/// [`UPSTREAM_TASK_REF_PREFIX`]'s `<<task:...>>` form.
pub const REVIEW_REF_PREFIX: &str = "<<review:";

/// Parse a `[[task.cell]].review` value (RAL-269). The value MUST be wrapped
/// in `<<...>>` -- a bare, unwrapped string is rejected (see
/// `core/src/validate.rs`'s `check_review`) -- either as `<<review:<id>>>`
/// naming an existing `[[review]].id` literally, or as
/// `<<ralphus:new-review/<key>>>` wrapping the new-review placeholder (see
/// [`REVIEW_LINK_PREFIX`]). Returns the unwrapped id/placeholder -- the value
/// to match against `[[review]].id` -- or `None` for a bare/unwrapped or
/// malformed value.
#[must_use]
pub fn parse_cell_review_sentinel(review: &str) -> Option<&str> {
    if let Some(inner) = review
        .strip_prefix(REVIEW_REF_PREFIX)
        .and_then(|s| s.strip_suffix(">>"))
    {
        return (!inner.is_empty()).then_some(inner);
    }
    let inner = review.strip_prefix("<<")?.strip_suffix(">>")?;
    review_link_key(inner).is_some().then_some(inner)
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

/// Whether cross-cell session sharing is enabled for a cell: its own
/// [`CellDef::share_session`], else the owning task's
/// [`TaskDef::share_session`], else `false` (off by default).
#[must_use]
pub fn resolve_cell_share_session(task: &TaskDef, cell: &CellDef) -> bool {
    cell.share_session.or(task.share_session).unwrap_or(false)
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

/// The effective `maximum_tool_output_tokens` for a cell: its own value, else
/// the owning task's, else `None` (no cap). Mirrors [`resolve_cell_machine`]'s
/// cell-then-task inheritance shape (RAL-333).
#[must_use]
pub fn resolve_cell_maximum_tool_output_tokens(task: &TaskDef, cell: &CellDef) -> Option<u64> {
    cell.maximum_tool_output_tokens
        .or(task.maximum_tool_output_tokens)
}

/// The effective `maximum_tool_output_tokens` for a task-scope proof step (one
/// with no owning cell): the step's own value, else the task's (RAL-333).
#[must_use]
pub fn resolve_task_proof_maximum_tool_output_tokens(
    task: &TaskDef,
    proof: &ProofStep,
) -> Option<u64> {
    proof
        .maximum_tool_output_tokens
        .or(task.maximum_tool_output_tokens)
}

/// The effective `maximum_tool_output_tokens` for a cell-scope proof step: the
/// step's own value, else the owning cell's resolved value (which itself
/// falls back to the task's) (RAL-333).
#[must_use]
pub fn resolve_cell_proof_maximum_tool_output_tokens(
    task: &TaskDef,
    cell: &CellDef,
    proof: &ProofStep,
) -> Option<u64> {
    proof
        .maximum_tool_output_tokens
        .or_else(|| resolve_cell_maximum_tool_output_tokens(task, cell))
}

/// A top-level review (guardian) declaration via `[[review]]`.
///
/// Cells opt in by declaring `review = "<<review:<id>>>"`. When several cells resolve
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
    /// `RALPHUS_RESOLVER_AGENT` env override, then the project-level
    /// `.ralphus.toml [review] default_resolver_agent` setting, then `ollama`.
    #[serde(default)]
    pub agent: Option<String>,
    /// Model the resolver `agent` runs, e.g. `"qwen3:8b"`. Unset falls back to
    /// the `RALPHUS_RESOLVER_MODEL` env override, then the project-level
    /// `.ralphus.toml [review] default_resolver_model` setting, then
    /// `qwen3:8b` for the ollama backend (other backends take their own
    /// default).
    #[serde(default)]
    pub model: Option<String>,
    /// The branch this review's stack rebases onto, declared rather than
    /// discovered (RAL-185).
    ///
    /// A local review normally infers its upstream from each contributing
    /// worktree's git upstream. That read only works on the machine holding
    /// the worktree, so a review fed by a **remote** cell must state its
    /// upstream here — the daemon has no way to look it up across machines,
    /// and guessing the project's default branch would silently produce a
    /// review against the wrong upstream.
    ///
    /// Optional for an all-local review, where inference still applies and
    /// this simply overrides it.
    #[serde(default)]
    pub upstream: Option<String>,
    /// The machine this review's worktrees and merge live on (RAL-185),
    /// written `scheme:uri`. Independent of any task's machine — a review may
    /// run somewhere none of its contributing tasks did. Unset inherits the
    /// project-level `.ralphus.toml [review] default_machine` setting, then
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
    /// fails it. Unset inherits the project-level `.ralphus.toml [review]
    /// default_maximum_budget_usd` setting, then unbounded.
    #[serde(default)]
    pub maximum_budget_usd: Option<f64>,
    /// This review's own Proof-scope override: which branches run their
    /// proof steps during the Guardian merge -- one of
    /// [`PROOF_SCOPE_EACH_BRANCH`]/[`PROOF_SCOPE_FINAL_BRANCH`]/
    /// [`PROOF_SCOPE_NOTHING`] (see [`PROOF_SCOPE_VALUES`]). Unset inherits
    /// the project-level `.ralphus.toml [review] default_proof_scope`
    /// setting, then `"each_branch"`. Equivalent to setting it later via `ralphus review
    /// settings <selector> --proof-scope <value>`, but declared up front so
    /// the review is created with the right scope from its first merge
    /// rather than needing a follow-up command.
    #[serde(default)]
    pub proof_scope: Option<String>,
    /// RAL-317: this review's own override for whether the PR stack is
    /// auto-submitted/grown as each branch reaches a terminal merge state,
    /// instead of requiring the manual "Pull in PR feedback"/`review pr
    /// submit` action. Unset inherits the project-level `.ralphus.toml
    /// [review] auto_submit_pr_stack` default, then `false`. Equivalent to
    /// setting it later via `ralphus review settings <selector>
    /// --auto-submit-pr-stack`, but declared up front so the review is
    /// created with the right behavior from its first merge rather than
    /// needing a follow-up command.
    #[serde(default)]
    pub auto_submit_pr_stack: Option<bool>,
    /// Whether this review skips per-branch worktrees. An explicit value wins
    /// over the project-level `.ralphus.toml [review] skip_worktrees` default.
    #[serde(default)]
    pub skip_worktrees: Option<bool>,
    /// Whether this review automatically incorporates PR feedback comments.
    #[serde(default)]
    pub auto_pr_feedback: Option<bool>,
    /// Whether this review skips automatic base-branch update rebuilds.
    #[serde(default)]
    pub skip_base_updates: Option<bool>,
    /// Whether each-branch proof runs skip auto-clean branches. This requires
    /// `proof_scope = "each_branch"`.
    #[serde(default)]
    pub skip_auto_clean: Option<bool>,
    /// Whether submitted PRs use the exact worktree branch name rather than
    /// the convention-derived alias.
    #[serde(default)]
    pub match_pr_branch_name: Option<bool>,
    /// Whether this review pushes its PR to a branch separate from its review
    /// branch.
    #[serde(default)]
    pub separate_pr_branch: Option<bool>,
    /// User-declared test actions shown as labelled buttons in the board UI.
    #[serde(default)]
    pub action: Vec<ReviewActionDef>,
    /// This review's own declared build steps (RAL-342), run at merge/finalize
    /// time ahead of the project-level `.ralphus.toml [review] auto_build`
    /// default. Mutually exclusive with `skip_auto_build`. When multiple
    /// entries are present, they run in order.
    ///
    /// Submitting a task TOML that creates a `[[review]]` must set exactly one
    /// of `auto_build`, `skip_auto_build`, or rely on a project-level
    /// `.ralphus.toml [review] auto_build` default covering every project the
    /// review spans -- the daemon enforces this at submit time (`core`
    /// validation only checks this table's own shape; see
    /// [`AutoBuildDef`]).
    #[serde(default)]
    pub auto_build: Vec<AutoBuildDef>,
    /// Declares that this review deliberately has no build step. Mutually
    /// exclusive with `auto_build`; satisfies the submit-time requirement
    /// that every review say something about how (or whether) it builds.
    #[serde(default)]
    pub skip_auto_build: bool,
    /// RAL-395: whether this review automatically dispatches its agent to
    /// fix a failing PR/MR CI status, instead of leaving the failure for a
    /// human to notice and action manually. Unset inherits the
    /// project-level `.ralphus.toml [review] auto_fix_pr_errors` default,
    /// then `false`. Auto-created reviews (Arbiter/Triage) always use the
    /// project default and never set this directly, since they have no
    /// `[[review]]` block to read it from.
    #[serde(default)]
    pub auto_fix_pr_errors: Option<bool>,
    /// RAL-395: this review's own override of the prompt template handed to
    /// the resolver agent when `auto_fix_pr_errors` fires. Must contain the
    /// literal `<<prompt>>` placeholder, which is replaced with the
    /// concatenated prompts of every Cell attached to the specific branch
    /// whose PR failed (validated in `ralphus_core::validate`). Unset
    /// inherits the project-level `.ralphus.toml [review]
    /// auto_fix_prompt_template` default, then a built-in default template.
    /// Auto-created reviews (Arbiter/Triage) always use the project default
    /// and never set this directly, since they have no `[[review]]` block
    /// to read it from.
    #[serde(default)]
    pub auto_fix_prompt_template: Option<String>,
}

/// This review's own declared build step (RAL-342): either a static
/// `command` (run verbatim), or an agent-invocation shape (`prompt` plus
/// optionally `system_prompt`/`system_prompt_position`/`agent`/`model`) for
/// build shapes not knowable up front -- e.g. novel work where "what does
/// building this even mean" must be figured out by inspecting the worktree.
/// Exactly one of `command` or `prompt` must be set; the agent-invocation
/// fields are only meaningful alongside `prompt`. Unlike the old AI-guessed
/// build tier this replaces (RAL-110), a declared `auto_build` is
/// user-authored: it must be provided (or explicitly skipped via
/// [`ReviewDef::skip_auto_build`]) at submit time rather than inferred at
/// merge time.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct AutoBuildDef {
    /// Verbatim shell command run directly, no LLM involvement. Mutually
    /// exclusive with `prompt`.
    #[serde(default)]
    pub command: Option<String>,
    /// Prompt forwarded to a headless agent call to figure out and perform
    /// the build. Mutually exclusive with `command`.
    #[serde(default)]
    pub prompt: Option<String>,
    /// Appended system prompt for the agent call (only valid alongside
    /// `prompt`); see [`ReviewDef`]'s neighbors for the same convention.
    #[serde(default)]
    pub system_prompt: Option<String>,
    /// Must be [`SYSTEM_PROMPT_POSITION_APPEND`] when set (only valid
    /// alongside `prompt`).
    #[serde(default)]
    pub system_prompt_position: Option<String>,
    /// Backend for the agent call, e.g. `"claude"` (only valid alongside
    /// `prompt`). Unset falls back to this step's parent [`ReviewDef`]'s agent,
    /// then the daemon's default resolver agent.
    #[serde(default)]
    pub agent: Option<String>,
    /// Model the agent call runs (only valid alongside `prompt`). Unset falls
    /// back to this step's parent [`ReviewDef`]'s model, then the daemon's default.
    #[serde(default)]
    pub model: Option<String>,
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
    /// Per-proof-step cap on how many tokens a single tool-call output may
    /// inject into the agent's context (RAL-333). Falls back to the owning
    /// cell's resolved value for a cell-scope step, or the task's for a
    /// task-scope one -- see [`resolve_task_proof_maximum_tool_output_tokens`]/
    /// [`resolve_cell_proof_maximum_tool_output_tokens`]. See
    /// [`TaskDef::maximum_tool_output_tokens`] for the delivery mechanism.
    #[serde(default)]
    pub maximum_tool_output_tokens: Option<u64>,
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
/// `ralphus agent list` / `cli/src/agents.rs` and `PROFILE_BACKENDS` in
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

/// Whether `agent` is a backend with a real delivery mechanism for
/// `maximum_context` -- i.e. a way to actually cap the context-window
/// ceiling itself (RAL-304).
///
/// The Codex CLI (`codex`/`codex-cli`) maps it to
/// `-c model_context_window=...`; Pi maps it to a `models.json`
/// `providers.<provider>.modelOverrides.<model-id>.contextWindow` override
/// (requiring a `"<provider>/<model-id>"` resolved model). Every other
/// backend has no such mechanism, so validation rejects `maximum_context`
/// for it (mirrors [`agent_supports_system_prompt`]'s RAL-5 precedent).
///
/// The Claude Code CLI (`claude-code`/`claude-cli`) is deliberately absent:
/// its only related lever, `CLAUDE_CODE_MAX_OUTPUT_TOKENS`, reserves
/// output-generation budget out of the same fixed context window rather
/// than bounding the window itself, so there is no real delivery mechanism
/// to accept this field for. See [`agent_supports_auto_compact_threshold`]
/// for the separate (wider) set of backends that accept
/// `auto_compact_threshold`.
///
/// On the runner side, each supported backend overrides
/// `ModelBackend::supports_maximum_context` to match this set -- the two
/// checks are independent (`core` cannot see `runner`'s trait impls) and
/// must be kept in sync by hand.
#[must_use]
pub fn agent_supports_maximum_context(agent: &str) -> bool {
    matches!(agent, "codex" | "codex-cli" | "pi")
}

/// Whether `agent` is a backend with a real delivery mechanism for
/// `auto_compact_threshold` -- an absolute token count at which
/// auto-compaction should trigger (RAL-304).
///
/// The Codex CLI (`codex`/`codex-cli`) maps it to
/// `-c model_auto_compact_token_limit=...`; Pi maps it to `settings.json`'s
/// `compaction.reserveTokens` (only when `maximum_context` is also set,
/// since `reserveTokens` is a buffer computed against that ceiling, not an
/// absolute threshold -- see [`agent_supports_maximum_context`]); the Claude
/// Code CLI (`claude-code`/`claude-cli`) maps it directly to the
/// `CLAUDE_CODE_AUTO_COMPACT_WINDOW` env var, which (unlike Pi) already
/// takes a plain absolute token count, so no accompanying `maximum_context`
/// is required. Every other backend has no such mechanism, so validation
/// rejects `auto_compact_threshold` for it.
///
/// On the runner side, each supported backend overrides
/// `ModelBackend::supports_auto_compact_threshold` to match this set -- the
/// two checks are independent (`core` cannot see `runner`'s trait impls) and
/// must be kept in sync by hand.
#[must_use]
pub fn agent_supports_auto_compact_threshold(agent: &str) -> bool {
    matches!(
        agent,
        "codex" | "codex-cli" | "pi" | "claude-code" | "claude-cli"
    )
}

/// Whether `agent` is a backend with a real delivery mechanism for
/// `maximum_tool_output_tokens` -- a cap on how many tokens a single tool-call
/// output may inject into the agent's context (RAL-333).
///
/// The Claude Code CLI (`claude-code`/`claude-cli`) maps it to the
/// `CLAUDE_CODE_FILE_READ_MAX_OUTPUT_TOKENS` env var (file-read tool output
/// specifically); the Codex CLI (`codex`/`codex-cli`) maps it to
/// `-c tool_output_token_limit=...` (any tool output, stored in history);
/// Pi maps it to a `models.json`
/// `providers.<provider>.modelOverrides.<model-id>.maxTokens` override
/// (requiring a `"<provider>/<model-id>"` resolved model, same requirement as
/// [`agent_supports_maximum_context`]). Every other backend has no such
/// mechanism, so validation rejects `maximum_tool_output_tokens` for it.
///
/// Ralphus never enforces this cap itself -- it only configures each
/// backend's own native mechanism and defers entirely to that backend's own
/// behavior once the cap is set.
///
/// On the runner side, each supported backend overrides
/// `ModelBackend::supports_maximum_tool_output_tokens` to match this set -- the
/// two checks are independent (`core` cannot see `runner`'s trait impls) and
/// must be kept in sync by hand.
#[must_use]
pub fn agent_supports_maximum_tool_output_tokens(agent: &str) -> bool {
    matches!(
        agent,
        "codex" | "codex-cli" | "pi" | "claude-code" | "claude-cli"
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
            maximum_context: None,
            auto_compact_threshold: None,
            maximum_tool_output_tokens: None,
            max_retries: None,
            priority: None,
            timeout_minutes: None,
            depends_on: vec![],
            environment: BTreeMap::new(),
            no_commit_required: false,
            share_session: None,
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
            maximum_context: None,
            auto_compact_threshold: None,
            timeout_minutes: None,
            priority: None,
            environment: BTreeMap::new(),
            proof: vec![],
            review: None,
            upstream: None,
            share_session: None,
            maximum_tool_output_tokens: None,
            triage: false,
            triage_type: None,
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
            review = "<<review:backend>>"

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
        assert_eq!(
            parsed.task[0].cell[0].review.as_deref(),
            Some("<<review:backend>>")
        );
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
    fn review_proof_scope_deserializes_and_defaults_to_none() {
        let toml = r#"
            [[task]]
            name = "t"
            [[task.cell]]
            cwd = "/repo/.wt/feat"
            prompt = "do work"
            review = "<<review:backend>>"

            [[review]]
            id = "backend"
            proof_scope = "final_branch"
        "#;
        let parsed: TaskFile = toml::from_str(toml).expect("should deserialize");
        assert_eq!(
            parsed.review[0].proof_scope.as_deref(),
            Some("final_branch")
        );

        let toml_unset = r#"
            [[task]]
            name = "t"
            [[task.cell]]
            cwd = "/repo/.wt/feat"
            prompt = "do work"
            review = "<<review:backend>>"

            [[review]]
            id = "backend"
        "#;
        let parsed_unset: TaskFile = toml::from_str(toml_unset).expect("should deserialize");
        assert_eq!(parsed_unset.review[0].proof_scope, None);
    }

    #[test]
    fn review_auto_build_command_form_deserializes() {
        let toml = r#"
            [[task]]
            name = "t"
            [[task.cell]]
            cwd = "/repo/.wt/feat"
            prompt = "do work"
            review = "<<review:backend>>"

            [[review]]
            id = "backend"
            [[review.auto_build]]
            command = "cargo build"
        "#;
        let parsed: TaskFile = toml::from_str(toml).expect("should deserialize");
        let def = parsed.review[0]
            .auto_build
            .first()
            .expect("auto_build should be set");
        assert_eq!(def.command.as_deref(), Some("cargo build"));
        assert!(def.prompt.is_none());
        assert!(!parsed.review[0].skip_auto_build);
    }

    #[test]
    fn review_auto_build_agent_form_deserializes() {
        let toml = r#"
            [[task]]
            name = "t"
            [[task.cell]]
            cwd = "/repo/.wt/feat"
            prompt = "do work"
            review = "<<review:backend>>"

            [[review]]
            id = "backend"
            [[review.auto_build]]
            prompt = "figure out how to build this and do it"
            system_prompt = "be thorough"
            system_prompt_position = "append"
            agent = "claude"
            model = "claude-opus-4-8"
        "#;
        let parsed: TaskFile = toml::from_str(toml).expect("should deserialize");
        let def = parsed.review[0]
            .auto_build
            .first()
            .expect("auto_build should be set");
        assert!(def.command.is_none());
        assert_eq!(
            def.prompt.as_deref(),
            Some("figure out how to build this and do it")
        );
        assert_eq!(def.system_prompt.as_deref(), Some("be thorough"));
        assert_eq!(def.system_prompt_position.as_deref(), Some("append"));
        assert_eq!(def.agent.as_deref(), Some("claude"));
        assert_eq!(def.model.as_deref(), Some("claude-opus-4-8"));
    }

    #[test]
    fn review_skip_auto_build_defaults_to_false() {
        let toml = r#"
            [[task]]
            name = "t"
            [[task.cell]]
            cwd = "/repo/.wt/feat"
            prompt = "do work"
            review = "<<review:backend>>"

            [[review]]
            id = "backend"
        "#;
        let parsed: TaskFile = toml::from_str(toml).expect("should deserialize");
        assert!(!parsed.review[0].skip_auto_build);
        assert!(parsed.review[0].auto_build.is_empty());

        let toml_skip = r#"
            [[task]]
            name = "t"
            [[task.cell]]
            cwd = "/repo/.wt/feat"
            prompt = "do work"
            review = "<<review:backend>>"

            [[review]]
            id = "backend"
            skip_auto_build = true
        "#;
        let parsed_skip: TaskFile = toml::from_str(toml_skip).expect("should deserialize");
        assert!(parsed_skip.review[0].skip_auto_build);
    }

    #[test]
    fn review_action_cleanup_command_and_input_deserialize() {
        let toml = r#"
            [[task]]
            name = "t"
            [[task.cell]]
            cwd = "/repo/.wt/feat"
            prompt = "do work"
            review = "<<review:backend>>"

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
            review = "<<review:backend>>"

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
    fn triage_type_deserializes_bare_string_as_single_element_list() {
        let toml = r#"
            [[task]]
            name = "t"
            project = "p"
            [[task.cell]]
            cwd = "/repo"
            prompt = "do work"
            triage = true
            triage_type = "security"
        "#;
        let parsed: TaskFile = toml::from_str(toml).expect("should deserialize");
        assert_eq!(
            parsed.task[0].cell[0].triage_type.as_deref(),
            Some(["security".to_string()].as_slice())
        );
    }

    #[test]
    fn triage_type_deserializes_array_of_strings() {
        let toml = r#"
            [[task]]
            name = "t"
            project = "p"
            [[task.cell]]
            cwd = "/repo"
            prompt = "do work"
            triage = true
            triage_type = ["bug", "investigation"]
        "#;
        let parsed: TaskFile = toml::from_str(toml).expect("should deserialize");
        assert_eq!(
            parsed.task[0].cell[0].triage_type.as_deref(),
            Some(["bug".to_string(), "investigation".to_string()].as_slice())
        );
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
    fn auto_compact_threshold_supported_by_claude_code_but_not_maximum_context() {
        assert!(agent_supports_auto_compact_threshold("claude-code"));
        assert!(agent_supports_auto_compact_threshold("claude-cli"));
        assert!(!agent_supports_maximum_context("claude-code"));
        assert!(!agent_supports_maximum_context("claude-cli"));
    }

    #[test]
    fn auto_compact_threshold_and_maximum_context_both_supported_by_codex_and_pi() {
        for agent in ["codex", "codex-cli", "pi"] {
            assert!(agent_supports_auto_compact_threshold(agent), "{agent}");
            assert!(agent_supports_maximum_context(agent), "{agent}");
        }
    }

    #[test]
    fn auto_compact_threshold_rejected_for_bare_claude_and_ollama() {
        assert!(!agent_supports_auto_compact_threshold("claude"));
        assert!(!agent_supports_auto_compact_threshold("ollama"));
    }

    #[test]
    fn maximum_tool_output_tokens_supported_by_claude_code_codex_and_pi() {
        for agent in ["claude-code", "claude-cli", "codex", "codex-cli", "pi"] {
            assert!(agent_supports_maximum_tool_output_tokens(agent), "{agent}");
        }
    }

    #[test]
    fn maximum_tool_output_tokens_rejected_for_bare_claude_ollama_and_raw() {
        assert!(!agent_supports_maximum_tool_output_tokens("claude"));
        assert!(!agent_supports_maximum_tool_output_tokens("anthropic"));
        assert!(!agent_supports_maximum_tool_output_tokens("ollama"));
        assert!(!agent_supports_maximum_tool_output_tokens("raw"));
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
    fn cell_review_sentinel_matches_plain_id() {
        assert_eq!(
            parse_cell_review_sentinel("<<review:backend>>"),
            Some("backend")
        );
    }

    #[test]
    fn cell_review_sentinel_matches_new_review_placeholder() {
        assert_eq!(
            parse_cell_review_sentinel("<<ralphus:new-review/ral-batch>>"),
            Some("ralphus:new-review/ral-batch")
        );
    }

    #[test]
    fn cell_review_sentinel_none_for_bare_or_malformed() {
        assert_eq!(parse_cell_review_sentinel("backend"), None);
        assert_eq!(
            parse_cell_review_sentinel("ralphus:new-review/ral-batch"),
            None
        );
        assert_eq!(parse_cell_review_sentinel("<<review:>>"), None);
        assert_eq!(parse_cell_review_sentinel("<<unknown>>"), None);
        assert_eq!(parse_cell_review_sentinel(""), None);
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
    fn text_placeholders_keeps_a_nested_sentinel_inside_the_outer_body() {
        assert_eq!(
            text_placeholders("<<ralphus:new-worktree/feat?upstream=<<default>>>>"),
            vec!["ralphus:new-worktree/feat?upstream=<<default>>"]
        );
        assert_eq!(
            text_placeholders(
                "a <<ralphus:new-worktree/feat?upstream=<<current_branch>>>> b <<task:x>> c"
            ),
            vec![
                "ralphus:new-worktree/feat?upstream=<<current_branch>>",
                "task:x"
            ]
        );
        // An unbalanced inner `<<` still terminates at the first `>>`, so
        // stray literal text behaves exactly as it did before nesting.
        assert_eq!(text_placeholders("<<a<<b>>"), vec!["a<<b"]);
        assert_eq!(text_placeholders("<<a<<b>>c"), vec!["a<<b"]);
    }

    #[test]
    fn first_worktree_placeholder_in_text_finds_a_nested_sentinel_form() {
        let wrapped = "<<ralphus:new-worktree/feat?upstream=<<default>>>>";
        assert_eq!(
            first_worktree_placeholder_in_text(wrapped),
            Some("ralphus:new-worktree/feat?upstream=<<default>>")
        );
        assert_eq!(
            first_worktree_placeholder_in_text(wrapped)
                .and_then(parse_worktree_placeholder_upstream),
            Some(WORKTREE_UPSTREAM_DEFAULT)
        );
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
    fn cell_maximum_tool_output_tokens_overrides_task_and_unset_falls_through_to_none() {
        let toml = r#"
            [[task]]
            name = "t"
            maximum_tool_output_tokens = 1000
            [[task.cell]]
            cwd = "/tmp"
            command = "x"
            [[task.cell]]
            cwd = "/tmp"
            command = "y"
            maximum_tool_output_tokens = 500
        "#;
        let parsed: TaskFile = toml::from_str(toml).expect("should deserialize");
        let task = &parsed.task[0];
        assert_eq!(
            resolve_cell_maximum_tool_output_tokens(task, &task.cell[0]),
            Some(1000),
            "an unset cell value inherits the task's"
        );
        assert_eq!(
            resolve_cell_maximum_tool_output_tokens(task, &task.cell[1]),
            Some(500),
            "an explicit cell value wins"
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
            resolve_cell_maximum_tool_output_tokens(task, &task.cell[0]),
            None,
            "no cap anywhere means no cap, represented as None"
        );
    }

    #[test]
    fn proof_maximum_tool_output_tokens_inherits_from_its_owner() {
        let toml = r#"
            [[task]]
            name = "t"
            maximum_tool_output_tokens = 1000
            [[task.cell]]
            cwd = "/tmp"
            command = "x"
            maximum_tool_output_tokens = 2000
            [[task.cell.proof]]
            command = "cargo test"
            [[task.proof]]
            command = "cargo fmt"
        "#;
        let parsed: TaskFile = toml::from_str(toml).expect("should deserialize");
        let task = &parsed.task[0];
        let cell = &task.cell[0];
        assert_eq!(
            resolve_cell_proof_maximum_tool_output_tokens(task, cell, &cell.proof[0]),
            Some(2000),
            "a cell-scope proof follows its cell, not the task"
        );
        assert_eq!(
            resolve_task_proof_maximum_tool_output_tokens(task, &task.proof[0]),
            Some(1000),
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
