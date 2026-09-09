//! SQLite-backed task store — the daemon's authoritative state.
//!
//! Only the daemon opens this database (WAL mode); the CLI and librarian reach
//! it through the HTTP API. On submission a task file is fully ingested into
//! these tables, so the database — not any on-disk TOML — is the source of truth
//! (the predecessor learned this the hard way; see `FINDINGS.local.md` §2.4 and
//! CCTL-149).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use ralphus_core::schema::{ResolvedAgent, TaskFile};
use rusqlite::{Connection, OptionalExtension, named_params, params};
use serde::Serialize;

use crate::runner::{effective_cell_system_prompt, effective_proof_system_prompt};

/// Errors the store can produce.
#[derive(Debug)]
pub enum StoreError {
    /// An underlying rusqlite error.
    Sqlite(rusqlite::Error),
    /// The requested squad does not exist.
    NotFound,
    /// The operation is not valid for the squad's current state.
    InvalidTransition(String),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Sqlite(e) => write!(f, "database error: {e}"),
            Self::NotFound => write!(f, "squad not found"),
            Self::InvalidTransition(m) => write!(f, "invalid transition: {m}"),
        }
    }
}

impl std::error::Error for StoreError {}

impl From<rusqlite::Error> for StoreError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Sqlite(e)
    }
}

/// Convenient result alias.
pub type Result<T> = std::result::Result<T, StoreError>;

/// A `(squad_id, task_idx, cell_idx)` triple identifying one cell, used to
/// key batched per-cell lookups such as [`Store::resolve_cell_env_overrides_batch`].
pub type CellRef = (String, i64, i64);

/// Lifecycle state of a whole squad (submission). See `docs/daemon-api.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SquadState {
    /// Staged but not scheduled (explicit hold).
    Queued,
    /// Schedulable; waiting for the scheduler / dependencies.
    Pending,
    /// Currently executing.
    Running,
    /// Finished successfully.
    Done,
    /// Finished with a failure.
    Failed,
    /// Cancelled by a user.
    Cancelled,
    /// Manually skipped by a user (RAL Queue). Not scheduled, but — unlike
    /// `cancelled` — it satisfies downstream dependencies exactly like `done`.
    /// Reversible: a user can set an ignored squad back to `pending`.
    Ignored,
}

impl SquadState {
    /// The stable lowercase string stored in the database.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Ignored => "ignored",
        }
    }

    /// Parse the stable lowercase state string, or `None` if unrecognized.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "queued" => Self::Queued,
            "pending" => Self::Pending,
            "running" => Self::Running,
            "done" => Self::Done,
            "failed" => Self::Failed,
            "cancelled" => Self::Cancelled,
            "ignored" => Self::Ignored,
            _ => return None,
        })
    }

    /// Whether this is a terminal state (no further transitions). `ignored` is
    /// deliberately NOT terminal — it is reversible back to `pending`.
    #[must_use]
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Done | Self::Failed | Self::Cancelled)
    }

    /// Whether this state satisfies a downstream dependency. `done` and
    /// `ignored` both unblock dependents; every other state does not.
    #[must_use]
    pub fn satisfies_dependents(self) -> bool {
        matches!(self, Self::Done | Self::Ignored)
    }
}

/// Execution state of a cell or a task node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeState {
    /// Not started.
    Pending,
    /// Running.
    Running,
    /// Completed successfully.
    Done,
    /// Failed.
    Failed,
    /// Cancelled.
    Cancelled,
    /// Manually skipped by a user (RAL Queue). Not executed, but satisfies
    /// downstream dependencies exactly like `done`. Reversible back to `pending`.
    Ignored,
}

impl NodeState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Ignored => "ignored",
        }
    }

    /// Parse the stable lowercase state string, or `None` if unrecognized.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "pending" => Self::Pending,
            "running" => Self::Running,
            "done" => Self::Done,
            "failed" => Self::Failed,
            "cancelled" => Self::Cancelled,
            "ignored" => Self::Ignored,
            _ => return None,
        })
    }

    /// Whether this state satisfies a downstream dependency: `done` and
    /// `ignored` both unblock dependents; everything else does not.
    #[must_use]
    pub fn satisfies_dependents(self) -> bool {
        matches!(self, Self::Done | Self::Ignored)
    }

    /// Whether this is a terminal state (no further transitions expected).
    /// `ignored` is deliberately NOT terminal — it is reversible back to
    /// `pending`, mirroring [`SquadState::is_terminal`].
    #[must_use]
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Done | Self::Failed | Self::Cancelled)
    }
}

// ── Read views (serialized straight to the API) ──────────────────────────────

/// One row from [`Store::proof_specs`]:
/// `(idx, kind, spec, model, timeout_sec, budget_tokens, maximum_tool_output_tokens)`.
pub type ProofSpecRow = (
    i64,
    String,
    String,
    Option<String>,
    Option<i64>,
    Option<i64>,
    Option<i64>,
);

/// A proof step as shown in the board.
#[derive(Debug, Clone, Serialize)]
pub struct ProofView {
    /// Optional step id.
    pub id: Option<String>,
    /// One of `command` / `brain` / `prompt` / `approval`.
    pub kind: String,
    /// Current state string.
    pub state: String,
    /// Captured command output, once the proof step has run (CCTL-99).
    pub output: Option<String>,
    /// The step definition: command text, prompt text, or empty for brain/approval.
    pub spec: String,
    /// Read-only effective system prompt actually appended to this agent
    /// invocation. Omitted for command/brain/approval kinds and for rows
    /// created before RAL-180 first populated it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    /// Model override (meaningful for `prompt`-kind steps).
    pub model: Option<String>,
    /// Resolved tool-output token cap (RAL-333): this step's own value, or
    /// inherited from its owning cell/task, or `None` for no cap. See
    /// `ralphus_core::schema::agent_supports_maximum_tool_output_tokens` for
    /// which backends accept this.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub maximum_tool_output_tokens: Option<i64>,
    /// Resolved agent program (inherited from the owning cell or task defaults).
    pub agent: String,
    /// Resumable CLI-agent cell/thread id captured when the step ran via a
    /// CLI backend with a resume mechanism (claude-code, codex). `None` for
    /// other agents or steps that have not yet run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_session_id: Option<String>,
    /// Input tokens used by this step's most recent run.
    pub tokens_in: i64,
    /// Output tokens used by this step's most recent run.
    pub tokens_out: i64,
    /// RAL-326: prompt-cache *write* tokens -- input billed at the
    /// cache-creation rate, tracked separately so `tokens_in` keeps meaning
    /// exactly what it always has (uncached input). `0` for a backend whose
    /// harness reports no cache breakdown, and for every row written before
    /// RAL-326.
    pub cache_creation_tokens: i64,
    /// RAL-326: prompt-cache *read* tokens -- input served from an existing
    /// cache entry at the discounted rate. See `cache_creation_tokens`.
    pub cache_read_tokens: i64,
    /// RAL-373: total input tokens spent on Claude Code's own
    /// auto-compaction summarization requests -- billed at the *uncached*
    /// input rate, the reason `cost_usd` and
    /// `tokens_in + cache_creation_tokens + cache_read_tokens` diverge on
    /// any step that compacts. `0` for a backend that reports no
    /// compaction data (`pi`, `codex`) -- see `compaction_count` before
    /// reading that as "never compacted".
    pub compaction_input_tokens: i64,
    /// RAL-373: count of compactions observed, incremented independently of
    /// whether each one's input size was reported. A nonzero count paired
    /// with `compaction_input_tokens == 0` means "compactions happened,
    /// sizes unreported by this backend/version", not "no compaction
    /// happened".
    pub compaction_count: i64,
    /// Cost of this step's most recent run, USD.
    pub cost_usd: f64,
    /// RAL-326: `true` when `cost_usd` and the token counts are the last
    /// live mid-run snapshot rather than the backend's own terminal
    /// accounting (the step's process was lost, cancelled, timed out, or
    /// killed before a final usage event arrived).
    pub cost_is_estimated: bool,
    /// RAL-191: environment-variable overrides set on **this individual step**,
    /// the narrowest layer — merged on top of the owning scope's
    /// `proof_env_overrides` (and its ancestors) when the step runs. Empty for
    /// the vast majority of steps; omitted from the JSON when empty so the
    /// common board payload is unchanged.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub env_overrides: BTreeMap<String, String>,
    /// RAL-271: cosmetic "out of date" badge -- `true` once this step's own
    /// `env_overrides` (or its owning scope's `proof_env_overrides`) has been
    /// edited since the step last ran/retried or had its status explicitly
    /// set. No behavioral effect; see [`Store::set_proof_step_env_overrides`].
    #[serde(default)]
    pub env_out_of_date: bool,
}

/// A cell as shown in the board.
#[derive(Debug, Clone, Serialize)]
pub struct CellView {
    /// Cell id (or a generated `cell-N`).
    pub id: String,
    /// Human-readable display name (from `name` in the TOML). `None` when not set;
    /// the board falls back to `id` for display.
    pub name: Option<String>,
    /// Working directory.
    pub cwd: Option<String>,
    /// Resolved agent program.
    pub agent: String,
    /// Resolved model, if any.
    pub model: Option<String>,
    /// AI prompt, if a prompt cell.
    pub prompt: Option<String>,
    /// Shell command, if a command cell.
    pub command: Option<String>,
    /// Read-only effective system prompt actually appended to this agent
    /// invocation. Omitted for command cells and for rows created before
    /// RAL-180 first populated it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    /// Current state string.
    pub state: String,
    /// Input tokens recorded so far.
    pub tokens_in: i64,
    /// Output tokens recorded so far.
    pub tokens_out: i64,
    /// RAL-326: prompt-cache *write* tokens -- input billed at the
    /// cache-creation rate, tracked separately so `tokens_in` keeps meaning
    /// exactly what it always has (uncached input). `0` for a backend whose
    /// harness reports no cache breakdown, and for every row written before
    /// RAL-326.
    pub cache_creation_tokens: i64,
    /// RAL-326: prompt-cache *read* tokens -- input served from an existing
    /// cache entry at the discounted rate. See `cache_creation_tokens`.
    pub cache_read_tokens: i64,
    /// RAL-373: total input tokens spent on Claude Code's own
    /// auto-compaction summarization requests -- billed at the *uncached*
    /// input rate, the reason `cost_usd` and
    /// `tokens_in + cache_creation_tokens + cache_read_tokens` diverge on
    /// any cell that compacts. `0` for a backend that reports no
    /// compaction data (`pi`, `codex`) -- see `compaction_count` before
    /// reading that as "never compacted".
    pub compaction_input_tokens: i64,
    /// RAL-373: count of compactions observed, incremented independently of
    /// whether each one's input size was reported. A nonzero count paired
    /// with `compaction_input_tokens == 0` means "compactions happened,
    /// sizes unreported by this backend/version", not "no compaction
    /// happened".
    pub compaction_count: i64,
    /// Cost recorded so far, USD.
    pub cost_usd: f64,
    /// RAL-326: `true` when `cost_usd` and the token counts are the last
    /// live mid-run snapshot rather than the backend's own terminal
    /// accounting (the cell's process was lost, cancelled, timed out, or
    /// killed before a final usage event arrived).
    pub cost_is_estimated: bool,
    /// Resolved USD spend cap (cell overrides task), or `None` for no cap.
    /// Once `cost_usd` exceeds this the daemon kills the cell mid-run
    /// (RAL-161).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub maximum_budget_usd: Option<f64>,
    /// Resolved context-window token limit (cell overrides task), or `None`
    /// for no cap (RAL-304). See
    /// `ralphus_core::schema::agent_supports_maximum_context` for which
    /// backends accept this.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub maximum_context: Option<i64>,
    /// Resolved auto-compact trigger threshold in tokens (cell overrides
    /// task), or `None` for no explicit threshold (RAL-304). See
    /// `ralphus_core::schema::agent_supports_auto_compact_threshold` for
    /// which backends accept this -- a wider set than
    /// [`Self::maximum_context`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_compact_threshold: Option<i64>,
    /// Resolved tool-output token cap (cell overrides task), or `None` for no
    /// cap (RAL-333). See
    /// `ralphus_core::schema::agent_supports_maximum_tool_output_tokens` for
    /// which backends accept this.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub maximum_tool_output_tokens: Option<i64>,
    /// Failure detail, when the cell failed.
    pub error: Option<String>,
    /// Dependency references (within-task cell ids or `task/cell`).
    pub depends_on: Vec<String>,
    /// Cell-level proof steps (`[[task.cell.proof]]`), in order.
    pub proof: Vec<ProofView>,
    /// Reviews (guardians) this cell participates in — those whose stack
    /// includes the cell's review branch (RAL-17). Empty for most cells.
    pub reviews: Vec<SquadReviewRef>,
    /// This cell's resolved Triage type(s) (RAL-318), alphabetical, if it
    /// opted into Triage via `triage = true` -- empty for a cell that never
    /// opted in. Populated at submit time (inline `triage_type` or the
    /// Arbiter's own classification) whether or not the cell has run yet, so
    /// non-empty here does not by itself mean the cell is done -- check
    /// `state`. Lets the board show a "scheduled"/"queued for auto-review"
    /// placeholder for a Triage cell whose pool hasn't drained into an
    /// actual review yet (see `Self::reviews`, which is what shows once it
    /// has).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub triage_types: Vec<String>,
    /// Resumable CLI-agent cell/thread id (for `claude --resume`/`codex
    /// resume`), captured from the owning backend's output. `None` for other
    /// agents or cells that have not yet completed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_session_id: Option<String>,
    /// The `machine` this cell is routed to (RAL-185), if any -- `None`
    /// means the daemon's own host. Exposed so the board can tell a cell
    /// running on a remote machine provider (no local tmux session to
    /// detach yet, RAL-288) apart from one that simply hasn't started.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub machine: Option<String>,
    /// Persistent environment-variable overrides set directly on this cell
    /// (hierarchical env overrides, extending RAL-150): merged on top of the
    /// owning task's/squad's when the cell's own subprocess is spawned. See
    /// [`Store::resolve_cell_env_overrides`]. Empty for the vast majority
    /// of cells.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env_overrides: BTreeMap<String, String>,
    /// Persistent environment-variable overrides set on this cell's own
    /// proof steps only, merged on top of `env_overrides` (and its
    /// ancestors) when a cell-scoped proof step runs. See
    /// [`Store::resolve_cell_proof_env_overrides`].
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub proof_env_overrides: BTreeMap<String, String>,
    /// When this cell first entered `running` (Unix epoch milliseconds).
    /// `None` until it starts. Details-pane "started at" / "time running".
    pub started_at_ms: Option<i64>,
    /// When this cell last reached a terminal state (Unix epoch
    /// milliseconds). `None` while pending/running.
    pub finished_at_ms: Option<i64>,
    /// RAL-271: cosmetic "out of date" badge -- `true` once this cell's own
    /// `env_overrides` has been edited since the cell last ran/retried or had
    /// its status explicitly set. No behavioral effect; see
    /// [`Store::set_cell_env_overrides`].
    #[serde(default)]
    pub env_out_of_date: bool,
    /// RAL-288: when this cell was cleanly stopped ("Open Agent" on a
    /// still-running cell) for a real interactive agent session to take
    /// over. `None` while not detached. `state` stays `"running"`
    /// throughout — this is an additive signal so the board can tell
    /// "paused for a human" apart from "actively executing headlessly"
    /// without a new terminal `NodeState`. Cleared automatically the next
    /// time the cell is dispatched (a restart, or resume-automation).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detached_at_ms: Option<i64>,
}

/// A task as shown in the board.
#[derive(Debug, Clone, Serialize)]
pub struct TaskView {
    /// Task name.
    pub name: String,
    /// Project identifier: the registered project name when the task's TOML
    /// set `project`, otherwise a fallback derived from the task's first
    /// cell `cwd` (RAL-141, see [`fallback_project_identifier`]). Always
    /// present -- never `null` in the API response -- so a project filter
    /// facet has real data for every task.
    pub project: String,
    /// Raw task-level agent value from the submitted TOML, if any. `None`
    /// means the task left `agent` unset and cells inherit further or fall
    /// back to the built-in default.
    pub agent: Option<String>,
    /// Raw task-level model value from the submitted TOML, if any. `None`
    /// means the task left `model` unset.
    pub model: Option<String>,
    /// Current state string.
    pub state: String,
    /// Failure detail, for a task that failed for a task-level reason with
    /// no underlying cell/proof error to point to (RAL-291) -- e.g. the
    /// RAL-156 no-commits-since-baseline guard. Mirrors [`CellView::error`]'s
    /// shape and lifecycle exactly. `None` when the task hasn't failed this
    /// way, including when it failed because one of its own cells/proofs
    /// did (that failure is already visible on the cell/proof itself).
    pub error: Option<String>,
    /// Cells in the task.
    pub cells: Vec<CellView>,
    /// Task-level proof steps.
    pub proof: Vec<ProofView>,
    /// Task-level dependency references (other task names).
    pub depends_on: Vec<String>,
    /// Persistent environment-variable overrides set directly on this task
    /// (hierarchical env overrides, extending RAL-150): merged on top of the
    /// squad's, and merged onto every cell under this task. See
    /// [`Store::resolve_cell_env_overrides`]. Empty for the vast majority
    /// of tasks.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env_overrides: BTreeMap<String, String>,
    /// Persistent environment-variable overrides set on this task's own
    /// (task-scoped) proof steps only, merged on top of `env_overrides` (and
    /// the squad's) when a task-scoped proof step runs. See
    /// [`Store::resolve_task_proof_env_overrides`].
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub proof_env_overrides: BTreeMap<String, String>,
    /// Whether this task is "soloed" (RAL-157) — while `true` on any task in
    /// the squad, the scheduler only dispatches soloed tasks' cells; every
    /// other task's cells stay paused (Pending) until un-soloed.
    pub soloed: bool,
    /// When this task first entered `running` (Unix epoch milliseconds).
    /// `None` until it starts. Details-pane "started at" / "time running".
    pub started_at_ms: Option<i64>,
    /// When this task last reached a terminal state (Unix epoch
    /// milliseconds). `None` while pending/running.
    pub finished_at_ms: Option<i64>,
    /// RAL-271: cosmetic "out of date" badge -- `true` once this task's own
    /// `env_overrides` has been edited since the task last ran/retried or had
    /// its status explicitly set. No behavioral effect; see
    /// [`Store::set_task_env_overrides`].
    #[serde(default)]
    pub env_out_of_date: bool,
}

/// A lightweight reference to a review (guardian) derived from a squad.
#[derive(Debug, Clone, Serialize)]
pub struct SquadReviewRef {
    /// Guardian id.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Guardian status string.
    pub status: String,
    /// The specific branch in the review stack this cell contributes to.
    /// `None` for squad-level review refs (not tied to a branch).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// `"explicit"` (an authored `[[review]]`, or any other non-Triage
    /// creation path) or `"arbiter"` (RAL-318: created automatically when a
    /// Triage pool's count threshold or cron schedule fired). Mirrors
    /// `GuardianView::origin`.
    pub origin: String,
}

/// One entry in the execution/transition log (CCTL-99).
#[derive(Debug, Clone, Serialize)]
pub struct EventView {
    /// Entity scope: `squad` / `task` / `cell` / `proof` / `guardian` / `branch`.
    pub scope: String,
    /// A reference within the scope (e.g. `cell s1`, `b003`), if any.
    #[serde(rename = "ref")]
    pub reference: Option<String>,
    /// Human-readable transition/note.
    pub message: String,
    /// When it happened (Unix epoch milliseconds).
    pub at_ms: i64,
}

/// A squad (submission) as shown in the board.
#[derive(Debug, Clone, Serialize)]
pub struct SquadView {
    /// Squad id, e.g. `squad-000000000001`.
    pub id: String,
    /// Optional human label.
    pub label: Option<String>,
    /// Current squad state string. Not a raw passthrough of the `squads.state`
    /// column — see [`effective_squad_state`] — so this reflects whether the
    /// squad's children are actually doing something right now, even in the
    /// window where a targeted proof/cell restart has reset the column
    /// to `pending` but the squad's worker is still busy with unrelated
    /// sibling cells.
    pub state: String,
    /// Creation time (Unix epoch milliseconds) — when the squad was submitted/
    /// queued, which can differ from when it actually started executing.
    pub created_at_ms: i64,
    /// When this squad first entered `running` (Unix epoch milliseconds).
    /// `None` until it starts. Details-pane "started at" / "time running".
    pub started_at_ms: Option<i64>,
    /// When this squad last reached a terminal state (Unix epoch
    /// milliseconds). `None` while queued/pending/running.
    pub finished_at_ms: Option<i64>,
    /// The tasks in the squad.
    pub tasks: Vec<TaskView>,
    /// Reviews (guardians) derived from this squad.
    pub reviews: Vec<SquadReviewRef>,
    /// Persistent environment-variable overrides applied to every subprocess
    /// spawned for this squad (RAL-150). Values here are the raw, unredacted
    /// overrides — safe to show in the board's squad detail view per the
    /// ticket's binding decision (only Cartographer/audit-log payloads mask
    /// non-allowlisted values, see `daemon::config::EnvOverridesConfig`).
    /// Empty for the vast majority of squads (no overrides ever set).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env_overrides: BTreeMap<String, String>,
}

/// A node in the cross-squad `[[default]] depends_on` gating graph
/// (CLI_PARITY_PLAN.local.md Phase 6, `ralphus graph --global`).
#[derive(Debug, Clone, Serialize)]
pub struct GlobalGraphNode {
    /// Squad id.
    pub id: String,
    /// Optional human label.
    pub label: Option<String>,
    /// Current squad state string.
    pub state: String,
}

/// The cross-squad gating graph: nodes are squads, edges are `[[default]]
/// depends_on` references (see [`Store::global_graph`]).
#[derive(Debug, Clone, Serialize)]
pub struct GlobalGraph {
    /// One entry per included squad.
    pub nodes: Vec<GlobalGraphNode>,
    /// `from` (the dependency squad) must complete before `to` (the dependent squad).
    pub edges: Vec<crate::plan::GraphEdge>,
}

/// A registered project (RAL-100): its on-disk location, VCS kind, and a
/// human description used for fuzzy lookup by name or description.
#[derive(Debug, Clone, Serialize)]
pub struct ProjectView {
    /// Unique project name.
    pub name: String,
    /// Human-readable description (also searched by fuzzy lookup).
    pub description: String,
    /// Absolute filesystem path to the project's root.
    pub path: String,
    /// Canonical clone URL used when a provider provisions this project on a
    /// different machine. Kept separate from `path`, which names this
    /// daemon host's existing checkout.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clone_url: Option<String>,
    /// VCS kind. Only `"git"` is implemented today.
    pub vcs: String,
    /// Registration time (Unix epoch milliseconds).
    pub created_at_ms: i64,
}

/// Outcome of a bulk [`Store::clear_all`].
pub struct ClearOutcome {
    /// Number of squads deleted (with their cells/tasks/proofs/events).
    pub squads_deleted: usize,
    /// Number of guardians (reviews) deleted, with their branches.
    pub guardians_deleted: usize,
    /// `(guardian_id, project_root)` pairs for each deleted guardian — one entry
    /// per project root — so the caller can purge all on-disk review worktrees.
    /// Multi-project guardians produce multiple entries with the same guardian_id.
    pub guardian_roots: Vec<(String, String)>,
    /// Ids of every squad actually deleted (RAL-154) — so the caller can purge
    /// `delete_squad` endpoint's cleanup.
    pub squad_ids: Vec<String>,
}

/// A persisted path that may be owned by a cell, proof, or review.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorktreeClaim {
    pub kind: String,
    pub owner: String,
    pub path: String,
    pub state: String,
}

/// One review-worktree path and the timestamp of its owner's last activity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GuardianWorktreeRecord {
    pub guardian_id: String,
    pub guardian_name: String,
    pub project_root: String,
    pub path: String,
    pub last_activity_ms: i64,
}

/// One durable retirement attempt (RAL-385, statuses widened by RAL-386): a
/// worktree whose removal was confirmed (`retired`), refused (`failed`, with
/// `error` carrying the failure), deferred by a machine provider's own policy
/// (`deferred`, with `error` carrying its reason and `retry_at_ms` its hint),
/// or declined outright (`opted_out`, with `error` carrying why). Every
/// status but `retired` is retried on the next daily sweep.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GuardianWorktreeRetirementRecord {
    pub guardian_id: String,
    pub path: String,
    pub status: String,
    pub error: Option<String>,
    pub eligible_at_ms: i64,
    pub last_attempt_ms: i64,
    pub retry_at_ms: Option<i64>,
}

// ── Store ────────────────────────────────────────────────────────────────────

/// The task store.
pub struct Store {
    pub(crate) conn: Connection,
    /// In-process SSE broadcast registry (RAL-167), fed by
    /// [`Store::cartographer_log`]. See `crate::events`.
    event_bus: crate::events::EventBus,
    /// Liveness signal (RAL-170): last time fresh pane output was observed
    /// for a running tmux-wrapped session, keyed by `crate::tmux::session_name`
    /// (the same deterministic key used for task cells, proof steps, and
    /// Guardian resolver/manual-check sessions alike — see
    /// [`Self::note_live_activity`]'s doc comment for why this is in-memory
    /// only, not a DB column). Entries are removed once the owning
    /// `run_via_tmux` call returns, so this stays bounded by the number of
    /// *currently running* tmux-wrapped sessions, not lifetime history.
    live_activity: HashMap<String, i64>,
    /// RAL-208: per-guardian debounce bookkeeping for the LLM-authored final
    /// change summary, keyed by guardian id. In-memory only, like
    /// `live_activity` above — losing this across a daemon restart just means
    /// the next enabled-branch-set change regenerates the summary once more
    /// than strictly necessary, not a correctness issue. See
    /// `guardian_merge::queue_final_summary_regen`/`sweep_pending_summaries`.
    guardian_summary_debounce: HashMap<String, GuardianSummaryDebounce>,
    /// Exclusive ownership of one review branch's mutable worktree, keyed by
    /// `(guardian_id, branch_id)` and valued by an owner tag (e.g.
    /// `feedback:{branch_id}`) — see the `worktree lease` glossary entry.
    /// `run_feedback` holds one for the duration of its resolver call so a
    /// concurrent restack can never rebase a branch out from under a
    /// still-running feedback pass. In-memory only: a daemon restart mid-lease
    /// simply drops it, which is safe since the worktree itself is re-checked
    /// for dirt on the next pass through `drive_rebase`.
    guardian_worktree_leases: HashMap<(String, String), String>,
    /// A pending restack request per guardian, coalesced to the lowest
    /// requested `from_position` — a later request for a *later* position
    /// while an earlier one is still queued would otherwise lose ground
    /// already claimed. Cleared by [`Store::try_claim_guardian_restack`].
    guardian_restack_requests: HashMap<String, i64>,
    /// Guardians with a restack currently claimed (running). A restack may
    /// only be claimed when no branch of the guardian holds a worktree lease,
    /// and no new worktree lease may be acquired while the guardian's id is
    /// in this set — see [`Store::try_claim_guardian_restack`] and
    /// [`Store::try_acquire_guardian_worktree_lease`].
    guardian_restack_running: std::collections::HashSet<String>,
    /// RAL-241: which `crate::tmux::session_name` keys have already had a
    /// stall escalation enqueued for their *current* stall onset, and when
    /// that onset's last-known-good activity timestamp was — so a still-
    /// ongoing stall doesn't re-enqueue a mailbox message on every poll.
    /// In-memory only, like `live_activity` above: cleared alongside it (see
    /// `Store::clear_live_activity`) once the owning `run_via_tmux` call has
    /// a terminal result, so a *new* attempt/cell can be escalated again.
    stall_escalated: HashMap<String, i64>,
    /// RAL-281: process-lifetime cache of `secret_env_names`, `None` when
    /// invalidated by a mutation. See `crate::secret_env_names`'s module doc
    /// comment for why this exists (the scheduler's per-cell/per-proof-step
    /// env-merge choke point reads it on every dispatch, so it must not cost
    /// a DB query per call). Scoped to this `Store` instance (not a global
    /// static) so it can't leak between the daemon's one real DB and the many
    /// independent in-memory stores each test opens.
    pub(crate) secret_env_names_cache:
        std::sync::RwLock<Option<std::collections::BTreeSet<String>>>,
}

/// RAL-208: see [`Store::guardian_summary_debounce`].
#[derive(Debug, Default, Clone)]
struct GuardianSummaryDebounce {
    /// The enabled-branch signature the current LLM-authored `change_summary`
    /// was generated from. `None` until the first final summary is produced.
    generated_signature: Option<String>,
    /// A signature awaiting generation, and when it was last (re)requested.
    /// Each new request overwrites both fields — that's what implements the
    /// trailing debounce: the "quiet period" clock restarts on every
    /// enable/disable toggle instead of accumulating separate pending jobs.
    pending_signature: Option<String>,
    pending_requested_at_ms: Option<i64>,
    /// RAL-303: whether this daemon process has already tried to repair a
    /// guardian left with no LLM-authored summary. See
    /// [`Self::claim_final_summary_repair`].
    repair_attempted: bool,
}

pub(crate) fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// Serialize a string list to the JSON stored in the DB.
pub(crate) fn to_json(v: &[String]) -> String {
    serde_json::to_string(v).unwrap_or_else(|_| "[]".to_string())
}

/// Parse a string list from stored JSON, defaulting to empty on error.
pub(crate) fn from_json(s: &str) -> Vec<String> {
    serde_json::from_str(s).unwrap_or_default()
}

/// Serialize a string-to-string map to the JSON stored in the DB (RAL-150:
/// `squads.env_overrides`). `BTreeMap` gives deterministic key order, which
/// keeps Cartographer payloads and API responses stable across calls.
pub(crate) fn to_json_map(v: &BTreeMap<String, String>) -> String {
    serde_json::to_string(v).unwrap_or_else(|_| "{}".to_string())
}

/// Parse a string-to-string map from stored JSON, defaulting to empty on error.
pub(crate) fn from_json_map(s: &str) -> BTreeMap<String, String> {
    serde_json::from_str(s).unwrap_or_default()
}

/// RAL-230: restrict the DB file (and, if already present, its WAL/SHM
/// siblings) to owner-only `0o600` on every open -- re-tightening a
/// pre-existing, looser-permissioned file the same way [`crate::state_dir`]
/// re-tightens the containing directory, rather than leaving an old file's
/// permissions untouched. The WAL/SHM files may not exist yet at this point
/// (SQLite creates them lazily) -- each is skipped if absent, and later
/// `Store::open` calls against the same path (e.g. daemon restarts) will
/// pick them up once they exist.
#[cfg(unix)]
fn tighten_unix_db_permissions(db_path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let mut wal = db_path.as_os_str().to_owned();
    wal.push("-wal");
    let mut shm = db_path.as_os_str().to_owned();
    shm.push("-shm");
    let paths: [std::path::PathBuf; 3] = [db_path.to_path_buf(), wal.into(), shm.into()];
    for p in paths {
        if p.exists() {
            let _ = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600));
        }
    }
}

/// All four boolean "stamp" columns a project can carry (RAL-250/307/317/
/// 378), for a single guardian's `git_root`. See
/// [`Store::load_all_project_stamps`]/[`Store::match_project_stamps`].
pub(crate) struct ProjectStamps {
    pub skip_base_updates: Option<bool>,
    pub match_pr_branch_name: Option<bool>,
    pub auto_submit_pr_stack: Option<bool>,
    pub separate_pr_branch: Option<bool>,
}

impl Store {
    /// Claim exclusive ownership of `branch_id`'s worktree for `owner`
    /// (typically `feedback:{branch_id}`). Fails while a restack is running
    /// for this guardian (a restack always wants every branch lease free
    /// before it starts, so a new lease must not appear mid-restack) or
    /// while another owner already holds this branch's lease. See the
    /// `worktree lease` glossary entry.
    pub(crate) fn try_acquire_guardian_worktree_lease(
        &mut self,
        guardian_id: &str,
        branch_id: &str,
        owner: &str,
    ) -> bool {
        if self.guardian_restack_running.contains(guardian_id) {
            return false;
        }
        let key = (guardian_id.to_string(), branch_id.to_string());
        if self.guardian_worktree_leases.contains_key(&key) {
            return false;
        }
        self.guardian_worktree_leases.insert(key, owner.to_string());
        true
    }

    /// Release `branch_id`'s worktree lease, but only if `owner` is the
    /// current holder — a stale caller (e.g. a retried request) can never
    /// release a lease it no longer owns. Returns whether it actually
    /// released one.
    pub(crate) fn release_guardian_worktree_lease(
        &mut self,
        guardian_id: &str,
        branch_id: &str,
        owner: &str,
    ) -> bool {
        let key = (guardian_id.to_string(), branch_id.to_string());
        if self.guardian_worktree_leases.get(&key).map(String::as_str) != Some(owner) {
            return false;
        }
        self.guardian_worktree_leases.remove(&key);
        true
    }

    /// The current lease owner for `branch_id`'s worktree, if any — used by
    /// `drive_rebase` to wait out a concurrent feedback pass before touching
    /// the worktree.
    pub(crate) fn guardian_worktree_lease_owner(
        &self,
        guardian_id: &str,
        branch_id: &str,
    ) -> Option<String> {
        self.guardian_worktree_leases
            .get(&(guardian_id.to_string(), branch_id.to_string()))
            .cloned()
    }

    /// Queue a restack starting at `from_position` for `guardian_id`,
    /// coalescing with any already-queued request by keeping the lower
    /// position — a restack from a later position can never safely replace
    /// one already promised to reach further back into the stack.
    pub(crate) fn request_guardian_restack(&mut self, guardian_id: &str, from_position: i64) {
        self.guardian_restack_requests
            .entry(guardian_id.to_string())
            .and_modify(|p| *p = (*p).min(from_position))
            .or_insert(from_position);
    }

    /// Claim the queued restack request for `guardian_id`, if one exists and
    /// no branch of the guardian currently holds a worktree lease (a restack
    /// must see every branch's worktree quiescent before it starts rebasing
    /// them) and no restack is already running for it. On success, the
    /// guardian is marked running until [`Store::finish_guardian_restack`]
    /// is called, and the coalesced position is returned and removed from
    /// the queue.
    pub(crate) fn try_claim_guardian_restack(&mut self, guardian_id: &str) -> Option<i64> {
        if self.guardian_restack_running.contains(guardian_id)
            || self
                .guardian_worktree_leases
                .keys()
                .any(|(gid, _)| gid == guardian_id)
        {
            return None;
        }
        let position = self.guardian_restack_requests.remove(guardian_id)?;
        self.guardian_restack_running
            .insert(guardian_id.to_string());
        Some(position)
    }

    /// Mark `guardian_id`'s claimed restack finished, allowing a new restack
    /// claim or worktree lease acquisition.
    pub(crate) fn finish_guardian_restack(&mut self, guardian_id: &str) {
        self.guardian_restack_running.remove(guardian_id);
    }

    /// Open (creating if needed) a store at `path`, in WAL mode.
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        let store = Self {
            conn,
            event_bus: crate::events::EventBus::new(),
            live_activity: HashMap::new(),
            guardian_summary_debounce: HashMap::new(),
            guardian_worktree_leases: HashMap::new(),
            guardian_restack_requests: HashMap::new(),
            guardian_restack_running: std::collections::HashSet::new(),
            stall_escalated: HashMap::new(),
            secret_env_names_cache: std::sync::RwLock::new(None),
        };
        store.init_schema()?;
        #[cfg(unix)]
        tighten_unix_db_permissions(path);
        Ok(store)
    }

    /// Open an in-memory store (used by tests).
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        let store = Self {
            conn,
            event_bus: crate::events::EventBus::new(),
            live_activity: HashMap::new(),
            guardian_summary_debounce: HashMap::new(),
            guardian_worktree_leases: HashMap::new(),
            guardian_restack_requests: HashMap::new(),
            guardian_restack_running: std::collections::HashSet::new(),
            stall_escalated: HashMap::new(),
            secret_env_names_cache: std::sync::RwLock::new(None),
        };
        store.init_schema()?;
        Ok(store)
    }

    /// The SSE broadcast registry (RAL-167) — subscribe from the `/api/events`
    /// HTTP handler, published to automatically by [`Store::cartographer_log`].
    #[must_use]
    pub fn event_bus(&self) -> &crate::events::EventBus {
        &self.event_bus
    }

    fn init_schema(&self) -> Result<()> {
        // RAL-281: captured *before* the `CREATE TABLE IF NOT EXISTS` below so
        // the default seed (after the batch) runs exactly once, at first-ever
        // creation -- a user who deletes every default entry must not see them
        // silently reappear on the next daemon restart.
        let secret_env_names_preexisting: bool = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='secret_env_names'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .unwrap_or(0)
            > 0;
        // RAL-318: same rationale as `secret_env_names_preexisting` above --
        // captured before the `CREATE TABLE IF NOT EXISTS` below so the
        // starter Triage types (`crate::triage::DEFAULT_TRIAGE_TYPES`) are
        // seeded exactly once, at first-ever creation. A user who
        // deregisters one of these must not see it silently reappear on the
        // next daemon restart -- unlike the built-in `unclassified` type,
        // which always exists and is reseeded unconditionally below.
        let triage_types_preexisting: bool = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='triage_types'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .unwrap_or(0)
            > 0;
        // RAL-364: `follows` was renamed to `watches`. The `CREATE TABLE IF
        // NOT EXISTS watches` inside the batch below would otherwise create
        // an empty `watches` table on a database that still has the old
        // `follows` table populated; a migration attempted *after* that
        // (e.g. `ALTER TABLE follows RENAME TO watches`) would then fail
        // with "table watches already exists" -- silently, since every
        // migration statement below is wrapped in `let _ =`. So this has to
        // run as a copy-and-drop *before* the batch instead, guarded so it
        // only fires once (on the next call `follows` is gone and this
        // block is skipped).
        let follows_table_preexisting: bool = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='follows'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .unwrap_or(0)
            > 0;
        if follows_table_preexisting {
            let _ = self.conn.execute_batch(
                "
                CREATE TABLE IF NOT EXISTS watches (
                    id            TEXT PRIMARY KEY,
                    user_name     TEXT NOT NULL REFERENCES users(name) ON DELETE CASCADE,
                    entity_uri    TEXT NOT NULL,
                    notify_tiers  TEXT NOT NULL,
                    created_at_ms INTEGER NOT NULL,
                    UNIQUE (user_name, entity_uri)
                );
                INSERT INTO watches SELECT * FROM follows;
                UPDATE watches SET id = REPLACE(id, 'follow-', 'watch-');
                UPDATE meta SET key = 'watch_seq' WHERE key = 'follow_seq';
                DROP TABLE follows;
                ",
            );
        }
        self.conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS meta (
                key   TEXT PRIMARY KEY,
                value INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS squads (
                id            TEXT PRIMARY KEY,
                label         TEXT,
                state         TEXT NOT NULL,
                depends_on    TEXT NOT NULL DEFAULT '[]',
                created_at_ms INTEGER NOT NULL,
                updated_at_ms INTEGER NOT NULL,
                trace_context TEXT,
                env_overrides TEXT NOT NULL DEFAULT '{}',
                started_at_ms  INTEGER,
                finished_at_ms INTEGER
            );
            CREATE TABLE IF NOT EXISTS tasks (
                squad_id     TEXT NOT NULL REFERENCES squads(id) ON DELETE CASCADE,
                idx        INTEGER NOT NULL,
                name       TEXT NOT NULL,
                project    TEXT,
                agent      TEXT,
                model      TEXT,
                state      TEXT NOT NULL,
                depends_on TEXT NOT NULL DEFAULT '[]',
                queue_rank REAL,
                started_at_ms  INTEGER,
                finished_at_ms INTEGER,
                no_commit_required INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY (squad_id, idx)
            );
            CREATE TABLE IF NOT EXISTS cells (
                squad_id     TEXT NOT NULL REFERENCES squads(id) ON DELETE CASCADE,
                task_idx   INTEGER NOT NULL,
                idx        INTEGER NOT NULL,
                sid        TEXT NOT NULL,
                cwd        TEXT,
                subprojects TEXT,
                prompt     TEXT,
                command    TEXT,
                agent      TEXT NOT NULL,
                model      TEXT,
                system_prompt          TEXT,
                system_prompt_position TEXT,
                effective_system_prompt TEXT,
                state      TEXT NOT NULL,
                depends_on TEXT NOT NULL DEFAULT '[]',
                tokens_in  INTEGER NOT NULL DEFAULT 0,
                tokens_out INTEGER NOT NULL DEFAULT 0,
                cache_creation_tokens INTEGER NOT NULL DEFAULT 0,
                cache_read_tokens     INTEGER NOT NULL DEFAULT 0,
                compaction_input_tokens INTEGER NOT NULL DEFAULT 0,
                compaction_count        INTEGER NOT NULL DEFAULT 0,
                cost_usd   REAL NOT NULL DEFAULT 0,
                cost_is_estimated INTEGER NOT NULL DEFAULT 0,
                error      TEXT,
                review_branch TEXT,
                timeout_sec   INTEGER,
                budget_tokens INTEGER,
                agent_session_id TEXT,
                maximum_budget_usd REAL,
                maximum_context INTEGER,
                auto_compact_threshold INTEGER,
                maximum_tool_output_tokens INTEGER,
                upstream      TEXT,
                queue_rank    REAL,
                machine       TEXT,
                materialized_env_overrides TEXT,
                started_at_ms  INTEGER,
                finished_at_ms INTEGER,
                PRIMARY KEY (squad_id, task_idx, idx)
            );
            CREATE TABLE IF NOT EXISTS proofs (
                squad_id      TEXT NOT NULL REFERENCES squads(id) ON DELETE CASCADE,
                task_idx    INTEGER NOT NULL,
                scope       TEXT NOT NULL,
                cell_idx INTEGER NOT NULL,
                idx         INTEGER NOT NULL,
                vid         TEXT,
                kind        TEXT NOT NULL,
                spec        TEXT NOT NULL,
                effective_system_prompt TEXT,
                model       TEXT,
                agent       TEXT NOT NULL DEFAULT 'claude',
                state       TEXT NOT NULL,
                output      TEXT,
                agent_session_id TEXT,
                timeout_sec   INTEGER,
                budget_tokens INTEGER,
                maximum_tool_output_tokens INTEGER,
                queue_rank    REAL,
                env_overrides TEXT NOT NULL DEFAULT '{}',
                materialized_env_overrides TEXT,
                PRIMARY KEY (squad_id, task_idx, scope, cell_idx, idx)
            );
            CREATE TABLE IF NOT EXISTS guardians (
                id                TEXT PRIMARY KEY,
                name              TEXT NOT NULL,
                base_branch       TEXT NOT NULL,
                base_commit       TEXT,
                git_root          TEXT NOT NULL,
                -- The registered project (`projects.name`) used to create
                -- this review. NULL means the review was created from a raw
                -- directory path instead. `git_root` remains the concrete
                -- path used by git, but is not the review's identity.
                project           TEXT,
                review_branch     TEXT,
                status            TEXT NOT NULL,
                detail            TEXT,
                checks            TEXT NOT NULL DEFAULT '[]',
                squad_id            TEXT,
                combined_worktree TEXT,
                conflicts_total     INTEGER,
                conflicts_remaining INTEGER,
                conflicts_found     INTEGER,
                conflicts_fixed     INTEGER,
                conflicts_committed INTEGER,
                skip_auto_build   INTEGER NOT NULL DEFAULT 0,
                -- Read-only legacy data (RAL-285): nothing writes this column
                -- anymore. `hydrate_guardian` reads it only to resolve a row
                -- carrying the bit with no explicit `proof_scope` to an
                -- `effective_proof_scope` of 'nothing'.
                skip_worktree_checks INTEGER NOT NULL DEFAULT 0,
                review_type       TEXT NOT NULL DEFAULT 'git',
                skip_worktrees    INTEGER NOT NULL DEFAULT 0,
                squash_projects   TEXT NOT NULL DEFAULT '[]',
                review_key        TEXT,
                resolver_agent    TEXT,
                resolver_model    TEXT,
                proof_scope      TEXT,
                proof_skip_auto_clean INTEGER,
                skip_base_updates INTEGER,
                created_at_ms     INTEGER NOT NULL,
                updated_at_ms     INTEGER NOT NULL,
                base_changed_at_ms INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE IF NOT EXISTS guardian_branches (
                guardian_id         TEXT NOT NULL REFERENCES guardians(id) ON DELETE CASCADE,
                position            INTEGER NOT NULL,
                branch              TEXT NOT NULL,
                merge_status        TEXT NOT NULL,
                detail              TEXT,
                review_branch       TEXT,
                worktree            TEXT,
                conflicts_total     INTEGER,
                conflicts_remaining INTEGER,
                conflicts_found     INTEGER,
                conflicts_fixed     INTEGER,
                conflicts_committed INTEGER,
                is_empty            INTEGER NOT NULL DEFAULT 0,
                env_overrides       TEXT NOT NULL DEFAULT '{}',
                started_at_ms       INTEGER,
                PRIMARY KEY (guardian_id, position)
            );
            -- RAL-193: per-call cost line items for a guardian's own
            -- conflict-resolution and proof-step LLM calls (guardian_merge.rs),
            -- which previously were logged at best and otherwise discarded.
            -- `attempt` mirrors `guardians.merge_attempt` at call time, so a
            -- single merge attempt's total and the cumulative total across
            -- every rebase/re-merge attempt are both derivable by
            -- filtering/summing this table. `branch_id` is the stable
            -- per-branch id (`guardian_branches.id`) for a call scoped to one
            -- stacked branch, NULL for a review-wide call (chat, combined
            -- final proof).
            CREATE TABLE IF NOT EXISTS guardian_costs (
                id            INTEGER PRIMARY KEY AUTOINCREMENT,
                guardian_id   TEXT NOT NULL REFERENCES guardians(id) ON DELETE CASCADE,
                branch_id     TEXT,
                attempt       INTEGER NOT NULL,
                kind          TEXT NOT NULL,
                tokens_in     INTEGER NOT NULL DEFAULT 0,
                tokens_out    INTEGER NOT NULL DEFAULT 0,
                cost_usd      REAL NOT NULL DEFAULT 0,
                created_at_ms INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_guardian_costs_guardian ON guardian_costs(guardian_id);
            -- RAL-385: durable record of guardian worktree retirement attempts.
            -- A successful retirement clears `guardian_branches.worktree`/
            -- `guardians.combined_worktree`, which would otherwise erase all
            -- trace of the removal; this row is the audit trail. Retained for
            -- exactly as long as its review exists (the FK cascade plus the
            -- explicit sweep in `delete_guardian`), matching the review's own
            -- retention lifecycle. `status` is `retired` (removed), `failed`
            -- (an attempt was made and refused; `error` says why and the next
            -- daily sweep retries), `deferred` (RAL-386: a machine provider's
            -- own policy asked to try again later; `error` carries its reason
            -- and `retry_at_ms` its hint, though the sweep's own retry cadence
            -- is still the daily interval), or `opted_out` (RAL-386: a machine
            -- provider or an operator's static machine policy declined to ever
            -- retire this worktree automatically; `error` carries why). Only
            -- `failed`/`deferred`/`opted_out` are retried on the next sweep --
            -- `retired` is terminal. Worktrees that have never been attempted
            -- need no row -- their scheduled/eligible/claimed state is derived
            -- live in `guardian_merge::worktree_retirement_view`.
            CREATE TABLE IF NOT EXISTS guardian_worktree_retirements (
                guardian_id     TEXT NOT NULL REFERENCES guardians(id) ON DELETE CASCADE,
                path            TEXT NOT NULL,
                status          TEXT NOT NULL CHECK(status IN ('retired', 'failed', 'deferred', 'opted_out')),
                error           TEXT,
                eligible_at_ms  INTEGER NOT NULL,
                last_attempt_ms INTEGER NOT NULL,
                retry_at_ms     INTEGER,
                PRIMARY KEY (guardian_id, path)
            );
            CREATE TABLE IF NOT EXISTS events (
                seq         INTEGER PRIMARY KEY AUTOINCREMENT,
                squad_id      TEXT,
                guardian_id TEXT,
                scope       TEXT NOT NULL,
                ref         TEXT,
                message     TEXT NOT NULL,
                at_ms       INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_events_squad ON events(squad_id, seq);
            CREATE INDEX IF NOT EXISTS idx_events_guardian ON events(guardian_id, seq);
            CREATE TABLE IF NOT EXISTS guardian_messages (
                seq         INTEGER PRIMARY KEY AUTOINCREMENT,
                guardian_id TEXT NOT NULL,
                role        TEXT NOT NULL,
                text        TEXT NOT NULL,
                at_ms       INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_gmsg ON guardian_messages(guardian_id, seq);
            CREATE TABLE IF NOT EXISTS cartographer_events (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                at_ms       INTEGER NOT NULL,
                level       TEXT NOT NULL,
                source      TEXT NOT NULL,
                message     TEXT NOT NULL,
                scope       TEXT,
                squad_id      TEXT,
                guardian_id TEXT,
                cell_id  TEXT,
                task        TEXT,
                log_path    TEXT,
                payload     TEXT NOT NULL DEFAULT '{}'
            );
            CREATE INDEX IF NOT EXISTS idx_carto_at ON cartographer_events(at_ms);
            CREATE INDEX IF NOT EXISTS idx_carto_squad ON cartographer_events(squad_id);
            CREATE INDEX IF NOT EXISTS idx_carto_guardian ON cartographer_events(guardian_id);
            CREATE INDEX IF NOT EXISTS idx_carto_cell ON cartographer_events(cell_id);
            CREATE INDEX IF NOT EXISTS idx_carto_source ON cartographer_events(source);
            CREATE TABLE IF NOT EXISTS projects (
                name          TEXT PRIMARY KEY,
                description   TEXT NOT NULL DEFAULT '',
                path          TEXT NOT NULL,
                clone_url     TEXT,
                vcs           TEXT NOT NULL DEFAULT 'git',
                created_at_ms INTEGER NOT NULL,
                skip_base_updates INTEGER
            );
            -- Minimal user registry (RAL-?): a placeholder identity a request
            -- can name itself as, for `AgentAccess` (`agent_access.rs`) to key
            -- off. TODO: Replace with user auth once RAL-252 is done -- there
            -- is no login, no password, no session here, just a name a caller
            -- can claim.
            CREATE TABLE IF NOT EXISTS users (
                name                  TEXT PRIMARY KEY,
                created_at_ms         INTEGER NOT NULL,
                auto_watch            INTEGER NOT NULL DEFAULT 0,
                default_notify_tiers  TEXT NOT NULL DEFAULT 'urgent,high,normal'
            );
            -- RAL-338: a project's writable fork, keyed by the user who pushes
            -- to it (`user = ''` is the project-wide fallback row). Rows
            -- deliberately do not cascade on user deletion -- an
            -- unregistered-user fork row must stay visible (see
            -- `project_forks.rs`), not silently vanish.
            CREATE TABLE IF NOT EXISTS project_forks (
                project       TEXT NOT NULL,
                user          TEXT NOT NULL,
                fork_url      TEXT NOT NULL,
                remote_name   TEXT NOT NULL,
                fork_owner    TEXT NOT NULL DEFAULT '',
                created_at_ms INTEGER NOT NULL,
                updated_at_ms INTEGER NOT NULL,
                PRIMARY KEY (project, user)
            );
            -- RAL-328: view preferences are scoped to a registered user and
            -- reference exactly one squad or review. Entity deletion removes
            -- the preference before sequential ids can be reused.
            CREATE TABLE IF NOT EXISTS hidden_items (
                kind          TEXT NOT NULL CHECK(kind IN ('squad', 'review')),
                squad_id      TEXT REFERENCES squads(id) ON DELETE CASCADE,
                guardian_id   TEXT REFERENCES guardians(id) ON DELETE CASCADE,
                user_name     TEXT NOT NULL REFERENCES users(name) ON DELETE CASCADE ON UPDATE CASCADE,
                hidden_at_ms  INTEGER NOT NULL,
                CHECK(
                    (kind = 'squad' AND squad_id IS NOT NULL AND guardian_id IS NULL)
                    OR
                    (kind = 'review' AND squad_id IS NULL AND guardian_id IS NOT NULL)
                ),
                UNIQUE(user_name, squad_id),
                UNIQUE(user_name, guardian_id)
            );
            CREATE INDEX IF NOT EXISTS idx_hidden_items_user ON hidden_items(user_name);
            CREATE INDEX IF NOT EXISTS idx_hidden_items_squad ON hidden_items(squad_id);
            CREATE INDEX IF NOT EXISTS idx_hidden_items_guardian ON hidden_items(guardian_id);
            -- RAL-281: user-editable list of env-var *names* treated as secret,
            -- additive to the value-based `crate::redact` registry (RAL-264).
            -- See `crate::secret_env_names`'s module doc comment for how this is
            -- consulted and cached.
            CREATE TABLE IF NOT EXISTS secret_env_names (
                name          TEXT PRIMARY KEY,
                created_at_ms INTEGER NOT NULL
            );
            -- RAL-185: the machine provider registry. A machine value of the form
            -- scheme:uri looks `scheme` up here to find the executable the daemon
            -- runs; `uri` is opaque and handed to that executable verbatim.
            -- Registration is an explicit admin action and is deliberately NOT
            -- declarable in a task file -- see `crate::machines` for the reasoning
            -- (a TOML that could both name and define an executable would make
            -- submit equivalent to arbitrary code execution).
            CREATE TABLE IF NOT EXISTS machine_providers (
                scheme           TEXT PRIMARY KEY,
                description      TEXT NOT NULL DEFAULT '',
                program          TEXT NOT NULL,
                args             TEXT NOT NULL DEFAULT '[]',
                protocol_version INTEGER NOT NULL DEFAULT 1,
                created_at_ms    INTEGER NOT NULL,
                -- RAL-185 Q3: last explicit reachability probe. NULL until one
                -- has run -- distinct from both reachable and unreachable, so
                -- the board can say not-checked-yet rather than implying a
                -- machine is fine or broken on no evidence.
                last_check_ms    INTEGER,
                last_check_ok    INTEGER,
                last_check_note  TEXT,
                -- RAL-185 D7: whether this provider implements the `channel`
                -- verb (one long-lived process, many commands) instead of being
                -- spawned per command. Opt-in; a provider that does not is used
                -- exactly as before.
                supports_channel INTEGER NOT NULL DEFAULT 0
            );
            -- RAL-201: the opaque `handle` a machine provider returned for an
            -- in-flight async `exec` (docs/machine-providers.md), so a daemon
            -- restart can reconcile it instead of silently orphaning whatever
            -- was still running remotely. `provision` re-derives the same
            -- workspace deterministically on restart, but an `exec` handle has
            -- no such idempotent re-derivation -- without this row, the only
            -- record that remote work is in flight lived in a Rust
            -- `Instant`/loop on a thread that a restart just killed.
            CREATE TABLE IF NOT EXISTS remote_exec_handles (
                squad_id        TEXT NOT NULL,
                cell_id    TEXT NOT NULL,
                scheme        TEXT NOT NULL,
                uri           TEXT NOT NULL,
                handle        TEXT NOT NULL,
                created_at_ms INTEGER NOT NULL,
                PRIMARY KEY (squad_id, cell_id)
            );
            -- RAL-117: a durable, mutable mapping from a guardian's worktree(s) to
            -- the pull request(s) submitted for it. `branch_id` is NULL for a
            -- PR submitted from the combined review worktree (all branches in one
            -- PR); otherwise it names one stacked branch's own PR by its stable id
            -- (RAL-122: renamed from `branch_position`, which silently rotted
            -- across a reorder). A single guardian can have many rows (one per
            -- stacked branch, or one combined row) -- new table rather than
            -- columns on `guardian_branches` because a submission need not mirror
            -- the branch set 1:1 (a branch can be excluded from submission, or
            -- resubmitted under a new alias).
            CREATE TABLE IF NOT EXISTS guardian_pull_requests (
                id             TEXT PRIMARY KEY,
                guardian_id    TEXT NOT NULL REFERENCES guardians(id) ON DELETE CASCADE,
                branch_id      TEXT,
                forge          TEXT NOT NULL,
                repo           TEXT NOT NULL,
                branch_alias   TEXT NOT NULL,
                base_ref       TEXT NOT NULL,
                title          TEXT NOT NULL,
                description    TEXT NOT NULL,
                pr_number      INTEGER,
                pr_url         TEXT,
                state          TEXT NOT NULL DEFAULT 'open',
                created_at_ms  INTEGER NOT NULL,
                updated_at_ms  INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_pr_guardian ON guardian_pull_requests(guardian_id);
            CREATE INDEX IF NOT EXISTS idx_pr_lookup ON guardian_pull_requests(forge, repo, pr_number);
            -- RAL-117: which PR comments/notes have already been actioned into the
            -- owning worktree, so re-running \"pull in PR feedback\" only picks up
            -- new comments instead of re-feeding the same text to the resolver.
            CREATE TABLE IF NOT EXISTS guardian_pr_feedback_actioned (
                id                  TEXT PRIMARY KEY,
                pr_id               TEXT NOT NULL REFERENCES guardian_pull_requests(id) ON DELETE CASCADE,
                external_comment_id TEXT NOT NULL,
                actioned_at_ms      INTEGER NOT NULL,
                UNIQUE(pr_id, external_comment_id)
            );
            CREATE INDEX IF NOT EXISTS idx_pr_feedback_pr ON guardian_pr_feedback_actioned(pr_id);
            -- RAL-164: tracks in-flight/completed 'set it for me' AI resolution
            -- of a named CheckInput, one row per (guardian_id, input_name).
            -- Existence of this table (rather than a JSON blob on `guardians`)
            -- is what makes the claim in `claim_guardian_input_resolution`
            -- atomic -- a concurrent duplicate request (double-click, second
            -- browser tab) is rejected via the UNIQUE-key upsert's WHERE
            -- clause, not a debounce. status: 'resolving' | 'ready' | 'failed'.
            CREATE TABLE IF NOT EXISTS guardian_input_resolutions (
                guardian_id   TEXT NOT NULL,
                input_name    TEXT NOT NULL,
                status        TEXT NOT NULL,
                value         TEXT,
                updated_at_ms INTEGER NOT NULL,
                PRIMARY KEY (guardian_id, input_name)
            );
            -- RAL-389: durable trailing-debounce requests for asynchronous
            -- PR-stack submission. A later branch completion moves the same
            -- guardian-level request forward instead of adding another job.
            CREATE TABLE IF NOT EXISTS guardian_auto_submit_requests (
                guardian_id     TEXT PRIMARY KEY REFERENCES guardians(id) ON DELETE CASCADE,
                requested_at_ms INTEGER NOT NULL
            );
            -- RAL-136: ephemeral, queryable handoff notes ('ghosts') a task
            -- cell or review worktree publishes for downstream work.  One
            -- row per owner (`owner_uri`) -- a rewrite merges onto the
            -- existing row rather than inserting a second one (see
            -- `ghost::merge_content`). `squad_id`/`guardian_id` are mutually
            -- exclusive depending on `kind` and exist so a squad/guardian
            -- deletion can cascade-clean its ghosts (done explicitly in
            -- `delete_squad`/`delete_guardian`/`clear_all`, like every other
            -- child table -- see the comment on `delete_squad`).
            CREATE TABLE IF NOT EXISTS ghosts (
                owner_uri     TEXT PRIMARY KEY,
                kind          TEXT NOT NULL,
                squad_id        TEXT REFERENCES squads(id) ON DELETE CASCADE,
                guardian_id   TEXT REFERENCES guardians(id) ON DELETE CASCADE,
                content       TEXT NOT NULL,
                user_note     TEXT,
                revision      TEXT,
                created_at_ms INTEGER NOT NULL,
                updated_at_ms INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_ghosts_squad ON ghosts(squad_id);
            CREATE INDEX IF NOT EXISTS idx_ghosts_guardian ON ghosts(guardian_id);
            -- RAL-241: the escalation mailbox. `mailbox_clients` is who can
            -- drain (registered via `POST /api/mailbox/register`);
            -- `mailbox_messages` is what got enqueued (a failed cell, a
            -- stalled session, ...); `mailbox_drains` is per-client-per-
            -- message read state -- one escalation can broadcast to many
            -- clients, and each drains independently. See
            -- `crate::mailbox`'s module doc comment.
            CREATE TABLE IF NOT EXISTS mailbox_clients (
                id               TEXT PRIMARY KEY,
                registered_at_ms INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS mailbox_messages (
                id            TEXT PRIMARY KEY,
                priority      TEXT NOT NULL,
                message       TEXT NOT NULL,
                squad_id      TEXT REFERENCES squads(id) ON DELETE CASCADE,
                task          TEXT,
                cell_id       TEXT,
                created_at_ms INTEGER NOT NULL,
                entity_uri    TEXT,
                event_kind    TEXT,
                category      TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_mailbox_messages_created ON mailbox_messages(created_at_ms);
            CREATE INDEX IF NOT EXISTS idx_mailbox_messages_priority ON mailbox_messages(priority);
            CREATE INDEX IF NOT EXISTS idx_mailbox_messages_squad ON mailbox_messages(squad_id);
            CREATE TABLE IF NOT EXISTS mailbox_drains (
                message_id    TEXT NOT NULL REFERENCES mailbox_messages(id) ON DELETE CASCADE,
                client_id     TEXT NOT NULL REFERENCES mailbox_clients(id) ON DELETE CASCADE,
                drained_at_ms INTEGER NOT NULL,
                PRIMARY KEY (message_id, client_id)
            );
            -- RAL-320: a user's personal drain state for the same
            -- `mailbox_messages` rows, kept separate from the client-scoped
            -- `mailbox_drains` above because a personal-mailbox view is
            -- per-user, not per-client (a user may poll from many clients).
            CREATE TABLE IF NOT EXISTS user_mailbox_drains (
                message_id    TEXT NOT NULL REFERENCES mailbox_messages(id) ON DELETE CASCADE,
                user_name     TEXT NOT NULL REFERENCES users(name) ON DELETE CASCADE,
                drained_at_ms INTEGER NOT NULL,
                PRIMARY KEY (message_id, user_name)
            );
            CREATE INDEX IF NOT EXISTS idx_user_mailbox_drains_user ON user_mailbox_drains(user_name);
            -- RAL-320: a user's personal subscription to an `EntityUri`
            -- (squad/task/cell/proof/review/review-worktree). Watching a
            -- parent cascades to its children via `EntityUri::covers()` at
            -- read time -- no expansion is stored here. `notify_tiers` is a
            -- per-watch override of which `MailboxPriority` tiers reach the
            -- watcher (see `crate::mailbox::{parse_tiers, tiers_to_csv}`).
            -- RAL-343 describes this same entity-subscription concept under
            -- the name \"Monitor\"; if/when it's built, it should reuse this
            -- table rather than add a parallel data model.
            CREATE TABLE IF NOT EXISTS watches (
                id            TEXT PRIMARY KEY,
                user_name     TEXT NOT NULL REFERENCES users(name) ON DELETE CASCADE,
                entity_uri    TEXT NOT NULL,
                notify_tiers  TEXT NOT NULL,
                created_at_ms INTEGER NOT NULL,
                UNIQUE (user_name, entity_uri)
            );
            CREATE INDEX IF NOT EXISTS idx_watches_user ON watches(user_name);
            -- Ark escalation dedup is entity-scoped and durable. Mailbox
            -- drain state is client-scoped and cannot provide this guarantee.
            CREATE TABLE IF NOT EXISTS ark_notifications (
                entity_kind TEXT NOT NULL,
                entity_id TEXT NOT NULL,
                notified_at_ms INTEGER NOT NULL,
                PRIMARY KEY (entity_kind, entity_id)
            );
            CREATE TABLE IF NOT EXISTS ark_sweeps (
                project_path TEXT PRIMARY KEY,
                swept_at_ms INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_mailbox_drains_client ON mailbox_drains(client_id);
            -- RAL-318: the Triage type registry (user-facing name for the
            -- Arbiter subsystem's classification categories). Mirrors
            -- `machine_providers`' register/list/get/deregister shape -- see
            -- `crate::triage`. The built-in `unclassified` row (seeded below,
            -- every startup) can never be deregistered.
            CREATE TABLE IF NOT EXISTS triage_types (
                name          TEXT PRIMARY KEY,
                label         TEXT NOT NULL DEFAULT '',
                description   TEXT NOT NULL DEFAULT '',
                created_at_ms INTEGER NOT NULL
            );
            -- RAL-318: a cell's Arbiter-classified (or inline-declared) Triage
            -- type(s), resolved once at `ralphus submit` time and persisted so
            -- a daemon restart never re-classifies (single-attempt, no
            -- retry). One row per (cell, type) -- a cell can carry more than
            -- one type (e.g. both \"bug\" and \"investigation\"), each pooled
            -- independently -- so the primary key includes `triage_type`
            -- rather than being one row per cell.
            CREATE TABLE IF NOT EXISTS triage_cell_types (
                squad_id      TEXT NOT NULL,
                task_idx      INTEGER NOT NULL,
                idx           INTEGER NOT NULL,
                triage_type   TEXT NOT NULL,
                created_at_ms INTEGER NOT NULL,
                PRIMARY KEY (squad_id, task_idx, idx, triage_type)
            );
            -- RAL-318: cells pending a pooled Triage review, keyed by
            -- (project, triage_type). Drained (deleted) the moment a pool's
            -- count threshold or a cron schedule fires and a review is
            -- created from whatever is currently pooled.
            CREATE TABLE IF NOT EXISTS triage_pool_cells (
                id            INTEGER PRIMARY KEY AUTOINCREMENT,
                project       TEXT NOT NULL,
                triage_type   TEXT NOT NULL,
                squad_id      TEXT NOT NULL,
                task_idx      INTEGER NOT NULL,
                idx           INTEGER NOT NULL,
                branch        TEXT NOT NULL,
                upstream      TEXT NOT NULL,
                created_at_ms INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_triage_pool_cells_key
                ON triage_pool_cells(project, triage_type);
            -- RAL-318: per-(project, triage_type) pool count threshold.
            -- Absent means \"no count-based trigger configured\" -- a pool with
            -- neither a threshold nor a schedule simply accumulates.
            CREATE TABLE IF NOT EXISTS triage_pool_thresholds (
                project         TEXT NOT NULL,
                triage_type     TEXT NOT NULL,
                threshold_count INTEGER NOT NULL,
                updated_at_ms   INTEGER NOT NULL,
                PRIMARY KEY (project, triage_type)
            );
            -- RAL-318: one independent cron-style schedule entry for a
            -- (project, triage_type) pool. `anchor_date_ms` establishes
            -- interval parity (e.g. \"every other Monday\") together with
            -- `every_n` -- a bare cron expression alone can only express
            -- \"every Monday\". `occurrence_count`/`last_checked_ms` are
            -- scheduler-owned cursor state (see `crate::scheduler`'s Triage
            -- tick): advanced one cron occurrence at a time so parity is
            -- always computed incrementally, never by replaying history.
            CREATE TABLE IF NOT EXISTS triage_schedules (
                id                INTEGER PRIMARY KEY AUTOINCREMENT,
                project           TEXT NOT NULL,
                triage_type       TEXT NOT NULL,
                cron_expr         TEXT NOT NULL,
                anchor_date_ms    INTEGER NOT NULL,
                every_n           INTEGER NOT NULL DEFAULT 1,
                occurrence_count  INTEGER NOT NULL DEFAULT 0,
                last_checked_ms   INTEGER,
                created_at_ms     INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_triage_schedules_key
                ON triage_schedules(project, triage_type);
            -- RAL-318: the Arbiter's own cost ledger. The Arbiter is a
            -- daemon-singleton with no cell_id/guardian_id to hang a cost row
            -- off (unlike `guardian_costs`), so this is a standalone table --
            -- reuses that table's insert-line-item -> sum -> compare-to-cap
            -- enforcement pattern, not its columns/foreign keys. `kind` is
            -- \"classification\" or \"health_check\".
            CREATE TABLE IF NOT EXISTS arbiter_costs (
                id            INTEGER PRIMARY KEY AUTOINCREMENT,
                kind          TEXT NOT NULL,
                tokens_in     INTEGER NOT NULL DEFAULT 0,
                tokens_out    INTEGER NOT NULL DEFAULT 0,
                cost_usd      REAL NOT NULL DEFAULT 0,
                created_at_ms INTEGER NOT NULL
            );
            -- RAL-337: which squad owns a task worktree branch. A
            -- `ralphus:new-worktree/<base_branch>` placeholder is resolved
            -- against this table so a *new* squad gets its own branch
            -- (`<base_branch>-2`, `-3`, ...) instead of silently inheriting the
            -- branch -- and therefore the finished commits -- of whichever
            -- squad materialized it first, while a *restart of the owning
            -- squad* still resolves back to the row it already claimed. See
            -- `crate::worktree_claims`.
            CREATE TABLE IF NOT EXISTS task_worktree_claims (
                project       TEXT NOT NULL,
                base_branch   TEXT NOT NULL,
                branch        TEXT NOT NULL,
                squad_id      TEXT NOT NULL,
                created_at_ms INTEGER NOT NULL,
                PRIMARY KEY (project, branch)
            );
            CREATE INDEX IF NOT EXISTS idx_wt_claims_family
                ON task_worktree_claims(project, base_branch);
            CREATE INDEX IF NOT EXISTS idx_wt_claims_squad
                ON task_worktree_claims(project, base_branch, squad_id);
            ",
        )?;
        // RAL-318: the built-in `unclassified` Triage type always exists and
        // can never be deregistered (see `crate::triage::deregister_triage_type`).
        // Re-seeded (harmlessly, via INSERT OR IGNORE) on every startup rather
        // than gated on first-ever-creation like `secret_env_names`'s defaults,
        // since this one row is a permanent invariant, not a user-editable
        // starter set.
        self.conn.execute(
            "INSERT OR IGNORE INTO triage_types(name, label, description, created_at_ms)
             VALUES(?,?,?,?)",
            params![
                crate::triage::UNCLASSIFIED_TYPE,
                "Unclassified",
                "Fallback type for a cell the Arbiter could not classify, or that failed classification.",
                now_ms()
            ],
        )?;
        if !triage_types_preexisting {
            for (name, label, description) in crate::triage::DEFAULT_TRIAGE_TYPES {
                self.conn.execute(
                    "INSERT OR IGNORE INTO triage_types(name, label, description, created_at_ms) VALUES(?,?,?,?)",
                    params![name, label, description, now_ms()],
                )?;
            }
        }
        if !secret_env_names_preexisting {
            for name in crate::secret_env_names::DEFAULT_SECRET_ENV_NAMES {
                self.conn.execute(
                    "INSERT OR IGNORE INTO secret_env_names(name, created_at_ms) VALUES(?,?)",
                    params![name, now_ms()],
                )?;
            }
        }
        // Best-effort migrations for databases created before these columns
        // existed. Each fails harmlessly (duplicate column) once present.
        for stmt in [
            "ALTER TABLE guardians ADD COLUMN squad_id TEXT",
            "ALTER TABLE guardians ADD COLUMN combined_worktree TEXT",
            "ALTER TABLE guardians ADD COLUMN conflicts_total INTEGER",
            "ALTER TABLE guardians ADD COLUMN conflicts_remaining INTEGER",
            "ALTER TABLE guardians ADD COLUMN review_type TEXT NOT NULL DEFAULT 'git'",
            "ALTER TABLE guardians ADD COLUMN skip_worktrees INTEGER NOT NULL DEFAULT 0",
            "ALTER TABLE guardians ADD COLUMN review_key TEXT",
            "ALTER TABLE guardians ADD COLUMN resolver_agent TEXT",
            "ALTER TABLE guardians ADD COLUMN resolver_model TEXT",
            // The base-branch commit the review was last built against, so a shift
            // in the base branch can be detected and auto-rebuilt (RAL base-shift).
            "ALTER TABLE guardians ADD COLUMN base_commit TEXT",
            // RAL-39: agent-generated cross-branch change summary for the reviewer.
            "ALTER TABLE guardians ADD COLUMN change_summary TEXT",
            // Per-project base commits for multi-project guardians (RAL-29).
            // JSON map of {project_root: sha}. For single-project guardians this
            // mirrors base_commit; for multi-project it tracks each root independently.
            "ALTER TABLE guardians ADD COLUMN base_commits TEXT NOT NULL DEFAULT '{}'",
            // RAL-265: hash of the enabled-branch configuration + per-project base
            // commits the guardian's stack was last built against. Lets an
            // incremental staged merge recognize a still-valid `Done` prefix
            // (resume from its tip) versus a changed base/config that forces a
            // rebuild of that prefix. NULL until the first (partial or full)
            // build pass records one.
            "ALTER TABLE guardians ADD COLUMN build_signature TEXT",
            // RAL-27: LLM-generated shell commands for manual review verification.
            // JSON array of command strings, regenerated on every rebase.
            "ALTER TABLE guardians ADD COLUMN manual_commands TEXT NOT NULL DEFAULT '[]'",
            "ALTER TABLE guardian_branches ADD COLUMN review_branch TEXT",
            "ALTER TABLE guardian_branches ADD COLUMN worktree TEXT",
            "ALTER TABLE guardian_branches ADD COLUMN conflicts_total INTEGER",
            "ALTER TABLE guardian_branches ADD COLUMN conflicts_remaining INTEGER",
            // RAL-72: three-metric conflict progress replacing the old two-column pair.
            "ALTER TABLE guardians ADD COLUMN conflicts_found INTEGER",
            "ALTER TABLE guardians ADD COLUMN conflicts_fixed INTEGER",
            "ALTER TABLE guardians ADD COLUMN conflicts_committed INTEGER",
            "ALTER TABLE guardian_branches ADD COLUMN conflicts_found INTEGER",
            "ALTER TABLE guardian_branches ADD COLUMN conflicts_fixed INTEGER",
            "ALTER TABLE guardian_branches ADD COLUMN conflicts_committed INTEGER",
            // RAL-43: branch can be disabled (dropped from the stack) without deletion.
            "ALTER TABLE guardian_branches ADD COLUMN enabled INTEGER NOT NULL DEFAULT 1",
            // Which git project (repository root) this branch lives in (RAL-29).
            // NULL means the guardian's own git_root (backward compatible).
            "ALTER TABLE guardian_branches ADD COLUMN project TEXT",
            "ALTER TABLE proofs ADD COLUMN model TEXT",
            "ALTER TABLE proofs ADD COLUMN agent TEXT NOT NULL DEFAULT 'claude'",
            // Raw task-level agent/model values used by the board to show
            // whether child resolved values were inherited from the task
            // rather than set explicitly at the child level (RAL-82).
            "ALTER TABLE tasks ADD COLUMN agent TEXT",
            "ALTER TABLE tasks ADD COLUMN model TEXT",
            "ALTER TABLE cells ADD COLUMN review_branch TEXT",
            "ALTER TABLE cells ADD COLUMN timeout_sec INTEGER",
            "ALTER TABLE cells ADD COLUMN budget_tokens INTEGER",
            "ALTER TABLE cells ADD COLUMN system_prompt TEXT",
            "ALTER TABLE cells ADD COLUMN system_prompt_position TEXT",
            "ALTER TABLE cells ADD COLUMN subprojects TEXT",
            "ALTER TABLE cells ADD COLUMN name TEXT",
            "ALTER TABLE proofs ADD COLUMN timeout_sec INTEGER",
            "ALTER TABLE proofs ADD COLUMN budget_tokens INTEGER",
            // RAL-50: branch-chaining upstream sentinel.
            "ALTER TABLE cells ADD COLUMN upstream TEXT",
            // RAL-69: tracks whether the user dismissed the "can re-enable" icon
            // on a force-started disabled branch. Once dismissed it never reappears.
            "ALTER TABLE guardian_branches ADD COLUMN dismissed_reenable INTEGER NOT NULL DEFAULT 0",
            // RAL-77: user-declared action hints from [[review.action]] in TOML.
            // JSON array of {label, command?, prompt?} objects, set at submit time.
            "ALTER TABLE guardians ADD COLUMN action_hints TEXT NOT NULL DEFAULT '[]'",
            // RAL-59: optional base64 image attached to a chat message.
            "ALTER TABLE guardian_messages ADD COLUMN image TEXT",
            // RAL-88: per-artifact provenance — which resolved agent/model produced
            // the change summary, the manual-check commands, and the feedback-chat
            // replies. Recorded at generation time so a reviewer can inspect and
            // debug AI-produced review content.
            "ALTER TABLE guardians ADD COLUMN summary_agent TEXT",
            "ALTER TABLE guardians ADD COLUMN summary_model TEXT",
            "ALTER TABLE guardians ADD COLUMN manual_commands_agent TEXT",
            "ALTER TABLE guardians ADD COLUMN manual_commands_model TEXT",
            "ALTER TABLE guardians ADD COLUMN chat_agent TEXT",
            "ALTER TABLE guardians ADD COLUMN chat_model TEXT",
            // RAL-91: per-(git-project) squash setting. JSON array of project roots
            // whose task branches are collapsed to a single commit in the review
            // worktree during the stacked rebase. Empty = squash disabled everywhere.
            "ALTER TABLE guardians ADD COLUMN squash_projects TEXT NOT NULL DEFAULT '[]'",
            // RAL-92: the commit the daemon last built this branch's review branch
            // at. A later move of the review-branch ref (a reviewer's manual
            // push/amend in the worktree) diverges from this baseline and triggers
            // a downstream restack; the daemon re-baselines after every build so its
            // own writes never look like a manual push.
            "ALTER TABLE guardian_branches ADD COLUMN review_head TEXT",
            // RAL Queue: user-orderable global priority rank on runnable work.
            // NULL = unranked (sorts after ranked items). Seeded from the TOML
            // `priority` key at submit; mutated by the Queue reorder/set-position.
            "ALTER TABLE tasks ADD COLUMN queue_rank REAL",
            "ALTER TABLE cells ADD COLUMN queue_rank REAL",
            "ALTER TABLE proofs ADD COLUMN queue_rank REAL",
            // RAL-96: the W3C `traceparent` of the request that created this squad
            // (browser click or CLI submit), persisted so the scheduler's later,
            // asynchronous work (squad-claim, cell execution, proof execution)
            // continues the same OpenTelemetry trace instead of starting a new one.
            "ALTER TABLE squads ADD COLUMN trace_context TEXT",
            // RAL-110: split the old single `skip_checks` flag into two independent
            // opt-outs -- see the backfill-and-drop block below, which carries
            // forward any existing `skip_checks` value into both.
            "ALTER TABLE guardians ADD COLUMN skip_auto_build INTEGER NOT NULL DEFAULT 0",
            // Read-only legacy data (RAL-285) -- see the `CREATE TABLE guardians`
            // comment on this column.
            "ALTER TABLE guardians ADD COLUMN skip_worktree_checks INTEGER NOT NULL DEFAULT 0",
            // RAL-117: opts a review into automatically incorporating PR feedback
            // comments without the manual "Pull in PR feedback" button. The data
            // model anticipates this; no background poller acts on it yet -- v1
            // is the explicit button only (see `pr::action_feedback`).
            "ALTER TABLE guardians ADD COLUMN auto_pr_feedback INTEGER NOT NULL DEFAULT 0",
            // RAL-118: provenance pointer set when a branch is moved into this
            // guardian from another review -- the *original* owning guardian id,
            // so a branch moved more than once still identifies where it truly
            // started (see `Store::move_guardian_branch`). NULL for a branch
            // that has never been moved.
            "ALTER TABLE guardian_branches ADD COLUMN moved_from_guardian_id TEXT",
            // RAL-122: stable, globally-unique branch id that survives reorders
            // and cross-guardian moves -- see the backfill block and unique index
            // below. `position` remains a mutable display-order attribute only.
            "ALTER TABLE guardian_branches ADD COLUMN id TEXT",
            // RAL-122: rename `branch_position` (an index that silently rots
            // across a reorder) to `branch_id` -- see the backfill-and-drop block
            // below.
            "ALTER TABLE guardian_pull_requests ADD COLUMN branch_id TEXT",
            // RAL-164: resolved/submitted values for named CheckInputs referenced
            // by manual_commands/action_hints, scoped to this guardian/review.
            // JSON map of {input_name: value}; a value here becomes the new
            // default the next time that check is viewed.
            "ALTER TABLE guardians ADD COLUMN input_values TEXT NOT NULL DEFAULT '{}'",
            // RAL-168: per-review override of the "Proof" scope -- one of
            // "each_branch"/"final_branch"/"nothing". NULL means "inherit the
            // project-level .ralphus.toml [review] proof_scope default"
            // (same nullable-override pattern as resolver_agent/resolver_model
            // above), resolved at hydration time into `effective_proof_scope`.
            "ALTER TABLE guardians ADD COLUMN proof_scope TEXT",
            // RAL-168: per-review override of the "each_branch" auto-clean-skip
            // sub-option. NULL means "inherit the project-level default".
            "ALTER TABLE guardians ADD COLUMN proof_skip_auto_clean INTEGER",
            // RAL-185: the machine this review's worktrees and merge run on.
            // NULL means the daemon's own host, which is every pre-RAL-185 row.
            "ALTER TABLE guardians ADD COLUMN machine TEXT",
            // RAL-250: per-review opt-out of the automatic base-branch
            // auto-update rebuild. NULL = inherit the project/global default.
            "ALTER TABLE guardians ADD COLUMN skip_base_updates INTEGER",
            // Proof steps never recorded their own token/cost usage -- only
            // cells did -- so a `prompt`/`command`-kind proof step's LLM
            // spend was silently discarded instead of being shown in the
            // board or folded into a lifetime cost total.
            "ALTER TABLE proofs ADD COLUMN tokens_in INTEGER NOT NULL DEFAULT 0",
            "ALTER TABLE proofs ADD COLUMN tokens_out INTEGER NOT NULL DEFAULT 0",
            "ALTER TABLE proofs ADD COLUMN cost_usd REAL NOT NULL DEFAULT 0",
            // RAL-326: prompt-cache write/read tokens, kept out of
            // `tokens_in` so that column keeps meaning uncached input. `0`
            // on every pre-RAL-326 row and on any backend whose harness
            // reports no cache breakdown -- indistinguishable, and
            // deliberately so: both mean "nothing to show here".
            "ALTER TABLE cells ADD COLUMN cache_creation_tokens INTEGER NOT NULL DEFAULT 0",
            "ALTER TABLE cells ADD COLUMN cache_read_tokens INTEGER NOT NULL DEFAULT 0",
            "ALTER TABLE proofs ADD COLUMN cache_creation_tokens INTEGER NOT NULL DEFAULT 0",
            "ALTER TABLE proofs ADD COLUMN cache_read_tokens INTEGER NOT NULL DEFAULT 0",
            // RAL-326: whether the recorded usage is a live mid-run snapshot
            // (process lost/cancelled/timed out before a terminal usage
            // event) rather than the backend's own final accounting.
            "ALTER TABLE cells ADD COLUMN cost_is_estimated INTEGER NOT NULL DEFAULT 0",
            "ALTER TABLE proofs ADD COLUMN cost_is_estimated INTEGER NOT NULL DEFAULT 0",
            // RAL-180: persist the effective read-only system prompt the board
            // shows in the details pane, separate from a cell's authored
            // `system_prompt` config so re-runs don't accidentally re-synthesise
            // from already-expanded text.
            "ALTER TABLE cells ADD COLUMN effective_system_prompt TEXT",
            "ALTER TABLE proofs ADD COLUMN effective_system_prompt TEXT",
            // RAL-185: the resolved machine a cell/proof runs on. NULL means
            // the daemon's own host, which is what every pre-RAL-185 row is.
            // RAL-190: a branch that contributes no diff over the stack tip
            // below it. Almost always means its task never committed, so the
            // review would otherwise look healthy while containing nothing.
            "ALTER TABLE guardian_branches ADD COLUMN is_empty INTEGER NOT NULL DEFAULT 0",
            "ALTER TABLE cells ADD COLUMN machine TEXT",
            "ALTER TABLE proofs ADD COLUMN machine TEXT",
            // RAL-174: free-form text a human attaches to a restart via the
            // board's restart popup. Kept separate from `content` so it can be
            // overwritten on every restart instead of merged/accumulated --
            // see `ghost::Store::set_ghost_user_note`.
            "ALTER TABLE ghosts ADD COLUMN user_note TEXT",
            // RAL-210: when a cell last transitioned to `running`, so the
            // board can show cell start time. Overwritten on every restart
            // (see `set_cell_state`) rather than kept as a first-start-only
            // value, per the ticket's decision that only the most recent
            // running-transition matters.
            "ALTER TABLE cells ADD COLUMN started_at_ms INTEGER",
            // RAL-190: the commit sha last pushed to `branch_alias` on the
            // remote, so a later sync check can tell "remote moved since we
            // last touched it" (a reviewer pushed to the PR branch) apart from
            // "remote still matches what we pushed" (safe to force-push again).
            // NULL for a PR row created before this column existed, or one
            // whose push has not completed yet -- both fall back to a
            // merge-base computation instead of a recorded baseline.
            "ALTER TABLE guardian_pull_requests ADD COLUMN last_pushed_sha TEXT",
            // RAL-156: opt-out from the automatic no-new-commits guard the
            // finalizer runs for git-backed tasks.
            "ALTER TABLE tasks ADD COLUMN no_commit_required INTEGER NOT NULL DEFAULT 0",
            // The GitHub-native PR stack number registered for this guardian's
            // chain of stacked PRs (`pr::submit_stack_for_guardian`), once 2+
            // branches have been submitted. NULL for GitLab reviews (no
            // equivalent concept) and for a GitHub review that hasn't
            // registered a stack yet.
            "ALTER TABLE guardians ADD COLUMN forge_stack_number INTEGER",
            // RAL-250: the `skip_base_updates` value a project stamped from the
            // live global config at first registration, so a later global
            // change doesn't retroactively flip an already-created project.
            // NULL = never stamped (pre-RAL-250 project, deliberately not
            // backfilled -- the ticket's explicit no-migration decision).
            "ALTER TABLE projects ADD COLUMN skip_base_updates INTEGER",
            // A project's authoritative remote provisioning source. `path`
            // remains the daemon-local checkout for compatibility and local
            // execution; no migration guesses this value from that checkout.
            "ALTER TABLE projects ADD COLUMN clone_url TEXT",
            // RAL-304: context-window/auto-compact resolved caps, delivered
            // to the backend via its own mechanism (env var/CLI arg/settings
            // file) -- see `ralphus_core::schema::agent_supports_maximum_context`.
            "ALTER TABLE cells ADD COLUMN maximum_context INTEGER",
            "ALTER TABLE cells ADD COLUMN auto_compact_threshold INTEGER",
            // RAL-314: the exact guardian a review-opted-in cell's membership
            // resolved to at submit time, set alongside `review_branch` by
            // `reviews::derive_reviews`. Read paths prefer this direct link
            // over the `cells.review_branch = guardian_branches.branch`
            // string join, which conflates unrelated guardians that happen to
            // share a branch name (e.g. repeat submissions against the same
            // worktree). NULL for a cell created before this column existed,
            // or one whose review linkage came from the manual
            // `POST /api/guardians/{id}/branches` attach path rather than
            // submission-time derivation -- both cases still need the
            // branch-string join as a fallback.
            "ALTER TABLE cells ADD COLUMN review_guardian_id TEXT",
            // RAL-333: tool-output token cap, delivered to the backend via
            // its own mechanism (env var/CLI arg/settings file) -- see
            // `ralphus_core::schema::agent_supports_maximum_tool_output_tokens`.
            "ALTER TABLE cells ADD COLUMN maximum_tool_output_tokens INTEGER",
            "ALTER TABLE proofs ADD COLUMN maximum_tool_output_tokens INTEGER",
            // RAL-332: UI-level convenience gate only -- there is no verified
            // login yet (RAL-252), so this does not stop anyone holding the
            // daemon's shared bearer token from calling the same endpoints
            // directly. See `crate::users`'s module doc comment.
            "ALTER TABLE users ADD COLUMN is_admin INTEGER NOT NULL DEFAULT 0",
            // RAL-332: per-row Cartographer visibility -- see
            // `crate::cartographer::Note::admin_only`'s doc comment. `0` for
            // every pre-RAL-332 row, unrestricted exactly as before.
            "ALTER TABLE cartographer_events ADD COLUMN admin_only INTEGER NOT NULL DEFAULT 0",
            // RAL-378: `1` for a branch registered after readable review
            // branches landed, `0` for every branch that predates them. The
            // discriminator has to be its own column rather than
            // `review_branch_name IS NULL`, because that name is resolved
            // lazily at the branch's first build and so is legitimately NULL
            // in between -- and it can't be `review_branch IS NULL` either,
            // since `reset_guardian_branch` clears that on every rebuild.
            // A `0` branch keeps its internal `guardian/<id>/wt-<branch>` ref
            // forever, so no PR already open against one ever moves.
            "ALTER TABLE guardian_branches ADD COLUMN readable_review_branch INTEGER NOT NULL DEFAULT 0",
            // RAL-378: the *readable* name this branch's review branch is
            // built under -- `<task-branch>-review`, collision-suffixed
            // (`-2`, `-3`, ...) by
            // `guardian_merge::claim_branch_review_branch_name`. Resolved at
            // the branch's first build and sticky from then on: deliberately
            // NOT cleared by `reset_guardian_branch`/`reset_guardian_branches`
            // (which do clear `review_branch`), because re-resolving on every
            // rebuild would walk the suffix forward (`-2` -> `-3` -> ...) and
            // orphan any PR already open on the previous name. Only ever set
            // when `readable_review_branch` is `1`.
            "ALTER TABLE guardian_branches ADD COLUMN review_branch_name TEXT",
            // RAL-378: the branch-level pair of columns above, for a
            // combined-worktree review's single shared branch. The name is
            // derived from the review's own `name` rather than from any one
            // task branch, and -- being sticky -- survives a later rename of
            // the review unchanged.
            "ALTER TABLE guardians ADD COLUMN readable_review_branch INTEGER NOT NULL DEFAULT 0",
            "ALTER TABLE guardians ADD COLUMN review_branch_name TEXT",
            // RAL-378: whether this review pushes its PR to a branch *other*
            // than the review branch itself. NULL inherits the project/global
            // `[review] separate_pr_branch` default, which resolves to `0` --
            // the PR branch and the review branch are one and the same, and
            // `forge.pull_request_branch_convention`/`match_pr_branch_name`
            // are not consulted at all. See
            // `GuardianView::effective_separate_pr_branch`.
            "ALTER TABLE guardians ADD COLUMN separate_pr_branch INTEGER",
            // RAL-378: the `separate_pr_branch` value a project stamped from
            // the live global config when it was first registered, so a later
            // global change does not retroactively flip reviews created under
            // the old one. Same always-from-global shape, and the same
            // deliberate no-backfill (NULL for every project registered
            // earlier), as `auto_submit_pr_stack`.
            "ALTER TABLE projects ADD COLUMN separate_pr_branch INTEGER",
        ] {
            let _ = self.conn.execute(stmt, []);
        }
        // Codex support: `*claude_session_id` columns were named after the only
        // CLI harness that existed at the time, but they hold the resumable
        // cell/thread id of *whichever* CLI agent produced it (claude-code or
        // codex) -- renamed here to `*agent_session_id` for accuracy. A best-effort
        // rename against a database that still has the old column name; harmless
        // no-op (old column already renamed, or never existed) otherwise. The
        // fallback ADD COLUMN afterwards guarantees the new column exists even for
        // a database old enough to have neither -- mirrors the "fails harmlessly"
        // idiom of the ADD-COLUMN loop above, just with RENAME COLUMN first.
        for stmt in [
            "ALTER TABLE cells RENAME COLUMN claude_session_id TO agent_session_id",
            "ALTER TABLE proofs RENAME COLUMN claude_session_id TO agent_session_id",
            "ALTER TABLE guardian_branches RENAME COLUMN resolver_claude_session_id TO resolver_agent_session_id",
            "ALTER TABLE guardians RENAME COLUMN manual_commands_claude_session_id TO manual_commands_agent_session_id",
            // RAL-364: `follows` was renamed to `watches`; same best-effort
            // rename/fallback-ADD-COLUMN idiom as the `*claude_session_id`
            // columns above.
            "ALTER TABLE users RENAME COLUMN auto_follow TO auto_watch",
        ] {
            let _ = self.conn.execute(stmt, []);
        }
        for stmt in [
            "ALTER TABLE cells ADD COLUMN agent_session_id TEXT",
            "ALTER TABLE proofs ADD COLUMN agent_session_id TEXT",
            "ALTER TABLE guardian_branches ADD COLUMN resolver_agent_session_id TEXT",
            "ALTER TABLE guardians ADD COLUMN manual_commands_agent_session_id TEXT",
            // RAL-149: opts the per-branch conflict-resolution fix pass into also
            // running the quality-bar instructions (formatters/linters/tests),
            // in addition to always running them in the dedicated final-proof
            // call that follows a fix pass. Default off -- quality checks may
            // incur real cost, so they run once (in the final-proof call) by
            // default rather than twice per conflict-resolution cycle.
            "ALTER TABLE guardians ADD COLUMN proof_mid_resolution INTEGER NOT NULL DEFAULT 0",
            // RAL-150: persistent, user-set environment-variable overrides applied
            // to every subprocess spawned for this squad (agent + command cells,
            // and proof steps). JSON map of {key: value}; set/unset via
            // `POST /api/squads/{id}/env`, survives across retries until unset.
            "ALTER TABLE squads ADD COLUMN env_overrides TEXT NOT NULL DEFAULT '{}'",
            // Hierarchical env overrides (RAL-150 extension): task/cell-level
            // layers, plus separate layers for a task's/cell's own proof
            // steps, each overriding its parent's values on a per-key basis --
            // squad < task < cell, and squad < task < task.proof /
            // squad < task < cell < cell.proof. See
            // `Store::resolve_cell_env_overrides` and siblings.
            "ALTER TABLE tasks ADD COLUMN env_overrides TEXT NOT NULL DEFAULT '{}'",
            "ALTER TABLE tasks ADD COLUMN proof_env_overrides TEXT NOT NULL DEFAULT '{}'",
            "ALTER TABLE cells ADD COLUMN env_overrides TEXT NOT NULL DEFAULT '{}'",
            "ALTER TABLE cells ADD COLUMN proof_env_overrides TEXT NOT NULL DEFAULT '{}'",
            // RAL-267: the effective env map a cell first started with, after
            // placeholder expansion against its task's project type. Stored
            // separately from the authored override layers so retries/restarts
            // reuse the already-materialized values without changing what the
            // board/API means by `env_overrides`.
            "ALTER TABLE cells ADD COLUMN materialized_env_overrides TEXT",
            // RAL-157: a task can be "soloed" to pause its non-soloed siblings
            // within the same squad -- see `Store::solo_task`/`unsolo_task` and the
            // scheduler dispatcher's live solo gate. Multiple tasks in the same
            // squad can be soloed at once; default 0 (not soloed) preserves today's
            // behavior for every existing squad.
            "ALTER TABLE tasks ADD COLUMN soloed INTEGER NOT NULL DEFAULT 0",
            // RAL-161: resolved per-cell USD spend cap (cell overrides
            // task). Exceeding the live `cost_usd` kills the cell mid-run.
            "ALTER TABLE cells ADD COLUMN maximum_budget_usd REAL",
            // RAL-191: the narrowest env-override layer -- one individual proof
            // step's own variables, merged on top of its owning scope's
            // `proof_env_overrides`. Per-step rather than per-scope because
            // `proof` is an array: two `[[task.proof]]` blocks setting the
            // same key to different values must not collide. See
            // `Store::resolve_task_proof_step_env_overrides` and its sibling.
            "ALTER TABLE proofs ADD COLUMN env_overrides TEXT NOT NULL DEFAULT '{}'",
            // RAL-267: proof-step analogue of
            // `cells.materialized_env_overrides`.
            "ALTER TABLE proofs ADD COLUMN materialized_env_overrides TEXT",
            // RAL-191: a review branch's own env overrides, layered on top of
            // whatever its *source cell* resolves to, so a review worktree
            // inherits the environment the work was produced under. Unlike
            // every other layer this one is a JSON map of {key: value|null},
            // where `null` is a tombstone meaning "remove this inherited key
            // entirely" -- see `Store::resolve_guardian_branch_env`.
            "ALTER TABLE guardian_branches ADD COLUMN env_overrides TEXT NOT NULL DEFAULT '{}'",
            // RAL-203: the combined review worktree has no upstream task
            // cell of its own to inherit an environment from, so the
            // finalize-time build/check-gate step against it instead borrows
            // the last enabled branch's own resolved environment
            // (`guardian::combined_env_from_branches`) -- this column layers
            // the review's own build-step overrides on top of that.
            // Same `{key: value|null}` shape as `guardian_branches.env_overrides`.
            "ALTER TABLE guardians ADD COLUMN build_env_overrides TEXT NOT NULL DEFAULT '{}'",
            // RAL-203: same shape and baseline, for the manual-checks step
            // (the LLM-suggested commands run via `ralphus review checks
            // run` / the board's "Run all"). Independent of
            // `build_env_overrides` -- setting one never affects the other.
            "ALTER TABLE guardians ADD COLUMN manual_checks_env_overrides TEXT NOT NULL DEFAULT '{}'",
            // RAL-193: this review's own USD spend cap (from `[[review]]`'s
            // `maximum_budget_usd`), enforced against the cumulative sum of
            // `guardian_costs` the same way a task/cell cap is enforced
            // against a live `cost_usd` (RAL-161).
            "ALTER TABLE guardians ADD COLUMN maximum_budget_usd REAL",
            // RAL-193: incrementing counter bumped once per merge/rebase
            // attempt, so cost line items in `guardian_costs` can be
            // attributed to the attempt that produced them.
            "ALTER TABLE guardians ADD COLUMN merge_attempt INTEGER NOT NULL DEFAULT 0",
            // Details-pane "time running" / "started at" (UTC): when a squad/task/
            // cell first entered `running` and when it last reached a terminal
            // state. NULL until reached. Distinct from `created_at_ms`
            // (submission/queue time), which can differ from actual execution
            // start. See `Store::set_squad_state`/`set_task_state`/
            // `set_cell_state`/`record_cell_result` for where these are
            // stamped, and the restart/reset paths that clear them for entities
            // being genuinely re-executed. `cells.started_at_ms` is already
            // added by the RAL-210 migration above, so only `finished_at_ms`
            // is needed for `cells` here.
            "ALTER TABLE squads ADD COLUMN started_at_ms INTEGER",
            "ALTER TABLE squads ADD COLUMN finished_at_ms INTEGER",
            "ALTER TABLE tasks ADD COLUMN started_at_ms INTEGER",
            "ALTER TABLE tasks ADD COLUMN finished_at_ms INTEGER",
            "ALTER TABLE cells ADD COLUMN finished_at_ms INTEGER",
            // RAL-259: when a review branch's conflict-resolver agent (fix pass
            // or final-proof call) most recently began running, so the Review
            // Live View can show both when work began and how long it's been
            // going. NULL until a resolver session actually starts. Stamped
            // via COALESCE once per merge attempt and cleared at each attempt
            // start (see `Store::clear_branch_started_at`/`stamp_branch_started_at`),
            // mirroring the cell `started_at_ms` pattern (RAL-210).
            "ALTER TABLE guardian_branches ADD COLUMN started_at_ms INTEGER",
            // RAL-259: when this review's manual-checks generation agent most
            // recently began work, for the manual-checks Live View panel. NULL
            // until generation starts. Stamped fresh on every regeneration.
            "ALTER TABLE guardians ADD COLUMN manual_checks_started_at_ms INTEGER",
            // RAL-155: path to an on-disk log file a Cartographer row
            // references (e.g. a RAL-154 durable terminal-log attempt file),
            // carried by path rather than embedding the file's content — see
            // `crate::cartographer::CartographerRow::log_path`.
            "ALTER TABLE cartographer_events ADD COLUMN log_path TEXT",
            // RAL-271: cosmetic "out of date" badge -- set whenever a
            // task's/cell's own `env_overrides` (or, for a task/cell, its
            // own proof steps' `proof_env_overrides`/per-step
            // `env_overrides`) is edited after the row already exists,
            // cascading exactly one ownership level down (task edit marks
            // its cells; cell edit marks its own proof steps). Cleared when
            // the row is reset to `pending` by a restart/retry, or by any
            // `set_task_state`/`set_cell_state`/`set_proof_state` call
            // (covers both an explicit `Set Status` and the scheduler's own
            // transition into `running`). Purely informational -- never
            // read by the scheduler or proof logic.
            "ALTER TABLE tasks ADD COLUMN env_out_of_date INTEGER NOT NULL DEFAULT 0",
            "ALTER TABLE cells ADD COLUMN env_out_of_date INTEGER NOT NULL DEFAULT 0",
            "ALTER TABLE proofs ADD COLUMN env_out_of_date INTEGER NOT NULL DEFAULT 0",
            // RAL-272: scopes a feedback-chat message to one review branch, so the
            // board can show a per-branch read-only thread instead of one global
            // thread. NULL for every pre-existing row (the old global RAL-22
            // thread) -- those rows simply never match a branch-scoped query,
            // which is fine since this ticket doesn't migrate old history.
            "ALTER TABLE guardian_messages ADD COLUMN branch_id TEXT",
            // RAL-288 Stage 6: set by `resume_automation` (an explicit human
            // trigger, never by a generic `restart_cell`) right before it
            // resets the cell to `pending`, so the scheduler's next dispatch
            // of this specific cell resumes the *cell's own* previously
            // recorded `agent_session_id` instead of starting a fresh
            // conversation -- the normal dispatch path only ever looks at a
            // *dependency's* session (RAL-248 cross-cell sharing), never a
            // cell's own prior one. Consumed and cleared by
            // `run_cell_worker` the moment it's read, so it can never leak
            // into a later, unrelated restart of the same cell.
            "ALTER TABLE cells ADD COLUMN force_resume_own_session INTEGER NOT NULL DEFAULT 0",
            // RAL-288: when this cell was cleanly stopped ("Open Agent" on a
            // still-running cell) for a real interactive agent session to
            // take over. NULL means not detached. The cell's own `state`
            // stays `running` throughout -- this is a separate, additive
            // signal so the board can tell "paused for a human" apart from
            // "actively executing headlessly" without a new NodeState.
            // Cleared the moment the cell is next dispatched (a restart, or
            // the explicit resume-automation trigger), at the same point
            // `state` is set back to `Running` for a fresh attempt.
            "ALTER TABLE cells ADD COLUMN detached_at_ms INTEGER",
            // Cross-cell session sharing (RAL-248) used to be implicit in any
            // `depends_on` link with no opt-out; this makes it opt-in. Resolved
            // at submit from the cell's/task's `share_session` TOML field --
            // see `ralphus_core::schema::resolve_cell_share_session`.
            "ALTER TABLE cells ADD COLUMN share_session INTEGER NOT NULL DEFAULT 0",
            // RAL-291: failure detail for a task-level failure with no
            // underlying cell/proof error to point to (e.g. the RAL-156
            // no-commits-since-baseline guard) -- mirrors the `cells.error`
            // column at task granularity. NULL for a task that never failed
            // this way, including one that failed because a child cell/proof
            // did (that failure is already visible on the cell/proof itself).
            "ALTER TABLE tasks ADD COLUMN error TEXT",
            // RAL-273: a one-shot, GUI-facing notice (e.g. "a GitHub reorder
            // interrupted your in-flight local reorder") surfaced once and
            // cleared by whichever poll first observes `notice_at_ms` newer
            // than what it last showed -- see `Store::set_guardian_notice`.
            "ALTER TABLE guardians ADD COLUMN notice_kind TEXT",
            "ALTER TABLE guardians ADD COLUMN notice_message TEXT",
            "ALTER TABLE guardians ADD COLUMN notice_at_ms INTEGER",
            // RAL-277: source timestamp of the last base-ref edit. This is
            // separate from `updated_at_ms`, which changes for unrelated
            // review activity and cannot arbitrate base edits correctly.
            "ALTER TABLE guardians ADD COLUMN base_changed_at_ms INTEGER NOT NULL DEFAULT 0",
            "UPDATE guardians SET base_changed_at_ms=updated_at_ms WHERE base_changed_at_ms=0",
            // RAL-279: the base ref this daemon last confirmed the forge
            // actually accepted for this PR (set only after a successful
            // `update_pull_request_base` forge call, mirroring
            // `last_pushed_sha`'s "last state both sides are known to have
            // agreed on" role but for the base ref instead of the branch
            // tip). The forge-side drift poll uses this alongside `base_ref`
            // to tell "the forge genuinely retargeted this PR" apart from
            // "our own last resync's forge PATCH just hasn't landed/failed",
            // so neither direction thrashes the other on its next pass — see
            // `pr::poll_pr_base_drift`.
            "ALTER TABLE guardian_pull_requests ADD COLUMN last_pushed_base_ref TEXT",
            // RAL-302: identifies which single "submit a stack" call created a
            // PR row, so a past submission's sibling branches are queryable as
            // one group instead of guessed at via timestamp proximity. NULL
            // for a row created before this column existed.
            "ALTER TABLE guardian_pull_requests ADD COLUMN stack_id TEXT",
            // RAL-302: `settle_pr_merge_states` used to hard-DELETE a PR row
            // once its linked PR merged out-of-band mid-flight (RAL-300),
            // which lost the history a "view past PR stacks" screen needs.
            // It now soft-deletes by setting `state='dropped'` and recording
            // why here, leaving the row (and its `stack_id` grouping) queryable.
            "ALTER TABLE guardian_pull_requests ADD COLUMN dropped_reason TEXT",
            // RAL-307: per-review opt-in to default a newly submitted PR's
            // branch to the exact worktree/feature branch name instead of the
            // convention-derived alias. NULL = inherit the project/global
            // default, same layering as `skip_base_updates`.
            "ALTER TABLE guardians ADD COLUMN match_pr_branch_name INTEGER",
            // RAL-307: the `match_pr_branch_name` value a project stamped from
            // the live global config (or an explicit `ralphus project git`
            // flag) at first registration, so a later global change doesn't
            // retroactively flip an already-created project. NULL = never
            // stamped (pre-RAL-307 project, deliberately not backfilled, same
            // as `skip_base_updates`).
            "ALTER TABLE projects ADD COLUMN match_pr_branch_name INTEGER",
            // RAL-317: per-review opt-in to auto-submit/grow the PR stack as
            // each branch reaches a terminal (`done`/`conflict_resolved`)
            // merge state, instead of requiring the manual `review pr
            // submit` call. NULL = inherit the project/global default, same
            // layering as `match_pr_branch_name`.
            "ALTER TABLE guardians ADD COLUMN auto_submit_pr_stack INTEGER",
            // RAL-317: the `auto_submit_pr_stack` value a project stamped
            // from the live global config at first registration, so a later
            // global change doesn't retroactively flip an already-created
            // project. NULL = never stamped (pre-RAL-317 project,
            // deliberately not backfilled, same as `match_pr_branch_name`).
            "ALTER TABLE projects ADD COLUMN auto_submit_pr_stack INTEGER",
            // RAL-317: a one-shot, per-branch failure marker for the most
            // recent auto-submit attempt on this branch (best-effort side
            // channel -- never blocks a Guardian merge transition). NULL
            // means no failure to report; cleared again by the next
            // successful auto-submit attempt on this branch.
            "ALTER TABLE guardian_branches ADD COLUMN auto_submit_error TEXT",
            // RAL-318: distinguishes a review the Arbiter created by draining a
            // Triage pool (`crate::guardian::GUARDIAN_ORIGIN_ARBITER`) from one
            // an explicit `[[review]]` block declared
            // (`crate::guardian::GUARDIAN_ORIGIN_EXPLICIT`, the default for
            // every existing/manually-created row).
            "ALTER TABLE guardians ADD COLUMN origin TEXT NOT NULL DEFAULT 'explicit'",
            // RAL-320: the `EntityUri` a mailbox message is about, so a
            // user's personal watches can match against it (see
            // `crate::mailbox::personal_mailbox_messages_for_user`). NULL for
            // pre-RAL-320 rows and for messages with no addressable entity.
            "ALTER TABLE mailbox_messages ADD COLUMN entity_uri TEXT",
            "ALTER TABLE mailbox_messages ADD COLUMN event_kind TEXT",
            // RAL-320: per-user preference, consulted by the `ralphus submit`
            // auto-watch hook -- when set, every entity a user submits is
            // watched automatically using `default_notify_tiers` below.
            "ALTER TABLE users ADD COLUMN auto_watch INTEGER NOT NULL DEFAULT 0",
            // RAL-320: the `MailboxPriority` tier set (CSV, see
            // `crate::mailbox::{parse_tiers, tiers_to_csv}`) a new watch
            // defaults to when the caller doesn't specify one explicitly.
            "ALTER TABLE users ADD COLUMN default_notify_tiers TEXT NOT NULL DEFAULT 'urgent,high,normal'",
            // RAL-375: feedback text still awaiting application by
            // `guardian_merge::run_feedback`, persisted the moment feedback is
            // received (see `Store::set_branch_pending_feedback`) rather than
            // held only as a spawned thread's in-memory argument -- so an
            // unclean shutdown mid-`run_feedback` leaves a durable record
            // startup recovery can find and reapply, instead of the feedback
            // being silently dropped. Cleared by every real completion path
            // (success or a legitimate failure); only a literal crash mid-run
            // leaves it set.
            "ALTER TABLE guardian_branches ADD COLUMN pending_feedback TEXT",
            // RAL-373: total input tokens spent on Claude Code's own
            // auto-compaction summarization requests -- billed at the
            // *uncached* input rate, the reason `cost_usd` and
            // `tokens_in + cache_creation_tokens + cache_read_tokens`
            // diverge on any cell that compacts. `compaction_count` is the
            // number of compactions observed, incremented independently of
            // whether each one's input size was reported: a nonzero count
            // paired with `compaction_input_tokens = 0` means "compactions
            // happened, sizes unreported by this backend/version", not "no
            // compaction happened". `0` on every pre-RAL-373 row and for a
            // backend that reports no compaction data (`pi`, `codex`).
            "ALTER TABLE cells ADD COLUMN compaction_input_tokens INTEGER NOT NULL DEFAULT 0",
            "ALTER TABLE cells ADD COLUMN compaction_count INTEGER NOT NULL DEFAULT 0",
            "ALTER TABLE proofs ADD COLUMN compaction_input_tokens INTEGER NOT NULL DEFAULT 0",
            "ALTER TABLE proofs ADD COLUMN compaction_count INTEGER NOT NULL DEFAULT 0",
            // RAL-342: this review's own declared build step, authored via
            // `[[review.auto_build]]` and resolved once at submit time
            // (`reviews::derive_reviews`) into a JSON-serialized
            // `GuardianAutoBuild`. NULL means the review declared
            // `skip_auto_build = true` instead -- unlike the
            // nullable-override columns above (`resolver_agent`,
            // `proof_scope`, ...), NULL here never means "inherit the
            // project config default": every guardian created after this
            // migration has one of `auto_build_json` or `skip_auto_build`
            // set, enforced by `reviews::require_auto_build_declaration` at
            // submit time. A guardian created before this shipped (neither
            // column meaningfully set) simply gets no auto_build tier at
            // finalize time (see `guardian_merge::final_checks`).
            "ALTER TABLE guardians ADD COLUMN auto_build_json TEXT",
            // Registered-project creation identity. NULL preserves the
            // distinct raw-directory creation route for existing rows.
            "ALTER TABLE guardians ADD COLUMN project TEXT",
            // RAL-338: set on a fork-internal PR row once reconcile-first
            // promotion closes it and files a fresh cross-repository PR
            // against the parent in its place (its own branch became the
            // stack's new root after the prior root merged) -- names the
            // superseding row's own `id`. NULL for every other row,
            // including one that was never promoted or is itself the
            // current promoted replacement. Kept (not deleted) so the
            // closed PR's discussion stays visible in `PrStackView`
            // history, per this ticket's Q3.3.
            "ALTER TABLE guardian_pull_requests ADD COLUMN superseded_by TEXT",
            // RAL-379: dual identity on a feedback-thread message. `author` is
            // the registered user the feedback is attributed to (client-set,
            // defaulting to the submitter) -- this is who the UI shows.
            // `submitted_by` is the requester resolved from `X-Ralphus-User` /
            // `[daemon].default_user` at the time the request was made and can
            // never be set by request data -- kept for audit/provenance only,
            // never shown in the UI. NULL on both for every pre-existing row
            // and for a "guardian"-role message, which has no human author.
            // Caller-claimed until RAL-252 makes authentication authoritative.
            "ALTER TABLE guardian_messages ADD COLUMN author TEXT",
            "ALTER TABLE guardian_messages ADD COLUMN submitted_by TEXT",
            // RAL-380: durable, read-only completion status for a "reviewer"-role
            // message -- `received` (Ralphus accepted the feedback and started
            // applying it), `done` (the resolver agent's pass finished
            // successfully), `failed` (it errored), or `superseded` (a newer
            // feedback message was submitted on the same branch before this one
            // finished, so its outcome is no longer trustworthy). Set at insert
            // time by `Store::add_guardian_message` for every "reviewer"-role row
            // and NULL for a "guardian"-role row, which isn't actionable. NULL
            // for every pre-existing row.
            "ALTER TABLE guardian_messages ADD COLUMN action_status TEXT",
            // RAL-375: a broad classification of what a mailbox message is
            // about (e.g. `"review"` for a PR/CI-watch notice), so a client
            // like QuickStart Reviewer can default to draining only messages
            // in its own category instead of everything broadcast. NULL for
            // pre-RAL-375 rows and for messages with no specific category --
            // treated as "not this category" by a category filter (see
            // `crate::mailbox::mailbox_messages_for_client_filtered`).
            "ALTER TABLE mailbox_messages ADD COLUMN category TEXT",
        ] {
            let _ = self.conn.execute(stmt, []);
        }
        // RAL-155: task-scoped Cartographer filtering (`?task=`, and the
        // `entity=task:...` addressing scheme) needs this to not degrade into
        // a full-table scan as `cartographer_events` grows. Created after the
        // ALTER-TABLE migrations above, same reasoning as `idx_cells_review_branch`.
        let _ = self.conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_carto_task ON cartographer_events(task)",
            [],
        );
        // RAL-121: `hydrate_guardian` looks up each branch's most recent cell
        // by `review_branch` (set once, at submit time, by
        // `reviews::derive_reviews` -> `set_cell_review_branch`; RAL-118's
        // `move_guardian_branch` reassigns a branch's *guardian*, never its
        // `cells.review_branch` value, so this stays correct across moves).
        // Without an index every guardian-list load did a full `cells` table
        // scan per branch; `cells` only grows over a project's life. Created
        // after the ALTER-TABLE migrations above so it's safe against a database
        // created before the `review_branch` column existed.
        let _ = self.conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_cells_review_branch ON cells(review_branch)",
            [],
        );
        // RAL-314: same reasoning as `idx_cells_review_branch` above, for the
        // direct guardian-id join `reviews_by_branch`/`collecting_guardians_for_cells`
        // now prefer over the branch-string join.
        let _ = self.conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_cells_review_guardian_id ON cells(review_guardian_id)",
            [],
        );
        // RAL-122: enforce branch-id uniqueness at the DB layer (not just via
        // `next_id`'s own monotonic guarantee) -- a separate index because
        // SQLite's `ALTER TABLE ADD COLUMN` can't itself declare `UNIQUE` on a
        // non-empty table.
        let _ = self.conn.execute(
            "CREATE UNIQUE INDEX IF NOT EXISTS idx_guardian_branches_branch_id ON guardian_branches(id)",
            [],
        );
        // RAL-110: one-time backfill of the old `skip_checks` column (present on
        // any database created before this change) into both new columns --
        // preserving prior behavior exactly (skip_checks used to gate both the
        // finalize-time build/check step AND the per-worktree quality prompt)
        // until a user explicitly changes one of the split flags. Guarded on the
        // column still existing so this runs exactly once: the DROP COLUMN below
        // makes it a no-op (harmless error, ignored) on every later startup,
        // instead of re-clobbering a later explicit toggle back to the old value.
        let has_old_skip_checks = self
            .conn
            .prepare("SELECT 1 FROM pragma_table_info('guardians') WHERE name='skip_checks'")
            .and_then(|mut s| s.query_row([], |_| Ok(())).optional())
            .unwrap_or(None)
            .is_some();
        if has_old_skip_checks {
            let _ = self.conn.execute(
                "UPDATE guardians SET skip_auto_build = skip_checks, \
                 skip_worktree_checks = skip_checks",
                [],
            );
            let _ = self
                .conn
                .execute("ALTER TABLE guardians DROP COLUMN skip_checks", []);
        }
        // RAL-168: `verify_mid_resolution` is retired -- replaced outright by
        // `proof_scope`/`proof_skip_auto_clean` above, not mapped forward
        // (its old meaning -- also run quality-bar checks during the fix pass
        // -- no longer exists now that the fix pass never runs them; see
        // `resolve_conflicts_with_agent` in `guardian_merge.rs`). No backfill
        // needed: every existing review simply gets the new columns' default
        // "inherit the project default" (NULL), which resolves to
        // "each_branch" -- the documented AC that existing reviews preserve
        // today's default proof behavior. Same guard-on-column-existing
        // idiom as the `skip_checks` block above, so the DROP runs exactly once.
        let has_old_verify_mid_resolution = self
            .conn
            .prepare(
                "SELECT 1 FROM pragma_table_info('guardians') WHERE name='verify_mid_resolution'",
            )
            .and_then(|mut s| s.query_row([], |_| Ok(())).optional())
            .unwrap_or(None)
            .is_some();
        if has_old_verify_mid_resolution {
            let _ = self.conn.execute(
                "ALTER TABLE guardians DROP COLUMN verify_mid_resolution",
                [],
            );
        }
        // RAL-122: one-time backfill of `guardian_branches.id` for any row
        // created before this change -- naturally idempotent, since the
        // `WHERE id IS NULL` filter makes it a no-op once every row has one.
        let needs_branch_id_backfill: Vec<(String, i64)> = self
            .conn
            .prepare("SELECT guardian_id, position FROM guardian_branches WHERE id IS NULL")?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        for (guardian_id, position) in needs_branch_id_backfill {
            let branch_id = self.next_id("branch_seq", "branch")?;
            self.conn.execute(
                "UPDATE guardian_branches SET id = ?1 WHERE guardian_id = ?2 AND position = ?3",
                params![branch_id, guardian_id, position],
            )?;
        }
        // RAL-122: backfill `guardian_pull_requests.branch_id` from the old
        // `branch_position` column (an index) by joining to the now-backfilled
        // `guardian_branches.id`, then drop the old column -- same
        // guard-on-column-existing idiom as the `skip_checks` block above, so
        // this runs exactly once.
        let has_old_branch_position = self
            .conn
            .prepare(
                "SELECT 1 FROM pragma_table_info('guardian_pull_requests') WHERE name='branch_position'",
            )
            .and_then(|mut s| s.query_row([], |_| Ok(())).optional())
            .unwrap_or(None)
            .is_some();
        if has_old_branch_position {
            let _ = self.conn.execute(
                "UPDATE guardian_pull_requests
                 SET branch_id = (
                     SELECT gb.id FROM guardian_branches gb
                     WHERE gb.guardian_id = guardian_pull_requests.guardian_id
                       AND gb.position = guardian_pull_requests.branch_position
                 )
                 WHERE branch_position IS NOT NULL",
                [],
            );
            let _ = self.conn.execute(
                "ALTER TABLE guardian_pull_requests DROP COLUMN branch_position",
                [],
            );
        }
        // RAL-293: the no-new-commits guard now reads a worktree's own
        // `@{upstream}` live (`crate::reviews::worktree_has_commits_ahead_of_upstream`)
        // instead of comparing HEAD against a squad-run-scoped baseline sha
        // captured once at cell-start -- the baseline approach falsely failed
        // a task whose worktree was reused across squad runs, since the
        // baseline was captured *after* an earlier run's real commit already
        // landed. No backfill needed: the column held only a transient,
        // run-scoped value, never anything worth preserving. Same
        // guard-on-column-existing idiom as the `skip_checks` block above, so
        // the DROP runs exactly once.
        let has_old_baseline_commit_sha = self
            .conn
            .prepare("SELECT 1 FROM pragma_table_info('tasks') WHERE name='baseline_commit_sha'")
            .and_then(|mut s| s.query_row([], |_| Ok(())).optional())
            .unwrap_or(None)
            .is_some();
        if has_old_baseline_commit_sha {
            let _ = self
                .conn
                .execute("ALTER TABLE tasks DROP COLUMN baseline_commit_sha", []);
        }
        // RAL-386: broaden `guardian_worktree_retirements.status`'s CHECK
        // constraint to add `deferred`/`opted_out` and add `retry_at_ms`.
        // SQLite cannot alter a CHECK constraint or add a column with a
        // meaningful default retroactively in place, so a database created
        // under the RAL-385 schema is migrated by rebuilding the table --
        // detected by sniffing its own recorded DDL for the new status,
        // rather than a version counter this codebase doesn't otherwise
        // keep. Every existing row is `retired` or `failed` (the only
        // statuses that ever existed before this migration), both still
        // valid under the broadened constraint, so the copy is a plain
        // `INSERT ... SELECT` with `retry_at_ms` defaulting to `NULL`.
        let retirements_need_rebuild = self
            .conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type='table' AND name='guardian_worktree_retirements'",
                [],
                |r| r.get::<_, String>(0),
            )
            .map(|sql| !sql.contains("opted_out"))
            .unwrap_or(false);
        if retirements_need_rebuild {
            self.conn.execute_batch(
                "ALTER TABLE guardian_worktree_retirements RENAME TO guardian_worktree_retirements_ral385;
                 CREATE TABLE guardian_worktree_retirements (
                     guardian_id     TEXT NOT NULL REFERENCES guardians(id) ON DELETE CASCADE,
                     path            TEXT NOT NULL,
                     status          TEXT NOT NULL CHECK(status IN ('retired', 'failed', 'deferred', 'opted_out')),
                     error           TEXT,
                     eligible_at_ms  INTEGER NOT NULL,
                     last_attempt_ms INTEGER NOT NULL,
                     retry_at_ms     INTEGER,
                     PRIMARY KEY (guardian_id, path)
                 );
                 INSERT INTO guardian_worktree_retirements
                     (guardian_id, path, status, error, eligible_at_ms, last_attempt_ms, retry_at_ms)
                 SELECT guardian_id, path, status, error, eligible_at_ms, last_attempt_ms, NULL
                 FROM guardian_worktree_retirements_ral385;
                 DROP TABLE guardian_worktree_retirements_ral385;",
            )?;
        }
        Ok(())
    }

    /// Allocate the next id for a sequence stored in `meta`, formatted as
    /// `<prefix>-<zero-padded seq>`.
    pub(crate) fn next_id(&self, seq_key: &str, prefix: &str) -> Result<String> {
        self.conn.execute(
            "INSERT INTO meta(key, value) VALUES(?1, 1)
             ON CONFLICT(key) DO UPDATE SET value = value + 1",
            params![seq_key],
        )?;
        let seq: i64 = self.conn.query_row(
            "SELECT value FROM meta WHERE key=?1",
            params![seq_key],
            |r| r.get(0),
        )?;
        Ok(format!("{prefix}-{seq:012}"))
    }

    fn next_squad_id(&self) -> Result<String> {
        self.next_id("squad_seq", "squad")
    }

    /// Ingest a validated task file, returning the new squad id. `hold` submits to
    /// `Queued` (staged); otherwise the squad goes straight to `Pending`.
    pub fn insert_squad(
        &mut self,
        file: &TaskFile,
        label: Option<&str>,
        hold: bool,
    ) -> Result<String> {
        let squad_id = self.next_squad_id()?;
        self.insert_squad_with_id(&squad_id, file, label, hold)?;
        Ok(squad_id)
    }

    /// Shared implementation behind [`Self::insert_squad`], taking `squad_id` as
    /// a parameter instead of always minting a fresh one — lets a test
    /// supply its own caller-chosen id (RAL-177) instead of the store's
    /// deterministic sequential one. A fresh in-memory `Store` always
    /// assigns the same first id (`squad-000000000001`), which is identical
    /// across every worktree's identical test — fine for a DB-only
    /// assertion, but a collision risk for a live-tmux test whose fixture
    /// squad_id also seeds a *real*, machine-wide tmux session name
    /// (`ralphus_{squad_id}_...`) or feeds a prefix-scoped kill
    /// (`kill_run_tmux_sessions`/`kill_guardian_tmux_sessions` in
    /// `server.rs`) against the same shared psmux server. Letting such a
    /// test supply its own per-process-unique id (see
    /// `crate::tmux::unique_test_tag`) closes that gap without touching the
    /// production id-assignment path or any of `insert_squad`'s existing
    /// callers. `pub(crate)`, not `pub`, since only this crate's own tests
    /// need direct access to the id.
    pub(crate) fn insert_squad_with_id(
        &mut self,
        squad_id: &str,
        file: &TaskFile,
        label: Option<&str>,
        hold: bool,
    ) -> Result<()> {
        let now = now_ms();
        let state = if hold {
            SquadState::Queued
        } else {
            SquadState::Pending
        };

        let squad_deps = file
            .defaults
            .first()
            .map(|d| d.depends_on.as_slice())
            .unwrap_or(&[]);
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO squads(id, label, state, depends_on, created_at_ms, updated_at_ms) VALUES(?,?,?,?,?,?)",
            params![squad_id, label, state.as_str(), to_json(squad_deps), now, now],
        )?;

        for (t_idx, task) in file.task.iter().enumerate() {
            let t_idx_i = i64::try_from(t_idx).unwrap_or(0);
            tx.execute(
                "INSERT INTO tasks(squad_id, idx, name, project, agent, model, state, depends_on, queue_rank, env_overrides, no_commit_required) VALUES(?,?,?,?,?,?,?,?,?,?,?)",
                params![
                    squad_id,
                    t_idx_i,
                    task.name,
                    task.project,
                    task.agent,
                    task.model,
                    NodeState::Pending.as_str(),
                    to_json(&task.depends_on),
                    task.priority.map(f64::from),
                    // RAL-172: TOML-declared `environment` seeds this task's
                    // row in the same hierarchical env-override store a
                    // later `POST /api/squads/{id}/tasks/{ti}/env` call would
                    // write to (RAL-150) -- from here on the two are
                    // indistinguishable.
                    to_json_map(&task.environment),
                    task.no_commit_required,
                ],
            )?;

            for (s_idx, cell) in task.cell.iter().enumerate() {
                let resolved = ResolvedAgent::resolve(task, cell);
                let sid = cell.id.clone().unwrap_or_else(|| format!("cell-{s_idx}"));
                // A cell inherits the task's timeout/budget unless it sets its
                // own. Timeout is stored in seconds; budget in total tokens.
                let timeout_sec = resolve_timeout_sec(cell.timeout_minutes, task.timeout_minutes);
                let budget_tokens = resolve_budget(cell.budget_tokens, task.budget_tokens);
                let maximum_budget_usd =
                    resolve_maximum_budget_usd(cell.maximum_budget_usd, task.maximum_budget_usd);
                let maximum_context =
                    resolve_maximum_context(cell.maximum_context, task.maximum_context);
                let auto_compact_threshold = resolve_auto_compact_threshold(
                    cell.auto_compact_threshold,
                    task.auto_compact_threshold,
                );
                let maximum_tool_output_tokens =
                    ralphus_core::schema::resolve_cell_maximum_tool_output_tokens(task, cell)
                        .map(|v| i64::try_from(v).unwrap_or(i64::MAX));
                let share_session = ralphus_core::schema::resolve_cell_share_session(task, cell);
                let effective_system_prompt = cell.prompt.as_ref().map(|_| {
                    effective_cell_system_prompt(cell.system_prompt.as_deref(), &cell.subprojects)
                });
                tx.execute(
                    "INSERT INTO cells(squad_id, task_idx, idx, sid, name, cwd, subprojects, prompt, command, agent, model, system_prompt, system_prompt_position, effective_system_prompt, state, depends_on, timeout_sec, budget_tokens, maximum_budget_usd, maximum_context, auto_compact_threshold, maximum_tool_output_tokens, upstream, queue_rank, env_overrides, machine, share_session)
                     VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
                    params![
                        squad_id,
                        t_idx_i,
                        i64::try_from(s_idx).unwrap_or(0),
                        sid,
                        cell.name,
                        cell.cwd,
                        to_json(&cell.subprojects),
                        cell.prompt,
                        cell.command,
                        resolved.program,
                        resolved.model,
                        cell.system_prompt,
                        cell.system_prompt_position,
                        effective_system_prompt,
                        NodeState::Pending.as_str(),
                        to_json(&cell.depends_on),
                        timeout_sec,
                        budget_tokens,
                        maximum_budget_usd,
                        maximum_context,
                        auto_compact_threshold,
                        maximum_tool_output_tokens,
                        cell.upstream,
                        // Seed the queue rank from the cell's own priority, or
                        // the owning task's priority as a fallback, so a task-level
                        // `priority` nudges all its cells' starting position.
                        cell.priority.or(task.priority).map(f64::from),
                        // RAL-172: same seeding as the task's own `env_overrides`
                        // above, scoped to this cell -- merges on top of the
                        // task's/squad's via `Store::resolve_cell_env_overrides`.
                        to_json_map(&cell.environment),
                        // RAL-185: resolved once at submit so the scheduler never
                        // has to re-derive inheritance, and so a later edit to the
                        // task file can't silently move an in-flight squad's machine.
                        ralphus_core::schema::resolve_cell_machine(task, cell),
                        share_session,
                    ],
                )?;

                for (v_idx, v) in cell.proof.iter().enumerate() {
                    insert_proof(
                        &tx,
                        squad_id,
                        t_idx_i,
                        "cell",
                        i64::try_from(s_idx).unwrap_or(0),
                        v_idx,
                        v,
                        task,
                        Some(cell),
                        &resolved.program,
                    )?;
                }
            }

            // Task-level proofs inherit the first cell's resolved agent to
            // match the scheduler's runtime behaviour (scheduler takes
            // `task_cell.map(|s| s.agent)`). Fall back to the task-level
            // agent field when there are no cells.
            let task_proof_agent = task
                .cell
                .first()
                .map(|s| ResolvedAgent::resolve(task, s).program)
                .unwrap_or_else(|| ResolvedAgent::from_task(task).program);
            for (v_idx, v) in task.proof.iter().enumerate() {
                insert_proof(
                    &tx,
                    squad_id,
                    t_idx_i,
                    "task",
                    -1,
                    v_idx,
                    v,
                    task,
                    None,
                    &task_proof_agent,
                )?;
            }
        }

        tx.commit()?;
        crate::rlog!(
            INFO,
            "ralphus [submit] squad {squad_id} inserted state={} tasks={}",
            state.as_str(),
            file.task.len()
        );
        let _ = self.cartographer_log(crate::cartographer::CartographerEntry {
            level: crate::logging::LogLevel::INFO,
            source: "submit",
            message: "squad inserted",
            scope: Some("squad"),
            squad_id: Some(squad_id),
            guardian_id: None,
            cell_id: None,
            task: None,
            log_path: None,
            payload: serde_json::json!({"state": state.as_str(), "tasks": file.task.len()}),
            admin_only: false,
        });
        Ok(())
    }

    /// Append an entry to the execution/transition log (CCTL-99). Failures to
    /// log are swallowed by callers (a missing audit line must never break a
    /// transition), so this returns the raw rusqlite result only for tests.
    pub fn log_event(
        &self,
        squad_id: Option<&str>,
        guardian_id: Option<&str>,
        scope: &str,
        reference: Option<&str>,
        message: &str,
    ) -> Result<()> {
        self.log_event_with_task(squad_id, guardian_id, scope, reference, message, None)
    }

    /// [`Store::log_event`], plus a task name for callers that already know
    /// it (RAL-155 Q2: task-scoped Cartographer filtering needs the `task`
    /// column populated on task/cell state transitions, not just on the
    /// scheduler/runner's own cell-execution events).
    pub fn log_event_with_task(
        &self,
        squad_id: Option<&str>,
        guardian_id: Option<&str>,
        scope: &str,
        reference: Option<&str>,
        message: &str,
        task: Option<&str>,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO events(squad_id, guardian_id, scope, ref, message, at_ms)
             VALUES(?,?,?,?,?,?)",
            params![squad_id, guardian_id, scope, reference, message, now_ms()],
        )?;
        // Cartographer subsumes this per-squad/per-guardian audit trail (RAL-98):
        // every `log_event` call also lands in the global structured log, so
        // the two never drift apart.
        let _ = self.cartographer_log(crate::cartographer::CartographerEntry {
            level: crate::logging::LogLevel::INFO,
            source: "store",
            message,
            scope: Some(scope),
            squad_id,
            guardian_id,
            cell_id: None,
            task,
            log_path: None,
            payload: reference.map_or(serde_json::json!({}), |r| serde_json::json!({"ref": r})),
            admin_only: false,
        });
        Ok(())
    }

    /// The audit log for a squad, oldest first, capped at `limit` most-recent rows.
    pub fn events_for_squad(&self, squad_id: &str, limit: i64) -> Result<Vec<EventView>> {
        self.events_where("squad_id", squad_id, limit)
    }

    /// The audit log for a guardian (review cycle), oldest first, capped.
    pub fn events_for_guardian(&self, guardian_id: &str, limit: i64) -> Result<Vec<EventView>> {
        self.events_where("guardian_id", guardian_id, limit)
    }

    fn events_where(&self, column: &str, value: &str, limit: i64) -> Result<Vec<EventView>> {
        // `column` is a fixed internal literal ("squad_id"/"guardian_id"), never
        // user input, so interpolating it into the SQL is safe here.
        let sql = format!(
            "SELECT scope, ref, message, at_ms FROM events
             WHERE {column}=? ORDER BY seq DESC LIMIT ?"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let mut rows = stmt
            .query_map(params![value, limit], |r| {
                Ok(EventView {
                    scope: r.get(0)?,
                    reference: r.get(1)?,
                    message: r.get(2)?,
                    at_ms: r.get(3)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        rows.reverse(); // oldest-first for display
        Ok(rows)
    }

    /// The current state of a squad.
    pub fn squad_state(&self, id: &str) -> Result<SquadState> {
        let s: Option<String> = self
            .conn
            .query_row("SELECT state FROM squads WHERE id=?", params![id], |r| {
                r.get(0)
            })
            .optional()?;
        let s = s.ok_or(StoreError::NotFound)?;
        SquadState::parse(&s).ok_or(StoreError::NotFound)
    }

    /// Set a squad's state. Also stamps `started_at_ms` (once, the first time the
    /// squad enters `running`) and `finished_at_ms` (every time it enters a
    /// terminal state, so a re-finish after a proof-only restart reflects the
    /// latest completion) — see the "Details-pane" migration comment in
    /// `init_schema` for the field semantics.
    pub fn set_squad_state(&self, id: &str, state: SquadState) -> Result<()> {
        let old = self
            .squad_state(id)
            .map(|s| s.as_str())
            .unwrap_or("unknown");
        let now = now_ms();
        let entering_running = i64::from(state == SquadState::Running);
        let entering_terminal = i64::from(state.is_terminal());
        let n = self.conn.execute(
            "UPDATE squads SET state=?, updated_at_ms=?,
                 started_at_ms = CASE WHEN ?=1 THEN COALESCE(started_at_ms, ?) ELSE started_at_ms END,
                 finished_at_ms = CASE WHEN ?=1 THEN ? ELSE finished_at_ms END
             WHERE id=?",
            params![
                state.as_str(),
                now,
                entering_running,
                now,
                entering_terminal,
                now,
                id
            ],
        )?;
        if n == 0 {
            Err(StoreError::NotFound)
        } else {
            crate::rlog!(
                INFO,
                "ralphus [state] squad {id} {old} → {}",
                state.as_str()
            );
            let _ = self.log_event(
                Some(id),
                None,
                "squad",
                None,
                &format!("squad → {}", state.as_str()),
            );
            Ok(())
        }
    }

    /// The W3C `traceparent` recorded against a squad at submit time (RAL-96),
    /// if the submitting request carried one. `Ok(None)` for a squad submitted
    /// with no trace context (or one predating this column).
    pub fn squad_trace_context(&self, id: &str) -> Result<Option<String>> {
        self.conn
            .query_row(
                "SELECT trace_context FROM squads WHERE id=?",
                params![id],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()?
            .ok_or(StoreError::NotFound)
    }

    /// Record the `traceparent` of the request that created `id` (RAL-96), so
    /// the scheduler's later asynchronous work (squad-claim, cell execution,
    /// proof execution) can rebuild a [`crate::otel::Context`] that continues
    /// the same trace instead of starting a disconnected one.
    pub fn set_squad_trace_context(&self, id: &str, trace_context: &str) -> Result<()> {
        let n = self.conn.execute(
            "UPDATE squads SET trace_context=? WHERE id=?",
            params![trace_context, id],
        )?;
        if n == 0 {
            Err(StoreError::NotFound)
        } else {
            Ok(())
        }
    }

    /// Activate a held (`Queued`) squad, moving it to `Pending`.
    pub fn activate(&self, id: &str) -> Result<SquadState> {
        match self.squad_state(id)? {
            SquadState::Queued => {
                self.set_squad_state(id, SquadState::Pending)?;
                Ok(SquadState::Pending)
            }
            other => Err(StoreError::InvalidTransition(format!(
                "can only activate a queued squad, squad is {}",
                other.as_str()
            ))),
        }
    }

    /// Cancel a squad, regardless of its current state (RAL-116). Always
    /// available and idempotent — even a squad that already reached a terminal
    /// state (`done`/`failed`/already `cancelled`) is (re-)flipped to
    /// `cancelled`, so it can never be picked up again by another trigger
    /// (a restart, cross-squad gating, etc). Every task/cell/proof that did
    /// not finish successfully — in-flight *and* already-`failed` ones — is
    /// flipped to `cancelled` alongside it; the worker thread stops on its own
    /// via the cancel token, so it will not re-run any node this flips.
    pub fn cancel(&self, id: &str) -> Result<SquadState> {
        self.squad_state(id)?;
        self.set_squad_state(id, SquadState::Cancelled)?;
        self.cancel_unfinished_nodes(id)?;
        Ok(SquadState::Cancelled)
    }

    /// Flip every task/cell/proof that did not finish successfully
    /// (`pending`/`running`/`failed`) to `cancelled`, so the board reflects a
    /// cancelled squad immediately and consistently across all three node
    /// levels. This mirrors the squad row itself, which is (re-)flipped to
    /// `cancelled` even from a terminal state (RAL-116): a squad whose cells
    /// had already failed must not be left showing `failed` tasks and cells
    /// under a `cancelled` squad. Only nodes carrying a real successful
    /// outcome — `done`, and the deliberately user-set `ignored` — are left
    /// untouched. The worker thread stops on its own via the cancel token, so
    /// it will not re-run any node this flips.
    fn cancel_unfinished_nodes(&self, squad_id: &str) -> Result<()> {
        const UNFINISHED: &str = "('pending','running','failed')";
        for table in ["cells", "tasks", "proofs"] {
            self.conn.execute(
                &format!("UPDATE {table} SET state='cancelled' WHERE squad_id=? AND state IN {UNFINISHED}"),
                params![squad_id],
            )?;
        }
        Ok(())
    }

    /// Squad ids that are ready to schedule: Pending, and with every cross-squad
    /// dependency (from the squad's `[[default]]` `depends_on`) already Done.
    /// A dependency reference `squad-id` or `squad-id/task/cell` is satisfied when
    /// that whole squad is Done (path-precise gating is a later refinement).
    pub fn list_ready(&self) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, depends_on FROM squads WHERE state='pending' ORDER BY created_at_ms ASC",
        )?;
        let pending = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        let mut ready = Vec::new();
        for (id, deps_json) in pending {
            let deps = from_json(&deps_json);
            if self.deps_satisfied(&deps)? {
                ready.push(id);
            }
        }
        Ok(ready)
    }

    /// Whether every cross-squad dependency reference points at a Done squad.
    fn deps_satisfied(&self, deps: &[String]) -> Result<bool> {
        for dep in deps {
            let dep_squad = dep.split('/').next().unwrap_or(dep);
            let state: Option<String> = self
                .conn
                .query_row(
                    "SELECT state FROM squads WHERE id=?",
                    params![dep_squad],
                    |r| r.get(0),
                )
                .optional()?;
            match state.as_deref().and_then(SquadState::parse) {
                Some(s) if s.satisfies_dependents() => {}
                _ => return Ok(false),
            }
        }
        Ok(true)
    }

    /// Count of currently running squads.
    pub fn running_count(&self) -> Result<i64> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*) FROM squads WHERE state='running'",
            [],
            |r| r.get(0),
        )?)
    }

    /// Count of cells currently executing — the real in-flight work, bounded
    /// by the scheduler's task-level concurrency limit. Unlike `running_count`
    /// (which counts squads), this reflects how many agent cells run at once.
    pub fn running_cell_count(&self) -> Result<i64> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*) FROM cells WHERE state='running'",
            [],
            |r| r.get(0),
        )?)
    }

    /// Guardian reviews that are currently building their stacked rebase, for
    /// display in the concurrency-counter dropdown.
    pub fn merging_guardians(&self) -> Result<Vec<(String, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, name FROM guardians WHERE status='merging' ORDER BY created_at_ms, id",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// Set a cell's state. Also stamps `started_at_ms` (once, the first
    /// time the cell enters `running`) and `finished_at_ms` (every time it
    /// enters a terminal state, so a re-finish after a proof-only restart
    /// reflects the latest completion) — see [`Store::set_squad_state`]'s doc
    /// comment for the shared semantics.
    pub fn set_cell_state(
        &self,
        squad_id: &str,
        task_idx: i64,
        idx: i64,
        state: NodeState,
    ) -> Result<()> {
        let old = self
            .conn
            .query_row(
                "SELECT state FROM cells WHERE squad_id=? AND task_idx=? AND idx=?",
                params![squad_id, task_idx, idx],
                |r| r.get::<_, String>(0),
            )
            .optional()
            .ok()
            .flatten()
            .unwrap_or_else(|| "unknown".to_string());
        let now = now_ms();
        let entering_running = i64::from(state == NodeState::Running);
        let entering_terminal = i64::from(state.is_terminal());
        // RAL-271: any explicit state transition -- an automatic dispatch
        // into `running` (the "next run") or a manual `Set Status` call to
        // any state -- clears this cell's cosmetic "out of date" badge.
        self.conn.execute(
            "UPDATE cells SET state=?, env_out_of_date=0,
                 started_at_ms = CASE WHEN ?=1 THEN COALESCE(started_at_ms, ?) ELSE started_at_ms END,
                 finished_at_ms = CASE WHEN ?=1 THEN ? ELSE finished_at_ms END
             WHERE squad_id=? AND task_idx=? AND idx=?",
            params![
                state.as_str(),
                entering_running,
                now,
                entering_terminal,
                now,
                squad_id,
                task_idx,
                idx
            ],
        )?;
        crate::rlog!(
            DEBUG,
            "ralphus [state] cell {squad_id}/t{task_idx}/s{idx} {old} → {}",
            state.as_str()
        );
        // RAL-155 Q2: populate Cartographer's `task` column on cell
        // transitions too, not just `log_event`'s legacy free-text `ref`, so
        // task-scoped filtering (and the uber-log-viewer) surfaces these.
        // Best-effort: a cell whose owning task was deleted mid-flight
        // (shouldn't happen — cascade-deleted together) just logs with no task.
        let task_name = self.task_name_at(squad_id, task_idx).ok().flatten();
        let _ = self.log_event_with_task(
            Some(squad_id),
            None,
            "cell",
            Some(&format!("t{task_idx}/s{idx}")),
            &format!("cell → {}", state.as_str()),
            task_name.as_deref(),
        );
        Ok(())
    }

    /// Set a task node's state. Also stamps `started_at_ms`/`finished_at_ms` —
    /// see [`Store::set_squad_state`]'s doc comment for the shared semantics.
    pub fn set_task_state(&self, squad_id: &str, task_idx: i64, state: NodeState) -> Result<()> {
        let row = self
            .conn
            .query_row(
                "SELECT state, name FROM tasks WHERE squad_id=? AND idx=?",
                params![squad_id, task_idx],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
            )
            .optional()
            .ok()
            .flatten();
        let old = row
            .as_ref()
            .map_or_else(|| "unknown".to_string(), |(s, _)| s.clone());
        let task_name = row.map(|(_, name)| name);
        let now = now_ms();
        let entering_running = i64::from(state == NodeState::Running);
        let entering_terminal = i64::from(state.is_terminal());
        // RAL-271: see the matching comment in `set_cell_state`.
        self.conn.execute(
            "UPDATE tasks SET state=?, env_out_of_date=0,
                 started_at_ms = CASE WHEN ?=1 THEN COALESCE(started_at_ms, ?) ELSE started_at_ms END,
                 finished_at_ms = CASE WHEN ?=1 THEN ? ELSE finished_at_ms END
             WHERE squad_id=? AND idx=?",
            params![
                state.as_str(),
                entering_running,
                now,
                entering_terminal,
                now,
                squad_id,
                task_idx
            ],
        )?;
        crate::rlog!(
            INFO,
            "ralphus [state] task {squad_id}/t{task_idx} {old} → {}",
            state.as_str()
        );
        let _ = self.log_event_with_task(
            Some(squad_id),
            None,
            "task",
            Some(&format!("t{task_idx}")),
            &format!("task → {}", state.as_str()),
            task_name.as_deref(),
        );
        Ok(())
    }

    /// Set (or clear) a task's failure-detail message (RAL-291), mirroring
    /// [`Store::record_cell_outcome`]'s `error` write at task granularity.
    /// Callers are task-level failure paths with no underlying cell/proof
    /// error to point to (e.g. `check_task_no_commits_guard` in the
    /// scheduler); a task failed by a child cell/proof failure never calls
    /// this. Cleared back to `None` by every "reset this task to Pending"
    /// site (`restart_task`, `restart_cell`, `restart_cell_proof`,
    /// `restart_task_proof`, `revive_failed_downstream_cells`,
    /// `reset_squad_to_pending`), same lifecycle as `cells.error`.
    pub fn set_task_error(&self, squad_id: &str, task_idx: i64, error: Option<&str>) -> Result<()> {
        self.conn.execute(
            "UPDATE tasks SET error=? WHERE squad_id=? AND idx=?",
            params![error, squad_id, task_idx],
        )?;
        Ok(())
    }

    /// Solo a task within a squad (RAL-157): while any task in the squad is
    /// soloed, the scheduler's dispatcher only starts cells belonging to a
    /// soloed task — every other task's not-yet-started cells stay
    /// paused (Pending) until un-soloed, even once the soloed task itself
    /// finishes (a dependent must not start racing ahead just because its
    /// soloed upstream completed). Cells already `running` when a sibling
    /// gets soloed are left to finish on their own — there is no per-cell
    /// interrupt in this codebase today (cancellation is squad-wide only, see
    /// `Cancellations`), so "pause" for in-flight work means "don't dispatch
    /// its task's *next* cell," not a mid-cell kill. Multiple tasks may
    /// be soloed simultaneously; soloing one does not un-solo another.
    /// Idempotent. Errors with [`StoreError::NotFound`] if the task doesn't
    /// exist.
    pub fn solo_task(&self, squad_id: &str, task_idx: i64) -> Result<()> {
        self.set_task_soloed(squad_id, task_idx, true)
    }

    /// Un-solo a task (RAL-157) — the reverse of [`Store::solo_task`]. Solo
    /// state never auto-clears (not on squad restart, not on the soloed task's
    /// own completion); this is the only way to resume paused siblings.
    /// Idempotent. Errors with [`StoreError::NotFound`] if the task doesn't
    /// exist.
    pub fn unsolo_task(&self, squad_id: &str, task_idx: i64) -> Result<()> {
        self.set_task_soloed(squad_id, task_idx, false)
    }

    fn set_task_soloed(&self, squad_id: &str, task_idx: i64, soloed: bool) -> Result<()> {
        let n = self.conn.execute(
            "UPDATE tasks SET soloed=?1 WHERE squad_id=?2 AND idx=?3",
            params![soloed, squad_id, task_idx],
        )?;
        if n == 0 {
            return Err(StoreError::NotFound);
        }
        crate::rlog!(
            INFO,
            "ralphus [state] task {squad_id}/t{task_idx} soloed={soloed}"
        );
        // `log_event` also writes this into Cartographer (RAL-98), so a single
        // call keeps the per-squad audit trail and the structured log in sync.
        let _ = self.log_event(
            Some(squad_id),
            None,
            "task",
            Some(&format!("t{task_idx}")),
            if soloed {
                "task soloed"
            } else {
                "task un-soloed"
            },
        );
        Ok(())
    }

    /// Indices of every currently-soloed task in a squad (RAL-157), read live so
    /// the scheduler's dispatcher observes a mid-run solo/unsolo toggle on its
    /// very next pass rather than only at the squad's next (re)start.
    pub fn soloed_task_indices(&self, squad_id: &str) -> Result<HashSet<i64>> {
        let mut stmt = self
            .conn
            .prepare("SELECT idx FROM tasks WHERE squad_id=? AND soloed=1")?;
        let rows = stmt
            .query_map(params![squad_id], |r| r.get::<_, i64>(0))?
            .collect::<std::result::Result<HashSet<_>, _>>()?;
        Ok(rows)
    }

    /// Current state of one cell, or `None` if it doesn't exist. Used by
    /// the restart handlers (`server::restart_cell`/`restart_cell_proof`)
    /// to decide whether cancelling the *whole squad's* worker is actually
    /// necessary — see those functions' doc comments (RAL-1xx: restart
    /// collateral damage).
    pub fn cell_state(&self, squad_id: &str, task_idx: i64, idx: i64) -> Result<Option<NodeState>> {
        let raw: Option<String> = self
            .conn
            .query_row(
                "SELECT state FROM cells WHERE squad_id=? AND task_idx=? AND idx=?",
                params![squad_id, task_idx, idx],
                |r| r.get(0),
            )
            .optional()?;
        Ok(raw.and_then(|s| NodeState::parse(&s)))
    }

    /// The effective (proof-aware) state of one cell -- see
    /// [`effective_cell_state`]'s doc comment for why the persisted
    /// `cells.state` alone can misreport a cell whose proof failed as
    /// `done`. Used by Triage pooling (`crate::triage`) to decide whether a
    /// pooled cell is still a viable candidate (pending/running, or done and
    /// passed) or has definitively failed and must never count toward a
    /// threshold or be swept into an auto-review. `None` if the cell doesn't
    /// exist.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub(crate) fn effective_state_for_cell(
        &self,
        squad_id: &str,
        task_idx: i64,
        idx: i64,
    ) -> Result<Option<String>> {
        let Some(raw) = self
            .conn
            .query_row(
                "SELECT state FROM cells WHERE squad_id=? AND task_idx=? AND idx=?",
                params![squad_id, task_idx, idx],
                |r| r.get::<_, String>(0),
            )
            .optional()?
        else {
            return Ok(None);
        };
        let mut stmt = self.conn.prepare(
            "SELECT state FROM proofs WHERE squad_id=? AND task_idx=? AND scope='cell' AND cell_idx=?",
        )?;
        let proof_states: Vec<String> = stmt
            .query_map(params![squad_id, task_idx, idx], |r| r.get(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(Some(effective_cell_state(
            &raw,
            proof_states.iter().map(String::as_str),
        )))
    }

    /// Current state of one task, or `None` if it doesn't exist. Same purpose
    /// as [`Store::cell_state`], for `server::restart_task_proof`.
    pub fn task_state(&self, squad_id: &str, task_idx: i64) -> Result<Option<NodeState>> {
        let raw: Option<String> = self
            .conn
            .query_row(
                "SELECT state FROM tasks WHERE squad_id=? AND idx=?",
                params![squad_id, task_idx],
                |r| r.get(0),
            )
            .optional()?;
        Ok(raw.and_then(|s| NodeState::parse(&s)))
    }

    /// The task name at `(squad_id, task_idx)`, or `None` if no such task
    /// exists. Used to translate an [`crate::entity_uri::EntityUri::Task`]
    /// (addressed by index, like every other entity URI) into Cartographer's
    /// `task` column, which stores the task's *name* (RAL-155 Q2).
    pub fn task_name_at(&self, squad_id: &str, task_idx: i64) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT name FROM tasks WHERE squad_id=? AND idx=?",
                params![squad_id, task_idx],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// The registered project name for one task, if any.
    pub fn task_project_at(&self, squad_id: &str, task_idx: i64) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT project FROM tasks WHERE squad_id=? AND idx=?",
                params![squad_id, task_idx],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten())
    }

    /// The cell id (`sid`) at `(squad_id, task_idx, cell_idx)`, or `None`
    /// if no such cell exists. Used the same way as [`Store::task_name_at`]
    /// to translate an index-addressed [`crate::entity_uri::EntityUri::Cell`]
    /// into Cartographer's `cell_id` column.
    pub fn cell_sid_at(
        &self,
        squad_id: &str,
        task_idx: i64,
        cell_idx: i64,
    ) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT sid FROM cells WHERE squad_id=? AND task_idx=? AND idx=?",
                params![squad_id, task_idx, cell_idx],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Whether any cell-scope proof step for `(task_idx, cell_idx)` at
    /// index >= `from_idx` is currently `Running`. Proof steps within one
    /// scope run sequentially, so at most one can be, but this checks
    /// defensively. Mirrors [`Store::restart_cell_proof`]'s own WHERE
    /// clause; used by `server::restart_cell_proof` (RAL-1xx).
    pub fn cell_proof_running_from(
        &self,
        squad_id: &str,
        task_idx: i64,
        cell_idx: i64,
        from_idx: i64,
    ) -> Result<bool> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM proofs WHERE squad_id=? AND task_idx=? AND scope='cell' AND cell_idx=? AND idx>=? AND state='running'",
            params![squad_id, task_idx, cell_idx, from_idx],
            |r| r.get(0),
        )?;
        Ok(count > 0)
    }

    /// Whether any task-scope proof step for `task_idx` at index >=
    /// `from_idx` is currently `Running`. Mirrors
    /// [`Store::restart_task_proof`]'s own WHERE clause; used by
    /// `server::restart_task_proof` (RAL-1xx).
    pub fn task_proof_running_from(
        &self,
        squad_id: &str,
        task_idx: i64,
        from_idx: i64,
    ) -> Result<bool> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM proofs WHERE squad_id=? AND task_idx=? AND scope='task' AND idx>=? AND state='running'",
            params![squad_id, task_idx, from_idx],
            |r| r.get(0),
        )?;
        Ok(count > 0)
    }

    /// Fetch a single squad's full board view.
    pub fn get_squad(&self, id: &str) -> Result<SquadView> {
        let row = self
            .conn
            .query_row(
                "SELECT id, label, state, created_at_ms, started_at_ms, finished_at_ms, env_overrides FROM squads WHERE id=?",
                params![id],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, Option<String>>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, i64>(3)?,
                        r.get::<_, Option<i64>>(4)?,
                        r.get::<_, Option<i64>>(5)?,
                        r.get::<_, String>(6)?,
                    ))
                },
            )
            .optional()?
            .ok_or(StoreError::NotFound)?;
        self.build_squad_view(
            row.0,
            row.1,
            row.2,
            row.3,
            row.4,
            row.5,
            from_json_map(&row.6),
        )
    }

    /// Fetch all squads, newest first.
    pub fn list_squads(&self) -> Result<Vec<SquadView>> {
        // Tie-break on id so squads created within the same millisecond still order
        // deterministically. Squad ids are monotonic, zero-padded, fixed-width, so
        // lexicographic `id DESC` == newest-first.
        let mut stmt = self.conn.prepare(
            "SELECT id, label, state, created_at_ms, started_at_ms, finished_at_ms, env_overrides FROM squads ORDER BY created_at_ms DESC, id DESC",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, Option<i64>>(4)?,
                    r.get::<_, Option<i64>>(5)?,
                    r.get::<_, String>(6)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        rows.into_iter()
            .map(|(id, label, state, ts, started, finished, env)| {
                self.build_squad_view(id, label, state, ts, started, finished, from_json_map(&env))
            })
            .collect()
    }

    /// The cross-squad `[[default]] depends_on` gating graph across every
    /// submitted squad (CLI_PARITY_PLAN.local.md Phase 6, `ralphus graph
    /// --global`). `include_terminal` selects between "active squads only"
    /// (queued/pending/running -- the default per plan Q5) and "every squad
    /// including done/failed/cancelled" (`--all`).
    ///
    /// A dependency reference to a squad outside the included set (filtered out,
    /// or simply unresolvable) produces no edge -- same best-effort philosophy
    /// as [`crate::plan::plan`] for within-squad refs.
    pub fn global_graph(&self, include_terminal: bool) -> Result<GlobalGraph> {
        let all_squads = self.list_squads()?;
        let included: Vec<&SquadView> = all_squads
            .iter()
            .filter(|r| {
                include_terminal || matches!(r.state.as_str(), "queued" | "pending" | "running")
            })
            .collect();
        let included_ids: HashSet<&str> = included.iter().map(|r| r.id.as_str()).collect();

        let nodes = included
            .iter()
            .map(|r| GlobalGraphNode {
                id: r.id.clone(),
                label: r.label.clone(),
                state: r.state.clone(),
            })
            .collect();

        let mut edges = Vec::new();
        for r in &included {
            for dep in self.squad_depends_on(&r.id)? {
                let dep_squad = dep.split('/').next().unwrap_or(&dep);
                if included_ids.contains(dep_squad) {
                    edges.push(crate::plan::GraphEdge {
                        from: dep_squad.to_string(),
                        to: r.id.clone(),
                    });
                }
            }
        }
        Ok(GlobalGraph { nodes, edges })
    }

    #[allow(clippy::too_many_arguments)]
    fn build_squad_view(
        &self,
        id: String,
        label: Option<String>,
        state: String,
        created_at_ms: i64,
        started_at_ms: Option<i64>,
        finished_at_ms: Option<i64>,
        env_overrides: BTreeMap<String, String>,
    ) -> Result<SquadView> {
        let mut tstmt = self.conn.prepare(
            "SELECT idx, name, project, agent, model, state, depends_on, env_overrides, proof_env_overrides, soloed, started_at_ms, finished_at_ms, env_out_of_date, error
             FROM tasks WHERE squad_id=? ORDER BY idx",
        )?;
        let task_rows = tstmt
            .query_map(params![id], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, Option<String>>(4)?,
                    r.get::<_, String>(5)?,
                    r.get::<_, String>(6)?,
                    r.get::<_, String>(7)?,
                    r.get::<_, String>(8)?,
                    r.get::<_, bool>(9)?,
                    r.get::<_, Option<i64>>(10)?,
                    r.get::<_, Option<i64>>(11)?,
                    r.get::<_, bool>(12)?,
                    r.get::<_, Option<String>>(13)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        // Map each guardian branch of this squad back to its review, so a cell
        // whose review branch is in a guardian's stack lists that review (RAL-17).
        let review_by_branch = self.reviews_by_branch(&id)?;
        // Fetch every proof step and cell belonging to this squad in one
        // statement each (grouped in memory below), rather than one query per
        // task/cell as before -- a squad with hundreds of tasks turned that
        // into thousands of individual SQL statements, all serialized under
        // the daemon's single store lock, which is what made `GET /api/tasks`
        // slow enough to stall restart/status-flip requests queued behind it.
        let proofs_by_scope = self.proofs_by_scope(&id)?;
        let triage_by_cell = self.triage_types_by_cell(&id)?;
        let mut cells_by_task =
            self.cells_by_task(&id, &review_by_branch, &proofs_by_scope, &triage_by_cell)?;
        let mut tasks = Vec::with_capacity(task_rows.len());
        for (
            t_idx,
            name,
            project,
            agent,
            model,
            tstate,
            deps,
            task_env,
            task_proof_env,
            soloed,
            t_started,
            t_finished,
            t_env_out_of_date,
            t_error,
        ) in task_rows
        {
            let cells = cells_by_task.remove(&t_idx).unwrap_or_default();
            let project = project.unwrap_or_else(|| {
                fallback_project_identifier(cells.first().and_then(|s| s.cwd.as_deref()))
            });
            let proof = proofs_by_scope
                .get(&(t_idx, "task".to_string(), -1))
                .cloned()
                .unwrap_or_default();
            tasks.push(TaskView {
                name,
                project,
                agent,
                model,
                state: tstate,
                error: t_error,
                cells,
                proof,
                depends_on: from_json(&deps),
                env_overrides: from_json_map(&task_env),
                proof_env_overrides: from_json_map(&task_proof_env),
                soloed,
                started_at_ms: t_started,
                finished_at_ms: t_finished,
                env_out_of_date: t_env_out_of_date,
            });
        }

        let reviews = self.reviews_for_squad(&id)?;
        let state = effective_squad_state(&self.conn, state, &id)?;
        Ok(SquadView {
            id,
            label,
            state,
            created_at_ms,
            started_at_ms,
            finished_at_ms,
            tasks,
            reviews,
            env_overrides,
        })
    }

    /// The reviews (guardians) derived from a squad, oldest first.
    fn reviews_for_squad(&self, squad_id: &str) -> Result<Vec<SquadReviewRef>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, name, status, origin FROM guardians WHERE squad_id=? ORDER BY created_at_ms, id",
        )?;
        let rows = stmt
            .query_map(params![squad_id], |r| {
                Ok(SquadReviewRef {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    status: r.get(2)?,
                    branch: None,
                    origin: r.get(3)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// All cells belonging to `squad_id`, fetched in one statement and
    /// grouped by `task_idx` — the bulk counterpart to a per-task cell
    /// query. `proofs_by_scope` must already hold this squad's proof steps
    /// (see [`Self::proofs_by_scope`]) so each cell's own proof list
    /// can be attached without a further per-cell query.
    fn cells_by_task(
        &self,
        squad_id: &str,
        review_by_branch: &HashMap<(i64, i64), Vec<SquadReviewRef>>,
        proofs_by_scope: &HashMap<(i64, String, i64), Vec<ProofView>>,
        triage_by_cell: &HashMap<(i64, i64), Vec<String>>,
    ) -> Result<HashMap<i64, Vec<CellView>>> {
        let mut stmt = self.conn.prepare(
            "SELECT task_idx, idx, sid, name, cwd, agent, model, state, tokens_in, tokens_out, cost_usd, error, prompt, command, effective_system_prompt, depends_on, review_branch, agent_session_id, maximum_budget_usd, env_overrides, proof_env_overrides, started_at_ms, finished_at_ms, env_out_of_date, machine, detached_at_ms, maximum_context, auto_compact_threshold, cache_creation_tokens, cache_read_tokens, cost_is_estimated, maximum_tool_output_tokens, compaction_input_tokens, compaction_count
             FROM cells WHERE squad_id=? ORDER BY task_idx, idx",
        )?;
        let rows = stmt
            .query_map(params![squad_id], |r| {
                let task_idx: i64 = r.get(0)?;
                let idx: i64 = r.get(1)?;
                let reviews = review_by_branch
                    .get(&(task_idx, idx))
                    .cloned()
                    .unwrap_or_default();
                let triage_types = triage_by_cell
                    .get(&(task_idx, idx))
                    .cloned()
                    .unwrap_or_default();
                Ok((
                    task_idx,
                    idx,
                    CellView {
                        id: r.get::<_, String>(2)?,
                        name: r.get::<_, Option<String>>(3)?,
                        cwd: r.get::<_, Option<String>>(4)?,
                        agent: r.get::<_, String>(5)?,
                        model: r.get::<_, Option<String>>(6)?,
                        state: r.get::<_, String>(7)?,
                        tokens_in: r.get::<_, i64>(8)?,
                        tokens_out: r.get::<_, i64>(9)?,
                        cost_usd: r.get::<_, f64>(10)?,
                        error: r.get::<_, Option<String>>(11)?,
                        prompt: r.get::<_, Option<String>>(12)?,
                        command: r.get::<_, Option<String>>(13)?,
                        system_prompt: r.get::<_, Option<String>>(14)?,
                        depends_on: from_json(&r.get::<_, String>(15)?),
                        proof: Vec::new(),
                        reviews,
                        triage_types,
                        agent_session_id: r.get::<_, Option<String>>(17)?,
                        maximum_budget_usd: r.get::<_, Option<f64>>(18)?,
                        env_overrides: from_json_map(&r.get::<_, String>(19)?),
                        proof_env_overrides: from_json_map(&r.get::<_, String>(20)?),
                        started_at_ms: r.get::<_, Option<i64>>(21)?,
                        finished_at_ms: r.get::<_, Option<i64>>(22)?,
                        env_out_of_date: r.get::<_, bool>(23)?,
                        machine: r.get::<_, Option<String>>(24)?,
                        detached_at_ms: r.get::<_, Option<i64>>(25)?,
                        maximum_context: r.get::<_, Option<i64>>(26)?,
                        auto_compact_threshold: r.get::<_, Option<i64>>(27)?,
                        cache_creation_tokens: r.get::<_, i64>(28)?,
                        cache_read_tokens: r.get::<_, i64>(29)?,
                        cost_is_estimated: r.get::<_, bool>(30)?,
                        maximum_tool_output_tokens: r.get::<_, Option<i64>>(31)?,
                        compaction_input_tokens: r.get::<_, i64>(32)?,
                        compaction_count: r.get::<_, i64>(33)?,
                    },
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let mut map: HashMap<i64, Vec<CellView>> = HashMap::new();
        for (task_idx, idx, mut cell) in rows {
            cell.proof = proofs_by_scope
                .get(&(task_idx, "cell".to_string(), idx))
                .cloned()
                .unwrap_or_default();
            cell.state =
                effective_cell_state(&cell.state, cell.proof.iter().map(|p| p.state.as_str()));
            map.entry(task_idx).or_default().push(cell);
        }
        Ok(map)
    }

    /// Map each of this squad's cells back to the reviews it contributes to,
    /// so a cell can list the reviews its branch participates in (RAL-17).
    ///
    /// A cell whose `review_guardian_id` is set (RAL-314: recorded at submit
    /// time by `reviews::derive_reviews` -> `Store::set_cell_review_guardian`,
    /// alongside `review_branch`) resolves directly to that guardian --
    /// this is what keeps two unrelated squads' cells from being conflated
    /// just because their submissions happened to record the identical
    /// branch *string* (e.g. repeat submissions against the same worktree,
    /// which always mint a fresh guardian per submission but reuse the
    /// worktree's currently-checked-out branch name).
    ///
    /// A cell with no `review_guardian_id` (a pre-RAL-314 row, or one whose
    /// review linkage came from the manual
    /// `POST /api/guardians/{id}/branches` attach path, which has no
    /// submission-time cell membership to record one against) falls back to
    /// the old `cells.review_branch = guardian_branches.branch` string join.
    /// This is also what lets a guardian *found* (not created) by a later
    /// submission that shares a `ralphus:new-review/<key>` link keep
    /// resolving correctly -- see `collecting_guardians_for_cells`, which
    /// needs the same two-tier lookup.
    fn reviews_by_branch(
        &self,
        squad_id: &str,
    ) -> Result<HashMap<(i64, i64), Vec<SquadReviewRef>>> {
        let mut map: HashMap<(i64, i64), Vec<SquadReviewRef>> = HashMap::new();

        let mut direct_stmt = self.conn.prepare(
            "SELECT DISTINCT s.task_idx, s.idx, g.id, g.name, g.status, s.review_branch, g.origin
             FROM cells s
             JOIN guardians g ON g.id = s.review_guardian_id
             WHERE s.squad_id = ? AND s.review_guardian_id IS NOT NULL",
        )?;
        let direct_rows = direct_stmt
            .query_map(params![squad_id], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    SquadReviewRef {
                        id: r.get(2)?,
                        name: r.get(3)?,
                        status: r.get(4)?,
                        branch: r.get::<_, Option<String>>(5)?,
                        origin: r.get(6)?,
                    },
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        for (task_idx, idx, rref) in direct_rows {
            map.entry((task_idx, idx)).or_default().push(rref);
        }

        let mut fallback_stmt = self.conn.prepare(
            "SELECT DISTINCT s.task_idx, s.idx, g.id, g.name, g.status, gb.branch, g.origin
             FROM cells s
             JOIN guardian_branches gb ON gb.branch = s.review_branch
             JOIN guardians g ON g.id = gb.guardian_id
             WHERE s.squad_id = ? AND s.review_guardian_id IS NULL
                   AND s.review_branch IS NOT NULL
             ORDER BY g.created_at_ms, g.id",
        )?;
        let fallback_rows = fallback_stmt
            .query_map(params![squad_id], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    SquadReviewRef {
                        id: r.get(2)?,
                        name: r.get(3)?,
                        status: r.get(4)?,
                        branch: r.get::<_, Option<String>>(5)?,
                        origin: r.get(6)?,
                    },
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        for (task_idx, idx, rref) in fallback_rows {
            map.entry((task_idx, idx)).or_default().push(rref);
        }

        Ok(map)
    }

    /// All proof steps belonging to `squad_id`, fetched in one statement and
    /// grouped by `(task_idx, scope, cell_idx)` — the same key
    /// [`Self::proofs_for`] filters on, but for the whole squad at once.
    /// Used by `build_squad_view` so listing a squad's board view costs a
    /// constant number of queries regardless of how many tasks/cells it
    /// has, instead of one query per task/cell.
    fn proofs_by_scope(
        &self,
        squad_id: &str,
    ) -> Result<HashMap<(i64, String, i64), Vec<ProofView>>> {
        let mut stmt = self.conn.prepare(
            "SELECT task_idx, scope, cell_idx, vid, kind, state, output, spec, effective_system_prompt, model, agent, agent_session_id, tokens_in, tokens_out, cost_usd, env_overrides, env_out_of_date, cache_creation_tokens, cache_read_tokens, cost_is_estimated, maximum_tool_output_tokens, compaction_input_tokens, compaction_count FROM proofs
             WHERE squad_id=? ORDER BY task_idx, scope, cell_idx, idx",
        )?;
        let rows = stmt
            .query_map(params![squad_id], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                    ProofView {
                        id: r.get::<_, Option<String>>(3)?,
                        kind: r.get::<_, String>(4)?,
                        state: r.get::<_, String>(5)?,
                        output: r.get::<_, Option<String>>(6)?,
                        spec: r.get::<_, String>(7)?,
                        system_prompt: r.get::<_, Option<String>>(8)?,
                        model: r.get::<_, Option<String>>(9)?,
                        agent: r.get::<_, String>(10)?,
                        agent_session_id: r.get::<_, Option<String>>(11)?,
                        tokens_in: r.get::<_, i64>(12)?,
                        tokens_out: r.get::<_, i64>(13)?,
                        cost_usd: r.get::<_, f64>(14)?,
                        env_overrides: from_json_map(&r.get::<_, String>(15)?),
                        env_out_of_date: r.get::<_, bool>(16)?,
                        cache_creation_tokens: r.get::<_, i64>(17)?,
                        cache_read_tokens: r.get::<_, i64>(18)?,
                        cost_is_estimated: r.get::<_, bool>(19)?,
                        maximum_tool_output_tokens: r.get::<_, Option<i64>>(20)?,
                        compaction_input_tokens: r.get::<_, i64>(21)?,
                        compaction_count: r.get::<_, i64>(22)?,
                    },
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let mut map: HashMap<(i64, String, i64), Vec<ProofView>> = HashMap::new();
        for (task_idx, scope, cell_idx, v) in rows {
            map.entry((task_idx, scope, cell_idx)).or_default().push(v);
        }
        Ok(map)
    }

    pub(crate) fn proofs_for(
        &self,
        squad_id: &str,
        task_idx: i64,
        scope: &str,
        cell_idx: i64,
    ) -> Result<Vec<ProofView>> {
        let mut stmt = self.conn.prepare(
            "SELECT vid, kind, state, output, spec, effective_system_prompt, model, agent, agent_session_id, tokens_in, tokens_out, cost_usd, env_overrides, env_out_of_date, cache_creation_tokens, cache_read_tokens, cost_is_estimated, maximum_tool_output_tokens, compaction_input_tokens, compaction_count FROM proofs
             WHERE squad_id=? AND task_idx=? AND scope=? AND cell_idx=? ORDER BY idx",
        )?;
        let rows = stmt
            .query_map(params![squad_id, task_idx, scope, cell_idx], |r| {
                Ok(ProofView {
                    id: r.get::<_, Option<String>>(0)?,
                    kind: r.get::<_, String>(1)?,
                    state: r.get::<_, String>(2)?,
                    output: r.get::<_, Option<String>>(3)?,
                    spec: r.get::<_, String>(4)?,
                    system_prompt: r.get::<_, Option<String>>(5)?,
                    model: r.get::<_, Option<String>>(6)?,
                    agent: r.get::<_, String>(7)?,
                    agent_session_id: r.get::<_, Option<String>>(8)?,
                    tokens_in: r.get::<_, i64>(9)?,
                    tokens_out: r.get::<_, i64>(10)?,
                    cost_usd: r.get::<_, f64>(11)?,
                    env_overrides: from_json_map(&r.get::<_, String>(12)?),
                    env_out_of_date: r.get::<_, bool>(13)?,
                    cache_creation_tokens: r.get::<_, i64>(14)?,
                    cache_read_tokens: r.get::<_, i64>(15)?,
                    cost_is_estimated: r.get::<_, bool>(16)?,
                    maximum_tool_output_tokens: r.get::<_, Option<i64>>(17)?,
                    compaction_input_tokens: r.get::<_, i64>(18)?,
                    compaction_count: r.get::<_, i64>(19)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    // ── Project registry (RAL-100) ───────────────────────────────────────────

    /// Register (or re-register, updating its fields) a project by name.
    pub fn register_project(
        &self,
        name: &str,
        description: &str,
        path: &str,
        vcs: &str,
    ) -> Result<()> {
        self.register_project_ex(name, description, path, vcs, None)
    }

    /// Full form of [`Self::register_project`] that also accepts an explicit
    /// per-project override for `match_pr_branch_name` (RAL-307, e.g. from
    /// `ralphus project git --match-pr-branch-name`). `None` falls back to
    /// stamping the live global config's value, the same always-from-global
    /// shape `skip_base_updates` already uses.
    pub fn register_project_ex(
        &self,
        name: &str,
        description: &str,
        path: &str,
        vcs: &str,
        match_pr_branch_name: Option<bool>,
    ) -> Result<()> {
        self.register_project_with_clone_url_ex(
            name,
            description,
            path,
            vcs,
            None,
            match_pr_branch_name,
        )
    }

    /// Register a project with the canonical clone URL providers use to
    /// provision it on other machines. Omitting `clone_url` preserves any URL
    /// already stored by an earlier registration.
    pub fn register_project_with_clone_url_ex(
        &self,
        name: &str,
        description: &str,
        path: &str,
        vcs: &str,
        clone_url: Option<&str>,
        match_pr_branch_name: Option<bool>,
    ) -> Result<()> {
        // RAL-250: stamp the *current* global `skip_base_updates` value into a
        // brand-new project at first registration, so a later global change
        // does not retroactively flip it.
        let skip_base_updates = crate::config::global_review_config().skip_base_updates();
        let match_pr_branch_name = match_pr_branch_name
            .unwrap_or_else(|| crate::config::global_review_config().match_pr_branch_name());
        // RAL-317: same always-from-global stamping shape as
        // `skip_base_updates` -- no explicit per-registration override exists
        // for this one.
        let auto_submit_pr_stack = crate::config::global_review_config().auto_submit_pr_stack();
        // RAL-378: same always-from-global stamping shape as
        // `auto_submit_pr_stack`.
        let separate_pr_branch = crate::config::global_review_config().separate_pr_branch();
        self.register_project_with_clone_url_and_stamp(
            name,
            description,
            path,
            vcs,
            clone_url,
            Some(skip_base_updates),
            Some(match_pr_branch_name),
            Some(auto_submit_pr_stack),
            Some(separate_pr_branch),
        )
    }

    /// RAL-250: `register_project` with the global value that would normally be
    /// read from the process's `global_review_config()` passed in explicitly,
    /// so the stamping behavior is testable in-process -- the workspace forbids
    /// `unsafe_code` outright, so `std::env::set_var`/`remove_var` can't be
    /// used to point `$RALPHUS_CONFIG_HOME` at a controlled value in a test
    /// (the same rationale `agent_profiles.rs::configuration_path_entries`
    /// documents for its own env parameter).
    #[allow(clippy::too_many_arguments)]
    #[cfg(test)]
    fn register_project_with_stamp(
        &self,
        name: &str,
        description: &str,
        path: &str,
        vcs: &str,
        skip_base_updates_stamp: Option<bool>,
        match_pr_branch_name_stamp: Option<bool>,
        auto_submit_pr_stack_stamp: Option<bool>,
        separate_pr_branch_stamp: Option<bool>,
    ) -> Result<()> {
        self.register_project_with_clone_url_and_stamp(
            name,
            description,
            path,
            vcs,
            None,
            skip_base_updates_stamp,
            match_pr_branch_name_stamp,
            auto_submit_pr_stack_stamp,
            separate_pr_branch_stamp,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn register_project_with_clone_url_and_stamp(
        &self,
        name: &str,
        description: &str,
        path: &str,
        vcs: &str,
        clone_url: Option<&str>,
        skip_base_updates_stamp: Option<bool>,
        match_pr_branch_name_stamp: Option<bool>,
        auto_submit_pr_stack_stamp: Option<bool>,
        separate_pr_branch_stamp: Option<bool>,
    ) -> Result<()> {
        // An existing project being re-registered (an upsert update, not a
        // first insert) is deliberately left untouched -- the ticket's explicit
        // "no backfill" decision, so `stamp` is only applied on a true insert.
        let exists = self
            .conn
            .query_row("SELECT 1 FROM projects WHERE name=?", params![name], |_| {
                Ok(())
            })
            .optional()?
            .is_some();
        let skip_base_updates_stamp = if exists {
            None
        } else {
            skip_base_updates_stamp
        };
        let match_pr_branch_name_stamp = if exists {
            None
        } else {
            match_pr_branch_name_stamp
        };
        let auto_submit_pr_stack_stamp = if exists {
            None
        } else {
            auto_submit_pr_stack_stamp
        };
        let separate_pr_branch_stamp = if exists {
            None
        } else {
            separate_pr_branch_stamp
        };
        self.conn.execute(
            "INSERT INTO projects(name, description, path, clone_url, vcs, created_at_ms, skip_base_updates, match_pr_branch_name, auto_submit_pr_stack, separate_pr_branch)
             VALUES(?,?,?,?,?,?,?,?,?,?)
             ON CONFLICT(name) DO UPDATE SET description=excluded.description, path=excluded.path, clone_url=COALESCE(excluded.clone_url, projects.clone_url), vcs=excluded.vcs",
            params![
                name,
                description,
                path,
                clone_url,
                vcs,
                now_ms(),
                skip_base_updates_stamp.map(i64::from),
                match_pr_branch_name_stamp.map(i64::from),
                auto_submit_pr_stack_stamp.map(i64::from),
                separate_pr_branch_stamp.map(i64::from)
            ],
        )?;
        crate::rlog!(
            INFO,
            "ralphus [store] project \"{name}\" registered path={path} vcs={vcs}"
        );
        let _ = self.cartographer_log(crate::cartographer::CartographerEntry {
            level: crate::logging::LogLevel::INFO,
            source: "store",
            message: "project registered",
            scope: Some("project"),
            squad_id: None,
            guardian_id: None,
            cell_id: None,
            task: None,
            log_path: None,
            payload: serde_json::json!({"name": name, "path": path, "vcs": vcs}),
            admin_only: false,
        });
        Ok(())
    }

    /// Explicitly clear a registered project's clone URL (RAL-355).
    ///
    /// This is the one path that intentionally overrides
    /// [`Self::register_project_with_clone_url_ex`]'s "omitting `clone_url`
    /// preserves whatever is already stored" contract: an ordinary
    /// re-registration from an older client that doesn't send the field must
    /// never accidentally erase a previously registered URL, so clearing one
    /// requires this separate, explicit call instead of a magic value (an
    /// empty string) passed through the normal registration path -- the same
    /// "if it's supported, expose it explicitly" reasoning the URL-clearing
    /// design question called for.
    ///
    /// # Errors
    /// If no project named `name` is registered.
    pub fn clear_project_clone_url(&self, name: &str) -> Result<()> {
        let changed = self.conn.execute(
            "UPDATE projects SET clone_url = NULL WHERE name = ?1",
            params![name],
        )?;
        if changed == 0 {
            return Err(StoreError::NotFound);
        }
        crate::rlog!(INFO, "ralphus [store] project \"{name}\" clone URL cleared");
        let _ = self.cartographer_log(crate::cartographer::CartographerEntry {
            level: crate::logging::LogLevel::INFO,
            source: "store",
            message: "project clone URL cleared",
            scope: Some("project"),
            squad_id: None,
            guardian_id: None,
            cell_id: None,
            task: None,
            log_path: None,
            payload: serde_json::json!({"name": name}),
            admin_only: false,
        });
        Ok(())
    }

    /// RAL-250: the `skip_base_updates` value a project stamped from the live
    /// global config when it was first registered, looked up by repo path.
    /// `None` when no registered project path is `path` itself or an ancestor
    /// of it (or that project was registered before this column existed --
    /// those are deliberately not backfilled). This frozen value is what keeps
    /// a later global change from retroactively flipping an already-created
    /// project.
    pub fn project_skip_base_updates_stamp(&self, path: &str) -> Option<bool> {
        self.project_bool_stamp(path, "skip_base_updates")
    }

    /// RAL-307: the `match_pr_branch_name` value a project stamped (from the
    /// live global config, or an explicit `ralphus project git` flag) when it
    /// was first registered, looked up by repo path -- same lookup/ancestry
    /// semantics as [`Self::project_skip_base_updates_stamp`].
    pub fn project_match_pr_branch_name_stamp(&self, path: &str) -> Option<bool> {
        self.project_bool_stamp(path, "match_pr_branch_name")
    }

    /// RAL-317: the `auto_submit_pr_stack` value a project stamped (from the
    /// live global config) when it was first registered, looked up by repo
    /// path -- same lookup/ancestry semantics as
    /// [`Self::project_skip_base_updates_stamp`].
    pub fn project_auto_submit_pr_stack_stamp(&self, path: &str) -> Option<bool> {
        self.project_bool_stamp(path, "auto_submit_pr_stack")
    }

    /// RAL-378: the `separate_pr_branch` value a project stamped (from the
    /// live global config) when it was first registered, looked up by repo
    /// path -- same lookup/ancestry semantics as
    /// [`Self::project_skip_base_updates_stamp`].
    pub fn project_separate_pr_branch_stamp(&self, path: &str) -> Option<bool> {
        self.project_bool_stamp(path, "separate_pr_branch")
    }

    /// The registered project name whose `path` is `path` itself or an
    /// ancestor of it (RAL-338) -- same lookup/ancestry semantics as
    /// [`Self::project_skip_base_updates_stamp`], but returning the project's
    /// `name` (the key [`Self::resolve_fork`] takes) instead of a stamped
    /// bool column. Used to find "which registered project (if any) does
    /// this guardian's `git_root` belong to" for fork resolution -- a
    /// guardian has no direct project foreign key, only a filesystem path.
    pub fn project_name_for_path(&self, path: &str) -> Option<String> {
        let trimmed = Self::normalize_for_project_lookup(path);
        let mut stmt = self.conn.prepare("SELECT name, path FROM projects").ok()?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .ok()?
            .collect::<std::result::Result<Vec<_>, _>>()
            .ok()?;
        for (name, proj_path) in rows {
            let proj_path = Self::normalize_for_project_lookup(&proj_path);
            let matches = trimmed == proj_path
                || trimmed
                    .strip_prefix(&proj_path)
                    .map(|rest| rest.starts_with('/'))
                    .unwrap_or(false);
            if matches {
                return Some(name);
            }
        }
        None
    }

    /// Shared lookup behind [`Self::project_skip_base_updates_stamp`] and
    /// [`Self::project_match_pr_branch_name_stamp`]: `column`'s value on
    /// whichever registered project's path is `path` itself or an ancestor of
    /// it. `column` is always a fixed internal string literal, never
    /// caller/user-supplied, so interpolating it into the query is safe.
    fn project_bool_stamp(&self, path: &str, column: &str) -> Option<bool> {
        let trimmed = Self::normalize_for_project_lookup(path);
        let mut stmt = match self
            .conn
            .prepare(&format!("SELECT path, {column} FROM projects"))
        {
            Ok(s) => s,
            Err(_) => return None,
        };
        let rows = stmt
            .query_map([], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, Option<i64>>(1)?))
            })
            .ok()?
            .collect::<std::result::Result<Vec<_>, _>>()
            .ok()?;
        for (proj, stamp) in rows {
            let proj = Self::normalize_for_project_lookup(&proj);
            let matches = trimmed == proj
                || trimmed
                    .strip_prefix(&proj)
                    .map(|rest| rest.starts_with('/'))
                    .unwrap_or(false);
            if matches {
                return stamp.map(|v| v != 0);
            }
        }
        None
    }

    /// Loads every registered project's path and all four `project_bool_stamp`
    /// columns in one query (GUARDIAN_PERF.local.md) -- callers that need
    /// several guardians' worth of stamps (e.g. `Store::list_guardians`)
    /// should call this once and reuse the result via
    /// [`Self::match_project_stamps`], instead of `project_bool_stamp`'s four
    /// separate full-table-scan queries *per guardian*. Same
    /// swallow-and-return-empty failure mode as `project_bool_stamp` (a
    /// project lookup is best-effort, never a hard error).
    pub(crate) fn load_all_project_stamps(&self) -> Vec<(String, ProjectStamps)> {
        let mut stmt = match self.conn.prepare(
            "SELECT path, skip_base_updates, match_pr_branch_name, auto_submit_pr_stack, separate_pr_branch FROM projects",
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let Ok(rows) = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                ProjectStamps {
                    skip_base_updates: r.get::<_, Option<i64>>(1)?.map(|v| v != 0),
                    match_pr_branch_name: r.get::<_, Option<i64>>(2)?.map(|v| v != 0),
                    auto_submit_pr_stack: r.get::<_, Option<i64>>(3)?.map(|v| v != 0),
                    separate_pr_branch: r.get::<_, Option<i64>>(4)?.map(|v| v != 0),
                },
            ))
        }) else {
            return Vec::new();
        };
        rows.filter_map(std::result::Result::ok).collect()
    }

    /// Prefix-matches `path` against `stamps` (from
    /// [`Self::load_all_project_stamps`]), using the exact same
    /// exact-or-ancestor rule as `project_bool_stamp`'s per-column lookup --
    /// the match is a pure function of `path` and each registered project's
    /// `path`, independent of which column is read, so one match serves all
    /// four columns at once. `None` if no registered project owns `path`.
    pub(crate) fn match_project_stamps<'a>(
        path: &str,
        stamps: &'a [(String, ProjectStamps)],
    ) -> Option<&'a ProjectStamps> {
        let trimmed = Self::normalize_for_project_lookup(path);
        stamps.iter().find_map(|(proj, s)| {
            let proj = Self::normalize_for_project_lookup(proj);
            let matches = trimmed == proj
                || trimmed
                    .strip_prefix(&proj)
                    .map(|rest| rest.starts_with('/'))
                    .unwrap_or(false);
            matches.then_some(s)
        })
    }

    /// Normalizes `path` the same way [`crate::triage::pool_key_for_path`]
    /// does (RAL-318): canonicalized, verbatim-prefix stripped, forward-slash
    /// separated, with any resulting trailing slash trimmed. Without this, a
    /// guardian's git-reported `git_root` (forward-slashed, e.g.
    /// `C:/repos/ralphus`) and a project registered via a backslashed
    /// Windows path (e.g. `C:\repos\ralphus`) compare unequal under a raw
    /// string comparison, so `project_name_for_path`/`project_bool_stamp`
    /// silently fail to find the project -- which in turn defeats
    /// fork-aware PR routing ([`crate::pr::resolve_pr_repo_routing`]) and
    /// sends a review's PR stack to the wrong remote with no error surfaced.
    fn normalize_for_project_lookup(path: &str) -> String {
        crate::triage::normalize_path_key(std::path::Path::new(path))
            .trim_end_matches('/')
            .to_string()
    }

    /// A project by its exact registered name, or `None` when absent.
    pub fn get_project(&self, name: &str) -> Result<Option<ProjectView>> {
        self.conn
            .query_row(
                "SELECT name, description, path, clone_url, vcs, created_at_ms FROM projects WHERE name=?",
                params![name],
                |r| {
                    Ok(ProjectView {
                        name: r.get(0)?,
                        description: r.get(1)?,
                        path: r.get(2)?,
                        clone_url: r.get(3)?,
                        vcs: r.get(4)?,
                        created_at_ms: r.get(5)?,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    /// All registered projects, newest first.
    pub fn list_projects(&self) -> Result<Vec<ProjectView>> {
        let mut stmt = self.conn.prepare(
            "SELECT name, description, path, clone_url, vcs, created_at_ms FROM projects ORDER BY created_at_ms DESC, name",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(ProjectView {
                    name: r.get(0)?,
                    description: r.get(1)?,
                    path: r.get(2)?,
                    clone_url: r.get(3)?,
                    vcs: r.get(4)?,
                    created_at_ms: r.get(5)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Resolve `query` against the project registry: an exact name match first,
    /// then a case-insensitive name/description substring match (only if it
    /// singles out one project), then the closest project name by edit
    /// distance (RAL-100) — so a near-miss spoken/typed name (e.g. from
    /// speech-to-text) can still resolve to the correct registered project.
    /// Returns `None` when nothing registered is close enough.
    pub fn resolve_project(&self, query: &str) -> Result<Option<ProjectView>> {
        if let Some(p) = self.get_project(query)? {
            return Ok(Some(p));
        }
        let all = self.list_projects()?;
        if all.is_empty() {
            return Ok(None);
        }
        let q = query.trim().to_lowercase();
        if q.is_empty() {
            return Ok(None);
        }
        if let Some(p) = all.iter().find(|p| p.name.to_lowercase() == q) {
            return Ok(Some(p.clone()));
        }
        let substring_matches: Vec<&ProjectView> = all
            .iter()
            .filter(|p| {
                p.name.to_lowercase().contains(&q) || p.description.to_lowercase().contains(&q)
            })
            .collect();
        if let [only] = substring_matches.as_slice() {
            return Ok(Some((*only).clone()));
        }
        let (best, dist) = all
            .iter()
            .map(|p| (p, levenshtein(&p.name.to_lowercase(), &q)))
            .min_by_key(|(_, d)| *d)
            .expect("all is non-empty");
        // Accept a fuzzy match only when it's "close" relative to name length,
        // so an unrelated project name never silently wins.
        let threshold = (best.name.len().max(q.len()) / 3).max(2);
        if dist <= threshold {
            Ok(Some(best.clone()))
        } else {
            Ok(None)
        }
    }

    /// Rewrite a cell's `cwd` — used to resolve a worktree placeholder
    /// (`ralphus:new-worktree/<branch>`) to its real materialized path
    /// (RAL-100) before the cell runs.
    pub fn set_cell_cwd(&self, squad_id: &str, task_idx: i64, idx: i64, cwd: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE cells SET cwd=? WHERE squad_id=? AND task_idx=? AND idx=?",
            params![cwd, squad_id, task_idx, idx],
        )?;
        Ok(())
    }

    /// Persisted review worktrees, including per-branch and combined paths.
    pub(crate) fn guardian_worktree_records(&self) -> Result<Vec<GuardianWorktreeRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT g.id, g.name, COALESCE(gb.project, g.git_root), gb.worktree,
                    MAX(g.updated_at_ms, COALESCE(gb.started_at_ms, 0))
             FROM guardians g
             JOIN guardian_branches gb ON gb.guardian_id=g.id
             WHERE gb.worktree IS NOT NULL
             UNION ALL
             SELECT id, name, git_root, combined_worktree, updated_at_ms
             FROM guardians WHERE combined_worktree IS NOT NULL",
        )?;
        Ok(stmt
            .query_map([], |r| {
                Ok(GuardianWorktreeRecord {
                    guardian_id: r.get(0)?,
                    guardian_name: r.get(1)?,
                    project_root: r.get(2)?,
                    path: r.get(3)?,
                    last_activity_ms: r.get(4)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// Every cell/proof/review claim whose persisted path can name a worktree.
    /// Proof cwd selection mirrors `scheduler::run_proofs`.
    pub(crate) fn worktree_claims(&self) -> Result<Vec<WorktreeClaim>> {
        let mut stmt = self.conn.prepare(
            "SELECT 'cell', c.squad_id || ':' || c.task_idx || ':' || c.idx, c.cwd, c.state
             FROM cells c WHERE c.cwd IS NOT NULL
             UNION ALL
             SELECT 'proof', p.squad_id || ':' || p.task_idx || ':' || p.scope || ':' || p.cell_idx || ':' || p.idx,
                    c.cwd, p.state
             FROM proofs p JOIN cells c ON c.squad_id=p.squad_id AND c.task_idx=p.task_idx
              AND ((p.scope='cell' AND c.idx=p.cell_idx) OR
                   (p.scope='task' AND c.idx=(SELECT MIN(c2.idx) FROM cells c2
                     WHERE c2.squad_id=p.squad_id AND c2.task_idx=p.task_idx)))
             WHERE c.cwd IS NOT NULL
             UNION ALL
             SELECT 'review', g.id, gb.worktree, g.status
             FROM guardians g JOIN guardian_branches gb ON gb.guardian_id=g.id
             WHERE gb.worktree IS NOT NULL
             UNION ALL
             SELECT 'review', id, combined_worktree, status
             FROM guardians WHERE combined_worktree IS NOT NULL",
        )?;
        Ok(stmt
            .query_map([], |r| {
                Ok(WorktreeClaim {
                    kind: r.get(0)?,
                    owner: r.get(1)?,
                    path: r.get(2)?,
                    state: r.get(3)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// Forget a review path after git confirmed that worktree was removed.
    pub(crate) fn clear_guardian_worktree_path(&self, path: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE guardian_branches SET worktree=NULL WHERE worktree=?1",
            params![path],
        )?;
        self.conn.execute(
            "UPDATE guardians SET combined_worktree=NULL WHERE combined_worktree=?1",
            params![path],
        )?;
        Ok(())
    }

    /// Record (or overwrite) a worktree retirement attempt (RAL-385; `status`
    /// widened by RAL-386 to include `deferred`/`opted_out`, see
    /// [`GuardianWorktreeRetirementRecord`]). A later attempt for the same
    /// `(guardian, path)` replaces the earlier one, so a worktree that failed
    /// once and was removed on a later daily sweep reads as `retired`, while a
    /// repeated failure keeps only the latest error.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn record_guardian_worktree_retirement(
        &self,
        guardian_id: &str,
        path: &str,
        status: &str,
        error: Option<&str>,
        eligible_at_ms: i64,
        last_attempt_ms: i64,
        retry_at_ms: Option<i64>,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO guardian_worktree_retirements
                 (guardian_id, path, status, error, eligible_at_ms, last_attempt_ms, retry_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(guardian_id, path) DO UPDATE SET
                 status=excluded.status,
                 error=excluded.error,
                 eligible_at_ms=excluded.eligible_at_ms,
                 last_attempt_ms=excluded.last_attempt_ms,
                 retry_at_ms=excluded.retry_at_ms",
            params![
                guardian_id,
                path,
                status,
                error,
                eligible_at_ms,
                last_attempt_ms,
                retry_at_ms,
            ],
        )?;
        Ok(())
    }

    /// Every durable retirement attempt, oldest first.
    pub(crate) fn guardian_worktree_retirements(
        &self,
    ) -> Result<Vec<GuardianWorktreeRetirementRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT guardian_id, path, status, error, eligible_at_ms, last_attempt_ms, retry_at_ms
             FROM guardian_worktree_retirements ORDER BY last_attempt_ms, guardian_id, path",
        )?;
        Ok(stmt
            .query_map([], |r| {
                Ok(GuardianWorktreeRetirementRecord {
                    guardian_id: r.get(0)?,
                    path: r.get(1)?,
                    status: r.get(2)?,
                    error: r.get(3)?,
                    eligible_at_ms: r.get(4)?,
                    last_attempt_ms: r.get(5)?,
                    retry_at_ms: r.get(6)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// A retired worktree row outlives its path columns, so the retirement
    /// view resolves its review's display name by id (RAL-385).
    pub(crate) fn guardian_display_name(&self, id: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .prepare("SELECT name FROM guardians WHERE id=?1")?
            .query_row([id], |r| r.get(0))
            .optional()?)
    }

    /// Persist the effective read-only system prompt shown for a cell in
    /// the board details pane.
    pub fn set_cell_effective_system_prompt(
        &self,
        squad_id: &str,
        task_idx: i64,
        idx: i64,
        system_prompt: Option<&str>,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE cells SET effective_system_prompt=? WHERE squad_id=? AND task_idx=? AND idx=?",
            params![system_prompt, squad_id, task_idx, idx],
        )?;
        Ok(())
    }

    /// The effective environment map a cell first started with, after
    /// placeholder expansion. `None` means the cell has not materialized one
    /// yet; `Some({})` means it did, and it resolved to an empty map.
    pub fn get_cell_materialized_env_overrides(
        &self,
        squad_id: &str,
        task_idx: i64,
        idx: i64,
    ) -> Result<Option<BTreeMap<String, String>>> {
        let raw: Option<String> = self
            .conn
            .query_row(
                "SELECT materialized_env_overrides FROM cells WHERE squad_id=? AND task_idx=? AND idx=?",
                params![squad_id, task_idx, idx],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        Ok(raw.as_deref().map(from_json_map))
    }

    /// Persist the effective environment map a cell first started with.
    pub fn set_cell_materialized_env_overrides(
        &self,
        squad_id: &str,
        task_idx: i64,
        idx: i64,
        env: &BTreeMap<String, String>,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE cells SET materialized_env_overrides=? WHERE squad_id=? AND task_idx=? AND idx=?",
            params![to_json_map(env), squad_id, task_idx, idx],
        )?;
        Ok(())
    }

    /// Persist the effective read-only system prompt shown for a proof step in
    /// the board details pane.
    pub fn set_proof_effective_system_prompt(
        &self,
        squad_id: &str,
        task_idx: i64,
        scope: &str,
        cell_idx: i64,
        idx: i64,
        system_prompt: Option<&str>,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE proofs SET effective_system_prompt=? WHERE squad_id=? AND task_idx=? AND scope=? AND cell_idx=? AND idx=?",
            params![system_prompt, squad_id, task_idx, scope, cell_idx, idx],
        )?;
        Ok(())
    }

    /// The effective environment map a proof step first started with, after
    /// placeholder expansion. `None` means it has not materialized one yet.
    pub fn get_proof_materialized_env_overrides(
        &self,
        squad_id: &str,
        task_idx: i64,
        scope: &str,
        cell_idx: i64,
        idx: i64,
    ) -> Result<Option<BTreeMap<String, String>>> {
        let raw: Option<String> = self
            .conn
            .query_row(
                "SELECT materialized_env_overrides FROM proofs
                 WHERE squad_id=? AND task_idx=? AND scope=? AND cell_idx=? AND idx=?",
                params![squad_id, task_idx, scope, cell_idx, idx],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        Ok(raw.as_deref().map(from_json_map))
    }

    /// Persist the effective environment map a proof step first started with.
    pub fn set_proof_materialized_env_overrides(
        &self,
        squad_id: &str,
        task_idx: i64,
        scope: &str,
        cell_idx: i64,
        idx: i64,
        env: &BTreeMap<String, String>,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE proofs SET materialized_env_overrides=?
             WHERE squad_id=? AND task_idx=? AND scope=? AND cell_idx=? AND idx=?",
            params![to_json_map(env), squad_id, task_idx, scope, cell_idx, idx],
        )?;
        Ok(())
    }
}

/// Levenshtein edit distance between two strings (byte-oriented; inputs here
/// are always ASCII-ish project names/queries). Used by
/// [`Store::resolve_project`]'s fuzzy fallback.
fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            cur[j + 1] = (cur[j] + 1).min(prev[j + 1] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// The state shown for a cell in the board API/UI.
///
/// The persisted `cells.state` column is set to `done` as soon as the
/// cell's own agent body finishes, *before* its cell-level proof
/// steps run — deliberately, so crash recovery can tell "body finished,
/// proof pending" apart from a fresh cell and only re-run the proof
/// tail rather than redoing the (expensive) agent work (RAL-64; see
/// `Store::done_cells`). Left as-is, that reads to a viewer as the
/// cell being finished while its proof checklist is still running or
/// has failed. This folds proof progress back in for display only, without
/// touching the persisted column the scheduler relies on.
fn effective_cell_state<'a>(raw: &str, proof_states: impl IntoIterator<Item = &'a str>) -> String {
    if raw != "done" {
        return raw.to_string();
    }
    let states: Vec<&str> = proof_states.into_iter().collect();
    if states.contains(&"failed") {
        return "failed".to_string();
    }
    if states
        .iter()
        .any(|s| !matches!(*s, "done" | "failed" | "cancelled" | "ignored"))
    {
        return "running".to_string();
    }
    raw.to_string()
}

/// The state shown for a squad in the board API/UI (see [`STATUS_ISSUE.local.md`]
/// for the full writeup of the bug this fixes).
///
/// `squads.state == "pending"` is overloaded. For a fresh submission it means
/// exactly what it says: nothing has been claimed yet. But
/// `Store::restart_cell_proof`/`restart_task_proof`/`restart_cell`
/// also write the squad back to `pending` purely as a "reclaim me on the next
/// tick" signal to `scheduler::claim_ready` — and, when the squad's worker
/// thread is still alive driving *other*, unrelated cells (deliberately
/// left alone rather than cancelled, precisely so an unrelated sibling isn't
/// interrupted — see `claim_ready`'s doc comment), that worker won't discover
/// the restarted target until it finishes everything else and a fresh worker
/// re-claims the squad. During that whole window the persisted column reads
/// `pending` even though the squad plainly has live children.
///
/// This mirrors [`effective_cell_state`] one level up: fold the squad's
/// children's real state back in for display, without touching the
/// `squads.state` column the scheduler itself reads via `list_ready`/
/// `claim_ready`. Unlike the board's client-side `isDowntimeWaiting`
/// relabeling (a purely cosmetic pill swap — that squad truly has no live
/// children, so filters/menus deliberately keep using the raw `pending`),
/// this squad *does* have something genuinely in flight, so the correction
/// is real, not cosmetic, and is applied here so every consumer (CLI, board
/// filters/menus, `/api/squads`) sees the same corrected value.
///
/// Deliberately queries the raw `cells`/`proofs` columns rather than
/// scanning the already-built [`TaskView`]s: [`effective_cell_state`]
/// folds a "done, proof still pending" cell's *displayed* state to
/// `"running"` too (meaning "not fully resolved", not "currently
/// executing") — reusing that folded value here would fire for the ordinary,
/// no-live-worker restart case as well (nothing is actually executing, a
/// proof is merely queued), which is exactly [`Self::restart_cell_proof`]'s
/// own steady-state right after a restart. Only a literal raw `running` row
/// means an agent process is genuinely executing right now.
fn effective_squad_state(conn: &Connection, raw: String, squad_id: &str) -> Result<String> {
    if raw != "pending" {
        return Ok(raw);
    }
    let running_cells: i64 = conn.query_row(
        "SELECT COUNT(*) FROM cells WHERE squad_id=? AND state='running'",
        params![squad_id],
        |r| r.get(0),
    )?;
    if running_cells > 0 {
        return Ok("running".to_string());
    }
    let running_proofs: i64 = conn.query_row(
        "SELECT COUNT(*) FROM proofs WHERE squad_id=? AND state='running'",
        params![squad_id],
        |r| r.get(0),
    )?;
    Ok(if running_proofs > 0 {
        "running".to_string()
    } else {
        raw
    })
}

/// Fallback project identifier for a task whose TOML left `project` unset
/// (RAL-141): the basename of its first cell's `cwd`, so a task that only
/// sets a literal filesystem path still gets a usable, stable identifier for
/// a board project filter facet to group by. Falls back further to
/// `"unassigned"` when there's no cell, no `cwd`, or the `cwd` has no
/// filename component (e.g. `"/"`). Never applied to a task using the
/// `ralphus:new-worktree/<branch>` placeholder cwd -- `project` is already
/// structurally required for those (see `core::validate`), so this path is
/// only reached for plain-path tasks.
pub(crate) fn fallback_project_identifier(cwd: Option<&str>) -> String {
    cwd.and_then(|c| Path::new(c).file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unassigned".to_string())
}

/// Resolve an effective timeout in seconds from a step-level and a task-level
/// value in minutes (step wins; task is the default). `None` means no limit.
fn resolve_timeout_sec(step_min: Option<u32>, task_min: Option<u32>) -> Option<i64> {
    step_min.or(task_min).map(|m| i64::from(m) * 60)
}

/// Resolve an effective token budget from a step-level and a task-level value
/// (step wins; task is the default). `None` means no cap.
fn resolve_budget(step: Option<u64>, task: Option<u64>) -> Option<i64> {
    step.or(task).map(|b| i64::try_from(b).unwrap_or(i64::MAX))
}

/// Resolve an effective USD spend cap from a cell-level and a task-level
/// value (cell wins; task is the default). `None` means no cap.
fn resolve_maximum_budget_usd(cell: Option<f64>, task: Option<f64>) -> Option<f64> {
    cell.or(task)
}

/// Resolve an effective context-window token limit from a cell-level and a
/// task-level value (cell wins; task is the default). `None` means no cap
/// (RAL-304).
fn resolve_maximum_context(cell: Option<u64>, task: Option<u64>) -> Option<i64> {
    cell.or(task).map(|v| i64::try_from(v).unwrap_or(i64::MAX))
}

/// Resolve an effective auto-compact trigger threshold from a cell-level and
/// a task-level value (cell wins; task is the default). `None` means no
/// explicit threshold (RAL-304).
fn resolve_auto_compact_threshold(cell: Option<u64>, task: Option<u64>) -> Option<i64> {
    cell.or(task).map(|v| i64::try_from(v).unwrap_or(i64::MAX))
}

#[allow(clippy::too_many_arguments)]
fn insert_proof(
    tx: &rusqlite::Transaction<'_>,
    squad_id: &str,
    task_idx: i64,
    scope: &str,
    cell_idx: i64,
    v_idx: usize,
    v: &ralphus_core::schema::ProofStep,
    task: &ralphus_core::schema::TaskDef,
    // The proof step's real owning cell (`Some`) for a cell-scope proof, or
    // `None` for a task-scope proof -- so `maximum_tool_output_tokens` resolves
    // against the step's actual parent (RAL-333) rather than skipping the
    // cell level the way `timeout_sec`/`budget_tokens` above still do.
    owning_cell: Option<&ralphus_core::schema::CellDef>,
    agent: &str,
) -> Result<()> {
    let (kind, spec) = if let Some(c) = &v.command {
        ("command", c.clone())
    } else if let Some(b) = &v.brain {
        ("brain", b.clone())
    } else if let Some(p) = &v.prompt {
        ("prompt", p.clone())
    } else if v.requires_approval {
        ("approval", String::new())
    } else {
        ("unknown", String::new())
    };
    let timeout_sec = resolve_timeout_sec(v.timeout_minutes, task.timeout_minutes);
    let budget_tokens = resolve_budget(v.budget_tokens, task.budget_tokens);
    let maximum_tool_output_tokens = match owning_cell {
        Some(cell) => {
            ralphus_core::schema::resolve_cell_proof_maximum_tool_output_tokens(task, cell, v)
        }
        None => ralphus_core::schema::resolve_task_proof_maximum_tool_output_tokens(task, v),
    }
    .map(|val| i64::try_from(val).unwrap_or(i64::MAX));
    let effective_system_prompt = if kind == "prompt" {
        Some(effective_proof_system_prompt(None))
    } else {
        None
    };
    tx.execute(
        "INSERT INTO proofs(squad_id, task_idx, scope, cell_idx, idx, vid, kind, spec, effective_system_prompt, model, agent, state, timeout_sec, budget_tokens, maximum_tool_output_tokens, env_overrides)
         VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
        params![
            squad_id,
            task_idx,
            scope,
            cell_idx,
            i64::try_from(v_idx).unwrap_or(0),
            v.id,
            kind,
            spec,
            effective_system_prompt,
            v.model,
            agent,
            NodeState::Pending.as_str(),
            timeout_sec,
            budget_tokens,
            maximum_tool_output_tokens,
            // RAL-191: the step's TOML-declared `environment` seeds the same
            // column `POST .../proof/{vi}/env` writes to, so a declared value
            // and one set later are indistinguishable from here on.
            to_json_map(&v.environment),
        ],
    )?;
    Ok(())
}

/// The executable fields of a cell, as the scheduler needs them to build a
/// runner spec.
#[derive(Debug, Clone)]
pub struct CellRow {
    /// Index of the owning task within the squad.
    pub task_idx: i64,
    /// Index of the cell within the task.
    pub idx: i64,
    /// Owning task name.
    pub task_name: String,
    /// Cell id.
    pub cell_id: String,
    /// Working directory.
    pub cwd: Option<String>,
    /// Subproject paths within the repository root (e.g. `["packages/foo"]`).
    /// Empty when the cell targets the whole repository (RAL-23).
    pub subprojects: Vec<String>,
    /// AI prompt (mutually exclusive with `command`).
    pub prompt: Option<String>,
    /// Deterministic command (mutually exclusive with `prompt`).
    pub command: Option<String>,
    /// Resolved agent program.
    pub agent: String,
    /// Resolved model.
    pub model: Option<String>,
    /// Appended system-prompt text, delivered to the agent via its
    /// append-system-prompt mechanism (RAL-5). `None` when unset.
    pub system_prompt: Option<String>,
    /// Placement of `system_prompt` (only `"append"` today). `None` when unset.
    pub system_prompt_position: Option<String>,
    /// This cell's dependency references (within-task cell ids or
    /// cross-task `task/cell`).
    pub depends_on: Vec<String>,
    /// Effective wall-clock timeout in seconds (resolved from cell/task), or
    /// `None` for no limit. The daemon kills the runner subprocess past this.
    pub timeout_sec: Option<i64>,
    /// Effective total-token budget (resolved from cell/task), or `None` for
    /// no cap. The runner fails the cell if usage exceeds it.
    pub budget_tokens: Option<i64>,
    /// Effective USD spend cap (resolved from cell/task), or `None` for no
    /// cap. The runner kills the cell mid-run and fails it once the live
    /// `cost_usd` exceeds this (RAL-161).
    pub maximum_budget_usd: Option<f64>,
    /// Effective context-window token limit (resolved from cell/task), or
    /// `None` for no cap (RAL-304). Delivered to the backend via its own
    /// mechanism -- see `ralphus_core::schema::agent_supports_maximum_context`.
    pub maximum_context: Option<i64>,
    /// Effective auto-compact trigger threshold in tokens (resolved from
    /// cell/task), or `None` for no explicit threshold (RAL-304). Delivered
    /// to the backend via its own mechanism -- see
    /// `ralphus_core::schema::agent_supports_auto_compact_threshold`.
    /// Accepted by a wider set of backends than [`Self::maximum_context`].
    pub auto_compact_threshold: Option<i64>,
    /// Effective tool-output token cap (resolved from cell/task), or `None`
    /// for no cap (RAL-333). Delivered to the backend via its own mechanism
    /// -- see `ralphus_core::schema::agent_supports_maximum_tool_output_tokens`.
    pub maximum_tool_output_tokens: Option<i64>,
    /// Upstream sentinel, e.g. `"<<task:task-name>>"`. When present the
    /// scheduler rebases this cell's branch onto the named dependency's
    /// current branch tip before starting the runner (RAL-50).
    pub upstream: Option<String>,
    /// The resolved machine this cell runs on (RAL-185), as authored --
    /// e.g. `"incredibuild:A"`. `None` means the daemon's own host, which is
    /// every pre-RAL-185 row and every cell that never declared one.
    /// Resolved at submit so a later edit to the task file cannot move an
    /// in-flight squad's machine.
    pub machine: Option<String>,
    /// Whether this cell may resume a completed dependency's agent session
    /// (cross-cell session sharing), resolved at submit from the cell's/task's
    /// `share_session` TOML field (off by default) -- see
    /// `ralphus_core::schema::resolve_cell_share_session`.
    pub share_session: bool,
}

/// Editable cell definition fields (from the details pane).
///
/// `cwd`/`model`/`prompt`/`command`/`system_prompt` are nested `Option`s so a
/// caller can say three different things per field: `None` -- the caller
/// didn't mention this field, leave the column as it already is;
/// `Some(None)` -- the caller gave an empty value, clear the column to NULL;
/// `Some(Some(v))` -- set the column to `v`. `agent` can't be NULL
/// (`cells.agent` is `NOT NULL`), so it only has the "untouched" (`None`) and
/// "set" (`Some(v)`) states.
#[derive(Debug, Clone)]
pub struct CellEdit<'a> {
    /// Working directory.
    pub cwd: Option<Option<&'a str>>,
    /// Agent program.
    pub agent: Option<&'a str>,
    /// Model.
    pub model: Option<Option<&'a str>>,
    /// AI prompt (mutually exclusive with command).
    pub prompt: Option<Option<&'a str>>,
    /// Shell command.
    pub command: Option<Option<&'a str>>,
    /// Per-cell auto-compact trigger, in tokens (RAL-304).
    pub auto_compact_threshold: Option<Option<i64>>,
    /// Per-cell cap on a single tool-call output, in tokens (RAL-333). The
    /// caller (`edit_squad`'s `"cell"` arm) rejects this up front when the
    /// cell's effective agent has no delivery mechanism for it -- see
    /// `ralphus_core::schema::agent_supports_maximum_tool_output_tokens` --
    /// before it ever reaches the store.
    pub maximum_tool_output_tokens: Option<Option<i64>>,
    /// Appended system prompt (RAL-341). The caller (`edit_squad`'s `"cell"`
    /// arm) is responsible for rejecting this up front when the cell's
    /// effective agent doesn't support it -- see
    /// `ralphus_core::schema::agent_supports_system_prompt` -- before this
    /// ever reaches the store. `system_prompt_position` is out of scope for
    /// editing (RAL-341): it stays whatever it was set to at submit time.
    pub system_prompt: Option<Option<&'a str>>,
}

/// Editable task definition fields. Same nested-`Option` nullable-field
/// semantics as [`CellEdit`]. `name` can't be NULL (`tasks.name` is
/// `NOT NULL`), so like `CellEdit::agent` it only has the "untouched"
/// (`None`) and "set" (`Some(v)`) states.
#[derive(Debug, Clone)]
pub struct TaskEdit<'a> {
    /// Task name.
    pub name: Option<&'a str>,
    /// Project this task's cells resolve their cwd against.
    pub project: Option<Option<&'a str>>,
    /// Model.
    pub model: Option<Option<&'a str>>,
}

/// Editable proof step definition fields. A proof step has no separate
/// `agent` selector (it always runs under its owning cell's/task's resolved
/// agent program -- see `core::schema::ProofStep`'s doc comment), so `model`
/// is the only editable field. Same nullable-field semantics as
/// [`CellEdit::model`].
#[derive(Debug, Clone)]
pub struct ProofEdit<'a> {
    /// Model override (meaningful for `prompt`-kind steps).
    pub model: Option<Option<&'a str>>,
    /// Per-step cap on a single tool-call output, in tokens (RAL-333).
    /// Gated on the step's own `agent` by the caller, the same way
    /// [`CellEdit::maximum_tool_output_tokens`] is gated on the cell's.
    pub maximum_tool_output_tokens: Option<Option<i64>>,
}

/// A task's identity and dependencies, for scheduling.
#[derive(Debug, Clone)]
pub struct TaskRow {
    /// Task index within the squad.
    pub idx: i64,
    /// Task name.
    pub name: String,
    /// Registered project name (RAL-100). Required whenever any of this
    /// task's cells uses a `ralphus:new-worktree/<branch>` placeholder
    /// `cwd` -- that's the project the daemon materializes the worktree
    /// under.
    pub project: Option<String>,
    /// Task-level dependency references.
    pub depends_on: Vec<String>,
    /// Whether this task is currently soloed (RAL-157). See [`TaskView::soloed`].
    pub soloed: bool,
}

/// What the finalizer's no-new-commits guard (RAL-156, RAL-293-amended)
/// needs to decide whether a task passes: its registered project (to check
/// git-ness) and its `no_commit_required` opt-out. See
/// [`Store::task_commit_guard_info`].
#[derive(Debug, Clone)]
pub struct TaskCommitGuardInfo {
    /// Registered project name, if any. `None` means the task isn't
    /// git-backed (RAL-156 Q1) and the guard never runs.
    pub project: Option<String>,
    /// Opts the task out of the guard entirely (RAL-156 Q4).
    pub no_commit_required: bool,
}

/// The full set of entities a restart would dirty (RAL-104): cells/tasks
/// reset to Pending within the target squad, and other squads — transitively
/// dependent on it — that get dirtied too. Computed once by
/// [`Store::compute_squad_restart_impact`] / [`Store::compute_cell_restart_impact`]
/// and shared by both the non-mutating dry-run preview and the real restart,
/// so the two can never drift out of sync.
#[derive(Debug, Clone, Default, Serialize)]
pub struct RestartImpact {
    /// Cells within the target squad that will be reset to Pending.
    pub cells: Vec<RestartImpactCell>,
    /// Tasks within the target squad that will be reset to Pending.
    pub tasks: Vec<RestartImpactTask>,
    /// Other squads, transitively dependent on the target squad, that will be
    /// dirtied (reset to Pending).
    pub dirtied_squads: Vec<RestartImpactSquad>,
}

/// One cell affected by a restart, for display in the dry-run preview.
#[derive(Debug, Clone, Serialize)]
pub struct RestartImpactCell {
    /// Index of the owning task within the squad.
    pub task_idx: i64,
    /// Index of the cell within the task.
    pub idx: i64,
    /// Owning task name.
    pub task_name: String,
    /// Cell id.
    pub cell_id: String,
}

/// One task affected by a restart, for display in the dry-run preview.
#[derive(Debug, Clone, Serialize)]
pub struct RestartImpactTask {
    /// Task index within the squad.
    pub idx: i64,
    /// Task name.
    pub name: String,
}

/// One dependent squad that would be dirtied by a restart, for display in the
/// dry-run preview.
#[derive(Debug, Clone, Serialize)]
pub struct RestartImpactSquad {
    /// Squad id.
    pub id: String,
    /// Optional human label.
    pub label: Option<String>,
}

/// Everything cancelling a squad affects, computed by [`Store::cancel_squad`] and
/// shared by the non-mutating dry-run preview and the real cascading cancel
/// (RAL-116), so the two can never drift out of sync.
#[derive(Debug, Clone, Default, Serialize)]
pub struct CancelImpact {
    /// The target squad plus every squad transitively dependent on it — all of
    /// which will be (or were) cancelled. The target squad is always first.
    pub squads: Vec<RestartImpactSquad>,
}

impl Store {
    /// All cells of a squad, in insertion order.
    pub fn cells_of(&self, squad_id: &str) -> Result<Vec<CellRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT s.task_idx, s.idx, t.name, s.sid, s.cwd, s.subprojects, s.prompt, s.command, s.agent, s.model, s.system_prompt, s.system_prompt_position, s.depends_on, s.timeout_sec, s.budget_tokens, s.upstream, s.maximum_budget_usd, s.machine, s.maximum_context, s.auto_compact_threshold, s.maximum_tool_output_tokens, s.share_session
             FROM cells s JOIN tasks t ON t.squad_id = s.squad_id AND t.idx = s.task_idx
             WHERE s.squad_id = ? ORDER BY s.task_idx, s.idx",
        )?;
        let rows = stmt
            .query_map(params![squad_id], |r| {
                Ok(CellRow {
                    task_idx: r.get(0)?,
                    idx: r.get(1)?,
                    task_name: r.get(2)?,
                    cell_id: r.get(3)?,
                    cwd: r.get(4)?,
                    subprojects: r
                        .get::<_, Option<String>>(5)?
                        .map(|s| from_json(&s))
                        .unwrap_or_default(),
                    prompt: r.get(6)?,
                    command: r.get(7)?,
                    agent: r.get(8)?,
                    model: r.get(9)?,
                    system_prompt: r.get(10)?,
                    system_prompt_position: r.get(11)?,
                    depends_on: from_json(&r.get::<_, String>(12)?),
                    timeout_sec: r.get(13)?,
                    budget_tokens: r.get(14)?,
                    upstream: r.get(15)?,
                    maximum_budget_usd: r.get(16)?,
                    machine: r.get(17)?,
                    maximum_context: r.get(18)?,
                    auto_compact_threshold: r.get(19)?,
                    maximum_tool_output_tokens: r.get(20)?,
                    share_session: r.get(21)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// All tasks of a squad with their dependencies, in order.
    pub fn tasks_of(&self, squad_id: &str) -> Result<Vec<TaskRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT idx, name, project, depends_on, soloed FROM tasks WHERE squad_id=? ORDER BY idx",
        )?;
        let rows = stmt
            .query_map(params![squad_id], |r| {
                Ok(TaskRow {
                    idx: r.get(0)?,
                    name: r.get(1)?,
                    project: r.get(2)?,
                    depends_on: from_json(&r.get::<_, String>(3)?),
                    soloed: r.get(4)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Fetch what the RAL-156 no-new-commits guard needs for one task.
    /// `Ok(None)` when the `(squad_id, task_idx)` pair doesn't exist.
    pub fn task_commit_guard_info(
        &self,
        squad_id: &str,
        task_idx: i64,
    ) -> Result<Option<TaskCommitGuardInfo>> {
        self.conn
            .query_row(
                "SELECT project, no_commit_required FROM tasks WHERE squad_id=? AND idx=?",
                params![squad_id, task_idx],
                |r| {
                    Ok(TaskCommitGuardInfo {
                        project: r.get(0)?,
                        no_commit_required: r.get::<_, i64>(1)? != 0,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    /// The cross-squad dependency references declared in the squad's `[[default]]`.
    pub fn squad_depends_on(&self, squad_id: &str) -> Result<Vec<String>> {
        let s: Option<String> = self
            .conn
            .query_row(
                "SELECT depends_on FROM squads WHERE id=?",
                params![squad_id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(from_json(&s.ok_or(StoreError::NotFound)?))
    }

    /// Add a cross-squad dependency (RAL-105): appends `target_id` to `squad_id`'s
    /// `[[default]] depends_on`, reusing the existing whole-squad gating in
    /// [`Store::list_ready`]/[`Store::deps_satisfied`] — no new scheduling
    /// path. A `squad_id`/`target_id` that doesn't exist is `NotFound`; a
    /// self-reference or a reference that would create a cycle in the
    /// cross-squad dependency graph is rejected as `InvalidTransition`. Adding
    /// a dependency that is already present is a no-op. Returns the squad's
    /// updated `depends_on` list.
    pub fn add_squad_dependency(&self, squad_id: &str, target_id: &str) -> Result<Vec<String>> {
        if squad_id == target_id {
            return Err(StoreError::InvalidTransition(
                "a squad cannot depend on itself".to_string(),
            ));
        }
        let mut deps = self.squad_depends_on(squad_id)?;
        self.squad_depends_on(target_id)?; // existence check
        if deps
            .iter()
            .any(|d| d.split('/').next().unwrap_or(d) == target_id)
        {
            return Ok(deps);
        }
        if self.squad_transitively_depends_on(target_id, squad_id)? {
            return Err(StoreError::InvalidTransition(format!(
                "adding a dependency on {target_id} would create a cycle"
            )));
        }
        deps.push(target_id.to_string());
        self.conn.execute(
            "UPDATE squads SET depends_on=?, updated_at_ms=? WHERE id=?",
            params![to_json(&deps), now_ms(), squad_id],
        )?;
        let _ = self.log_event(
            Some(squad_id),
            None,
            "squad",
            None,
            &format!("dependency added: now depends on {target_id}"),
        );
        Ok(deps)
    }

    /// Whether `from` transitively depends on `to` via cross-squad `depends_on`
    /// edges (BFS). Dangling references (a dep id that no longer resolves to
    /// a squad) are skipped — same best-effort philosophy as
    /// [`Store::global_graph`].
    fn squad_transitively_depends_on(&self, from: &str, to: &str) -> Result<bool> {
        let mut seen: HashSet<String> = HashSet::new();
        let mut frontier = vec![from.to_string()];
        while let Some(cur) = frontier.pop() {
            if cur == to {
                return Ok(true);
            }
            if !seen.insert(cur.clone()) {
                continue;
            }
            for dep in self.squad_depends_on(&cur).unwrap_or_default() {
                let dep_squad = dep.split('/').next().unwrap_or(&dep).to_string();
                frontier.push(dep_squad);
            }
        }
        Ok(false)
    }

    /// The distinct task indices of a squad, ascending.
    pub fn task_indices(&self, squad_id: &str) -> Result<Vec<i64>> {
        let mut stmt = self
            .conn
            .prepare("SELECT idx FROM tasks WHERE squad_id=? ORDER BY idx")?;
        let ids = stmt
            .query_map(params![squad_id], |r| r.get::<_, i64>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(ids)
    }

    /// Returns `true` when every task in `task_indices` has state `done` for the
    /// given squad. An empty set is vacuously true. Used by the per-task review
    /// gate to decide whether a guardian's blocking tasks have all finished.
    pub fn all_tasks_done(&self, squad_id: &str, task_indices: &HashSet<i64>) -> Result<bool> {
        if task_indices.is_empty() {
            return Ok(true);
        }
        let mut stmt = self
            .conn
            .prepare("SELECT idx FROM tasks WHERE squad_id=? AND state != 'done'")?;
        let non_done: Vec<i64> = stmt
            .query_map(params![squad_id], |r| r.get::<_, i64>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(non_done.iter().all(|idx| !task_indices.contains(idx)))
    }

    /// The proof steps of a scope (`"task"` with `cell_idx = -1`, or
    /// `"cell"` with the cell's index), in order:
    /// `(idx, kind, spec, model)` — `model` is only meaningful for `agent`.
    pub fn proof_specs(
        &self,
        squad_id: &str,
        task_idx: i64,
        scope: &str,
        cell_idx: i64,
    ) -> Result<Vec<ProofSpecRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT idx, kind, spec, model, timeout_sec, budget_tokens, maximum_tool_output_tokens FROM proofs
             WHERE squad_id=? AND task_idx=? AND scope=? AND cell_idx=? ORDER BY idx",
        )?;
        let rows = stmt
            .query_map(params![squad_id, task_idx, scope, cell_idx], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, Option<i64>>(4)?,
                    r.get::<_, Option<i64>>(5)?,
                    r.get::<_, Option<i64>>(6)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Fresh current state of one proof step (not a snapshot from
    /// [`Store::proof_specs`]) — lets the scheduler notice a user manually
    /// setting a not-yet-executed step to `ignored` mid-run and honor it
    /// instead of racing ahead with a stale snapshot.
    pub fn proof_state(
        &self,
        squad_id: &str,
        task_idx: i64,
        scope: &str,
        cell_idx: i64,
        idx: i64,
    ) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT state FROM proofs WHERE squad_id=? AND task_idx=? AND scope=? AND cell_idx=? AND idx=?",
                params![squad_id, task_idx, scope, cell_idx, idx],
                |r| r.get::<_, String>(0),
            )
            .optional()?)
    }

    /// Set a proof step's state.
    pub fn set_proof_state(
        &self,
        squad_id: &str,
        task_idx: i64,
        scope: &str,
        cell_idx: i64,
        idx: i64,
        state: NodeState,
    ) -> Result<()> {
        let old = self
            .conn
            .query_row(
                "SELECT state FROM proofs WHERE squad_id=? AND task_idx=? AND scope=? AND cell_idx=? AND idx=?",
                params![squad_id, task_idx, scope, cell_idx, idx],
                |r| r.get::<_, String>(0),
            )
            .optional()
            .ok()
            .flatten()
            .unwrap_or_else(|| "unknown".to_string());
        // RAL-271: see the matching comment in `set_cell_state`.
        self.conn.execute(
            "UPDATE proofs SET state=?, env_out_of_date=0
             WHERE squad_id=? AND task_idx=? AND scope=? AND cell_idx=? AND idx=?",
            params![state.as_str(), squad_id, task_idx, scope, cell_idx, idx],
        )?;
        crate::rlog!(
            DEBUG,
            "ralphus [state] proof {squad_id}/t{task_idx}/{scope}/#{idx} {old} → {}",
            state.as_str()
        );
        // RAL-155 Q2: same reasoning as `set_task_state`/`set_cell_state`.
        let task_name = self.task_name_at(squad_id, task_idx).ok().flatten();
        let _ = self.cartographer_log(crate::cartographer::CartographerEntry {
            level: crate::logging::LogLevel::DEBUG,
            source: "store",
            message: "proof state transition",
            scope: Some("proof"),
            squad_id: Some(squad_id),
            guardian_id: None,
            cell_id: None,
            task: task_name.as_deref(),
            log_path: None,
            payload: serde_json::json!({
                "task_idx": task_idx,
                "proof_scope": scope,
                "cell_idx": cell_idx,
                "idx": idx,
                "old": old,
                "new": state.as_str(),
            }),
            admin_only: false,
        });
        Ok(())
    }

    /// Record a proof step's terminal state and captured output, logging it (CCTL-99).
    ///
    /// RAL-163: guarded by `state IN (...)` the same way and for the same
    /// reason as [`Self::record_cell_result`] — a manual `set-status`
    /// override on this proof step while it's still mid-flight must not be
    /// clobbered once the scheduler's own runner call for it unblocks.
    #[allow(clippy::too_many_arguments)]
    pub fn set_proof_result(
        &self,
        squad_id: &str,
        task_idx: i64,
        scope: &str,
        cell_idx: i64,
        idx: i64,
        state: NodeState,
        output: &str,
        agent_session_id: Option<&str>,
        usage: RecordedUsage,
    ) -> Result<()> {
        // Query old state and vid together before the UPDATE so we have both for
        // logging (vid doesn't change, but reading it before avoids a second round trip).
        let (old, vid): (String, Option<String>) = self
            .conn
            .query_row(
                "SELECT state, vid FROM proofs WHERE squad_id=? AND task_idx=? AND scope=? AND cell_idx=? AND idx=?",
                params![squad_id, task_idx, scope, cell_idx, idx],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .ok()
            .flatten()
            .unwrap_or_else(|| ("unknown".to_string(), None));
        self.conn.execute(
            "UPDATE proofs SET state=?, output=?, agent_session_id=?, tokens_in=?, tokens_out=?, cache_creation_tokens=?, cache_read_tokens=?, compaction_input_tokens=?, compaction_count=?, cost_usd=?, cost_is_estimated=?
             WHERE squad_id=? AND task_idx=? AND scope=? AND cell_idx=? AND idx=? AND state IN ('pending', 'running', ?)",
            params![
                state.as_str(),
                output,
                agent_session_id,
                usage.tokens_in,
                usage.tokens_out,
                usage.cache_creation_tokens,
                usage.cache_read_tokens,
                usage.compaction_input_tokens,
                usage.compaction_count,
                usage.cost_usd,
                usage.cost_is_estimated,
                squad_id,
                task_idx,
                scope,
                cell_idx,
                idx,
                state.as_str(),
            ],
        )?;
        let reference = match &vid {
            Some(v) => format!("{scope} t{task_idx}/{v}"),
            None => format!("{scope} t{task_idx} #{idx}"),
        };
        crate::rlog!(
            INFO,
            "ralphus [state] proof {squad_id}/t{task_idx}/{scope}/#{idx} {old} → {} output_len={}",
            state.as_str(),
            output.len()
        );
        let _ = self.log_event(
            Some(squad_id),
            None,
            "proof",
            Some(&reference),
            &format!("proof → {}", state.as_str()),
        );
        Ok(())
    }

    /// Edit a squad's label.
    pub fn edit_squad_label(&self, squad_id: &str, label: Option<&str>) -> Result<()> {
        let n = self.conn.execute(
            "UPDATE squads SET label=?, updated_at_ms=? WHERE id=?",
            params![label, now_ms(), squad_id],
        )?;
        if n == 0 {
            Err(StoreError::NotFound)
        } else {
            let _ = self.notify_watchers(
                crate::monitor::NotifiableEventKind::SquadAttributesChanged,
                &format!("squad:{squad_id}"),
                crate::mailbox::MailboxPriority::Normal,
                "squad attributes changed",
                Some(squad_id),
            );
            Ok(())
        }
    }

    /// Edit a task's editable definition fields. A field the caller didn't
    /// mention (`None` in `edit`) leaves the corresponding column untouched
    /// -- see [`TaskEdit`]'s doc comment.
    pub fn edit_task_fields(
        &self,
        squad_id: &str,
        task_idx: i64,
        edit: &TaskEdit<'_>,
    ) -> Result<()> {
        let name_touched = edit.name.is_some();
        let project_touched = edit.project.is_some();
        let project_value = edit.project.flatten();
        let model_touched = edit.model.is_some();
        let model_value = edit.model.flatten();
        let n = self.conn.execute(
            "UPDATE tasks SET
                name = CASE WHEN :name_touched THEN :name ELSE name END,
                project = CASE WHEN :project_touched THEN :project ELSE project END,
                model = CASE WHEN :model_touched THEN :model ELSE model END
             WHERE squad_id=:squad_id AND idx=:idx",
            named_params! {
                ":name_touched": name_touched,
                ":name": edit.name,
                ":project_touched": project_touched,
                ":project": project_value,
                ":model_touched": model_touched,
                ":model": model_value,
                ":squad_id": squad_id,
                ":idx": task_idx,
            },
        )?;
        if n == 0 {
            Err(StoreError::NotFound)
        } else {
            let _ = self.notify_watchers(
                crate::monitor::NotifiableEventKind::SquadContentChanged,
                &format!("squad:{squad_id}"),
                crate::mailbox::MailboxPriority::Normal,
                "squad task changed",
                Some(squad_id),
            );
            Ok(())
        }
    }

    /// Edit a proof step's editable definition fields. A field the caller
    /// didn't mention (`None` in `edit`) leaves the corresponding column
    /// untouched -- see [`ProofEdit`]'s doc comment.
    pub fn edit_proof_fields(
        &self,
        squad_id: &str,
        task_idx: i64,
        scope: &str,
        cell_idx: i64,
        idx: i64,
        edit: &ProofEdit<'_>,
    ) -> Result<()> {
        let model_touched = edit.model.is_some();
        let model_value = edit.model.flatten();
        let maximum_tool_output_tokens_touched = edit.maximum_tool_output_tokens.is_some();
        let maximum_tool_output_tokens_value = edit.maximum_tool_output_tokens.flatten();
        let n = self.conn.execute(
            "UPDATE proofs SET
                model = CASE WHEN :model_touched THEN :model ELSE model END,
                maximum_tool_output_tokens = CASE WHEN :maximum_tool_output_tokens_touched THEN :maximum_tool_output_tokens ELSE maximum_tool_output_tokens END
             WHERE squad_id=:squad_id AND task_idx=:task_idx AND scope=:scope AND cell_idx=:cell_idx AND idx=:idx",
            named_params! {
                ":model_touched": model_touched,
                ":model": model_value,
                ":maximum_tool_output_tokens_touched": maximum_tool_output_tokens_touched,
                ":maximum_tool_output_tokens": maximum_tool_output_tokens_value,
                ":squad_id": squad_id,
                ":task_idx": task_idx,
                ":scope": scope,
                ":cell_idx": cell_idx,
                ":idx": idx,
            },
        )?;
        if n == 0 {
            Err(StoreError::NotFound)
        } else {
            let _ = self.notify_watchers(
                crate::monitor::NotifiableEventKind::SquadContentChanged,
                &format!("squad:{squad_id}"),
                crate::mailbox::MailboxPriority::Normal,
                "squad proof changed",
                Some(squad_id),
            );
            Ok(())
        }
    }

    /// Edit a cell's editable definition fields. A field the caller didn't
    /// mention (`None` in `edit`) leaves the corresponding column
    /// untouched -- see [`CellEdit`]'s doc comment. Exactly one of
    /// `prompt` / `command` should be non-empty when both are touched (the
    /// other is cleared); the caller (`edit_squad`'s `"cell"` arm) resolves
    /// that XOR before building `edit`.
    ///
    /// `effective_system_prompt` is recomputed whenever either `prompt` (set,
    /// not cleared to a command) or `system_prompt` is touched: editing
    /// `system_prompt` directly recomputes from the *new* value (bypassing
    /// the recompute-from-authored-value path), while editing only `prompt`
    /// keeps recomputing from whatever `system_prompt` is already stored --
    /// the same precedence create-time submission uses (RAL-341).
    pub fn edit_cell_fields(
        &self,
        squad_id: &str,
        task_idx: i64,
        idx: i64,
        edit: &CellEdit<'_>,
    ) -> Result<()> {
        let (subprojects, authored_system_prompt): (Vec<String>, Option<String>) = self
            .conn
            .query_row(
                "SELECT subprojects, system_prompt FROM cells WHERE squad_id=? AND task_idx=? AND idx=?",
                params![squad_id, task_idx, idx],
                |r| {
                    Ok((
                        r.get::<_, Option<String>>(0)?
                            .map(|raw| from_json(&raw))
                            .unwrap_or_default(),
                        r.get::<_, Option<String>>(1)?,
                    ))
                },
            )
            .optional()?
            .ok_or(StoreError::NotFound)?;

        let cwd_touched = edit.cwd.is_some();
        let cwd_value = edit.cwd.flatten();
        let agent_touched = edit.agent.is_some();
        let model_touched = edit.model.is_some();
        let model_value = edit.model.flatten();
        let prompt_touched = edit.prompt.is_some();
        let prompt_value = edit.prompt.flatten();
        let command_touched = edit.command.is_some();
        let command_value = edit.command.flatten();
        let auto_compact_threshold_touched = edit.auto_compact_threshold.is_some();
        let auto_compact_threshold_value = edit.auto_compact_threshold.flatten();
        let maximum_tool_output_tokens_touched = edit.maximum_tool_output_tokens.is_some();
        let maximum_tool_output_tokens_value = edit.maximum_tool_output_tokens.flatten();
        let system_prompt_touched = edit.system_prompt.is_some();
        let system_prompt_value = edit.system_prompt.flatten();
        let effective_authored_system_prompt = if system_prompt_touched {
            system_prompt_value
        } else {
            authored_system_prompt.as_deref()
        };
        let effective_system_prompt_touched = system_prompt_touched || prompt_value.is_some();
        let effective_system_prompt = effective_system_prompt_touched
            .then(|| effective_cell_system_prompt(effective_authored_system_prompt, &subprojects));

        let n = self.conn.execute(
            "UPDATE cells SET
                cwd = CASE WHEN :cwd_touched THEN :cwd ELSE cwd END,
                agent = CASE WHEN :agent_touched THEN :agent ELSE agent END,
                model = CASE WHEN :model_touched THEN :model ELSE model END,
                prompt = CASE WHEN :prompt_touched THEN :prompt ELSE prompt END,
                command = CASE WHEN :command_touched THEN :command ELSE command END,
                system_prompt = CASE WHEN :system_prompt_touched THEN :system_prompt ELSE system_prompt END,
                effective_system_prompt = CASE WHEN :effective_system_prompt_touched THEN :effective_system_prompt ELSE effective_system_prompt END,
                auto_compact_threshold = CASE WHEN :auto_compact_threshold_touched THEN :auto_compact_threshold ELSE auto_compact_threshold END,
                maximum_tool_output_tokens = CASE WHEN :maximum_tool_output_tokens_touched THEN :maximum_tool_output_tokens ELSE maximum_tool_output_tokens END
             WHERE squad_id=:squad_id AND task_idx=:task_idx AND idx=:idx",
            named_params! {
                ":cwd_touched": cwd_touched,
                ":cwd": cwd_value,
                ":agent_touched": agent_touched,
                ":agent": edit.agent,
                ":model_touched": model_touched,
                ":model": model_value,
                ":prompt_touched": prompt_touched,
                ":prompt": prompt_value,
                ":command_touched": command_touched,
                ":command": command_value,
                ":system_prompt_touched": system_prompt_touched,
                ":system_prompt": system_prompt_value,
                ":effective_system_prompt_touched": effective_system_prompt_touched,
                ":effective_system_prompt": effective_system_prompt,
                ":auto_compact_threshold_touched": auto_compact_threshold_touched,
                ":auto_compact_threshold": auto_compact_threshold_value,
                ":maximum_tool_output_tokens_touched": maximum_tool_output_tokens_touched,
                ":maximum_tool_output_tokens": maximum_tool_output_tokens_value,
                ":squad_id": squad_id,
                ":task_idx": task_idx,
                ":idx": idx,
            },
        )?;
        if n == 0 {
            Err(StoreError::NotFound)
        } else {
            let _ = self.notify_watchers(
                crate::monitor::NotifiableEventKind::SquadContentChanged,
                &format!("squad:{squad_id}"),
                crate::mailbox::MailboxPriority::Normal,
                "squad cell changed",
                Some(squad_id),
            );
            Ok(())
        }
    }

    /// The persistent environment-variable overrides currently set on a squad
    /// (RAL-150). Empty when none have ever been set.
    pub fn get_squad_env_overrides(&self, squad_id: &str) -> Result<BTreeMap<String, String>> {
        let raw: Option<String> = self
            .conn
            .query_row(
                "SELECT env_overrides FROM squads WHERE id=?",
                params![squad_id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(from_json_map(&raw.ok_or(StoreError::NotFound)?))
    }

    /// Add/replace (`set`) and remove (`unset`) entries in a squad's persistent
    /// environment-variable overrides (RAL-150), returning the resulting map.
    /// Overrides are persistent by design (Q4 of the ticket): once set, a key
    /// stays set across any number of retries until explicitly unset — this
    /// merges into whatever is already stored rather than replacing it
    /// wholesale. `set` entries win when a key appears in both `set` and
    /// `unset`.
    pub fn set_squad_env_overrides(
        &self,
        squad_id: &str,
        set: &BTreeMap<String, String>,
        unset: &[String],
    ) -> Result<BTreeMap<String, String>> {
        let mut current = self.get_squad_env_overrides(squad_id)?;
        for key in unset {
            current.remove(key);
        }
        for (k, v) in set {
            current.insert(k.clone(), v.clone());
        }
        self.conn.execute(
            "UPDATE squads SET env_overrides=?, updated_at_ms=? WHERE id=?",
            params![to_json_map(&current), now_ms(), squad_id],
        )?;
        let _ = self.notify_watchers(
            crate::monitor::NotifiableEventKind::SquadContentChanged,
            &format!("squad:{squad_id}"),
            crate::mailbox::MailboxPriority::Normal,
            "squad environment overrides changed",
            Some(squad_id),
        );
        Ok(current)
    }

    // ── Hierarchical env overrides (RAL-150 extension) ─────────────────────
    //
    // Task/cell-level layers, plus a separate layer for a task's/cell's
    // own proof steps, mirroring `get_squad_env_overrides`/
    // `set_squad_env_overrides` exactly (same set-wins-over-unset-for-same-key
    // merge, same persist-until-unset semantics). The three `resolve_*`
    // methods below fold each layer on top of its parents in one place, so
    // scheduler call sites stay a single call and precedence stays
    // unit-testable independent of dispatch.

    /// The persistent environment-variable overrides set directly on a task
    /// (not merged with the squad's). Empty when none have ever been set.
    pub fn get_task_env_overrides(
        &self,
        squad_id: &str,
        task_idx: i64,
    ) -> Result<BTreeMap<String, String>> {
        let raw: Option<String> = self
            .conn
            .query_row(
                "SELECT env_overrides FROM tasks WHERE squad_id=? AND idx=?",
                params![squad_id, task_idx],
                |r| r.get(0),
            )
            .optional()?;
        Ok(from_json_map(&raw.ok_or(StoreError::NotFound)?))
    }

    /// Add/replace (`set`) and remove (`unset`) entries in a task's own
    /// environment-variable overrides, returning the resulting map.
    ///
    /// RAL-271: also marks the task, and every cell it owns, "out of date"
    /// (cosmetic only) -- one ownership level down from the edited scope,
    /// never touching sibling tasks/cells.
    pub fn set_task_env_overrides(
        &self,
        squad_id: &str,
        task_idx: i64,
        set: &BTreeMap<String, String>,
        unset: &[String],
    ) -> Result<BTreeMap<String, String>> {
        let mut current = self.get_task_env_overrides(squad_id, task_idx)?;
        for key in unset {
            current.remove(key);
        }
        for (k, v) in set {
            current.insert(k.clone(), v.clone());
        }
        self.conn.execute(
            "UPDATE tasks SET env_overrides=?, env_out_of_date=1 WHERE squad_id=? AND idx=?",
            params![to_json_map(&current), squad_id, task_idx],
        )?;
        self.conn.execute(
            "UPDATE cells SET env_out_of_date=1 WHERE squad_id=? AND task_idx=?",
            params![squad_id, task_idx],
        )?;
        let _ = self.notify_watchers(
            crate::monitor::NotifiableEventKind::SquadContentChanged,
            &format!("task:{squad_id}:{task_idx}"),
            crate::mailbox::MailboxPriority::Normal,
            "task environment overrides changed",
            Some(squad_id),
        );
        Ok(current)
    }

    /// The persistent environment-variable overrides set on a task's own
    /// (task-scoped) proof steps, not merged with the task's/squad's.
    pub fn get_task_proof_env_overrides(
        &self,
        squad_id: &str,
        task_idx: i64,
    ) -> Result<BTreeMap<String, String>> {
        let raw: Option<String> = self
            .conn
            .query_row(
                "SELECT proof_env_overrides FROM tasks WHERE squad_id=? AND idx=?",
                params![squad_id, task_idx],
                |r| r.get(0),
            )
            .optional()?;
        Ok(from_json_map(&raw.ok_or(StoreError::NotFound)?))
    }

    /// Add/replace (`set`) and remove (`unset`) entries in a task's
    /// proof-scoped environment-variable overrides, returning the resulting
    /// map.
    ///
    /// RAL-271: marks every one of this task's own (task-scoped) proof steps
    /// "out of date" (cosmetic only) -- this layer feeds those steps
    /// directly, not the task node itself, so only they are marked.
    pub fn set_task_proof_env_overrides(
        &self,
        squad_id: &str,
        task_idx: i64,
        set: &BTreeMap<String, String>,
        unset: &[String],
    ) -> Result<BTreeMap<String, String>> {
        let mut current = self.get_task_proof_env_overrides(squad_id, task_idx)?;
        for key in unset {
            current.remove(key);
        }
        for (k, v) in set {
            current.insert(k.clone(), v.clone());
        }
        self.conn.execute(
            "UPDATE tasks SET proof_env_overrides=? WHERE squad_id=? AND idx=?",
            params![to_json_map(&current), squad_id, task_idx],
        )?;
        self.conn.execute(
            "UPDATE proofs SET env_out_of_date=1 WHERE squad_id=? AND task_idx=? AND scope='task'",
            params![squad_id, task_idx],
        )?;
        let _ = self.notify_watchers(
            crate::monitor::NotifiableEventKind::SquadContentChanged,
            &format!("task:{squad_id}:{task_idx}"),
            crate::mailbox::MailboxPriority::Normal,
            "task proof environment overrides changed",
            Some(squad_id),
        );
        Ok(current)
    }

    /// The persistent environment-variable overrides set directly on a
    /// cell (not merged with its task's/squad's). Empty when none have ever
    /// been set.
    pub fn get_cell_env_overrides(
        &self,
        squad_id: &str,
        task_idx: i64,
        cell_idx: i64,
    ) -> Result<BTreeMap<String, String>> {
        let raw: Option<String> = self
            .conn
            .query_row(
                "SELECT env_overrides FROM cells WHERE squad_id=? AND task_idx=? AND idx=?",
                params![squad_id, task_idx, cell_idx],
                |r| r.get(0),
            )
            .optional()?;
        Ok(from_json_map(&raw.ok_or(StoreError::NotFound)?))
    }

    /// Add/replace (`set`) and remove (`unset`) entries in a cell's own
    /// environment-variable overrides, returning the resulting map.
    ///
    /// RAL-271: also marks the cell, and its own proof steps, "out of date"
    /// (cosmetic only) -- one ownership level down from the edited scope,
    /// never touching sibling cells or the owning task.
    pub fn set_cell_env_overrides(
        &self,
        squad_id: &str,
        task_idx: i64,
        cell_idx: i64,
        set: &BTreeMap<String, String>,
        unset: &[String],
    ) -> Result<BTreeMap<String, String>> {
        let mut current = self.get_cell_env_overrides(squad_id, task_idx, cell_idx)?;
        for key in unset {
            current.remove(key);
        }
        for (k, v) in set {
            current.insert(k.clone(), v.clone());
        }
        self.conn.execute(
            "UPDATE cells SET env_overrides=?, env_out_of_date=1 WHERE squad_id=? AND task_idx=? AND idx=?",
            params![to_json_map(&current), squad_id, task_idx, cell_idx],
        )?;
        self.conn.execute(
            "UPDATE proofs SET env_out_of_date=1 WHERE squad_id=? AND task_idx=? AND scope='cell' AND cell_idx=?",
            params![squad_id, task_idx, cell_idx],
        )?;
        let _ = self.notify_watchers(
            crate::monitor::NotifiableEventKind::SquadContentChanged,
            &format!("cell:{squad_id}:{task_idx}:{cell_idx}"),
            crate::mailbox::MailboxPriority::Normal,
            "cell environment overrides changed",
            Some(squad_id),
        );
        Ok(current)
    }

    /// The persistent environment-variable overrides set on a cell's own
    /// proof steps, not merged with the cell's/task's/squad's.
    pub fn get_cell_proof_env_overrides(
        &self,
        squad_id: &str,
        task_idx: i64,
        cell_idx: i64,
    ) -> Result<BTreeMap<String, String>> {
        let raw: Option<String> = self
            .conn
            .query_row(
                "SELECT proof_env_overrides FROM cells WHERE squad_id=? AND task_idx=? AND idx=?",
                params![squad_id, task_idx, cell_idx],
                |r| r.get(0),
            )
            .optional()?;
        Ok(from_json_map(&raw.ok_or(StoreError::NotFound)?))
    }

    /// Add/replace (`set`) and remove (`unset`) entries in a cell's
    /// proof-scoped environment-variable overrides, returning the resulting
    /// map.
    ///
    /// RAL-271: marks every one of this cell's own proof steps "out of date"
    /// (cosmetic only) -- this layer feeds those steps directly, not the
    /// cell node itself, so only they are marked.
    pub fn set_cell_proof_env_overrides(
        &self,
        squad_id: &str,
        task_idx: i64,
        cell_idx: i64,
        set: &BTreeMap<String, String>,
        unset: &[String],
    ) -> Result<BTreeMap<String, String>> {
        let mut current = self.get_cell_proof_env_overrides(squad_id, task_idx, cell_idx)?;
        for key in unset {
            current.remove(key);
        }
        for (k, v) in set {
            current.insert(k.clone(), v.clone());
        }
        self.conn.execute(
            "UPDATE cells SET proof_env_overrides=? WHERE squad_id=? AND task_idx=? AND idx=?",
            params![to_json_map(&current), squad_id, task_idx, cell_idx],
        )?;
        self.conn.execute(
            "UPDATE proofs SET env_out_of_date=1 WHERE squad_id=? AND task_idx=? AND scope='cell' AND cell_idx=?",
            params![squad_id, task_idx, cell_idx],
        )?;
        let _ = self.notify_watchers(
            crate::monitor::NotifiableEventKind::SquadContentChanged,
            &format!("cell:{squad_id}:{task_idx}:{cell_idx}"),
            crate::mailbox::MailboxPriority::Normal,
            "cell proof environment overrides changed",
            Some(squad_id),
        );
        Ok(current)
    }

    /// Effective environment-variable overrides for a cell's own
    /// subprocess: `squad < task < cell`, each layer's keys winning over its
    /// parent's.
    pub fn resolve_cell_env_overrides(
        &self,
        squad_id: &str,
        task_idx: i64,
        cell_idx: i64,
    ) -> Result<BTreeMap<String, String>> {
        let mut merged = self.get_squad_env_overrides(squad_id)?;
        merged.extend(self.get_task_env_overrides(squad_id, task_idx)?);
        merged.extend(self.get_cell_env_overrides(squad_id, task_idx, cell_idx)?);
        Ok(merged)
    }

    /// Batched form of [`Self::resolve_cell_env_overrides`] for a whole set of
    /// cells at once (RAL-121 follow-up: guardian hydration was doing three
    /// queries per branch here, re-scanning `squads`/`tasks`/`cells` once for
    /// every branch of every review on every board poll). Fetches each of the
    /// three tables once per distinct squad rather than once per cell, then
    /// folds the three layers together in memory the same way the per-cell
    /// version does. Missing rows (a squad/task/cell that no longer exists)
    /// resolve to an empty map for that layer, matching the per-cell
    /// version's `NotFound` short-circuit -- a deleted ancestor contributes
    /// nothing rather than failing the whole batch.
    pub fn resolve_cell_env_overrides_batch(
        &self,
        refs: &[CellRef],
    ) -> Result<HashMap<CellRef, BTreeMap<String, String>>> {
        if refs.is_empty() {
            return Ok(HashMap::new());
        }
        let squad_ids: Vec<&str> = refs
            .iter()
            .map(|(s, _, _)| s.as_str())
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        let placeholders = vec!["?"; squad_ids.len()].join(",");

        let mut squad_env: HashMap<String, BTreeMap<String, String>> = HashMap::new();
        {
            let mut stmt = self.conn.prepare(&format!(
                "SELECT id, env_overrides FROM squads WHERE id IN ({placeholders})"
            ))?;
            let rows = stmt.query_map(rusqlite::params_from_iter(squad_ids.iter()), |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
            })?;
            for row in rows {
                let (id, raw) = row?;
                squad_env.insert(id, from_json_map(&raw.unwrap_or_default()));
            }
        }

        let mut task_env: HashMap<(String, i64), BTreeMap<String, String>> = HashMap::new();
        {
            let mut stmt = self.conn.prepare(&format!(
                "SELECT squad_id, idx, env_overrides FROM tasks WHERE squad_id IN ({placeholders})"
            ))?;
            let rows = stmt.query_map(rusqlite::params_from_iter(squad_ids.iter()), |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, Option<String>>(2)?,
                ))
            })?;
            for row in rows {
                let (squad_id, idx, raw) = row?;
                task_env.insert((squad_id, idx), from_json_map(&raw.unwrap_or_default()));
            }
        }

        let mut cell_env: HashMap<(String, i64, i64), BTreeMap<String, String>> = HashMap::new();
        {
            let mut stmt = self.conn.prepare(&format!(
                "SELECT squad_id, task_idx, idx, env_overrides FROM cells WHERE squad_id IN ({placeholders})"
            ))?;
            let rows = stmt.query_map(rusqlite::params_from_iter(squad_ids.iter()), |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, Option<String>>(3)?,
                ))
            })?;
            for row in rows {
                let (squad_id, task_idx, idx, raw) = row?;
                cell_env.insert(
                    (squad_id, task_idx, idx),
                    from_json_map(&raw.unwrap_or_default()),
                );
            }
        }

        let mut out = HashMap::with_capacity(refs.len());
        for (squad_id, task_idx, cell_idx) in refs {
            let mut merged = squad_env.get(squad_id).cloned().unwrap_or_default();
            if let Some(t) = task_env.get(&(squad_id.clone(), *task_idx)) {
                merged.extend(t.clone());
            }
            if let Some(c) = cell_env.get(&(squad_id.clone(), *task_idx, *cell_idx)) {
                merged.extend(c.clone());
            }
            out.insert((squad_id.clone(), *task_idx, *cell_idx), merged);
        }
        Ok(out)
    }

    /// Effective environment-variable overrides for a task-scoped proof
    /// step: `squad < task < task.proof`.
    pub fn resolve_task_proof_env_overrides(
        &self,
        squad_id: &str,
        task_idx: i64,
    ) -> Result<BTreeMap<String, String>> {
        let mut merged = self.get_squad_env_overrides(squad_id)?;
        merged.extend(self.get_task_env_overrides(squad_id, task_idx)?);
        merged.extend(self.get_task_proof_env_overrides(squad_id, task_idx)?);
        Ok(merged)
    }

    /// Effective environment-variable overrides for a cell-scoped proof
    /// step: `squad < task < cell < cell.proof`.
    pub fn resolve_cell_proof_env_overrides(
        &self,
        squad_id: &str,
        task_idx: i64,
        cell_idx: i64,
    ) -> Result<BTreeMap<String, String>> {
        let mut merged = self.resolve_cell_env_overrides(squad_id, task_idx, cell_idx)?;
        merged.extend(self.get_cell_proof_env_overrides(squad_id, task_idx, cell_idx)?);
        Ok(merged)
    }

    /// The environment-variable overrides set on one individual proof step
    /// (RAL-191), not merged with any ancestor scope. `scope` is `"task"` or
    /// `"cell"`; `cell_idx` is the owning cell's index for a
    /// cell-scoped step and ignored (stored as the same value the row was
    /// inserted with) for a task-scoped one.
    pub fn get_proof_step_env_overrides(
        &self,
        squad_id: &str,
        task_idx: i64,
        scope: &str,
        cell_idx: i64,
        idx: i64,
    ) -> Result<BTreeMap<String, String>> {
        let raw: Option<String> = self
            .conn
            .query_row(
                "SELECT env_overrides FROM proofs
                 WHERE squad_id=? AND task_idx=? AND scope=? AND cell_idx=? AND idx=?",
                params![squad_id, task_idx, scope, cell_idx, idx],
                |r| r.get(0),
            )
            .optional()?;
        Ok(from_json_map(&raw.ok_or(StoreError::NotFound)?))
    }

    /// Add/replace (`set`) and remove (`unset`) entries in one proof step's
    /// own environment-variable overrides, returning the resulting map.
    ///
    /// Eight arguments because a proof step's primary key genuinely is
    /// five-part (`squad, task, scope, cell, idx`) — the same key every other
    /// `proofs` accessor here takes — plus the set/unset pair.
    ///
    /// RAL-271: also marks this individual step "out of date" (cosmetic
    /// only) -- a proof step is a leaf, so there is nothing further to
    /// cascade to.
    #[allow(clippy::too_many_arguments)]
    pub fn set_proof_step_env_overrides(
        &self,
        squad_id: &str,
        task_idx: i64,
        scope: &str,
        cell_idx: i64,
        idx: i64,
        set: &BTreeMap<String, String>,
        unset: &[String],
    ) -> Result<BTreeMap<String, String>> {
        let mut current =
            self.get_proof_step_env_overrides(squad_id, task_idx, scope, cell_idx, idx)?;
        for key in unset {
            current.remove(key);
        }
        for (k, v) in set {
            current.insert(k.clone(), v.clone());
        }
        self.conn.execute(
            "UPDATE proofs SET env_overrides=?, env_out_of_date=1
             WHERE squad_id=? AND task_idx=? AND scope=? AND cell_idx=? AND idx=?",
            params![
                to_json_map(&current),
                squad_id,
                task_idx,
                scope,
                cell_idx,
                idx
            ],
        )?;
        let entity_uri = format!("proof:{squad_id}:{task_idx}:{scope}:{cell_idx}:{idx}");
        let _ = self.notify_watchers(
            crate::monitor::NotifiableEventKind::SquadContentChanged,
            &entity_uri,
            crate::mailbox::MailboxPriority::Normal,
            "proof environment overrides changed",
            Some(squad_id),
        );
        Ok(current)
    }

    /// Effective overrides for one *task-scoped* proof step (RAL-191):
    /// `squad < task < task.proof < this step`.
    pub fn resolve_task_proof_step_env_overrides(
        &self,
        squad_id: &str,
        task_idx: i64,
        idx: i64,
    ) -> Result<BTreeMap<String, String>> {
        let mut merged = self.resolve_task_proof_env_overrides(squad_id, task_idx)?;
        merged.extend(self.get_proof_step_env_overrides(squad_id, task_idx, "task", -1, idx)?);
        Ok(merged)
    }

    /// Effective overrides for one *cell-scoped* proof step (RAL-191):
    /// `squad < task < cell < cell.proof < this step`.
    pub fn resolve_cell_proof_step_env_overrides(
        &self,
        squad_id: &str,
        task_idx: i64,
        cell_idx: i64,
        idx: i64,
    ) -> Result<BTreeMap<String, String>> {
        let mut merged = self.resolve_cell_proof_env_overrides(squad_id, task_idx, cell_idx)?;
        merged
            .extend(self.get_proof_step_env_overrides(squad_id, task_idx, "cell", cell_idx, idx)?);
        Ok(merged)
    }

    /// Reset a squad and all its nodes back to `Pending` — the dirty→pending gate
    /// applied after an edit, so the squad re-executes with the new values. A
    /// currently-running worker's final state write is skipped (see the
    /// scheduler), so this effectively stops in-flight work.
    ///
    /// Also clears `started_at_ms`/`finished_at_ms` on the squad and its tasks
    /// and cells: this is a genuine fresh re-execution, so the old start
    /// time must not linger (it would otherwise survive the `COALESCE` in
    /// [`Store::set_squad_state`]/[`Store::set_task_state`]/
    /// [`Store::set_cell_state`] and make the Details Pane show an
    /// inflated elapsed duration once it starts running again).
    pub fn reset_squad_to_pending(&self, squad_id: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE squads SET state='pending', updated_at_ms=?, started_at_ms=NULL, finished_at_ms=NULL WHERE id=?",
            params![now_ms(), squad_id],
        )?;
        self.conn.execute(
            "UPDATE tasks SET state='pending', error=NULL, started_at_ms=NULL, finished_at_ms=NULL, env_out_of_date=0 WHERE squad_id=?",
            params![squad_id],
        )?;
        self.conn.execute(
            "UPDATE cells SET state='pending', error=NULL, started_at_ms=NULL, finished_at_ms=NULL, env_out_of_date=0 WHERE squad_id=?",
            params![squad_id],
        )?;
        self.conn.execute(
            "UPDATE proofs SET state='pending', env_out_of_date=0 WHERE squad_id=?",
            params![squad_id],
        )?;
        Ok(())
    }

    /// Crash recovery: squads left `Running` after an unclean shutdown have no
    /// worker to finish them. Reset each such squad — and only its still-in-flight
    /// (`running`) tasks/cells/proofs — back to `Pending`, so the scheduler
    /// re-claims and resumes it. `Done` cells are deliberately left `Done` so
    /// re-execution skips finished work and only the unfinished tail re-runs
    /// (RAL-19). Runs a single pass at startup, before the scheduler begins;
    /// returns the recovered squad ids. Safe because nothing is executing yet, so
    /// any `running` row is by definition orphaned.
    pub fn recover_orphaned_squads(&self) -> Result<Vec<String>> {
        let ids: Vec<String> = {
            let mut stmt = self
                .conn
                .prepare("SELECT id FROM squads WHERE state='running'")?;
            stmt.query_map([], |r| r.get::<_, String>(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?
        };
        for id in &ids {
            crate::rlog!(
                WARNING,
                "ralphus [recovery] squad {id}: running → pending (orphaned on startup)"
            );
            let _ = self.cartographer_log(crate::cartographer::CartographerEntry {
                level: crate::logging::LogLevel::WARNING,
                source: "recovery",
                message: "squad recovered: running → pending (orphaned on startup)",
                scope: Some("squad"),
                squad_id: Some(id),
                guardian_id: None,
                cell_id: None,
                task: None,
                log_path: None,
                payload: serde_json::json!({}),
                admin_only: false,
            });
            self.conn.execute(
                "UPDATE cells SET state='pending', error=NULL WHERE squad_id=? AND state='running'",
                params![id],
            )?;
            self.conn.execute(
                "UPDATE proofs SET state='pending' WHERE squad_id=? AND state='running'",
                params![id],
            )?;
            self.conn.execute(
                "UPDATE tasks SET state='pending' WHERE squad_id=? AND state='running'",
                params![id],
            )?;
            self.conn.execute(
                "UPDATE squads SET state='pending', updated_at_ms=? WHERE id=?",
                params![now_ms(), id],
            )?;
        }
        Ok(ids)
    }

    /// Task indices that have at least one cell in [`done_cells`] whose
    /// cell-level proof previously **failed**. Used by the scheduler to seed
    /// `progress.failed` on a partial restart: those cells are skipped (they
    /// are already Done), but their prior failure must still condemn the task so
    /// that the task finalizer does not incorrectly set the task to Done.
    pub fn done_cells_with_failed_proof(&self, squad_id: &str) -> Result<HashSet<i64>> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT s.task_idx
             FROM cells s
             WHERE s.squad_id=? AND s.state='done'
             AND NOT EXISTS (
                 SELECT 1 FROM proofs v
                 WHERE v.squad_id=s.squad_id AND v.task_idx=s.task_idx
                 AND v.scope='cell' AND v.cell_idx=s.idx
                 AND v.state NOT IN ('done','failed','cancelled')
             )
             AND EXISTS (
                 SELECT 1 FROM proofs v2
                 WHERE v2.squad_id=s.squad_id AND v2.task_idx=s.task_idx
                 AND v2.scope='cell' AND v2.cell_idx=s.idx
                 AND v2.state='failed'
             )",
        )?;
        let rows = stmt
            .query_map(params![squad_id], |r| r.get::<_, i64>(0))?
            .collect::<std::result::Result<HashSet<_>, _>>()?;
        Ok(rows)
    }

    /// The `(task_idx, idx)` of every cell in a squad that is already `Done`
    /// *and* whose cell-level proof steps are all in a terminal state.
    /// The scheduler skips these so a restarted squad only re-runs its dirty
    /// (reset-to-pending) subset instead of redoing finished work (RAL-19).
    ///
    /// A cell whose proofs are still pending (e.g. the daemon was stopped
    /// between `record_cell_result` and `run_proofs`) is excluded so the
    /// cell worker re-runs and the proofs are executed (RAL-64).
    pub fn done_cells(&self, squad_id: &str) -> Result<HashSet<(i64, i64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT s.task_idx, s.idx
             FROM cells s
             WHERE s.squad_id=? AND s.state='done'
             AND NOT EXISTS (
                 SELECT 1 FROM proofs v
                 WHERE v.squad_id=s.squad_id
                 AND v.task_idx=s.task_idx
                 AND v.scope='cell'
                 AND v.cell_idx=s.idx
                 AND v.state NOT IN ('done','failed','cancelled')
             )",
        )?;
        let rows = stmt
            .query_map(params![squad_id], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
            })?
            .collect::<std::result::Result<HashSet<_>, _>>()?;
        Ok(rows)
    }

    /// The `(task_idx, idx)` of every cell already `Failed`. The scheduler
    /// seeds these as terminally Failed (never re-dispatched) rather than
    /// falling through to Pending -- otherwise a *scoped* cell/proof
    /// restart, which only resets its own target + downstream to Pending but
    /// still flips the whole squad back to Pending so the scheduler reactivates
    /// it, would silently redispatch every other still-`failed` cell in
    /// the squad too (a cell/task genuinely reset by the restart is already
    /// `pending` in the DB by the time this runs, so it's excluded here).
    pub fn failed_cells(&self, squad_id: &str) -> Result<HashSet<(i64, i64)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT task_idx, idx FROM cells WHERE squad_id=? AND state='failed'")?;
        let rows = stmt
            .query_map(params![squad_id], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
            })?
            .collect::<std::result::Result<HashSet<_>, _>>()?;
        Ok(rows)
    }

    /// The `(task_idx, idx)` of every cell manually set to `ignored`. The
    /// scheduler seeds these as satisfied so their downstream cells run,
    /// exactly as a `done` upstream would (they are themselves never executed).
    pub fn ignored_cells(&self, squad_id: &str) -> Result<HashSet<(i64, i64)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT task_idx, idx FROM cells WHERE squad_id=? AND state='ignored'")?;
        let rows = stmt
            .query_map(params![squad_id], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
            })?
            .collect::<std::result::Result<HashSet<_>, _>>()?;
        Ok(rows)
    }

    /// The `(task_idx, idx)` of every cell left `cancelled` — e.g. by a
    /// squad-level [`Store::cancel`], which flips every non-terminal cell to
    /// `cancelled` via `cancel_nonterminal_nodes`. The scheduler seeds these as
    /// terminally Cancelled (never re-dispatched), for exactly the same reason
    /// [`Store::failed_cells`] exists: a *scoped* `restart_cell` resets
    /// only its own target + downstream to `pending`, yet still flips the whole
    /// squad back to `pending` so the scheduler reactivates it — without this
    /// seed, every unrelated `cancelled` sibling in the squad silently fell
    /// through to `Pending` and was redispatched (RAL-185).
    ///
    /// A cell genuinely revived by a restart (directly, or as downstream of
    /// a restarted upstream) is already `pending` in the DB by the time this
    /// runs, so it is excluded here and runs normally. A whole-squad
    /// [`Store::restart_squad`] resets *every* cell via
    /// [`Store::reset_squad_to_pending`], so this returns nothing for that path.
    pub fn cancelled_cells(&self, squad_id: &str) -> Result<HashSet<(i64, i64)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT task_idx, idx FROM cells WHERE squad_id=? AND state='cancelled'")?;
        let rows = stmt
            .query_map(params![squad_id], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
            })?
            .collect::<std::result::Result<HashSet<_>, _>>()?;
        Ok(rows)
    }

    /// The `idx` of every task left `cancelled` (companion to
    /// [`Store::cancelled_cells`] — `cancel_nonterminal_nodes` cancels
    /// tasks, cells *and* proofs together). The scheduler pre-marks these
    /// as finalized so no task finalizer launches for them: without it, seeding
    /// a cancelled task's cells as terminal would make the dispatcher's
    /// "all cells terminal" check fire, run that task's proof steps, and
    /// flip a task you explicitly cancelled to Done (RAL-185).
    pub fn cancelled_tasks(&self, squad_id: &str) -> Result<HashSet<i64>> {
        let mut stmt = self
            .conn
            .prepare("SELECT idx FROM tasks WHERE squad_id=? AND state='cancelled'")?;
        let rows = stmt
            .query_map(params![squad_id], |r| r.get::<_, i64>(0))?
            .collect::<std::result::Result<HashSet<_>, _>>()?;
        Ok(rows)
    }

    /// `Failed` > `Cancelled` > `Done` precedence for a squad's own terminal
    /// state, given whether any of its tasks failed and whether any were left
    /// cancelled. Shared by the scheduler's own end-of-dispatch aggregation
    /// (`run_squad` in scheduler.rs) and [`Store::reconcile_squad_cancellation`]
    /// (RAL-315's task-by-task cancellation path), so the two verdicts can
    /// never drift apart.
    #[must_use]
    pub fn squad_terminal_state(any_failed: bool, any_cancelled: bool) -> SquadState {
        if any_failed {
            SquadState::Failed
        } else if any_cancelled {
            SquadState::Cancelled
        } else {
            SquadState::Done
        }
    }

    /// After a task reaches `cancelled` outside the scheduler's own dispatch
    /// loop (RAL-315: a direct `kind: "task"` cancel via `set_status`, or a
    /// cell/proof-level "Stop" whose `apply_stop_cascade` cancels the owning
    /// task), check whether the squad as a whole should now be reported
    /// `cancelled` too — mirroring the aggregation the scheduler runs at the
    /// tail of its own dispatch loop via [`Store::squad_terminal_state`], but
    /// triggered from outside that loop. Trigger granularity matches
    /// [`Store::cancelled_tasks`]: task state only, not raw cell/proof state.
    ///
    /// No-ops (leaving the squad's state untouched) unless all of the
    /// following hold, so a real failure or a squad with work still in flight
    /// is never overwritten:
    /// - the squad is currently `running` (an edit that reset it to `pending`
    ///   mid-flight must not be clobbered, matching the scheduler's own
    ///   guard);
    /// - every task has reached a terminal state (`done`/`failed`/
    ///   `cancelled`);
    /// - at least one task is `cancelled` (otherwise this is an ordinary
    ///   completion, which the scheduler's own dispatch-loop tail already
    ///   reports for squads it is actively running).
    pub fn reconcile_squad_cancellation(&self, squad_id: &str) -> Result<()> {
        if !matches!(self.squad_state(squad_id), Ok(SquadState::Running)) {
            return Ok(());
        }
        let mut stmt = self
            .conn
            .prepare("SELECT state FROM tasks WHERE squad_id=?")?;
        let states = stmt
            .query_map(params![squad_id], |r| r.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        if states.is_empty() {
            return Ok(());
        }
        let all_terminal = states
            .iter()
            .all(|s| NodeState::parse(s).is_some_and(NodeState::is_terminal));
        if !all_terminal {
            return Ok(());
        }
        let any_cancelled = states.iter().any(|s| s == "cancelled");
        if !any_cancelled {
            return Ok(());
        }
        let any_failed = states.iter().any(|s| s == "failed");
        self.set_squad_state(
            squad_id,
            Self::squad_terminal_state(any_failed, any_cancelled),
        )
    }

    /// Compute everything [`Store::restart_squad`] would dirty, without mutating
    /// anything: every cell/task in the squad (a whole-squad restart resets all
    /// of them) plus every squad transitively dependent on it. Shared by the
    /// non-mutating dry-run preview and the real restart (RAL-104) so the two
    /// can never drift out of sync.
    pub fn compute_squad_restart_impact(&self, squad_id: &str) -> Result<RestartImpact> {
        let exists: Option<String> = self
            .conn
            .query_row("SELECT id FROM squads WHERE id=?", params![squad_id], |r| {
                r.get(0)
            })
            .optional()?;
        if exists.is_none() {
            return Err(StoreError::NotFound);
        }
        let cells = self
            .cells_of(squad_id)?
            .into_iter()
            .map(|s| RestartImpactCell {
                task_idx: s.task_idx,
                idx: s.idx,
                task_name: s.task_name,
                cell_id: s.cell_id,
            })
            .collect();
        let tasks = self
            .tasks_of(squad_id)?
            .into_iter()
            .map(|t| RestartImpactTask {
                idx: t.idx,
                name: t.name,
            })
            .collect();
        let dirtied_squads = self.compute_dirty_dependents(squad_id)?;
        Ok(RestartImpact {
            cells,
            tasks,
            dirtied_squads,
        })
    }

    /// Compute (and, unless `dry_run`, perform) a cascading cancel of
    /// `squad_id`: the squad itself plus every squad transitively dependent on it
    /// (RAL-116). One function drives both the non-mutating dry-run preview
    /// and the real cancel, so they can never drift apart — mirrors
    /// [`Store::compute_squad_restart_impact`] (RAL-104). Every listed squad is
    /// cancelled regardless of its current state, including already-terminal
    /// ones, matching [`Store::cancel`]'s always-available semantics.
    pub fn cancel_squad(&self, squad_id: &str, dry_run: bool) -> Result<CancelImpact> {
        let row: Option<(String, Option<String>)> = self
            .conn
            .query_row(
                "SELECT id, label FROM squads WHERE id=?",
                params![squad_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((id, label)) = row else {
            return Err(StoreError::NotFound);
        };
        let mut squads = vec![RestartImpactSquad { id, label }];
        squads.extend(self.compute_dirty_dependents(squad_id)?);

        if !dry_run {
            for r in &squads {
                self.set_squad_state(&r.id, SquadState::Cancelled)?;
                self.cancel_unfinished_nodes(&r.id)?;
                let _ = self.log_event(Some(&r.id), None, "squad", None, "cancelled");
            }
        }

        Ok(CancelImpact { squads })
    }

    /// Restart a whole squad: reset it (and all its nodes) to Pending and dirty
    /// every squad that transitively depends on it, so the dependents re-run once
    /// this squad finishes again (RAL-19). Returns the dirtied dependent squad ids.
    pub fn restart_squad(&self, squad_id: &str) -> Result<Vec<String>> {
        let impact = self.compute_squad_restart_impact(squad_id)?;
        self.reset_squad_to_pending(squad_id)?;
        let _ = self.log_event(Some(squad_id), None, "squad", None, "restarted");
        self.apply_dirty_dependents(&impact.dirtied_squads)?;
        Ok(impact.dirtied_squads.into_iter().map(|r| r.id).collect())
    }

    /// Compute everything [`Store::restart_cell`] would dirty, without
    /// mutating anything: the target cell plus every cell downstream of
    /// it within the squad (forward reachability over the plan graph), the tasks
    /// that own any of those cells, and every squad transitively dependent on
    /// this one. Shared by the non-mutating dry-run preview and the real
    /// restart (RAL-104) so the two can never drift out of sync.
    pub fn compute_cell_restart_impact(
        &self,
        squad_id: &str,
        task_idx: i64,
        idx: i64,
    ) -> Result<RestartImpact> {
        let cells = self.cells_of(squad_id)?;
        let tasks = self.tasks_of(squad_id)?;
        let target = cells
            .iter()
            .position(|s| s.task_idx == task_idx && s.idx == idx)
            .ok_or(StoreError::NotFound)?;
        let plan = crate::plan::plan(&cells, &tasks).map_err(StoreError::InvalidTransition)?;

        // Forward reachability: cell `j` is downstream of `target` when
        // `target` is one of its transitive prerequisites. Invert deps into
        // child edges, then BFS out from `target`.
        let mut children: Vec<Vec<usize>> = vec![Vec::new(); cells.len()];
        for (j, prereqs) in plan.deps.iter().enumerate() {
            for &p in prereqs {
                children[p].push(j);
            }
        }
        let mut affected: HashSet<usize> = HashSet::new();
        affected.insert(target);
        let mut frontier = vec![target];
        while let Some(cur) = frontier.pop() {
            for &c in &children[cur] {
                if affected.insert(c) {
                    frontier.push(c);
                }
            }
        }

        let mut affected_task_idxs: HashSet<i64> = HashSet::new();
        let mut affected_cells: Vec<RestartImpactCell> = affected
            .iter()
            .map(|&pos| {
                let s = &cells[pos];
                affected_task_idxs.insert(s.task_idx);
                RestartImpactCell {
                    task_idx: s.task_idx,
                    idx: s.idx,
                    task_name: s.task_name.clone(),
                    cell_id: s.cell_id.clone(),
                }
            })
            .collect();
        affected_cells.sort_by_key(|s| (s.task_idx, s.idx));

        let mut affected_tasks: Vec<RestartImpactTask> = tasks
            .iter()
            .filter(|t| affected_task_idxs.contains(&t.idx))
            .map(|t| RestartImpactTask {
                idx: t.idx,
                name: t.name.clone(),
            })
            .collect();
        affected_tasks.sort_by_key(|t| t.idx);

        let dirtied_squads = self.compute_dirty_dependents(squad_id)?;

        Ok(RestartImpact {
            cells: affected_cells,
            tasks: affected_tasks,
            dirtied_squads,
        })
    }

    /// Restart a single cell: reset it and every cell downstream of it
    /// within the squad to Pending, put the squad (and each affected task) back to
    /// Pending, and dirty every squad that depends on this one (RAL-19). Upstream
    /// cells stay Done and are skipped on re-run. Returns the dirtied
    /// dependent squad ids.
    ///
    /// Clears `started_at_ms`/`finished_at_ms` on the restarted cells and
    /// their owning tasks (genuinely re-executing — see
    /// [`Store::reset_squad_to_pending`]'s doc comment), but deliberately leaves
    /// the squad's own `started_at_ms` alone: the squad as a whole already started
    /// earlier and other, unaffected cells may still be `Done`.
    pub fn restart_cell(&self, squad_id: &str, task_idx: i64, idx: i64) -> Result<Vec<String>> {
        let impact = self.compute_cell_restart_impact(squad_id, task_idx, idx)?;

        for s in &impact.cells {
            self.conn.execute(
                "UPDATE cells SET state='pending', error=NULL, started_at_ms=NULL, finished_at_ms=NULL, env_out_of_date=0 WHERE squad_id=? AND task_idx=? AND idx=?",
                params![squad_id, s.task_idx, s.idx],
            )?;
            self.conn.execute(
                "UPDATE proofs SET state='pending', env_out_of_date=0 WHERE squad_id=? AND task_idx=? AND scope='cell' AND cell_idx=?",
                params![squad_id, s.task_idx, s.idx],
            )?;
        }
        for t in &impact.tasks {
            self.conn.execute(
                "UPDATE tasks SET state='pending', error=NULL, started_at_ms=NULL, finished_at_ms=NULL, env_out_of_date=0 WHERE squad_id=? AND idx=?",
                params![squad_id, t.idx],
            )?;
            self.conn.execute(
                "UPDATE proofs SET state='pending', env_out_of_date=0 WHERE squad_id=? AND task_idx=? AND scope='task'",
                params![squad_id, t.idx],
            )?;
        }
        self.conn.execute(
            "UPDATE squads SET state='pending', updated_at_ms=?, finished_at_ms=NULL WHERE id=?",
            params![now_ms(), squad_id],
        )?;
        let _ = self.log_event(
            Some(squad_id),
            None,
            "cell",
            Some(&format!("{task_idx}/{idx}")),
            "restarted (with downstream)",
        );
        self.apply_dirty_dependents(&impact.dirtied_squads)?;
        Ok(impact.dirtied_squads.into_iter().map(|r| r.id).collect())
    }

    /// Compute everything [`Store::restart_task`] would dirty, without
    /// mutating anything: every cell belonging to `task_idx` plus every
    /// cell downstream of any of them within the squad (forward reachability
    /// over the plan graph, seeded from the whole task rather than a single
    /// cell — same BFS as [`Store::compute_cell_restart_impact`]), the
    /// tasks that own any of those cells, and every squad transitively
    /// dependent on this one (RAL-150).
    pub fn compute_task_restart_impact(
        &self,
        squad_id: &str,
        task_idx: i64,
    ) -> Result<RestartImpact> {
        let cells = self.cells_of(squad_id)?;
        let tasks = self.tasks_of(squad_id)?;
        if !tasks.iter().any(|t| t.idx == task_idx) {
            return Err(StoreError::NotFound);
        }
        let plan = crate::plan::plan(&cells, &tasks).map_err(StoreError::InvalidTransition)?;

        let mut children: Vec<Vec<usize>> = vec![Vec::new(); cells.len()];
        for (j, prereqs) in plan.deps.iter().enumerate() {
            for &p in prereqs {
                children[p].push(j);
            }
        }
        let targets: Vec<usize> = cells
            .iter()
            .enumerate()
            .filter(|(_, s)| s.task_idx == task_idx)
            .map(|(pos, _)| pos)
            .collect();
        let mut affected: HashSet<usize> = HashSet::new();
        let mut frontier = Vec::new();
        for t in targets {
            if affected.insert(t) {
                frontier.push(t);
            }
        }
        while let Some(cur) = frontier.pop() {
            for &c in &children[cur] {
                if affected.insert(c) {
                    frontier.push(c);
                }
            }
        }

        let mut affected_task_idxs: HashSet<i64> = HashSet::new();
        affected_task_idxs.insert(task_idx);
        let mut affected_cells: Vec<RestartImpactCell> = affected
            .iter()
            .map(|&pos| {
                let s = &cells[pos];
                affected_task_idxs.insert(s.task_idx);
                RestartImpactCell {
                    task_idx: s.task_idx,
                    idx: s.idx,
                    task_name: s.task_name.clone(),
                    cell_id: s.cell_id.clone(),
                }
            })
            .collect();
        affected_cells.sort_by_key(|s| (s.task_idx, s.idx));

        let mut affected_tasks: Vec<RestartImpactTask> = tasks
            .iter()
            .filter(|t| affected_task_idxs.contains(&t.idx))
            .map(|t| RestartImpactTask {
                idx: t.idx,
                name: t.name.clone(),
            })
            .collect();
        affected_tasks.sort_by_key(|t| t.idx);

        let dirtied_squads = self.compute_dirty_dependents(squad_id)?;

        Ok(RestartImpact {
            cells: affected_cells,
            tasks: affected_tasks,
            dirtied_squads,
        })
    }

    /// Restart a whole task: reset every cell it owns (and every cell
    /// downstream of them within the squad) to Pending, put the squad and each
    /// affected task back to Pending, and dirty every squad that depends on this
    /// one (RAL-150, mirrors [`Store::restart_cell`] at task granularity —
    /// there is deliberately no separate `edit`/`preview` pair for this yet,
    /// matching the ticket's "don't scale up scope" note). Returns the
    /// dirtied dependent squad ids.
    pub fn restart_task(&self, squad_id: &str, task_idx: i64) -> Result<Vec<String>> {
        let impact = self.compute_task_restart_impact(squad_id, task_idx)?;

        for s in &impact.cells {
            self.conn.execute(
                "UPDATE cells SET state='pending', error=NULL, env_out_of_date=0 WHERE squad_id=? AND task_idx=? AND idx=?",
                params![squad_id, s.task_idx, s.idx],
            )?;
            self.conn.execute(
                "UPDATE proofs SET state='pending', env_out_of_date=0 WHERE squad_id=? AND task_idx=? AND scope='cell' AND cell_idx=?",
                params![squad_id, s.task_idx, s.idx],
            )?;
        }
        for t in &impact.tasks {
            self.conn.execute(
                "UPDATE tasks SET state='pending', error=NULL, env_out_of_date=0 WHERE squad_id=? AND idx=?",
                params![squad_id, t.idx],
            )?;
            self.conn.execute(
                "UPDATE proofs SET state='pending', env_out_of_date=0 WHERE squad_id=? AND task_idx=? AND scope='task'",
                params![squad_id, t.idx],
            )?;
        }
        self.conn.execute(
            "UPDATE squads SET state='pending', updated_at_ms=? WHERE id=?",
            params![now_ms(), squad_id],
        )?;
        let _ = self.log_event(
            Some(squad_id),
            None,
            "task",
            Some(&format!("t{task_idx}")),
            "restarted (with downstream)",
        );
        self.apply_dirty_dependents(&impact.dirtied_squads)?;
        Ok(impact.dirtied_squads.into_iter().map(|r| r.id).collect())
    }

    /// Cell `(task_idx, idx)` pairs forward-reachable from `roots` within
    /// `squad_id`'s dependency plan (RAL-174: the "Apply To All Children"
    /// restart-note checkbox) -- the same reachability rule as
    /// [`Store::compute_cell_restart_impact`]'s BFS, kept as a separate,
    /// smaller helper since that function's result shape
    /// (`RestartImpactCell`, carrying task name/cell id for display)
    /// doesn't fit this call site's need for bare index pairs.
    fn forward_reachable_cell_indices(
        &self,
        squad_id: &str,
        roots: &[(i64, i64)],
    ) -> Result<Vec<(i64, i64)>> {
        let cells = self.cells_of(squad_id)?;
        let tasks = self.tasks_of(squad_id)?;
        let plan = crate::plan::plan(&cells, &tasks).map_err(StoreError::InvalidTransition)?;
        let mut children: Vec<Vec<usize>> = vec![Vec::new(); cells.len()];
        for (j, prereqs) in plan.deps.iter().enumerate() {
            for &p in prereqs {
                children[p].push(j);
            }
        }
        let mut affected: HashSet<usize> = HashSet::new();
        let mut frontier = Vec::new();
        for (pos, s) in cells.iter().enumerate() {
            if roots.contains(&(s.task_idx, s.idx)) && affected.insert(pos) {
                frontier.push(pos);
            }
        }
        while let Some(cur) = frontier.pop() {
            for &c in &children[cur] {
                if affected.insert(c) {
                    frontier.push(c);
                }
            }
        }
        Ok(affected
            .into_iter()
            .map(|pos| (cells[pos].task_idx, cells[pos].idx))
            .collect())
    }

    /// Write a human-authored restart note (RAL-174) onto the ghost(s) of the
    /// cell(s) a restart request targets. Unlike agent-authored ghost
    /// content, this note *replaces* rather than merges with whatever was
    /// previously stored for that owner (Q5 of the ticket's interview: no
    /// accumulation across restarts) -- see [`Store::set_ghost_user_note`].
    /// `roots` are the cell(s) the restart directly targets; when
    /// `include_downstream` is set (the "Apply To All Children" checkbox),
    /// the note is also written to every cell downstream of a root within
    /// the squad's dependency graph -- the same "children" a restart's
    /// downstream-impact cascade already resets to Pending.
    pub fn apply_restart_user_note(
        &self,
        squad_id: &str,
        roots: &[(i64, i64)],
        include_downstream: bool,
        note: &str,
    ) -> Result<()> {
        let targets = if include_downstream {
            self.forward_reachable_cell_indices(squad_id, roots)?
        } else {
            roots.to_vec()
        };
        for (task_idx, idx) in targets {
            let uri = crate::ghost::cell_uri(squad_id, task_idx, idx);
            self.set_ghost_user_note(&uri, crate::ghost::KIND_CELL, Some(squad_id), None, note)?;
        }
        Ok(())
    }

    /// Read-only BFS over cross-squad dependencies: every squad transitively
    /// dependent on `squad_id`, in discovery order. Does not mutate anything —
    /// shared by the dry-run preview and [`Store::dirty_dependents`] (RAL-104).
    pub fn compute_dirty_dependents(&self, squad_id: &str) -> Result<Vec<RestartImpactSquad>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, label, depends_on FROM squads")?;
        let all: Vec<(String, Option<String>, Vec<String>)> = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    from_json(&r.get::<_, String>(2)?),
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        drop(stmt);
        let mut seen: HashSet<String> = HashSet::new();
        seen.insert(squad_id.to_string());
        let mut frontier = vec![squad_id.to_string()];
        let mut dirtied = Vec::new();
        while let Some(cur) = frontier.pop() {
            for (id, label, deps) in &all {
                if seen.contains(id) {
                    continue;
                }
                // A dep reference is `squad-id` or `squad-id/task/cell`; the squad
                // is the first path segment.
                let depends = deps.iter().any(|d| d.split('/').next().unwrap_or(d) == cur);
                if depends {
                    seen.insert(id.clone());
                    dirtied.push(RestartImpactSquad {
                        id: id.clone(),
                        label: label.clone(),
                    });
                    frontier.push(id.clone());
                }
            }
        }
        Ok(dirtied)
    }

    /// Apply the mutations for [`Store::compute_dirty_dependents`]'s result:
    /// reset each listed squad to Pending and log the dirtying event.
    pub fn apply_dirty_dependents(&self, squads: &[RestartImpactSquad]) -> Result<()> {
        for r in squads {
            self.reset_squad_to_pending(&r.id)?;
            let _ = self.log_event(
                Some(&r.id),
                None,
                "squad",
                None,
                "dirtied by upstream restart",
            );
        }
        Ok(())
    }

    /// Reset every squad that transitively depends on `squad_id` back to Pending, so
    /// it re-runs once the upstream completes again (RAL-19). Cross-squad gating
    /// (`list_ready`) then holds each dependent until its upstreams are Done.
    /// Returns the dirtied squad ids.
    pub fn dirty_dependents(&self, squad_id: &str) -> Result<Vec<String>> {
        let impacted = self.compute_dirty_dependents(squad_id)?;
        self.apply_dirty_dependents(&impacted)?;
        Ok(impacted.into_iter().map(|r| r.id).collect())
    }

    /// Delete a squad and all of its child rows. Children are removed explicitly
    /// (rather than relying on `ON DELETE CASCADE`, which is off for in-memory
    /// test databases) inside one transaction.
    pub fn delete_squad(&mut self, squad_id: &str) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM events WHERE squad_id=?", params![squad_id])?;
        tx.execute("DELETE FROM proofs WHERE squad_id=?", params![squad_id])?;
        tx.execute("DELETE FROM cells WHERE squad_id=?", params![squad_id])?;
        tx.execute("DELETE FROM tasks WHERE squad_id=?", params![squad_id])?;
        tx.execute("DELETE FROM ghosts WHERE squad_id=?", params![squad_id])?;
        tx.execute(
            "DELETE FROM hidden_items WHERE squad_id=?",
            params![squad_id],
        )?;
        tx.execute(
            "DELETE FROM mailbox_messages WHERE squad_id=?",
            params![squad_id],
        )?;
        // RAL-320: watches are keyed by `EntityUri` string, not a `squad_id`
        // FK column, so a deleted squad's watches (and its tasks'/cells'/
        // proofs') need an explicit sweep rather than `ON DELETE CASCADE`.
        tx.execute(
            "DELETE FROM watches WHERE entity_uri = 'squad:'||?1
                OR entity_uri LIKE 'task:'||?1||':%'
                OR entity_uri LIKE 'cell:'||?1||':%'
                OR entity_uri LIKE 'proof:'||?1||':%'",
            params![squad_id],
        )?;
        let n = tx.execute("DELETE FROM squads WHERE id=?", params![squad_id])?;
        tx.commit()?;
        if n == 0 {
            Err(StoreError::NotFound)
        } else {
            Ok(())
        }
    }

    /// Bulk-clear state (RAL-13). With an empty `states` filter this wipes
    /// everything — all squads (and their cells/tasks/proofs/events), all
    /// guardians (and their branches), and the id sequences reset so the next
    /// squad/guardian id restarts at 1. When `states` is non-empty, only squads
    /// whose state is in the set are deleted (with their children); guardians
    /// and the id sequences are left untouched, since the filter is expressed
    /// in squad states. The returned git roots let the caller purge on-disk
    /// review worktrees for any deleted guardian.
    pub fn clear_all(&mut self, states: &[SquadState]) -> Result<ClearOutcome> {
        if states.is_empty() {
            // Emit one (guardian_id, project_root) pair per project root, so the
            // caller can purge worktrees for every project in multi-project guardians.
            let guardian_roots: Vec<(String, String)> = self
                .list_guardians()?
                .into_iter()
                .flat_map(|g| {
                    let id = g.id.clone();
                    g.projects.into_iter().map(move |p| (id.clone(), p))
                })
                .collect();
            let squad_ids: Vec<String> = {
                let mut stmt = self.conn.prepare("SELECT id FROM squads")?;
                stmt.query_map([], |r| r.get::<_, String>(0))?
                    .collect::<std::result::Result<Vec<_>, _>>()?
            };
            let tx = self.conn.transaction()?;
            tx.execute("DELETE FROM events", [])?;
            tx.execute("DELETE FROM proofs", [])?;
            tx.execute("DELETE FROM cells", [])?;
            tx.execute("DELETE FROM tasks", [])?;
            tx.execute("DELETE FROM ghosts", [])?;
            tx.execute("DELETE FROM hidden_items", [])?;
            tx.execute("DELETE FROM mailbox_messages", [])?;
            let squads_deleted = tx.execute("DELETE FROM squads", [])?;
            tx.execute("DELETE FROM guardian_branches", [])?;
            tx.execute("DELETE FROM guardian_messages", [])?;
            tx.execute("DELETE FROM guardian_input_resolutions", [])?;
            tx.execute("DELETE FROM guardian_costs", [])?;
            let guardians_deleted = tx.execute("DELETE FROM guardians", [])?;
            // Reset id sequences so the next squad/guardian id restarts at 1.
            tx.execute(
                "DELETE FROM meta WHERE key IN ('squad_seq', 'guardian_seq')",
                [],
            )?;
            tx.commit()?;
            return Ok(ClearOutcome {
                squads_deleted,
                guardians_deleted,
                guardian_roots,
                squad_ids,
            });
        }
        // Filtered: delete only squads whose state matches, plus their children.
        let wanted: Vec<&str> = states.iter().map(|s| s.as_str()).collect();
        let ids: Vec<String> = {
            let mut stmt = self.conn.prepare("SELECT id, state FROM squads")?;
            let rows = stmt
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            rows.into_iter()
                .filter(|(_, st)| wanted.contains(&st.as_str()))
                .map(|(id, _)| id)
                .collect()
        };
        let tx = self.conn.transaction()?;
        for id in &ids {
            tx.execute("DELETE FROM events WHERE squad_id=?", params![id])?;
            tx.execute("DELETE FROM proofs WHERE squad_id=?", params![id])?;
            tx.execute("DELETE FROM cells WHERE squad_id=?", params![id])?;
            tx.execute("DELETE FROM tasks WHERE squad_id=?", params![id])?;
            tx.execute("DELETE FROM ghosts WHERE squad_id=?", params![id])?;
            tx.execute("DELETE FROM hidden_items WHERE squad_id=?", params![id])?;
            tx.execute("DELETE FROM mailbox_messages WHERE squad_id=?", params![id])?;
            tx.execute("DELETE FROM squads WHERE id=?", params![id])?;
        }
        tx.commit()?;
        Ok(ClearOutcome {
            squads_deleted: ids.len(),
            guardians_deleted: 0,
            guardian_roots: Vec::new(),
            squad_ids: ids,
        })
    }

    /// Record the git branch a review cell contributes to a guardian stack,
    /// so the board can link the cell to its review(s) (RAL-17).
    pub fn set_cell_review_branch(
        &self,
        squad_id: &str,
        task_idx: i64,
        idx: i64,
        branch: &str,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE cells SET review_branch=? WHERE squad_id=? AND task_idx=? AND idx=?",
            params![branch, squad_id, task_idx, idx],
        )?;
        Ok(())
    }

    /// Record the exact guardian a review cell's membership resolved to at
    /// submit time (RAL-314), alongside [`Self::set_cell_review_branch`].
    /// Read paths (`reviews_by_branch`, `collecting_guardians_for_cells`)
    /// prefer this direct link over the branch-string join, which conflates
    /// unrelated guardians that happen to share a branch name -- e.g. a
    /// repeat submission against the same worktree/branch, which always
    /// mints its own fresh guardian (see `reviews::derive_reviews`) but
    /// records the same branch text as an earlier submission's guardian.
    pub fn set_cell_review_guardian(
        &self,
        squad_id: &str,
        task_idx: i64,
        idx: i64,
        guardian_id: &str,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE cells SET review_guardian_id=? WHERE squad_id=? AND task_idx=? AND idx=?",
            params![guardian_id, squad_id, task_idx, idx],
        )?;
        Ok(())
    }

    /// Record a cell's final outcome (state, usage, error, and cell UUID).
    ///
    /// `agent_session_id` uses `COALESCE(?, agent_session_id)` rather than a
    /// plain overwrite: a live mid-run scrape
    /// ([`Self::set_cell_agent_session_id_live`]) may already have
    /// recorded a real cell/thread id, but a failed outcome always carries
    /// `agent_session_id: None` (`RunnerResult::failure`) — a plain
    /// overwrite would clobber that good value back to `NULL` on every
    /// failure, permanently disabling "Open Agent" for a cell that really
    /// did start one.
    ///
    /// RAL-163: a manual `set-status` override (via
    /// `server::capture_and_stop_node`) may finalize this cell to a state
    /// other than `pending`/`running` while its agent is still mid-flight —
    /// that path captures the agent's in-progress pane into a ghost and kills
    /// it, but the scheduler's own runner call can still unblock and reach
    /// this write afterward. The `state IN (...)` guard makes that write a
    /// no-op in that case so the manual override sticks, while still
    /// allowing the two legitimate callers: the ordinary case (row is
    /// `running`), a cell that never started (`pending` — e.g. blocked by
    /// a failed dependency), and a same-state re-write (the blocked-by-
    /// failed-dependency path calls [`Self::set_cell_state`] directly
    /// before also calling this for the other outcome fields).
    ///
    /// Also stamps `finished_at_ms` when `outcome.state` is terminal — this is
    /// the primary path by which a cell's real completion is recorded (see
    /// [`Store::set_squad_state`]'s doc comment for the shared timestamp
    /// semantics); `started_at_ms` is not touched here since a cell only
    /// ever reaches this function after already having been marked `running`.
    pub fn record_cell_result(
        &self,
        squad_id: &str,
        task_idx: i64,
        idx: i64,
        outcome: &CellOutcome,
    ) -> Result<()> {
        let entering_terminal = i64::from(outcome.state.is_terminal());
        self.conn.execute(
            "UPDATE cells SET state=?, tokens_in=?, tokens_out=?, cache_creation_tokens=?, cache_read_tokens=?, compaction_input_tokens=?, compaction_count=?, cost_usd=?, cost_is_estimated=?, error=?, agent_session_id=COALESCE(?, agent_session_id),
                 finished_at_ms = CASE WHEN ?=1 THEN ? ELSE finished_at_ms END
             WHERE squad_id=? AND task_idx=? AND idx=? AND state IN ('pending', 'running', ?)",
            params![
                outcome.state.as_str(),
                outcome.usage.tokens_in,
                outcome.usage.tokens_out,
                outcome.usage.cache_creation_tokens,
                outcome.usage.cache_read_tokens,
                outcome.usage.compaction_input_tokens,
                outcome.usage.compaction_count,
                outcome.usage.cost_usd,
                outcome.usage.cost_is_estimated,
                outcome.error.as_deref(),
                outcome.agent_session_id.as_deref(),
                entering_terminal,
                now_ms(),
                squad_id,
                task_idx,
                idx,
                outcome.state.as_str(),
            ],
        )?;
        Ok(())
    }

    /// Fetch a cell's declared id (`sid`, e.g. `"s0"`) for the tmux
    /// capture-pane/attach endpoints (RAL-102). The daemon derives the tmux
    /// session name from `(squad_id, task, cell_id)` the same way the
    /// scheduler does when building a `RunnerSpec` (see
    /// `crate::tmux::session_name`), so this — paired with
    /// [`Store::get_task_name`] — lets those HTTP handlers recompute the same
    /// name without any new bookkeeping.
    ///
    /// Returns `Err(StoreError::NotFound)` when the squad or cell row does
    /// not exist.
    pub fn get_cell_id(&self, squad_id: &str, task_idx: i64, cell_idx: i64) -> Result<String> {
        self.conn
            .query_row(
                "SELECT sid FROM cells WHERE squad_id=? AND task_idx=? AND idx=?",
                params![squad_id, task_idx, cell_idx],
                |r| r.get::<_, String>(0),
            )
            .optional()?
            .ok_or(StoreError::NotFound)
    }

    /// A cell's currently stored `agent` program (RAL-341). Used by
    /// `edit_squad`'s `"cell"` arm to resolve the *effective* agent a
    /// `system_prompt` edit would run under when the caller isn't also
    /// changing `agent` in the same request.
    pub fn get_cell_agent(&self, squad_id: &str, task_idx: i64, cell_idx: i64) -> Result<String> {
        self.conn
            .query_row(
                "SELECT agent FROM cells WHERE squad_id=? AND task_idx=? AND idx=?",
                params![squad_id, task_idx, cell_idx],
                |r| r.get::<_, String>(0),
            )
            .optional()?
            .ok_or(StoreError::NotFound)
    }

    /// The agent a proof step runs under, addressed the way `proofs` rows are
    /// keyed: `scope` is `"cell"` or `"task"`, and `cell_idx` is ignored by
    /// task-scope rows but still part of the key. Used by the edit path to
    /// gate `maximum_tool_output_tokens` on the step's own agent rather than
    /// the owning cell's, since the two can differ (RAL-333).
    ///
    /// `proofs.agent` is `NOT NULL`, so this is a plain `String`.
    pub fn get_proof_agent(
        &self,
        squad_id: &str,
        task_idx: i64,
        scope: &str,
        cell_idx: i64,
        idx: i64,
    ) -> Result<String> {
        self.conn
            .query_row(
                "SELECT agent FROM proofs WHERE squad_id=? AND task_idx=? AND scope=? AND cell_idx=? AND idx=?",
                params![squad_id, task_idx, scope, cell_idx, idx],
                |r| r.get::<_, String>(0),
            )
            .optional()?
            .ok_or(StoreError::NotFound)
    }

    /// Fetch every cell's `(idx, sid)` pair for a task, ordered by idx.
    /// Used by manual status-set stop-and-capture (RAL-163) to find every
    /// tmux pane that might be running under a task-scope status change,
    /// since a task can have more than one cell — unlike
    /// [`Store::get_cell_id`], which addresses exactly one.
    ///
    /// Returns an empty vec (not an error) when the squad/task has no cells.
    pub fn get_task_cell_ids(&self, squad_id: &str, task_idx: i64) -> Result<Vec<(i64, String)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT idx, sid FROM cells WHERE squad_id=? AND task_idx=? ORDER BY idx")?;
        let rows = stmt
            .query_map(params![squad_id, task_idx], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Fetch a task's declared name for the tmux capture-pane/attach
    /// endpoints (RAL-102) — see [`Store::get_cell_id`]'s doc comment for
    /// why the caller needs this alongside the cell id.
    ///
    /// Returns `Err(StoreError::NotFound)` when the squad or task row does not
    /// exist.
    pub fn get_task_name(&self, squad_id: &str, task_idx: i64) -> Result<String> {
        self.conn
            .query_row(
                "SELECT name FROM tasks WHERE squad_id=? AND idx=?",
                params![squad_id, task_idx],
                |r| r.get::<_, String>(0),
            )
            .optional()?
            .ok_or(StoreError::NotFound)
    }

    /// Fetch a cell's cwd, agent, and (if any) recorded CLI-agent session
    /// id, for the "Open Agent" terminal action — resuming the real CLI
    /// (`claude --resume <id>` or `codex resume <id>`, depending on
    /// which agent the cell actually ran under) rather than re-attaching
    /// to the runner's tmux wrapper, which only shows its log/event stream
    /// (see `crate::server::open_agent_terminal`). The `agent` column is
    /// what lets `open_agent_terminal` pick the right resume command.
    ///
    /// Returns `Err(StoreError::NotFound)` when the squad or cell row does
    /// not exist; `agent_session_id` is `None` when the cell hasn't
    /// started (or ran under an agent with no resume mechanism) rather than
    /// an error.
    pub fn get_cell_agent_resume(
        &self,
        squad_id: &str,
        task_idx: i64,
        cell_idx: i64,
    ) -> Result<(String, String, Option<String>)> {
        self.conn
            .query_row(
                "SELECT cwd, agent, agent_session_id FROM cells WHERE squad_id=? AND task_idx=? AND idx=?",
                params![squad_id, task_idx, cell_idx],
                |r| {
                    Ok((
                        r.get::<_, Option<String>>(0)?.unwrap_or_default(),
                        r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                        r.get::<_, Option<String>>(2)?,
                    ))
                },
            )
            .optional()?
            .ok_or(StoreError::NotFound)
    }

    /// Records that a cell was just cleanly stopped for a real interactive
    /// agent session to take over (RAL-288 Stage 6) -- called from
    /// `scheduler::run_cell_worker`'s detached-outcome branch, right where
    /// `record_cell_result` persists the (still-`Running`) `NodeState`. See
    /// [`CellView::detached_at_ms`] for what this drives on the board.
    pub fn mark_cell_detached(&self, squad_id: &str, task_idx: i64, idx: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE cells SET detached_at_ms=? WHERE squad_id=? AND task_idx=? AND idx=?",
            params![now_ms(), squad_id, task_idx, idx],
        )?;
        Ok(())
    }

    /// Clears a cell's `detached_at_ms`, called at the same point
    /// `run_cell_worker` sets the cell's `NodeState` back to `Running` for a
    /// fresh dispatch -- a restart, or a resume-automation-triggered one.
    /// Unconditional (no-op if it was already clear) so every fresh dispatch
    /// clears any stale flag regardless of how the cell got here.
    pub fn clear_cell_detached(&self, squad_id: &str, task_idx: i64, idx: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE cells SET detached_at_ms=NULL WHERE squad_id=? AND task_idx=? AND idx=?",
            params![squad_id, task_idx, idx],
        )?;
        Ok(())
    }

    /// Hands a `Detached` cell back to headless automation
    /// (`server::resume_automation`, RAL-288 Stage 6) by resetting *only*
    /// that cell's own row to `pending` -- deliberately not
    /// [`Store::restart_cell`]'s squad/task/downstream-impact machinery,
    /// which exists for a genuine restart-from-scratch and would incorrectly
    /// touch sibling tasks, the squad's own row, and cross-squad dependents
    /// for what is really just handing a still-in-progress conversation back
    /// to automation. A detach never reaches `run_proofs` or records an
    /// `error`, so neither needs resetting here; `detached_at_ms` is cleared
    /// separately, at actual re-dispatch time (see
    /// [`Store::clear_cell_detached`]).
    pub fn resume_detached_cell(&self, squad_id: &str, task_idx: i64, idx: i64) -> Result<()> {
        let exists: Option<i64> = self
            .conn
            .query_row(
                "SELECT idx FROM cells WHERE squad_id=? AND task_idx=? AND idx=?",
                params![squad_id, task_idx, idx],
                |r| r.get(0),
            )
            .optional()?;
        if exists.is_none() {
            return Err(StoreError::NotFound);
        }
        self.conn.execute(
            "UPDATE cells SET state='pending' WHERE squad_id=? AND task_idx=? AND idx=?",
            params![squad_id, task_idx, idx],
        )?;
        let _ = self.log_event(
            Some(squad_id),
            None,
            "cell",
            Some(&format!("{task_idx}/{idx}")),
            "resumed (resume-automation)",
        );
        Ok(())
    }

    /// Marks a cell so its *next* dispatch resumes its own previously
    /// recorded `agent_session_id` instead of starting fresh (RAL-288 Stage
    /// 6) -- see `force_resume_own_session`'s migration comment for why this
    /// needs to be explicit rather than a blanket change to how every
    /// restart behaves. Set by `server::resume_automation` right before it
    /// resets the cell to `pending`; a cell without an `agent_session_id` at
    /// all has nothing to resume, so the caller is expected to check that
    /// first (`get_cell_agent_resume`) rather than this method silently
    /// no-op'ing on one.
    pub fn set_force_resume_own_session(
        &self,
        squad_id: &str,
        task_idx: i64,
        idx: i64,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE cells SET force_resume_own_session=1 WHERE squad_id=? AND task_idx=? AND idx=?",
            params![squad_id, task_idx, idx],
        )?;
        Ok(())
    }

    /// Reads and clears `force_resume_own_session` for one cell in a single
    /// call, so a hint is consumed at most once -- called from
    /// `scheduler::run_cell_worker` right before it would otherwise fall
    /// through to the normal (dependency-only) session-sharing resolution.
    /// Returns `false` (never errors) for a cell row that no longer exists,
    /// matching how a vanished cell should just fall through to a fresh
    /// dispatch rather than fail the whole worker.
    pub fn take_force_resume_own_session(&self, squad_id: &str, task_idx: i64, idx: i64) -> bool {
        let was_set = self
            .conn
            .query_row(
                "SELECT force_resume_own_session FROM cells WHERE squad_id=? AND task_idx=? AND idx=?",
                params![squad_id, task_idx, idx],
                |r| r.get::<_, i64>(0),
            )
            .optional()
            .ok()
            .flatten()
            .unwrap_or(0)
            == 1;
        if was_set {
            let _ = self.conn.execute(
                "UPDATE cells SET force_resume_own_session=0 WHERE squad_id=? AND task_idx=? AND idx=?",
                params![squad_id, task_idx, idx],
            );
        }
        was_set
    }

    /// The three pieces of a cell row `server::open_agent_terminal`'s
    /// `mode=agent` path (RAL-288 Stage 6) needs to decide whether a still-
    /// running cell can be cleanly detached at all, gathered in one query
    /// rather than three round trips: `machine` (RAL-185 routing; `Some`
    /// means a remote provider, which has no local tmux session to detach
    /// yet -- see FIX_AGENT.local.md's open decision on this), whether this
    /// is a `command`-kind cell (which never reaches `ClaudeCodeBackend::run`
    /// at all, so there is no live agent session to hand off), and its
    /// current run [`NodeState`] (detach only makes sense while genuinely
    /// `Running` -- a finished cell has no process left to detach).
    ///
    /// Returns `Err(StoreError::NotFound)` when the squad or cell row does
    /// not exist, matching [`Self::get_cell_agent_resume`]'s convention.
    pub fn get_cell_input_gate(
        &self,
        squad_id: &str,
        task_idx: i64,
        cell_idx: i64,
    ) -> Result<(Option<String>, bool, NodeState)> {
        self.conn
            .query_row(
                "SELECT machine, command IS NOT NULL, state FROM cells \
                 WHERE squad_id=? AND task_idx=? AND idx=?",
                params![squad_id, task_idx, cell_idx],
                |r| {
                    let machine: Option<String> = r.get(0)?;
                    let is_command_cell: bool = r.get(1)?;
                    let state_raw: Option<String> = r.get(2)?;
                    Ok((machine, is_command_cell, state_raw))
                },
            )
            .optional()?
            .ok_or(StoreError::NotFound)
            .map(|(machine, is_command_cell, state_raw)| {
                let state = state_raw
                    .and_then(|s| NodeState::parse(&s))
                    .unwrap_or(NodeState::Pending);
                (machine, is_command_cell, state)
            })
    }

    /// Resolve `(squad_id, task_name, cell_sid)` into a `cell:...`
    /// [`crate::entity_uri::EntityUri`] string, so RAL-320 watches can match
    /// against it. `None` when the triple doesn't match a row — mirrors
    /// [`Self::set_cell_agent_session_id_live`]'s same best-effort lookup.
    #[must_use]
    pub fn cell_entity_uri(
        &self,
        squad_id: &str,
        task_name: &str,
        cell_sid: &str,
    ) -> Option<String> {
        let (task_idx, cell_idx): (i64, i64) = self
            .conn
            .query_row(
                "SELECT c.task_idx, c.idx FROM cells c
                 JOIN tasks t ON t.squad_id = c.squad_id AND t.idx = c.task_idx
                 WHERE c.squad_id=?1 AND t.name=?2 AND c.sid=?3",
                params![squad_id, task_name, cell_sid],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .ok()??;
        Some(
            crate::entity_uri::EntityUri::Cell {
                squad_id: squad_id.to_string(),
                task_idx,
                cell_idx,
            }
            .to_string(),
        )
    }

    /// Resolve `(squad_id, task_name)` into a `task:...`
    /// [`crate::entity_uri::EntityUri`] string, so RAL-320 watches can match
    /// against it. `None` when the pair doesn't match a row.
    #[must_use]
    pub fn task_entity_uri(&self, squad_id: &str, task_name: &str) -> Option<String> {
        let task_idx: i64 = self
            .conn
            .query_row(
                "SELECT idx FROM tasks WHERE squad_id=?1 AND name=?2",
                params![squad_id, task_name],
                |r| r.get(0),
            )
            .optional()
            .ok()??;
        Some(
            crate::entity_uri::EntityUri::Task {
                squad_id: squad_id.to_string(),
                task_idx,
            }
            .to_string(),
        )
    }

    /// Persist a cell's CLI-agent session/thread id as soon as it's known —
    /// before the cell finishes — so "Open Agent" activates immediately
    /// rather than only once the whole cell completes. Called from
    /// `runner::forward_runner_event` for any event whose payload carries an
    /// `agent_session_id`, whichever backend emitted it (RAL-102 follow-up;
    /// mirrors `guardian::set_branch_resolver_session_id`'s "Watch Live" idea,
    /// applied to plain task cells instead of a side-channel file + watcher
    /// thread).
    ///
    /// Best-effort and silently a no-op when `(squad_id, task_name, cell_sid)`
    /// doesn't match a cell row — e.g. a proof step or a Guardian
    /// resolver invocation, which route through the very same event-forwarding
    /// code path but aren't rows in this table at all.
    pub fn set_cell_agent_session_id_live(
        &self,
        squad_id: &str,
        task_name: &str,
        cell_sid: &str,
        agent_session_id: &str,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE cells SET agent_session_id=?
             WHERE squad_id=? AND sid=? AND task_idx=(SELECT idx FROM tasks WHERE squad_id=? AND name=?)",
            params![agent_session_id, squad_id, cell_sid, squad_id, task_name],
        )?;
        Ok(())
    }

    /// Persist a cell's running token/cost usage as soon as fresh numbers
    /// are known -- before the cell finishes -- so the board shows live
    /// cost/tokens for a `running` cell instead of `$0.0000` / `0/0` until
    /// completion (RAL-161). Called from `runner::forward_runner_event`
    /// alongside [`Self::set_cell_claude_session_id_live`], which it
    /// mirrors: same best-effort, same silent no-op when
    /// `(squad_id, task_name, cell_sid)` doesn't match a cell row (a
    /// proof step or Guardian resolver invocation shares the same
    /// event-forwarding code path but isn't a row in this table).
    ///
    /// Unlike [`Self::record_cell_result`]'s final write, this is a plain
    /// overwrite with no `COALESCE` -- a live scrape always carries real
    /// numbers (never `None`), and the final write always happens after any
    /// live writes, so it naturally wins as the authoritative last word.
    pub fn set_cell_live_usage(
        &self,
        squad_id: &str,
        task_name: &str,
        cell_sid: &str,
        usage: crate::runner::LiveUsage,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE cells SET tokens_in=?, tokens_out=?, cache_creation_tokens=?, cache_read_tokens=?, cost_usd=?
             WHERE squad_id=? AND sid=? AND task_idx=(SELECT idx FROM tasks WHERE squad_id=? AND name=?)",
            params![
                usage.tokens_in,
                usage.tokens_out,
                usage.cache_creation_tokens,
                usage.cache_read_tokens,
                usage.cost_usd,
                squad_id,
                cell_sid,
                squad_id,
                task_name
            ],
        )?;
        Ok(())
    }

    /// Record that fresh pane output was just observed for the tmux-wrapped
    /// session named `session_name` (RAL-170) — the liveness signal behind
    /// the Live View's "last activity" timestamp. Called from
    /// `SubprocessRunner::run_via_tmux_attempt`'s poll loop every time a
    /// pane capture shows more lines than the previous poll, i.e. it
    /// piggybacks on work the daemon already does continuously for every
    /// running tmux-wrapped session (task cell, proof step, or Guardian
    /// resolver/manual-check alike — all share the same deterministic
    /// `crate::tmux::session_name` key), rather than being computed only
    /// when a Live View happens to be open.
    ///
    /// Deliberately **in-memory only, never a DB column**: at the existing
    /// 500ms tmux-poll cadence, a SQLite `UPDATE` per cell per tick would
    /// scale with concurrently-running cells (fine at "hundreds", a real
    /// contention/write-amplification risk at "thousands" sharing the one
    /// `Store` mutex) for a value nobody needs once the process exits. A
    /// plain in-process `HashMap` entry costs a pointer-sized insert instead
    /// of a WAL write, and [`Self::clear_live_activity`] removes it as soon
    /// as the owning `run_via_tmux` call returns, so memory stays bounded by
    /// *currently running* cells rather than growing across the daemon's
    /// lifetime.
    pub fn note_live_activity(&mut self, session_name: &str, at_ms: i64) {
        self.live_activity.insert(session_name.to_string(), at_ms);
    }

    /// The last time [`Self::note_live_activity`] was called for
    /// `session_name`, in Unix epoch milliseconds — `None` if the cell
    /// has never produced pane growth (fresh cell, no output yet) or has
    /// already ended (see [`Self::clear_live_activity`]).
    pub fn live_activity_ms(&self, session_name: &str) -> Option<i64> {
        self.live_activity.get(session_name).copied()
    }

    /// Drop the liveness entry for `session_name` once its owning
    /// `run_via_tmux` call has returned for good (not on an intermediate
    /// reattach kill — see the call site's doc comment). Best-effort: a
    /// missing entry (cell never produced output, or was already
    /// cleared) is not an error.
    pub fn clear_live_activity(&mut self, session_name: &str) {
        self.live_activity.remove(session_name);
    }

    /// RAL-241: has a stall escalation already been enqueued for
    /// `session_name`'s *current* stall onset — i.e. has this exact
    /// `last_activity_ms` value (the moment activity stopped) already fired
    /// a mailbox message? Comparing the stored value against the caller's
    /// current `last_activity_ms` (rather than just checking presence) means
    /// a fresh burst of activity followed by a *new* stall is escalated
    /// again, without needing an explicit "clear" between the two stalls.
    #[must_use]
    pub fn is_stall_escalated(&self, session_name: &str, last_activity_ms: i64) -> bool {
        self.stall_escalated.get(session_name) == Some(&last_activity_ms)
    }

    /// Record that a stall escalation was just enqueued for `session_name`'s
    /// current stall onset (`last_activity_ms`), so
    /// [`Self::is_stall_escalated`] suppresses a repeat enqueue for the same
    /// ongoing stall.
    pub fn note_stall_escalated(&mut self, session_name: &str, last_activity_ms: i64) {
        self.stall_escalated
            .insert(session_name.to_string(), last_activity_ms);
    }

    /// Drop the RAL-241 stall-escalation bookkeeping for `session_name`,
    /// mirroring [`Self::clear_live_activity`] — called from the same site,
    /// once the owning `run_via_tmux` call has a terminal result for good.
    pub fn clear_stall_escalated(&mut self, session_name: &str) {
        self.stall_escalated.remove(session_name);
    }

    /// RAL-208: request that guardian `id`'s LLM-authored final change
    /// summary be (re)generated for the given enabled-branch `signature`. A
    /// no-op if `signature` already matches the signature the *current*
    /// summary was generated from — nothing about the branch set actually
    /// changed, so there is nothing to regenerate (this is what keeps a
    /// restack triggered by feedback, a manual push, or a base-branch shift
    /// from re-firing the LLM: none of those change which branches are
    /// enabled). Otherwise (re)starts this guardian's debounce clock; see
    /// [`Self::take_due_final_summary_requests`].
    /// RAL-303: `force` overrides that no-op. The signature check assumes the
    /// stored summary is the LLM one this guardian's branch set last produced,
    /// which is false while `change_summary` still holds the deterministic
    /// git-log preliminary — there the branch set is unchanged but the summary
    /// has never been through the LLM at all, so the caller passes `force` to
    /// get the handoff it would otherwise be denied.
    pub fn request_final_summary(&mut self, id: &str, signature: &str, now_ms: i64, force: bool) {
        let d = self
            .guardian_summary_debounce
            .entry(id.to_string())
            .or_default();
        if !force && d.generated_signature.as_deref() == Some(signature) {
            return;
        }
        d.pending_signature = Some(signature.to_string());
        d.pending_requested_at_ms = Some(now_ms);
    }

    /// RAL-208: atomically claim every guardian whose pending
    /// [`Self::request_final_summary`] has gone `debounce_ms` without a
    /// newer request — i.e. its "quiet period" has elapsed — clearing their
    /// pending state so a concurrent duplicate sweep finds nothing left to
    /// claim. Returns `(guardian_id, signature)` pairs for the caller to
    /// actually generate (a background call, well outside this lock).
    pub fn take_due_final_summary_requests(
        &mut self,
        now_ms: i64,
        debounce_ms: i64,
    ) -> Vec<(String, String)> {
        let mut due = Vec::new();
        for (id, d) in &mut self.guardian_summary_debounce {
            let Some(requested_at) = d.pending_requested_at_ms else {
                continue;
            };
            if now_ms.saturating_sub(requested_at) >= debounce_ms {
                if let Some(sig) = d.pending_signature.take() {
                    due.push((id.clone(), sig));
                }
                d.pending_requested_at_ms = None;
            }
        }
        due
    }

    /// Queue a guardian's PR stack for asynchronous submission, restarting
    /// its trailing-debounce clock. The database row makes the request
    /// durable across daemon restarts.
    pub fn request_auto_submit_branch(&self, id: &str, now_ms: i64) -> Result<()> {
        self.conn.execute(
            "INSERT INTO guardian_auto_submit_requests (guardian_id, requested_at_ms) \
             VALUES (?1, ?2) \
             ON CONFLICT(guardian_id) DO UPDATE SET requested_at_ms = ?2",
            params![id, now_ms],
        )?;
        Ok(())
    }

    /// Atomically claim every guardian whose auto-submit request has been
    /// quiet for at least `debounce_ms`.
    pub fn take_due_auto_submits(&self, now_ms: i64, debounce_ms: i64) -> Result<Vec<String>> {
        let tx = self.conn.unchecked_transaction()?;
        let due: Vec<String> = {
            let mut stmt = tx.prepare(
                "SELECT guardian_id FROM guardian_auto_submit_requests \
                 WHERE ?1 - requested_at_ms >= ?2",
            )?;
            let rows = stmt.query_map(params![now_ms, debounce_ms], |r| r.get(0))?;
            rows.collect::<rusqlite::Result<Vec<String>>>()?
        };
        for id in &due {
            tx.execute(
                "DELETE FROM guardian_auto_submit_requests WHERE guardian_id=?",
                params![id],
            )?;
        }
        tx.commit()?;
        Ok(due)
    }

    /// RAL-208: record that guardian `id`'s change summary now reflects
    /// `signature`, so a later request for the same signature is recognized
    /// as already-satisfied (see [`Self::request_final_summary`]).
    pub fn mark_final_summary_generated(&mut self, id: &str, signature: &str) {
        let d = self
            .guardian_summary_debounce
            .entry(id.to_string())
            .or_default();
        d.generated_signature = Some(signature.to_string());
    }

    /// RAL-303: claim the one repair attempt this daemon process gets at a
    /// guardian whose change summary is missing or was never upgraded past the
    /// git-log preliminary one. Returns `true` for the first caller only.
    ///
    /// The bookkeeping above is in-memory, so a daemon restart between
    /// [`Self::request_final_summary`] and the sweep that would have fired it
    /// drops the pending request — and since only an enabled-branch change
    /// re-requests one, a review that is otherwise settled keeps whatever
    /// summary it had at restart forever. This lets the review-maintenance
    /// sweep re-request exactly once rather than re-firing the LLM on every
    /// tick when generation is failing for some other reason.
    pub fn claim_final_summary_repair(&mut self, id: &str) -> bool {
        let d = self
            .guardian_summary_debounce
            .entry(id.to_string())
            .or_default();
        if d.repair_attempted || d.generated_signature.is_some() {
            return false;
        }
        d.repair_attempted = true;
        true
    }

    /// Fetch a task's first (lowest-`idx`) cell's cwd — the cwd a
    /// task-scope proof step's "Open Agent" action resumes into, mirroring
    /// how `scheduler.rs::run_proofs` itself picks a cwd for a task-scope
    /// step (`cells.iter().find(|s| s.task_idx == task_idx)`).
    ///
    /// Returns `Err(StoreError::NotFound)` when the squad/task has no cells.
    pub fn get_task_first_cell_cwd(&self, squad_id: &str, task_idx: i64) -> Result<String> {
        self.conn
            .query_row(
                "SELECT cwd FROM cells WHERE squad_id=? AND task_idx=? ORDER BY idx LIMIT 1",
                params![squad_id, task_idx],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()?
            .map(|c| c.unwrap_or_default())
            .ok_or(StoreError::NotFound)
    }

    /// Fetch a `prompt`-kind proof step's agent and recorded CLI-agent
    /// cell id, for its "Open Agent" terminal action — see
    /// [`Store::get_cell_agent_resume`]'s doc comment for the same idea
    /// (including why `agent` is needed alongside the id) applied to a plain
    /// cell.
    ///
    /// Returns `Err(StoreError::NotFound)` when the proof row does not
    /// exist; the id itself is `None` when the step hasn't run yet (or ran
    /// under an agent with no resume mechanism) rather than an error.
    pub fn get_proof_agent_session_id(
        &self,
        squad_id: &str,
        task_idx: i64,
        scope: &str,
        cell_idx: i64,
        idx: i64,
    ) -> Result<(String, Option<String>)> {
        self.conn
            .query_row(
                "SELECT agent, agent_session_id FROM proofs
                 WHERE squad_id=? AND task_idx=? AND scope=? AND cell_idx=? AND idx=?",
                params![squad_id, task_idx, scope, cell_idx, idx],
                |r| {
                    Ok((
                        r.get::<_, Option<String>>(0)?.unwrap_or_default(),
                        r.get::<_, Option<String>>(1)?,
                    ))
                },
            )
            .optional()?
            .ok_or(StoreError::NotFound)
    }

    /// Reset every cell downstream of any of `roots` (but not the roots
    /// themselves) within `squad_id` that is currently `Failed` back to
    /// Pending — clearing its error, resetting its own cell-level
    /// proofs, and resetting its owning task. Used by
    /// [`Store::restart_cell_proof`] and [`Store::restart_task_proof`]
    /// (RAL-165): retrying a proof can change its outcome, and there's no
    /// case where a downstream cell shouldn't get a fresh chance once
    /// that new outcome is known — whether it was left Failed by a direct
    /// cascade from this failure or for its own, independent reason.
    /// Cells that are `Done`, `Pending`, or `Running` are left untouched.
    fn revive_failed_downstream_cells(&self, squad_id: &str, roots: &[(i64, i64)]) -> Result<()> {
        let mut downstream: HashSet<(i64, i64)> = HashSet::new();
        for &(task_idx, idx) in roots {
            let impact = self.compute_cell_restart_impact(squad_id, task_idx, idx)?;
            downstream.extend(impact.cells.iter().map(|s| (s.task_idx, s.idx)));
        }
        for r in roots {
            downstream.remove(r);
        }
        for (task_idx, idx) in downstream {
            let state: Option<String> = self
                .conn
                .query_row(
                    "SELECT state FROM cells WHERE squad_id=? AND task_idx=? AND idx=?",
                    params![squad_id, task_idx, idx],
                    |r| r.get(0),
                )
                .optional()?;
            if state.as_deref() != Some("failed") {
                continue;
            }
            self.conn.execute(
                "UPDATE cells SET state='pending', error=NULL WHERE squad_id=? AND task_idx=? AND idx=?",
                params![squad_id, task_idx, idx],
            )?;
            self.conn.execute(
                "UPDATE proofs SET state='pending' WHERE squad_id=? AND task_idx=? AND scope='cell' AND cell_idx=?",
                params![squad_id, task_idx, idx],
            )?;
            self.conn.execute(
                "UPDATE tasks SET state='pending', error=NULL WHERE squad_id=? AND idx=?",
                params![squad_id, task_idx],
            )?;
        }
        Ok(())
    }

    /// Restart a single cell's proof steps from `proof_from` onwards:
    /// reset only the cell-level proofs at index >= `proof_from` to
    /// Pending while leaving the cell itself Done. The owning task and squad
    /// are put back to Pending so the scheduler re-enters them. The scheduler
    /// detects that the cell is Done with pending proofs via
    /// [`Store::cells_needing_proof_only`] and skips re-running the
    /// cell body, executing only the proof steps. Any downstream cell
    /// left `Failed` by an earlier pass is revived back to Pending too
    /// (RAL-165) — see [`Store::revive_failed_downstream_cells`]. Returns
    /// dirtied dependent squad ids.
    pub fn restart_cell_proof(
        &self,
        squad_id: &str,
        task_idx: i64,
        cell_idx: i64,
        proof_from: i64,
    ) -> Result<Vec<String>> {
        let exists: Option<i64> = self
            .conn
            .query_row(
                "SELECT idx FROM cells WHERE squad_id=? AND task_idx=? AND idx=?",
                params![squad_id, task_idx, cell_idx],
                |r| r.get(0),
            )
            .optional()?;
        if exists.is_none() {
            return Err(StoreError::NotFound);
        }
        // Reset only proofs at idx >= proof_from — the cell body stays
        // Done so the scheduler's proof-only path re-runs proofs without
        // re-running the cell.
        self.conn.execute(
            "UPDATE proofs SET state='pending', env_out_of_date=0 WHERE squad_id=? AND task_idx=? AND scope='cell' AND cell_idx=? AND idx>=?",
            params![squad_id, task_idx, cell_idx, proof_from],
        )?;
        self.revive_failed_downstream_cells(squad_id, &[(task_idx, cell_idx)])?;
        self.conn.execute(
            "UPDATE tasks SET state='pending', error=NULL WHERE squad_id=? AND idx=?",
            params![squad_id, task_idx],
        )?;
        self.conn.execute(
            "UPDATE squads SET state='pending', updated_at_ms=? WHERE id=?",
            params![now_ms(), squad_id],
        )?;
        let _ = self.log_event(
            Some(squad_id),
            None,
            "proof",
            Some(&format!("cell t{task_idx}/s{cell_idx}")),
            "restarted",
        );
        self.dirty_dependents(squad_id)
    }

    /// Restart a task's task-level proof steps from `proof_from` onwards:
    /// reset only the task-scope proofs at index >= `proof_from` to Pending
    /// while leaving all cells and their cell-level proofs intact. The
    /// task and squad are put back to Pending so the scheduler's task finalizer
    /// fires and re-runs the task-level proofs. Any cell downstream of
    /// this task that was left `Failed` by an earlier pass is revived back to
    /// Pending too (RAL-165) — see [`Store::revive_failed_downstream_cells`].
    /// Returns dirtied dependent squad ids.
    pub fn restart_task_proof(
        &self,
        squad_id: &str,
        task_idx: i64,
        proof_from: i64,
    ) -> Result<Vec<String>> {
        let exists: Option<i64> = self
            .conn
            .query_row(
                "SELECT idx FROM tasks WHERE squad_id=? AND idx=?",
                params![squad_id, task_idx],
                |r| r.get(0),
            )
            .optional()?;
        if exists.is_none() {
            return Err(StoreError::NotFound);
        }
        // Reset only task-scope proofs at idx >= proof_from. Cell states
        // and cell-level proofs are intentionally left untouched: all
        // cells remain Done so the scheduler's task finalizer fires
        // immediately and re-runs only the affected task-level proofs,
        // without re-running any cell body.
        self.conn.execute(
            "UPDATE proofs SET state='pending', env_out_of_date=0 WHERE squad_id=? AND task_idx=? AND scope='task' AND idx>=?",
            params![squad_id, task_idx, proof_from],
        )?;
        let roots: Vec<(i64, i64)> = self
            .cells_of(squad_id)?
            .into_iter()
            .filter(|s| s.task_idx == task_idx)
            .map(|s| (s.task_idx, s.idx))
            .collect();
        self.revive_failed_downstream_cells(squad_id, &roots)?;
        self.conn.execute(
            "UPDATE tasks SET state='pending', error=NULL WHERE squad_id=? AND idx=?",
            params![squad_id, task_idx],
        )?;
        self.conn.execute(
            "UPDATE squads SET state='pending', updated_at_ms=? WHERE id=?",
            params![now_ms(), squad_id],
        )?;
        let _ = self.log_event(
            Some(squad_id),
            None,
            "proof",
            Some(&format!("task t{task_idx}")),
            "restarted",
        );
        self.dirty_dependents(squad_id)
    }

    /// Cells that are `done` in the DB but have at least one cell-level
    /// proof in a non-terminal state. The scheduler uses this to identify
    /// "proof-only restart" cases: these cells skip the runner and execute
    /// only their proof steps.
    pub fn cells_needing_proof_only(&self, squad_id: &str) -> Result<HashSet<(i64, i64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT s.task_idx, s.idx FROM cells s
             WHERE s.squad_id=? AND s.state='done'
             AND EXISTS (
                 SELECT 1 FROM proofs v
                 WHERE v.squad_id=s.squad_id AND v.task_idx=s.task_idx
                 AND v.scope='cell' AND v.cell_idx=s.idx
                 AND v.state NOT IN ('done','failed','cancelled')
             )",
        )?;
        let rows = stmt
            .query_map(params![squad_id], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
            })?
            .collect::<std::result::Result<HashSet<_>, _>>()?;
        Ok(rows)
    }

    // ── Queue view + reorder (RAL Queue) ─────────────────────────────────────

    /// The flat list of runnable work items across every schedulable squad
    /// (`pending`/`running`/`queued`), classified for the Queue view. Each item
    /// is a cell, a cell-level proof, or a task-level proof still in a
    /// `pending`/`running` state. Ordered canonically by
    /// `(queue_rank NULLS LAST, squad created_at, task_idx, cells-before-proofs, idx)`.
    pub fn queue(&self) -> Result<Vec<QueueItem>> {
        let squads: Vec<(String, Option<String>, String, i64)> = {
            let mut stmt = self.conn.prepare(
                "SELECT id, label, state, created_at_ms FROM squads
                 WHERE state IN ('pending','running','queued') ORDER BY created_at_ms, id",
            )?;
            stmt.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, i64>(3)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?
        };

        let mut out: Vec<QueueItem> = Vec::new();
        for (squad_id, squad_label, squad_state, squad_created) in squads {
            self.queue_items_for_squad(
                &squad_id,
                squad_label.as_deref(),
                &squad_state,
                squad_created,
                &mut out,
            )?;
        }
        // Canonical order: ranked items first (ascending), then unranked by the
        // stable insertion order already produced above.
        out.sort_by(|a, b| {
            queue_sort_key(a)
                .partial_cmp(&queue_sort_key(b))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        Ok(out)
    }

    fn queue_items_for_squad(
        &self,
        squad_id: &str,
        squad_label: Option<&str>,
        squad_state: &str,
        squad_created: i64,
        out: &mut Vec<QueueItem>,
    ) -> Result<()> {
        let cells = self.cells_of(squad_id)?;
        let tasks = self.tasks_of(squad_id)?;
        let plan = match crate::plan::plan(&cells, &tasks) {
            Ok(p) => p,
            Err(_) => return Ok(()), // a cyclic squad cannot be queued
        };
        let squad_deps_ok = self.deps_satisfied(&self.squad_depends_on(squad_id)?)?;

        // Each task's declared `depends_on` (task names), for the header display.
        let task_deps: HashMap<i64, Vec<String>> = tasks
            .iter()
            .map(|t| (t.idx, t.depends_on.clone()))
            .collect();

        // Position of each (task_idx, idx) within `cells` (== plan indexing).
        let mut pos_of: HashMap<(i64, i64), usize> = HashMap::new();
        for (i, s) in cells.iter().enumerate() {
            pos_of.insert((s.task_idx, s.idx), i);
        }

        // Cell metadata: state, display name, queue_rank.
        struct SMeta {
            state: String,
            name: String,
            rank: Option<f64>,
        }
        let smeta: HashMap<(i64, i64), SMeta> = {
            let mut stmt = self.conn.prepare(
                "SELECT task_idx, idx, sid, name, state, queue_rank FROM cells WHERE squad_id=?",
            )?;
            stmt.query_map(params![squad_id], |r| {
                let ti: i64 = r.get(0)?;
                let si: i64 = r.get(1)?;
                let sid: String = r.get(2)?;
                let name: Option<String> = r.get(3)?;
                Ok((
                    (ti, si),
                    SMeta {
                        state: r.get(4)?,
                        name: name.unwrap_or(sid),
                        rank: r.get(5)?,
                    },
                ))
            })?
            .collect::<std::result::Result<HashMap<_, _>, _>>()?
        };

        // Task display names, so blocked_by can reference them.
        let task_names: HashMap<i64, String> =
            tasks.iter().map(|t| (t.idx, t.name.clone())).collect();

        let state_of = |ti: i64, si: i64| -> String {
            smeta
                .get(&(ti, si))
                .map(|m| m.state.clone())
                .unwrap_or_else(|| "pending".to_string())
        };

        // ── cells ──
        for s in &cells {
            let meta = match smeta.get(&(s.task_idx, s.idx)) {
                Some(m) => m,
                None => continue,
            };
            if !matches!(meta.state.as_str(), "pending" | "running") {
                continue;
            }
            let pos = pos_of[&(s.task_idx, s.idx)];
            let mut blocked_by: Vec<String> = Vec::new();
            let mut excluded = false;
            if !squad_deps_ok {
                blocked_by.push("upstream squad".to_string());
            }
            let mut deps_paths: Vec<String> = Vec::new();
            for &d in &plan.deps[pos] {
                let (dti, dsi) = (cells[d].task_idx, cells[d].idx);
                deps_paths.push(cell_path(squad_id, dti, dsi));
                let dep_state = state_of(dti, dsi);
                let dep_label = smeta
                    .get(&(dti, dsi))
                    .map(|m| format!("cell {}", m.name))
                    .unwrap_or_else(|| format!("cell t{dti}/s{dsi}"));
                match dep_state.as_str() {
                    "done" | "ignored" => {}
                    "failed" | "cancelled" => {
                        excluded = true;
                        blocked_by.push(dep_label);
                    }
                    _ => blocked_by.push(dep_label),
                }
            }
            let readiness = classify(
                meta.state.as_str(),
                excluded,
                blocked_by.is_empty() && squad_deps_ok,
            );
            out.push(QueueItem {
                squad_id: squad_id.to_string(),
                squad_label: squad_label.map(str::to_string),
                squad_state: squad_state.to_string(),
                squad_created_at_ms: squad_created,
                kind: "cell".to_string(),
                path: cell_path(squad_id, s.task_idx, s.idx),
                indent: 2,
                task_idx: s.task_idx,
                task_name: s.task_name.clone(),
                cell_idx: s.idx,
                proof_idx: -1,
                proof_scope: String::new(),
                name: meta.name.clone(),
                state: meta.state.clone(),
                readiness,
                blocked_by,
                task_depends_on: task_deps.get(&s.task_idx).cloned().unwrap_or_default(),
                depends_on: s.depends_on.clone(),
                deps_paths,
                queue_rank: meta.rank,
            });
        }

        // ── proofs (cell-scope and task-scope) ──
        let proof_rows: Vec<ProofQueueRow> = {
            let mut stmt = self.conn.prepare(
                "SELECT task_idx, scope, cell_idx, idx, vid, kind, state, queue_rank
                 FROM proofs WHERE squad_id=? ORDER BY task_idx, scope, cell_idx, idx",
            )?;
            stmt.query_map(params![squad_id], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, Option<String>>(4)?,
                    r.get::<_, String>(5)?,
                    r.get::<_, String>(6)?,
                    r.get::<_, Option<f64>>(7)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?
        };
        for (ti, scope, sidx, vi, vid, kind, vstate, rank) in proof_rows {
            if !matches!(vstate.as_str(), "pending" | "running") {
                continue;
            }
            let mut blocked_by: Vec<String> = Vec::new();
            let mut excluded = false;
            let mut deps_paths: Vec<String> = Vec::new();
            if !squad_deps_ok {
                blocked_by.push("upstream squad".to_string());
            }
            let (name, path, indent, kind_str);
            if scope == "cell" {
                name = vid.unwrap_or_else(|| kind.clone());
                path = sproof_path(squad_id, ti, sidx, vi);
                indent = 3;
                kind_str = "cell_proof".to_string();
                // Depends on the owning cell, then prior cell-proof.
                deps_paths.push(cell_path(squad_id, ti, sidx));
                let ss = state_of(ti, sidx);
                match ss.as_str() {
                    "done" | "ignored" => {}
                    "failed" | "cancelled" => {
                        excluded = true;
                        blocked_by.push("owning cell".to_string());
                    }
                    _ => blocked_by.push("owning cell".to_string()),
                }
                if vi > 0 {
                    deps_paths.push(sproof_path(squad_id, ti, sidx, vi - 1));
                    blocked_by.push("prior proof step".to_string());
                }
            } else {
                name = vid.unwrap_or_else(|| kind.clone());
                path = tproof_path(squad_id, ti, vi);
                indent = 2;
                kind_str = "task_proof".to_string();
                // Depends on every cell in the task, then prior task-proof.
                for s in cells.iter().filter(|s| s.task_idx == ti) {
                    deps_paths.push(cell_path(squad_id, ti, s.idx));
                    let ss = state_of(ti, s.idx);
                    match ss.as_str() {
                        "done" | "ignored" => {}
                        "failed" | "cancelled" => excluded = true,
                        _ => {}
                    }
                }
                let tname = task_names
                    .get(&ti)
                    .cloned()
                    .unwrap_or_else(|| format!("t{ti}"));
                if cells.iter().filter(|s| s.task_idx == ti).any(|s| {
                    !NodeState::parse(&state_of(ti, s.idx))
                        .is_some_and(|n| n.satisfies_dependents())
                }) {
                    blocked_by.push(format!("task {tname} cells"));
                }
                if vi > 0 {
                    deps_paths.push(tproof_path(squad_id, ti, vi - 1));
                    blocked_by.push("prior proof step".to_string());
                }
            }
            let readiness = classify(
                vstate.as_str(),
                excluded,
                blocked_by.is_empty() && squad_deps_ok,
            );
            let tname = task_names.get(&ti).cloned().unwrap_or_default();
            out.push(QueueItem {
                squad_id: squad_id.to_string(),
                squad_label: squad_label.map(str::to_string),
                squad_state: squad_state.to_string(),
                squad_created_at_ms: squad_created,
                kind: kind_str,
                path,
                indent,
                task_idx: ti,
                task_name: tname,
                cell_idx: if scope == "cell" { sidx } else { -1 },
                proof_idx: vi,
                proof_scope: scope,
                name,
                state: vstate,
                readiness,
                blocked_by,
                task_depends_on: task_deps.get(&ti).cloned().unwrap_or_default(),
                depends_on: Vec::new(),
                deps_paths,
                queue_rank: rank,
            });
        }
        Ok(())
    }

    /// Persist `queue_rank` for a single queue item, addressed by its path.
    pub fn set_queue_rank(&self, path: &str, rank: f64) -> Result<()> {
        let p = parse_queue_path(path).ok_or(StoreError::NotFound)?;
        match p.kind {
            QueuePathKind::Cell => self.conn.execute(
                "UPDATE cells SET queue_rank=? WHERE squad_id=? AND task_idx=? AND idx=?",
                params![rank, p.squad_id, p.task_idx, p.cell_idx],
            )?,
            QueuePathKind::CellProof => self.conn.execute(
                "UPDATE proofs SET queue_rank=? WHERE squad_id=? AND task_idx=? AND scope='cell' AND cell_idx=? AND idx=?",
                params![rank, p.squad_id, p.task_idx, p.cell_idx, p.proof_idx],
            )?,
            QueuePathKind::TaskProof => self.conn.execute(
                "UPDATE proofs SET queue_rank=? WHERE squad_id=? AND task_idx=? AND scope='task' AND cell_idx=-1 AND idx=?",
                params![rank, p.squad_id, p.task_idx, p.proof_idx],
            )?,
        };
        Ok(())
    }

    /// Reorder the queue to match `desired` (a list of item paths). The order is
    /// first stabilized against item dependencies (a dependency dragged below its
    /// dependent is pulled back above it), then persisted as evenly-spaced
    /// `queue_rank`s. Returns the repaired canonical order. Items not present in
    /// `desired` keep their current relative position at the end.
    pub fn reorder_queue(&self, desired: &[String]) -> Result<Vec<String>> {
        let items = self.queue()?;
        let pairs: Vec<(String, Vec<String>)> = items
            .iter()
            .map(|i| (i.path.clone(), i.deps_paths.clone()))
            .collect();
        let ordered = stabilize_order(desired, &pairs);
        for (i, path) in ordered.iter().enumerate() {
            self.set_queue_rank(path, i as f64)?;
        }
        crate::rlog!(
            INFO,
            "ralphus [state] queue reordered items={}",
            ordered.len()
        );
        let _ = self.cartographer_log(crate::cartographer::CartographerEntry {
            level: crate::logging::LogLevel::INFO,
            source: "store",
            message: "queue reordered",
            scope: Some("queue"),
            squad_id: None,
            guardian_id: None,
            cell_id: None,
            task: None,
            log_path: None,
            payload: serde_json::json!({"items": ordered.len()}),
            admin_only: false,
        });
        Ok(ordered)
    }

    /// Move `selected` items to `position` in the queue. `absolute` places them
    /// at that 0-based index (clamped to the queue length — a huge value drops to
    /// the bottom); relative moves them up by `position` places (negative moves
    /// down). Preserves the caller's `selected` order for the moved block.
    pub fn set_queue_position(
        &self,
        selected: &[String],
        position: i64,
        absolute: bool,
    ) -> Result<Vec<String>> {
        let items = self.queue()?;
        let current: Vec<String> = items.iter().map(|i| i.path.clone()).collect();
        let desired = move_block(&current, selected, position, absolute);
        crate::rlog!(
            INFO,
            "ralphus [state] queue set-position n={} position={position} mode={}",
            selected.len(),
            if absolute { "absolute" } else { "relative" }
        );
        let _ = self.cartographer_log(crate::cartographer::CartographerEntry {
            level: crate::logging::LogLevel::INFO,
            source: "store",
            message: "queue set-position",
            scope: Some("queue"),
            squad_id: None,
            guardian_id: None,
            cell_id: None,
            task: None,
            log_path: None,
            payload: serde_json::json!({
                "n": selected.len(),
                "position": position,
                "mode": if absolute { "absolute" } else { "relative" },
            }),
            admin_only: false,
        });
        self.reorder_queue(&desired)
    }
}

/// The token/cost figures recorded for one cell or proof-step run (RAL-326).
///
/// Bundled rather than passed as loose parameters because both write paths
/// ([`Store::record_cell_result`] via [`CellOutcome`], and
/// [`Store::set_proof_result`]) need the same six values, and
/// [`Store::set_proof_result`] already carries enough positional arguments
/// without them.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct RecordedUsage {
    /// Uncached input tokens.
    pub tokens_in: i64,
    /// Output tokens.
    pub tokens_out: i64,
    /// Prompt-cache *write* tokens -- input billed at the cache-creation
    /// rate. Deliberately not folded into `tokens_in`, which keeps meaning
    /// exactly what it always has. `0` for a backend whose harness reports
    /// no cache breakdown, and for every row written before RAL-326.
    pub cache_creation_tokens: i64,
    /// Prompt-cache *read* tokens -- input served from an existing cache
    /// entry at the discounted rate. See `cache_creation_tokens`.
    pub cache_read_tokens: i64,
    /// RAL-373: total input tokens spent on Claude Code's own
    /// auto-compaction summarization requests -- billed at the *uncached*
    /// input rate, the reason `cost_usd` and
    /// `tokens_in + cache_creation_tokens + cache_read_tokens` diverge on
    /// any cell that compacts. Compaction *output* (the summary itself) is
    /// not currently reported by Claude Code and so is not captured here.
    /// `0` for a backend that reports no compaction data (`pi`, `codex`) --
    /// see `compaction_count` before reading that as "never compacted".
    pub compaction_input_tokens: i64,
    /// RAL-373: count of compactions observed, incremented independently of
    /// whether each one's input size was reported. A nonzero count paired
    /// with `compaction_input_tokens == 0` means "compactions happened,
    /// sizes unreported by this backend/version", not "no compaction
    /// happened".
    pub compaction_count: i64,
    /// Cost in USD.
    pub cost_usd: f64,
    /// `true` when the figures above are the last live mid-run snapshot
    /// rather than the backend's own terminal accounting -- the process was
    /// lost, cancelled, timed out, or killed before a final usage event
    /// arrived, so what got recorded is priced by the runner's approximate
    /// estimate and stops at whatever turn died.
    pub cost_is_estimated: bool,
}

impl From<&crate::runner::RunnerResult> for RecordedUsage {
    fn from(r: &crate::runner::RunnerResult) -> Self {
        Self {
            tokens_in: r.tokens_in,
            tokens_out: r.tokens_out,
            cache_creation_tokens: r.cache_creation_tokens,
            cache_read_tokens: r.cache_read_tokens,
            compaction_input_tokens: r.compaction_input_tokens,
            compaction_count: r.compaction_count,
            cost_usd: r.cost_usd,
            cost_is_estimated: r.cost_is_estimated,
        }
    }
}

/// The recorded outcome of running a cell.
#[derive(Debug, Clone)]
pub struct CellOutcome {
    /// Final cell state.
    pub state: NodeState,
    /// Tokens/cost this run spent.
    pub usage: RecordedUsage,
    /// Error detail, if failed.
    pub error: Option<String>,
    /// Resumable CLI-agent cell/thread id (for `claude --resume`/`codex exec
    /// resume`), if captured.
    pub agent_session_id: Option<String>,
}

// ── Queue view types + helpers (RAL Queue) ───────────────────────────────────

/// A single reorderable unit of runnable work in the Queue view.
#[derive(Debug, Clone, Serialize)]
pub struct QueueItem {
    /// Owning squad id.
    pub squad_id: String,
    /// Owning squad label, if any.
    pub squad_label: Option<String>,
    /// Owning squad state.
    pub squad_state: String,
    /// Owning squad creation time (stable grouping / tie-break).
    pub squad_created_at_ms: i64,
    /// `cell` | `cell_proof` | `task_proof`.
    pub kind: String,
    /// Selector path addressing this item (see [`parse_queue_path`]).
    pub path: String,
    /// Tree depth: squad=0, task=1, cell/task_proof=2, cell_proof=3.
    pub indent: u8,
    /// Owning task index.
    pub task_idx: i64,
    /// Owning task display name.
    pub task_name: String,
    /// Cell index (-1 for task-scope proofs).
    pub cell_idx: i64,
    /// Proof index (-1 for cells).
    pub proof_idx: i64,
    /// `""` | `cell` | `task`.
    pub proof_scope: String,
    /// Display name.
    pub name: String,
    /// Current node state.
    pub state: String,
    /// `ready` | `blocked` | `excluded` | `running`.
    pub readiness: String,
    /// Human labels of the upstreams keeping this item from being ready.
    pub blocked_by: Vec<String>,
    /// The owning task's declared `depends_on` (task names), for display — the
    /// Queue shows this on the task header, mirroring the Tasks view.
    pub task_depends_on: Vec<String>,
    /// This item's own declared dependency references (cell `depends_on`;
    /// empty for proofs). Shown on the row so the link is visible.
    pub depends_on: Vec<String>,
    /// Paths of the queue items this one depends on (for drag-along + reorder).
    pub deps_paths: Vec<String>,
    /// Global priority rank (lower = sooner). `None` = unranked (sorts last).
    pub queue_rank: Option<f64>,
}

/// One row of `queue_items_for_squad`'s proof query:
/// `(task_idx, scope, cell_idx, idx, vid, kind, state, queue_rank)`.
type ProofQueueRow = (
    i64,
    String,
    i64,
    i64,
    Option<String>,
    String,
    String,
    Option<f64>,
);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QueuePathKind {
    Cell,
    CellProof,
    TaskProof,
}

struct ParsedQueuePath {
    squad_id: String,
    kind: QueuePathKind,
    task_idx: i64,
    cell_idx: i64,
    proof_idx: i64,
}

fn cell_path(squad_id: &str, ti: i64, si: i64) -> String {
    format!("{squad_id}/t{ti}/s{si}")
}
fn sproof_path(squad_id: &str, ti: i64, si: i64, vi: i64) -> String {
    format!("{squad_id}/t{ti}/s{si}/v{vi}")
}
fn tproof_path(squad_id: &str, ti: i64, vi: i64) -> String {
    format!("{squad_id}/t{ti}/tv{vi}")
}

/// Parse a queue item path. Grammar (the squad id itself never contains `/`):
///  - `<squad>/t<ti>/s<si>`       → a cell
///  - `<squad>/t<ti>/s<si>/v<vi>` → a cell-scope proof
///  - `<squad>/t<ti>/tv<vi>`      → a task-scope proof
fn parse_queue_path(path: &str) -> Option<ParsedQueuePath> {
    let segs: Vec<&str> = path.split('/').collect();
    match segs.as_slice() {
        [squad, t, s] => {
            let ti = t.strip_prefix('t')?.parse().ok()?;
            if let Some(si) = s.strip_prefix('s').and_then(|v| v.parse().ok()) {
                Some(ParsedQueuePath {
                    squad_id: (*squad).to_string(),
                    kind: QueuePathKind::Cell,
                    task_idx: ti,
                    cell_idx: si,
                    proof_idx: -1,
                })
            } else {
                s.strip_prefix("tv")
                    .and_then(|v| v.parse().ok())
                    .map(|vi| ParsedQueuePath {
                        squad_id: (*squad).to_string(),
                        kind: QueuePathKind::TaskProof,
                        task_idx: ti,
                        cell_idx: -1,
                        proof_idx: vi,
                    })
            }
        }
        [squad, t, s, v] => {
            let ti = t.strip_prefix('t')?.parse().ok()?;
            let si = s.strip_prefix('s')?.parse().ok()?;
            let vi = v.strip_prefix('v')?.parse().ok()?;
            Some(ParsedQueuePath {
                squad_id: (*squad).to_string(),
                kind: QueuePathKind::CellProof,
                task_idx: ti,
                cell_idx: si,
                proof_idx: vi,
            })
        }
        _ => None,
    }
}

/// Classify an item's readiness from its state + dependency analysis.
fn classify(state: &str, excluded: bool, ready: bool) -> String {
    if state == "running" {
        "running".to_string()
    } else if excluded {
        "excluded".to_string()
    } else if ready {
        "ready".to_string()
    } else {
        "blocked".to_string()
    }
}

/// Canonical sort key: ranked items first (ascending), then unranked by squad
/// creation, task, cells-before-proofs, and index.
fn queue_sort_key(i: &QueueItem) -> (f64, i64, i64, i64, i64, i64) {
    let (a, b, c) = match i.kind.as_str() {
        "cell" => (i.cell_idx, 0, 0),
        "cell_proof" => (i.cell_idx, 1, i.proof_idx),
        _ => (i64::MAX, 0, i.proof_idx), // task_proof sorts after its cells
    };
    (
        i.queue_rank.unwrap_or(f64::INFINITY),
        i.squad_created_at_ms,
        i.task_idx,
        a,
        b,
        c,
    )
}

/// Stabilize a desired order against item dependencies: a Kahn topological sort
/// whose tie-break is the desired position, so the result respects every
/// dependency edge while staying as close as possible to what the user asked
/// for. Items absent from `desired` fall to the end in their original order.
fn stabilize_order(desired: &[String], items: &[(String, Vec<String>)]) -> Vec<String> {
    use std::collections::{BTreeSet, HashMap};
    let n = items.len();
    let idx_of: HashMap<&str, usize> = items
        .iter()
        .enumerate()
        .map(|(i, (p, _))| (p.as_str(), i))
        .collect();
    let mut desired_rank: HashMap<usize, usize> = HashMap::new();
    for (r, p) in desired.iter().enumerate() {
        if let Some(&i) = idx_of.get(p.as_str()) {
            desired_rank.entry(i).or_insert(r);
        }
    }
    let rank = |i: usize| desired_rank.get(&i).copied().unwrap_or(desired.len() + i);

    let mut indeg = vec![0usize; n];
    let mut children: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (i, (_, deps)) in items.iter().enumerate() {
        for d in deps {
            if let Some(&di) = idx_of.get(d.as_str()) {
                children[di].push(i);
                indeg[i] += 1;
            }
        }
    }
    let mut ready: BTreeSet<(usize, usize)> = BTreeSet::new();
    for (i, &deg) in indeg.iter().enumerate() {
        if deg == 0 {
            ready.insert((rank(i), i));
        }
    }
    let mut out: Vec<String> = Vec::with_capacity(n);
    while let Some(&(_, node)) = ready.iter().next() {
        ready.remove(&(rank(node), node));
        out.push(items[node].0.clone());
        for &c in &children[node] {
            indeg[c] -= 1;
            if indeg[c] == 0 {
                ready.insert((rank(c), c));
            }
        }
    }
    // Cycle fallback (plan() already rejects cycles, so this is defensive).
    if out.len() < n {
        for (p, _) in items {
            if !out.contains(p) {
                out.push(p.clone());
            }
        }
    }
    out
}

/// Move `selected` (in caller order) to a new position within `current`.
/// `absolute` clamps to `[0, len]`; otherwise moves up by `position` places
/// (negative moves down).
fn move_block(
    current: &[String],
    selected: &[String],
    position: i64,
    absolute: bool,
) -> Vec<String> {
    use std::collections::HashSet;
    let sel_set: HashSet<&str> = selected.iter().map(String::as_str).collect();
    let present: HashSet<&str> = current.iter().map(String::as_str).collect();
    let remaining: Vec<String> = current
        .iter()
        .filter(|p| !sel_set.contains(p.as_str()))
        .cloned()
        .collect();
    let block: Vec<String> = selected
        .iter()
        .filter(|p| present.contains(p.as_str()))
        .cloned()
        .collect();
    let insert_at = if absolute {
        usize::try_from(position.max(0))
            .unwrap_or(0)
            .min(remaining.len())
    } else {
        let first = current
            .iter()
            .position(|p| sel_set.contains(p.as_str()))
            .and_then(|f| i64::try_from(f).ok())
            .unwrap_or(0);
        usize::try_from((first - position).max(0))
            .unwrap_or(0)
            .min(remaining.len())
    };
    let mut out = Vec::with_capacity(current.len());
    out.extend_from_slice(&remaining[..insert_at]);
    out.extend(block);
    out.extend_from_slice(&remaining[insert_at..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
[[task]]
name = "build"
[[task.cell]]
id = "worker"
cwd = "/repo"
prompt = "make it build"
[[task.cell.proof]]
id = "fmt"
command = "cargo fmt --check"
[[task.proof]]
command = "cargo test"
"#;

    fn parse(src: &str) -> TaskFile {
        toml::from_str(src).expect("valid toml")
    }

    /// `Store::open_in_memory()` always starts from the current schema, so it
    /// never exercises the `RENAME COLUMN claude_session_id TO
    /// agent_session_id` migration (and its siblings) added for Codex
    /// support. This hand-rolls a pre-migration database -- just the four
    /// affected tables, with the old column names and real data in them --
    /// and runs the real `init_schema()` migration path
    /// (`Store::open`/`open_in_memory` both just call this) against it, to
    /// prove an upgrading user's existing cell/resolver ids actually
    /// survive the rename rather than silently becoming `NULL`.
    #[test]
    fn migration_renames_legacy_claude_session_id_columns() {
        let conn = Connection::open_in_memory().expect("open sqlite");
        conn.execute_batch(
            "CREATE TABLE cells (
                squad_id TEXT NOT NULL, task_idx INTEGER NOT NULL, idx INTEGER NOT NULL,
                sid TEXT, agent TEXT NOT NULL DEFAULT 'claude', state TEXT NOT NULL,
                depends_on TEXT NOT NULL DEFAULT '[]', tokens_in INTEGER NOT NULL DEFAULT 0,
                tokens_out INTEGER NOT NULL DEFAULT 0, cost_usd REAL NOT NULL DEFAULT 0,
                claude_session_id TEXT,
                PRIMARY KEY (squad_id, task_idx, idx)
             );
             CREATE TABLE proofs (
                squad_id TEXT NOT NULL, task_idx INTEGER NOT NULL, scope TEXT NOT NULL,
                cell_idx INTEGER NOT NULL, idx INTEGER NOT NULL, vid TEXT,
                kind TEXT NOT NULL, spec TEXT NOT NULL, state TEXT NOT NULL,
                agent TEXT NOT NULL DEFAULT 'claude', claude_session_id TEXT,
                PRIMARY KEY (squad_id, task_idx, scope, cell_idx, idx)
             );
             CREATE TABLE guardians (
                id TEXT PRIMARY KEY, name TEXT NOT NULL, base_branch TEXT NOT NULL,
                manual_commands_claude_session_id TEXT
             );
             CREATE TABLE guardian_branches (
                guardian_id TEXT NOT NULL, position INTEGER NOT NULL, branch TEXT NOT NULL,
                status TEXT NOT NULL DEFAULT 'pending', resolver_claude_session_id TEXT,
                PRIMARY KEY (guardian_id, position)
             );",
        )
        .expect("create legacy (pre-rename) schema");
        conn.execute(
            "INSERT INTO cells (squad_id, task_idx, idx, sid, state, claude_session_id)
             VALUES ('r1', 0, 0, 's0', 'done', 'legacy-cell-id')",
            [],
        )
        .expect("insert legacy cell row");
        conn.execute(
            "INSERT INTO proofs (squad_id, task_idx, scope, cell_idx, idx, kind, spec, state, claude_session_id)
             VALUES ('r1', 0, 'cell', 0, 0, 'command', 'true', 'done', 'legacy-proof-sid')",
            [],
        )
        .expect("insert legacy proof row");
        conn.execute(
            "INSERT INTO guardians (id, name, base_branch, manual_commands_claude_session_id)
             VALUES ('g1', 'g', 'main', 'legacy-manual-sid')",
            [],
        )
        .expect("insert legacy guardian row");
        conn.execute(
            "INSERT INTO guardian_branches (guardian_id, position, branch, resolver_claude_session_id)
             VALUES ('g1', 0, 'feature', 'legacy-resolver-sid')",
            [],
        )
        .expect("insert legacy guardian_branches row");

        let store = Store {
            conn,
            event_bus: crate::events::EventBus::new(),
            live_activity: HashMap::new(),
            guardian_summary_debounce: HashMap::new(),
            guardian_worktree_leases: HashMap::new(),
            guardian_restack_requests: HashMap::new(),
            guardian_restack_running: std::collections::HashSet::new(),
            stall_escalated: HashMap::new(),
            secret_env_names_cache: std::sync::RwLock::new(None),
        };
        store
            .init_schema()
            .expect("migration must succeed against a legacy schema");

        let cell_sid: String = store
            .conn
            .query_row(
                "SELECT agent_session_id FROM cells WHERE squad_id='r1'",
                [],
                |r| r.get(0),
            )
            .expect("agent_session_id column must exist and hold the migrated value");
        assert_eq!(cell_sid, "legacy-cell-id");

        let proof_sid: String = store
            .conn
            .query_row(
                "SELECT agent_session_id FROM proofs WHERE squad_id='r1'",
                [],
                |r| r.get(0),
            )
            .expect("agent_session_id column must exist and hold the migrated value");
        assert_eq!(proof_sid, "legacy-proof-sid");

        let manual_sid: String = store
            .conn
            .query_row(
                "SELECT manual_commands_agent_session_id FROM guardians WHERE id='g1'",
                [],
                |r| r.get(0),
            )
            .expect(
                "manual_commands_agent_session_id column must exist and hold the migrated value",
            );
        assert_eq!(manual_sid, "legacy-manual-sid");

        let resolver_sid: String = store
            .conn
            .query_row(
                "SELECT resolver_agent_session_id FROM guardian_branches WHERE guardian_id='g1'",
                [],
                |r| r.get(0),
            )
            .expect("resolver_agent_session_id column must exist and hold the migrated value");
        assert_eq!(resolver_sid, "legacy-resolver-sid");

        // The old columns must actually be gone (RENAME COLUMN, not a copy),
        // confirming this is a real rename rather than an ADD-COLUMN-and-leave-
        // the-old-one-behind.
        let old_column_still_exists: bool = store
            .conn
            .query_row(
                "SELECT 1 FROM pragma_table_info('cells') WHERE name='claude_session_id'",
                [],
                |_| Ok(()),
            )
            .optional()
            .expect("pragma_table_info query must succeed")
            .is_some();
        assert!(
            !old_column_still_exists,
            "old claude_session_id column should have been renamed away, not left behind"
        );
    }

    /// Same shape as `migration_renames_legacy_claude_session_id_columns`,
    /// but for RAL-364's `follows` -> `watches` copy-and-drop migration and
    /// the paired `auto_follow` -> `auto_watch` column rename: hand-rolls a
    /// pre-migration database with the old table/column names and a real
    /// row in each, runs `init_schema()` against it, and proves the row
    /// (including its id and the `follow_seq` sequence counter) survives
    /// under the new names rather than being stranded in a dropped table.
    #[test]
    fn migration_renames_legacy_follows_table_and_auto_follow_column() {
        let conn = Connection::open_in_memory().expect("open sqlite");
        conn.execute_batch(
            "CREATE TABLE users (
                name TEXT PRIMARY KEY, created_at_ms INTEGER NOT NULL,
                auto_follow INTEGER NOT NULL DEFAULT 0,
                is_admin INTEGER NOT NULL DEFAULT 0
             );
             CREATE TABLE meta (
                key   TEXT PRIMARY KEY,
                value INTEGER NOT NULL
             );
             CREATE TABLE follows (
                id            TEXT PRIMARY KEY,
                user_name     TEXT NOT NULL,
                entity_uri    TEXT NOT NULL,
                notify_tiers  TEXT NOT NULL,
                created_at_ms INTEGER NOT NULL
             );",
        )
        .expect("create legacy (pre-rename) schema");
        conn.execute(
            "INSERT INTO users (name, created_at_ms, auto_follow) VALUES ('colin', 1, 1)",
            [],
        )
        .expect("insert legacy user row");
        conn.execute("INSERT INTO meta (key, value) VALUES ('follow_seq', 1)", [])
            .expect("insert legacy follow_seq counter");
        conn.execute(
            "INSERT INTO follows (id, user_name, entity_uri, notify_tiers, created_at_ms)
             VALUES ('follow-000000000001', 'colin', 'squad:squad-1', 'urgent', 2)",
            [],
        )
        .expect("insert legacy follow row");

        let store = Store {
            conn,
            event_bus: crate::events::EventBus::new(),
            live_activity: HashMap::new(),
            guardian_summary_debounce: HashMap::new(),
            guardian_worktree_leases: HashMap::new(),
            guardian_restack_requests: HashMap::new(),
            guardian_restack_running: std::collections::HashSet::new(),
            stall_escalated: HashMap::new(),
            secret_env_names_cache: std::sync::RwLock::new(None),
        };
        store
            .init_schema()
            .expect("migration must succeed against a legacy schema");

        let (id, entity_uri): (String, String) = store
            .conn
            .query_row(
                "SELECT id, entity_uri FROM watches WHERE user_name='colin'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .expect("watches table must exist and hold the migrated row");
        assert_eq!(id, "watch-000000000001", "row id prefix must be rewritten");
        assert_eq!(entity_uri, "squad:squad-1");

        let watch_seq: i64 = store
            .conn
            .query_row("SELECT value FROM meta WHERE key='watch_seq'", [], |r| {
                r.get(0)
            })
            .expect("watch_seq counter must exist and hold the migrated value");
        assert_eq!(watch_seq, 1);

        let auto_watch: bool = store
            .conn
            .query_row("SELECT auto_watch FROM users WHERE name='colin'", [], |r| {
                r.get(0)
            })
            .expect("auto_watch column must exist and hold the migrated value");
        assert!(auto_watch);

        let old_table_still_exists: bool = store
            .conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name='follows'",
                [],
                |_| Ok(()),
            )
            .optional()
            .expect("sqlite_master query must succeed")
            .is_some();
        assert!(
            !old_table_still_exists,
            "old follows table should have been dropped, not left behind"
        );

        let old_column_still_exists: bool = store
            .conn
            .query_row(
                "SELECT 1 FROM pragma_table_info('users') WHERE name='auto_follow'",
                [],
                |_| Ok(()),
            )
            .optional()
            .expect("pragma_table_info query must succeed")
            .is_some();
        assert!(
            !old_column_still_exists,
            "old auto_follow column should have been renamed away, not left behind"
        );
    }

    #[test]
    fn migration_adds_nullable_task_agent_and_model_columns() {
        let conn = Connection::open_in_memory().expect("open sqlite");
        conn.execute_batch(
            "CREATE TABLE tasks (
                squad_id TEXT NOT NULL,
                idx INTEGER NOT NULL,
                name TEXT NOT NULL,
                project TEXT,
                state TEXT NOT NULL,
                depends_on TEXT NOT NULL DEFAULT '[]',
                queue_rank REAL,
                PRIMARY KEY (squad_id, idx)
             );
             INSERT INTO tasks (squad_id, idx, name, project, state, depends_on, queue_rank)
             VALUES ('r1', 0, 'build', NULL, 'pending', '[]', NULL);",
        )
        .expect("create legacy tasks table");

        let store = Store {
            conn,
            event_bus: crate::events::EventBus::new(),
            live_activity: HashMap::new(),
            guardian_summary_debounce: HashMap::new(),
            guardian_worktree_leases: HashMap::new(),
            guardian_restack_requests: HashMap::new(),
            guardian_restack_running: std::collections::HashSet::new(),
            stall_escalated: HashMap::new(),
            secret_env_names_cache: std::sync::RwLock::new(None),
        };
        store
            .init_schema()
            .expect("migration must add raw task agent/model columns");

        let (agent, model): (Option<String>, Option<String>) = store
            .conn
            .query_row(
                "SELECT agent, model FROM tasks WHERE squad_id='r1' AND idx=0",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .expect("task row must survive migration with new nullable columns");
        assert!(agent.is_none());
        assert!(model.is_none());
    }

    #[test]
    fn migration_adds_nullable_clone_url_column_to_projects() {
        let conn = Connection::open_in_memory().expect("open sqlite");
        conn.execute_batch(
            "CREATE TABLE projects (
                name          TEXT PRIMARY KEY,
                description   TEXT NOT NULL DEFAULT '',
                path          TEXT NOT NULL,
                vcs           TEXT NOT NULL DEFAULT 'git',
                created_at_ms INTEGER NOT NULL,
                skip_base_updates INTEGER
             );
             INSERT INTO projects (name, description, path, vcs, created_at_ms, skip_base_updates)
             VALUES ('legacy-proj', 'pre-RAL-355 project', '/srv/legacy-proj', 'git', 0, NULL);",
        )
        .expect("create legacy projects table (pre-clone_url)");

        let store = Store {
            conn,
            event_bus: crate::events::EventBus::new(),
            live_activity: HashMap::new(),
            guardian_summary_debounce: HashMap::new(),
            guardian_worktree_leases: HashMap::new(),
            guardian_restack_requests: HashMap::new(),
            guardian_restack_running: std::collections::HashSet::new(),
            stall_escalated: HashMap::new(),
            secret_env_names_cache: std::sync::RwLock::new(None),
        };
        store
            .init_schema()
            .expect("migration must add the nullable clone_url column");

        let clone_url: Option<String> = store
            .conn
            .query_row(
                "SELECT clone_url FROM projects WHERE name='legacy-proj'",
                [],
                |r| r.get(0),
            )
            .expect("legacy project row must survive migration with the new column");
        assert!(
            clone_url.is_none(),
            "a pre-existing project row must read back with no clone_url rather than erroring"
        );

        // The row is also usable through the ordinary read path afterward --
        // not just readable via a raw SQL query against the migrated column.
        let project = store
            .get_project("legacy-proj")
            .expect("get_project must succeed on a migrated legacy row")
            .expect("legacy-proj must still be found");
        assert!(project.clone_url.is_none());
        assert_eq!(project.path, "/srv/legacy-proj");
    }

    #[test]
    fn insert_and_fetch_squad() {
        let mut store = Store::open_in_memory().unwrap();
        let id = store
            .insert_squad(&parse(SAMPLE), Some("my squad"), false)
            .unwrap();
        assert_eq!(id, "squad-000000000001");

        let squad = store.get_squad(&id).unwrap();
        assert_eq!(squad.label.as_deref(), Some("my squad"));
        assert_eq!(squad.state, "pending");
        assert_eq!(squad.tasks.len(), 1);
        let task = &squad.tasks[0];
        assert_eq!(task.name, "build");
        // No `project` set in TOML -> falls back to the cwd basename (RAL-141).
        assert_eq!(task.project, "repo");
        assert!(task.agent.is_none());
        assert!(task.model.is_none());
        assert_eq!(task.cells.len(), 1);
        assert_eq!(task.cells[0].id, "worker");
        assert_eq!(task.cells[0].agent, "claude");
        assert_eq!(task.cells[0].state, "pending");
        assert!(
            task.cells[0]
                .system_prompt
                .as_deref()
                .is_some_and(|sp| sp.contains("non-interactive cell")),
            "prompt cells should expose their effective system prompt"
        );
        // cell-level proof is exposed per cell in the board view
        assert_eq!(task.cells[0].proof.len(), 1);
        assert_eq!(task.cells[0].proof[0].id.as_deref(), Some("fmt"));
        assert_eq!(task.cells[0].proof[0].kind, "command");
        assert_eq!(task.cells[0].proof[0].spec, "cargo fmt --check");
        assert!(task.cells[0].proof[0].model.is_none());
        assert!(task.cells[0].proof[0].system_prompt.is_none());
        assert_eq!(task.proof.len(), 1); // task-level proof
        assert_eq!(task.proof[0].kind, "command");
        assert_eq!(task.proof[0].spec, "cargo test");
        assert!(task.proof[0].system_prompt.is_none());
    }

    #[test]
    fn prompt_proofs_expose_effective_system_prompt() {
        let src = r#"
[[task]]
name = "build"
[[task.cell]]
id = "worker"
cwd = "/repo"
prompt = "make it build"
[[task.cell.proof]]
id = "cell-check"
prompt = "confirm formatting"
[[task.proof]]
id = "task-check"
prompt = "confirm tests"
"#;
        let mut store = Store::open_in_memory().unwrap();
        let id = store
            .insert_squad(&parse(src), Some("prompt proof"), false)
            .unwrap();

        let squad = store.get_squad(&id).unwrap();
        let cell_proof = &squad.tasks[0].cells[0].proof[0];
        let task_proof = &squad.tasks[0].proof[0];
        assert!(
            cell_proof
                .system_prompt
                .as_deref()
                .is_some_and(|sp| sp.contains("PROOF step"))
        );
        assert!(
            task_proof
                .system_prompt
                .as_deref()
                .is_some_and(|sp| sp.contains("PROOF step"))
        );
    }

    #[test]
    fn cell_view_keeps_authored_system_prompt_text() {
        let src = r#"
[[task]]
name = "build"
[[task.cell]]
id = "worker"
cwd = "/repo"
prompt = "make it build"
system_prompt = "Do NOT commit and do NOT push under any circumstances."
system_prompt_position = "append"
"#;
        let mut store = Store::open_in_memory().unwrap();
        let id = store
            .insert_squad(&parse(src), Some("system prompt"), false)
            .unwrap();

        let squad = store.get_squad(&id).unwrap();
        let cell = &squad.tasks[0].cells[0];
        let effective = cell.system_prompt.as_deref().unwrap_or("");
        assert!(
            effective.contains("Do NOT commit and do NOT push under any circumstances."),
            "details pane payload should keep the authored cell system prompt text"
        );
        assert!(
            effective.contains("non-interactive cell"),
            "details pane payload should still include ralphus-added unattended instructions"
        );
    }

    #[test]
    fn cell_resolves_maximum_context_and_auto_compact_threshold_from_task() {
        let src = r#"
[[task]]
name = "build"
agent = "claude-code"
maximum_context = 100000
auto_compact_threshold = 80000
[[task.cell]]
id = "inherits"
cwd = "/repo"
prompt = "go"
[[task.cell]]
id = "overrides"
cwd = "/repo"
prompt = "go"
maximum_context = 50000
auto_compact_threshold = 40000
"#;
        let mut store = Store::open_in_memory().unwrap();
        let id = store.insert_squad(&parse(src), None, false).unwrap();
        let cells = store.cells_of(&id).unwrap();

        let inherits = cells.iter().find(|c| c.cell_id == "inherits").unwrap();
        assert_eq!(inherits.maximum_context, Some(100_000));
        assert_eq!(inherits.auto_compact_threshold, Some(80_000));

        let overrides = cells.iter().find(|c| c.cell_id == "overrides").unwrap();
        assert_eq!(overrides.maximum_context, Some(50_000));
        assert_eq!(overrides.auto_compact_threshold, Some(40_000));
    }

    #[test]
    fn squad_view_carries_resolved_maximum_context_and_auto_compact_threshold() {
        // The board's own `CellView` (what `GET /api/squads/{id}` and the
        // details pane actually see) is built from a separate query
        // (`cells_by_task`) than the scheduler-facing `CellRow` the test
        // above exercises -- this proves the same resolved values reach the
        // board payload too, not just the scheduler.
        let src = r#"
[[task]]
name = "build"
agent = "claude-code"
auto_compact_threshold = 80000
[[task.cell]]
id = "inherits"
cwd = "/repo"
prompt = "go"
"#;
        let mut store = Store::open_in_memory().unwrap();
        let id = store.insert_squad(&parse(src), None, false).unwrap();

        let squad = store.get_squad(&id).unwrap();
        let cell = &squad.tasks[0].cells[0];
        assert_eq!(cell.auto_compact_threshold, Some(80_000));
        assert_eq!(cell.maximum_context, None);
    }

    #[test]
    fn cell_maximum_context_defaults_to_none() {
        let src = r#"
[[task]]
name = "build"
[[task.cell]]
id = "worker"
cwd = "/repo"
prompt = "go"
"#;
        let mut store = Store::open_in_memory().unwrap();
        let id = store.insert_squad(&parse(src), None, false).unwrap();
        let cells = store.cells_of(&id).unwrap();
        assert_eq!(cells[0].maximum_context, None);
        assert_eq!(cells[0].auto_compact_threshold, None);
    }

    #[test]
    fn explicit_project_wins_over_cwd_fallback() {
        let src = r#"
[[task]]
name = "build"
project = "myrepo"
[[task.cell]]
cwd = "/some/other/path"
prompt = "go"
"#;
        let mut store = Store::open_in_memory().unwrap();
        let id = store.insert_squad(&parse(src), None, false).unwrap();
        let squad = store.get_squad(&id).unwrap();
        assert_eq!(squad.tasks[0].project, "myrepo");
    }

    #[test]
    fn task_view_preserves_raw_task_agent_and_model() {
        let src = r#"
[[task]]
name = "build"
agent = "codex"
model = "gpt-5-codex"
[[task.cell]]
id = "worker"
cwd = "/repo"
prompt = "go"
"#;
        let mut store = Store::open_in_memory().unwrap();
        let id = store.insert_squad(&parse(src), None, false).unwrap();
        let squad = store.get_squad(&id).unwrap();
        let task = &squad.tasks[0];
        assert_eq!(task.agent.as_deref(), Some("codex"));
        assert_eq!(task.model.as_deref(), Some("gpt-5-codex"));
        assert_eq!(task.cells[0].agent, "codex");
        assert_eq!(task.cells[0].model.as_deref(), Some("gpt-5-codex"));
    }

    #[test]
    fn task_with_no_cells_falls_back_to_unassigned() {
        let src = r#"
[[task]]
name = "empty"
"#;
        let mut store = Store::open_in_memory().unwrap();
        let id = store.insert_squad(&parse(src), None, false).unwrap();
        let squad = store.get_squad(&id).unwrap();
        assert_eq!(squad.tasks[0].project, "unassigned");
    }

    #[test]
    fn fallback_project_identifier_covers_edge_cases() {
        assert_eq!(fallback_project_identifier(Some("/repo")), "repo");
        assert_eq!(
            fallback_project_identifier(Some("C:/Users/me/repo")),
            "repo"
        );
        assert_eq!(fallback_project_identifier(Some("/")), "unassigned");
        assert_eq!(fallback_project_identifier(None), "unassigned");
    }

    #[test]
    fn task_name_at_resolves_index_to_name() {
        let mut store = Store::open_in_memory().unwrap();
        let id = store.insert_squad(&parse(SAMPLE), None, false).unwrap();
        assert_eq!(
            store.task_name_at(&id, 0).unwrap().as_deref(),
            Some("build")
        );
        assert_eq!(store.task_name_at(&id, 99).unwrap(), None);
        assert_eq!(store.task_name_at("nope", 0).unwrap(), None);
    }

    #[test]
    fn cell_sid_at_resolves_index_to_sid() {
        let mut store = Store::open_in_memory().unwrap();
        let id = store.insert_squad(&parse(SAMPLE), None, false).unwrap();
        assert_eq!(
            store.cell_sid_at(&id, 0, 0).unwrap().as_deref(),
            Some("worker")
        );
        assert_eq!(store.cell_sid_at(&id, 0, 99).unwrap(), None);
    }

    #[test]
    fn squad_ids_increment() {
        let mut store = Store::open_in_memory().unwrap();
        let a = store.insert_squad(&parse(SAMPLE), None, false).unwrap();
        let b = store.insert_squad(&parse(SAMPLE), None, false).unwrap();
        assert_eq!(a, "squad-000000000001");
        assert_eq!(b, "squad-000000000002");
    }

    #[test]
    fn hold_submits_as_queued_then_activates() {
        let mut store = Store::open_in_memory().unwrap();
        let id = store.insert_squad(&parse(SAMPLE), None, true).unwrap();
        assert_eq!(store.squad_state(&id).unwrap(), SquadState::Queued);
        assert!(store.list_ready().unwrap().is_empty());

        assert_eq!(store.activate(&id).unwrap(), SquadState::Pending);
        assert_eq!(store.list_ready().unwrap(), vec![id]);
    }

    #[test]
    fn default_submit_is_pending_and_ready() {
        // The old-project fix: submitting makes a squad schedulable, not stuck.
        let mut store = Store::open_in_memory().unwrap();
        let id = store.insert_squad(&parse(SAMPLE), None, false).unwrap();
        assert_eq!(store.squad_state(&id).unwrap(), SquadState::Pending);
        assert_eq!(store.list_ready().unwrap(), vec![id]);
    }

    #[test]
    fn cannot_activate_a_pending_squad() {
        let mut store = Store::open_in_memory().unwrap();
        let id = store.insert_squad(&parse(SAMPLE), None, false).unwrap();
        assert!(store.activate(&id).is_err());
    }

    #[test]
    fn cancel_sets_cancelled() {
        let mut store = Store::open_in_memory().unwrap();
        let id = store.insert_squad(&parse(SAMPLE), None, false).unwrap();
        assert_eq!(store.cancel(&id).unwrap(), SquadState::Cancelled);
        // Idempotent — cancelling an already-terminal squad still succeeds
        // (RAL-116), so it stays locked out of ever being picked up again.
        assert_eq!(store.cancel(&id).unwrap(), SquadState::Cancelled);
    }

    #[test]
    fn cancel_is_available_from_a_terminal_done_state() {
        let mut store = Store::open_in_memory().unwrap();
        let id = store.insert_squad(&parse(SAMPLE), None, false).unwrap();
        store.set_squad_state(&id, SquadState::Done).unwrap();
        assert_eq!(store.cancel(&id).unwrap(), SquadState::Cancelled);
        assert_eq!(store.squad_state(&id).unwrap(), SquadState::Cancelled);
    }

    #[test]
    fn cancel_flips_unfinished_nodes_but_preserves_succeeded_ones() {
        let mut store = Store::open_in_memory().unwrap();
        let id = store.insert_squad(&parse(SAMPLE), None, false).unwrap();
        store.set_squad_state(&id, SquadState::Running).unwrap();
        // One cell already finished; the task is still running.
        store.set_cell_state(&id, 0, 0, NodeState::Done).unwrap();
        store.set_task_state(&id, 0, NodeState::Running).unwrap();

        store.cancel(&id).unwrap();
        let squad = store.get_squad(&id).unwrap();
        assert_eq!(squad.state, "cancelled");
        // The still-running task flips to cancelled…
        assert_eq!(squad.tasks[0].state, "cancelled");
        // …but the cell that already completed keeps its real outcome.
        assert_eq!(squad.tasks[0].cells[0].state, "done");
    }

    #[test]
    fn cancel_flips_already_failed_tasks_cells_and_proofs() {
        let mut store = Store::open_in_memory().unwrap();
        let id = store.insert_squad(&parse(SAMPLE), None, false).unwrap();
        // The squad already burned down to failure before the user hit cancel:
        // the task and its cell are terminal-failed, the proof never ran.
        store.set_squad_state(&id, SquadState::Failed).unwrap();
        store.set_task_state(&id, 0, NodeState::Failed).unwrap();
        store.set_cell_state(&id, 0, 0, NodeState::Failed).unwrap();

        store.cancel(&id).unwrap();
        let squad = store.get_squad(&id).unwrap();
        assert_eq!(squad.state, "cancelled");
        assert_eq!(squad.tasks[0].state, "cancelled");
        assert_eq!(squad.tasks[0].cells[0].state, "cancelled");
    }

    #[test]
    fn cancel_preserves_ignored_nodes() {
        let mut store = Store::open_in_memory().unwrap();
        let id = store.insert_squad(&parse(SAMPLE), None, false).unwrap();
        store.set_cell_state(&id, 0, 0, NodeState::Ignored).unwrap();

        store.cancel(&id).unwrap();
        let squad = store.get_squad(&id).unwrap();
        assert_eq!(squad.tasks[0].cells[0].state, "ignored");
    }

    // RAL-157: two independent tasks, for solo/unsolo tests.
    const TWO_TASKS: &str = r#"
[[task]]
name = "a"
[[task.cell]]
cwd = "."
command = "build a"
[[task]]
name = "b"
[[task.cell]]
cwd = "."
command = "build b"
"#;

    #[test]
    fn solo_task_round_trips_and_is_visible_on_the_squad_view() {
        let mut store = Store::open_in_memory().unwrap();
        let id = store.insert_squad(&parse(TWO_TASKS), None, false).unwrap();

        let squad = store.get_squad(&id).unwrap();
        assert!(!squad.tasks[0].soloed, "not soloed by default");
        assert!(!squad.tasks[1].soloed);

        store.solo_task(&id, 0).unwrap();
        let squad = store.get_squad(&id).unwrap();
        assert!(squad.tasks[0].soloed);
        assert!(
            !squad.tasks[1].soloed,
            "soloing one task doesn't solo others"
        );

        store.unsolo_task(&id, 0).unwrap();
        let squad = store.get_squad(&id).unwrap();
        assert!(!squad.tasks[0].soloed);
    }

    #[test]
    fn solo_task_is_idempotent() {
        let mut store = Store::open_in_memory().unwrap();
        let id = store.insert_squad(&parse(TWO_TASKS), None, false).unwrap();
        store.solo_task(&id, 0).unwrap();
        store.solo_task(&id, 0).unwrap();
        assert!(store.get_squad(&id).unwrap().tasks[0].soloed);
        store.unsolo_task(&id, 0).unwrap();
        store.unsolo_task(&id, 0).unwrap();
        assert!(!store.get_squad(&id).unwrap().tasks[0].soloed);
    }

    #[test]
    fn solo_task_unknown_task_index_is_not_found() {
        let mut store = Store::open_in_memory().unwrap();
        let id = store.insert_squad(&parse(TWO_TASKS), None, false).unwrap();
        assert!(matches!(
            store.solo_task(&id, 99),
            Err(StoreError::NotFound)
        ));
    }

    #[test]
    fn multiple_tasks_can_be_soloed_at_once_with_no_auto_exclusivity() {
        let mut store = Store::open_in_memory().unwrap();
        let id = store.insert_squad(&parse(TWO_TASKS), None, false).unwrap();

        store.solo_task(&id, 0).unwrap();
        assert_eq!(store.soloed_task_indices(&id).unwrap(), [0].into());

        // Soloing a second task doesn't un-solo the first (RAL-157 Q4).
        store.solo_task(&id, 1).unwrap();
        assert_eq!(store.soloed_task_indices(&id).unwrap(), [0, 1].into());

        store.unsolo_task(&id, 0).unwrap();
        assert_eq!(store.soloed_task_indices(&id).unwrap(), [1].into());
    }

    #[test]
    fn cancel_squad_dry_run_reports_impact_without_mutating() {
        let mut store = Store::open_in_memory().unwrap();
        let (a, b, c) = dependent_chain(&mut store); // a <- b <- c

        let impact = store.cancel_squad(&a, true).unwrap();
        let ids: Vec<&str> = impact.squads.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, vec![a.as_str(), b.as_str(), c.as_str()]);

        // Nothing was actually mutated.
        assert_eq!(store.squad_state(&a).unwrap(), SquadState::Pending);
        assert_eq!(store.squad_state(&b).unwrap(), SquadState::Pending);
        assert_eq!(store.squad_state(&c).unwrap(), SquadState::Pending);
    }

    #[test]
    fn cancel_squad_cascades_to_downstream_dependents() {
        let mut store = Store::open_in_memory().unwrap();
        let (a, b, c) = dependent_chain(&mut store); // a <- b <- c

        let impact = store.cancel_squad(&a, false).unwrap();
        let ids: Vec<&str> = impact.squads.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, vec![a.as_str(), b.as_str(), c.as_str()]);

        assert_eq!(store.squad_state(&a).unwrap(), SquadState::Cancelled);
        assert_eq!(store.squad_state(&b).unwrap(), SquadState::Cancelled);
        assert_eq!(store.squad_state(&c).unwrap(), SquadState::Cancelled);
    }

    #[test]
    fn cancel_squad_cascades_even_to_already_terminal_dependents() {
        let mut store = Store::open_in_memory().unwrap();
        let (a, b, _c) = dependent_chain(&mut store); // a <- b <- c
        store.set_squad_state(&b, SquadState::Done).unwrap();

        store.cancel_squad(&a, false).unwrap();
        // Terminal or not, a squad that depends (even transitively) on a
        // cancelled squad is locked out of ever being picked up again.
        assert_eq!(store.squad_state(&b).unwrap(), SquadState::Cancelled);
    }

    #[test]
    fn cancel_squad_missing_is_not_found() {
        let store = Store::open_in_memory().unwrap();
        assert!(matches!(
            store.cancel_squad("nope", true),
            Err(StoreError::NotFound)
        ));
        assert!(matches!(
            store.cancel_squad("nope", false),
            Err(StoreError::NotFound)
        ));
    }

    #[test]
    fn cell_shows_running_while_its_own_proof_is_still_in_flight() {
        // Regression: the board must not show a cell as "done" while one of
        // its own cell-level proof steps is still pending/running — even
        // though the persisted `cells.state` column is (by design, RAL-64)
        // already "done" at that point.
        let mut store = Store::open_in_memory().unwrap();
        let id = store
            .insert_squad(&parse(THREE_CELL_PROOFS), None, false)
            .unwrap();
        store.set_cell_state(&id, 0, 0, NodeState::Done).unwrap();
        store
            .set_proof_state(&id, 0, "cell", 0, 0, NodeState::Done)
            .unwrap();
        store
            .set_proof_state(&id, 0, "cell", 0, 1, NodeState::Running)
            .unwrap();
        // Index 2 stays "pending".

        let squad = store.get_squad(&id).unwrap();
        assert_eq!(
            squad.tasks[0].cells[0].state, "running",
            "cell must read as running while a proof step is still in flight"
        );

        // Once the failing/last proof fails, the cell should read as failed.
        store
            .set_proof_state(&id, 0, "cell", 0, 1, NodeState::Failed)
            .unwrap();
        store
            .set_proof_state(&id, 0, "cell", 0, 2, NodeState::Cancelled)
            .unwrap();
        let squad = store.get_squad(&id).unwrap();
        assert_eq!(
            squad.tasks[0].cells[0].state, "failed",
            "cell must read as failed when one of its proofs failed"
        );
    }

    #[test]
    fn cell_reads_done_when_a_proof_step_is_ignored_not_stuck_running() {
        // Regression: an `ignored` proof step (a user-set skip/pass-through,
        // per the scheduler's `continue`-on-ignored handling) must count as a
        // terminal state for the cell rollup, same as done/failed/cancelled
        // — otherwise a cell with an ignored proof step reads "running"
        // forever even after the squad has actually finished.
        let mut store = Store::open_in_memory().unwrap();
        let id = store
            .insert_squad(&parse(THREE_CELL_PROOFS), None, false)
            .unwrap();
        store.set_cell_state(&id, 0, 0, NodeState::Done).unwrap();
        store
            .set_proof_state(&id, 0, "cell", 0, 0, NodeState::Done)
            .unwrap();
        store
            .set_proof_state(&id, 0, "cell", 0, 1, NodeState::Ignored)
            .unwrap();
        store
            .set_proof_state(&id, 0, "cell", 0, 2, NodeState::Done)
            .unwrap();

        let squad = store.get_squad(&id).unwrap();
        assert_eq!(
            squad.tasks[0].cells[0].state, "done",
            "an ignored proof step must not keep the cell stuck at 'running'"
        );
    }

    #[test]
    fn cell_and_task_state_transitions() {
        let mut store = Store::open_in_memory().unwrap();
        let id = store.insert_squad(&parse(SAMPLE), None, false).unwrap();
        store.set_squad_state(&id, SquadState::Running).unwrap();
        store.set_cell_state(&id, 0, 0, NodeState::Done).unwrap();
        // SAMPLE's cell has its own "fmt" proof — mark it done too so the
        // displayed cell state (which folds proof progress back in) reads
        // as fully done, not "running" on a still-pending proof.
        store
            .set_proof_state(&id, 0, "cell", 0, 0, NodeState::Done)
            .unwrap();
        store.set_task_state(&id, 0, NodeState::Done).unwrap();
        assert_eq!(store.running_count().unwrap(), 1);

        let squad = store.get_squad(&id).unwrap();
        assert_eq!(squad.state, "running");
        assert_eq!(squad.tasks[0].state, "done");
        assert_eq!(squad.tasks[0].cells[0].state, "done");
    }

    #[test]
    fn missing_squad_is_not_found() {
        let store = Store::open_in_memory().unwrap();
        assert!(matches!(store.get_squad("nope"), Err(StoreError::NotFound)));
    }

    #[test]
    fn recover_orphaned_squads_resets_running_but_keeps_done() {
        let two = r#"
[[task]]
name = "t"
[[task.cell]]
id = "a"
cwd = "/repo"
command = "x"
[[task.cell]]
id = "b"
cwd = "/repo"
command = "y"
"#;
        let mut store = Store::open_in_memory().unwrap();
        let id = store.insert_squad(&parse(two), None, false).unwrap();
        // Simulate an unclean shutdown mid-run: one cell finished, one was
        // still executing when the process died.
        store.set_squad_state(&id, SquadState::Running).unwrap();
        store.set_task_state(&id, 0, NodeState::Running).unwrap();
        store.set_cell_state(&id, 0, 0, NodeState::Done).unwrap();
        store.set_cell_state(&id, 0, 1, NodeState::Running).unwrap();
        assert_eq!(store.running_cell_count().unwrap(), 1);

        let recovered = store.recover_orphaned_squads().unwrap();
        assert_eq!(recovered, vec![id.clone()]);

        let squad = store.get_squad(&id).unwrap();
        assert_eq!(squad.state, "pending"); // re-claimable by the scheduler
        assert_eq!(squad.tasks[0].state, "pending");
        // Finished work is preserved (skipped on resume); orphaned work resets.
        assert_eq!(squad.tasks[0].cells[0].state, "done");
        assert_eq!(squad.tasks[0].cells[1].state, "pending");
        assert_eq!(store.running_cell_count().unwrap(), 0);
    }

    #[test]
    fn cross_squad_dependency_gates_readiness() {
        let mut store = Store::open_in_memory().unwrap();
        let a = store
            .insert_squad(&parse(SAMPLE), Some("a"), false)
            .unwrap();
        // Squad b depends on squad a via a [[default]] depends_on.
        let dep_toml = format!(
            "[[default]]\ndepends_on = [\"{a}\"]\n[[task]]\nname=\"b\"\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\n"
        );
        let b = store
            .insert_squad(&parse(&dep_toml), Some("b"), false)
            .unwrap();

        // Only `a` is ready while `a` is still pending.
        let ready = store.list_ready().unwrap();
        assert!(ready.contains(&a));
        assert!(!ready.contains(&b));

        // Once `a` is Done, `b` becomes ready.
        store.set_squad_state(&a, SquadState::Done).unwrap();
        let ready = store.list_ready().unwrap();
        assert!(ready.contains(&b));
    }

    #[test]
    fn add_squad_dependency_appends_and_gates_readiness() {
        let mut store = Store::open_in_memory().unwrap();
        let a = store
            .insert_squad(&parse(SAMPLE), Some("a"), false)
            .unwrap();
        let b = store
            .insert_squad(&parse(SAMPLE), Some("b"), false)
            .unwrap();

        assert!(store.list_ready().unwrap().contains(&b));
        let deps = store.add_squad_dependency(&b, &a).unwrap();
        assert_eq!(deps, vec![a.clone()]);
        assert!(!store.list_ready().unwrap().contains(&b));

        store.set_squad_state(&a, SquadState::Done).unwrap();
        assert!(store.list_ready().unwrap().contains(&b));
    }

    #[test]
    fn add_squad_dependency_is_idempotent() {
        let mut store = Store::open_in_memory().unwrap();
        let a = store
            .insert_squad(&parse(SAMPLE), Some("a"), false)
            .unwrap();
        let b = store
            .insert_squad(&parse(SAMPLE), Some("b"), false)
            .unwrap();
        store.add_squad_dependency(&b, &a).unwrap();
        let deps = store.add_squad_dependency(&b, &a).unwrap();
        assert_eq!(deps, vec![a]);
    }

    #[test]
    fn add_squad_dependency_rejects_self_reference() {
        let mut store = Store::open_in_memory().unwrap();
        let a = store
            .insert_squad(&parse(SAMPLE), Some("a"), false)
            .unwrap();
        assert!(matches!(
            store.add_squad_dependency(&a, &a),
            Err(StoreError::InvalidTransition(_))
        ));
    }

    #[test]
    fn add_squad_dependency_rejects_direct_cycle() {
        let mut store = Store::open_in_memory().unwrap();
        let a = store
            .insert_squad(&parse(SAMPLE), Some("a"), false)
            .unwrap();
        let b = store
            .insert_squad(&parse(SAMPLE), Some("b"), false)
            .unwrap();
        store.add_squad_dependency(&b, &a).unwrap(); // b depends on a
        assert!(matches!(
            store.add_squad_dependency(&a, &b), // a depends on b -> cycle
            Err(StoreError::InvalidTransition(_))
        ));
    }

    #[test]
    fn add_squad_dependency_rejects_transitive_cycle() {
        let mut store = Store::open_in_memory().unwrap();
        let (a, _b, c) = dependent_chain(&mut store); // a <- b <- c
        // c already (transitively) depends on a; making a depend on c is a cycle.
        assert!(matches!(
            store.add_squad_dependency(&a, &c),
            Err(StoreError::InvalidTransition(_))
        ));
    }

    #[test]
    fn add_squad_dependency_missing_squad_or_target_is_not_found() {
        let mut store = Store::open_in_memory().unwrap();
        let a = store
            .insert_squad(&parse(SAMPLE), Some("a"), false)
            .unwrap();
        assert!(matches!(
            store.add_squad_dependency("nope", &a),
            Err(StoreError::NotFound)
        ));
        assert!(matches!(
            store.add_squad_dependency(&a, "nope"),
            Err(StoreError::NotFound)
        ));
    }

    // RAL-19: helper to build a chain of squads a <- b <- c (each depends on the
    // prior via a [[default]] depends_on) and return their ids.
    fn dependent_chain(store: &mut Store) -> (String, String, String) {
        let a = store
            .insert_squad(&parse(SAMPLE), Some("a"), false)
            .unwrap();
        let dep_b = format!(
            "[[default]]\ndepends_on = [\"{a}\"]\n[[task]]\nname=\"b\"\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\n"
        );
        let b = store
            .insert_squad(&parse(&dep_b), Some("b"), false)
            .unwrap();
        let dep_c = format!(
            "[[default]]\ndepends_on = [\"{b}\"]\n[[task]]\nname=\"c\"\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\n"
        );
        let c = store
            .insert_squad(&parse(&dep_c), Some("c"), false)
            .unwrap();
        (a, b, c)
    }

    #[test]
    fn restarting_a_squad_dirties_all_downstream_squads() {
        let mut store = Store::open_in_memory().unwrap();
        let (a, b, c) = dependent_chain(&mut store);
        // Drive the whole chain to Done.
        for id in [&a, &b, &c] {
            store.set_squad_state(id, SquadState::Done).unwrap();
        }
        // Restart A: it and both downstream squads must be dirty (Pending) again.
        let dirtied = store.restart_squad(&a).unwrap();
        assert!(
            dirtied.contains(&b) && dirtied.contains(&c),
            "cascade to b and c"
        );
        assert_eq!(store.squad_state(&a).unwrap(), SquadState::Pending);
        assert_eq!(store.squad_state(&b).unwrap(), SquadState::Pending);
        assert_eq!(store.squad_state(&c).unwrap(), SquadState::Pending);
    }

    #[test]
    fn restarting_a_cell_dirties_downstream_squads_too() {
        let mut store = Store::open_in_memory().unwrap();
        let (a, b, _c) = dependent_chain(&mut store);
        for id in [&a, &b] {
            store.set_squad_state(id, SquadState::Done).unwrap();
        }
        // Restart A's single cell (task 0, cell 0): A goes Pending and the
        // downstream squad B is dirtied.
        let dirtied = store.restart_cell(&a, 0, 0).unwrap();
        assert!(dirtied.contains(&b));
        assert_eq!(store.squad_state(&a).unwrap(), SquadState::Pending);
        assert_eq!(store.squad_state(&b).unwrap(), SquadState::Pending);
    }

    #[test]
    fn squad_restart_preview_matches_the_real_restart_without_mutating() {
        // RAL-104: the dry-run preview must report exactly what the real
        // restart would dirty, and must not touch any state itself.
        let mut store = Store::open_in_memory().unwrap();
        let (a, b, c) = dependent_chain(&mut store);
        for id in [&a, &b, &c] {
            store.set_squad_state(id, SquadState::Done).unwrap();
        }

        let preview = store.compute_squad_restart_impact(&a).unwrap();
        let dirtied_ids: Vec<&str> = preview
            .dirtied_squads
            .iter()
            .map(|r| r.id.as_str())
            .collect();
        assert!(dirtied_ids.contains(&b.as_str()) && dirtied_ids.contains(&c.as_str()));
        assert_eq!(preview.cells.len(), 1, "a has one cell");
        assert_eq!(preview.tasks.len(), 1, "a has one task");

        // Nothing was mutated by computing the preview.
        assert_eq!(store.squad_state(&a).unwrap(), SquadState::Done);
        assert_eq!(store.squad_state(&b).unwrap(), SquadState::Done);
        assert_eq!(store.squad_state(&c).unwrap(), SquadState::Done);

        // The real restart dirties exactly the same set the preview reported.
        let dirtied = store.restart_squad(&a).unwrap();
        assert_eq!(dirtied.len(), preview.dirtied_squads.len());
        for id in &dirtied {
            assert!(dirtied_ids.contains(&id.as_str()));
        }
        assert_eq!(store.squad_state(&a).unwrap(), SquadState::Pending);
        assert_eq!(store.squad_state(&b).unwrap(), SquadState::Pending);
        assert_eq!(store.squad_state(&c).unwrap(), SquadState::Pending);
    }

    #[test]
    fn cell_restart_preview_matches_the_real_restart_without_mutating() {
        // RAL-104: same guarantee as the squad-level preview, for a single
        // cell restart with an in-squad downstream chain plus a dependent squad.
        let mut store = Store::open_in_memory().unwrap();
        let toml = "[[task]]\nname=\"t\"\n\
            [[task.cell]]\nid=\"s1\"\ncwd=\"/r\"\nprompt=\"p\"\n\
            [[task.cell]]\nid=\"s2\"\ncwd=\"/r\"\nprompt=\"p\"\ndepends_on=[\"s1\"]\n\
            [[task.cell]]\nid=\"s3\"\ncwd=\"/r\"\nprompt=\"p\"\ndepends_on=[\"s2\"]\n";
        let squad = store.insert_squad(&parse(toml), Some("r"), false).unwrap();
        let dep_toml = format!(
            "[[default]]\ndepends_on = [\"{squad}\"]\n[[task]]\nname=\"b\"\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\n"
        );
        let dependent = store
            .insert_squad(&parse(&dep_toml), Some("dependent"), false)
            .unwrap();
        store.set_squad_state(&dependent, SquadState::Done).unwrap();
        for idx in 0..3 {
            store
                .set_cell_state(&squad, 0, idx, NodeState::Done)
                .unwrap();
        }

        let preview = store.compute_cell_restart_impact(&squad, 0, 1).unwrap();
        let cell_ids: Vec<(i64, i64)> = preview.cells.iter().map(|s| (s.task_idx, s.idx)).collect();
        assert_eq!(cell_ids, vec![(0, 1), (0, 2)], "s2 and downstream s3 only");
        assert_eq!(preview.tasks.len(), 1);
        assert_eq!(
            preview
                .dirtied_squads
                .iter()
                .map(|r| r.id.clone())
                .collect::<Vec<_>>(),
            vec![dependent.clone()]
        );

        // Nothing was mutated by computing the preview.
        let done = store.done_cells(&squad).unwrap();
        assert!(done.contains(&(0, 0)) && done.contains(&(0, 1)) && done.contains(&(0, 2)));
        assert_eq!(store.squad_state(&dependent).unwrap(), SquadState::Done);

        // The real restart applies exactly what the preview reported.
        store.restart_cell(&squad, 0, 1).unwrap();
        let done = store.done_cells(&squad).unwrap();
        assert!(done.contains(&(0, 0)), "s1 stays done");
        assert!(!done.contains(&(0, 1)) && !done.contains(&(0, 2)));
        assert_eq!(store.squad_state(&dependent).unwrap(), SquadState::Pending);
    }

    #[test]
    fn restart_cell_resets_downstream_cells_in_squad_but_not_upstream() {
        // s1 -> s2 -> s3 within one task; restarting s2 dirties s2 and s3 while
        // s1 stays Done (and is later skipped on re-run).
        let mut store = Store::open_in_memory().unwrap();
        let toml = "[[task]]\nname=\"t\"\n\
            [[task.cell]]\nid=\"s1\"\ncwd=\"/r\"\nprompt=\"p\"\n\
            [[task.cell]]\nid=\"s2\"\ncwd=\"/r\"\nprompt=\"p\"\ndepends_on=[\"s1\"]\n\
            [[task.cell]]\nid=\"s3\"\ncwd=\"/r\"\nprompt=\"p\"\ndepends_on=[\"s2\"]\n";
        let squad = store.insert_squad(&parse(toml), Some("r"), false).unwrap();
        for idx in 0..3 {
            store
                .set_cell_state(&squad, 0, idx, NodeState::Done)
                .unwrap();
        }
        store.restart_cell(&squad, 0, 1).unwrap(); // restart s2
        let done = store.done_cells(&squad).unwrap();
        assert!(done.contains(&(0, 0)), "s1 stays done");
        assert!(!done.contains(&(0, 1)), "s2 dirtied");
        assert!(!done.contains(&(0, 2)), "s3 dirtied (downstream of s2)");
    }

    #[test]
    fn apply_restart_user_note_narrow_targets_only_the_root_cell() {
        // s1 -> s2 -> s3; a narrow (checkbox-off) restart note on s2 must not
        // reach s1 (upstream) or s3 (downstream).
        let mut store = Store::open_in_memory().unwrap();
        let toml = "[[task]]\nname=\"t\"\n\
            [[task.cell]]\nid=\"s1\"\ncwd=\"/r\"\nprompt=\"p\"\n\
            [[task.cell]]\nid=\"s2\"\ncwd=\"/r\"\nprompt=\"p\"\ndepends_on=[\"s1\"]\n\
            [[task.cell]]\nid=\"s3\"\ncwd=\"/r\"\nprompt=\"p\"\ndepends_on=[\"s2\"]\n";
        let squad = store.insert_squad(&parse(toml), Some("r"), false).unwrap();

        store
            .apply_restart_user_note(&squad, &[(0, 1)], false, "picking up mid-fix")
            .unwrap();

        assert_eq!(
            store
                .get_ghost(&crate::ghost::cell_uri(&squad, 0, 1))
                .unwrap()
                .unwrap()
                .user_note
                .as_deref(),
            Some("picking up mid-fix")
        );
        assert!(
            store
                .get_ghost(&crate::ghost::cell_uri(&squad, 0, 0))
                .unwrap()
                .is_none(),
            "upstream cell must not receive the note"
        );
        assert!(
            store
                .get_ghost(&crate::ghost::cell_uri(&squad, 0, 2))
                .unwrap()
                .is_none(),
            "downstream cell must not receive the note when include_downstream is false"
        );
    }

    #[test]
    fn apply_restart_user_note_include_downstream_reaches_children_not_upstream() {
        let mut store = Store::open_in_memory().unwrap();
        let toml = "[[task]]\nname=\"t\"\n\
            [[task.cell]]\nid=\"s1\"\ncwd=\"/r\"\nprompt=\"p\"\n\
            [[task.cell]]\nid=\"s2\"\ncwd=\"/r\"\nprompt=\"p\"\ndepends_on=[\"s1\"]\n\
            [[task.cell]]\nid=\"s3\"\ncwd=\"/r\"\nprompt=\"p\"\ndepends_on=[\"s2\"]\n";
        let squad = store.insert_squad(&parse(toml), Some("r"), false).unwrap();

        store
            .apply_restart_user_note(&squad, &[(0, 1)], true, "apply to all children")
            .unwrap();

        for idx in [1, 2] {
            assert_eq!(
                store
                    .get_ghost(&crate::ghost::cell_uri(&squad, 0, idx))
                    .unwrap()
                    .unwrap()
                    .user_note
                    .as_deref(),
                Some("apply to all children"),
                "cell {idx} should have the note"
            );
        }
        assert!(
            store
                .get_ghost(&crate::ghost::cell_uri(&squad, 0, 0))
                .unwrap()
                .is_none(),
            "upstream cell must still be untouched"
        );
    }

    #[test]
    fn restart_task_resets_all_of_its_cells_and_downstream_but_not_upstream() {
        // t0/s1 -> t0/s2, t1/s3 depends on t0/s2 (cross-task). Restarting t0
        // must dirty every t0 cell plus t1's downstream cell, but leave
        // an unrelated upstream-only cell alone.
        let mut store = Store::open_in_memory().unwrap();
        let toml = "[[task]]\nname=\"t0\"\n\
            [[task.cell]]\nid=\"s1\"\ncwd=\"/r\"\nprompt=\"p\"\n\
            [[task.cell]]\nid=\"s2\"\ncwd=\"/r\"\nprompt=\"p\"\ndepends_on=[\"s1\"]\n\
            [[task]]\nname=\"t1\"\n\
            [[task.cell]]\nid=\"s3\"\ncwd=\"/r\"\nprompt=\"p\"\ndepends_on=[\"t0/s2\"]\n";
        let squad = store.insert_squad(&parse(toml), Some("r"), false).unwrap();
        store.set_cell_state(&squad, 0, 0, NodeState::Done).unwrap();
        store.set_cell_state(&squad, 0, 1, NodeState::Done).unwrap();
        store.set_cell_state(&squad, 1, 0, NodeState::Done).unwrap();

        store.restart_task(&squad, 0).unwrap();
        let done = store.done_cells(&squad).unwrap();
        assert!(!done.contains(&(0, 0)), "t0/s1 dirtied");
        assert!(!done.contains(&(0, 1)), "t0/s2 dirtied");
        assert!(
            !done.contains(&(1, 0)),
            "t1/s3 dirtied (downstream of t0/s2)"
        );
    }

    #[test]
    fn restart_task_on_missing_task_is_not_found() {
        let mut store = Store::open_in_memory().unwrap();
        let toml = "[[task]]\nname=\"t\"\n[[task.cell]]\nid=\"s1\"\ncwd=\"/r\"\nprompt=\"p\"\n";
        let squad = store.insert_squad(&parse(toml), Some("r"), false).unwrap();
        assert!(matches!(
            store.restart_task(&squad, 5),
            Err(StoreError::NotFound)
        ));
    }

    // ── env overrides (RAL-150) ───────────────────────────────────────────

    #[test]
    fn squad_env_overrides_default_to_empty() {
        let mut store = Store::open_in_memory().unwrap();
        let toml = "[[task]]\nname=\"t\"\n[[task.cell]]\nid=\"s1\"\ncwd=\"/r\"\nprompt=\"p\"\n";
        let squad = store.insert_squad(&parse(toml), Some("r"), false).unwrap();
        assert!(store.get_squad_env_overrides(&squad).unwrap().is_empty());
        assert!(store.get_squad(&squad).unwrap().env_overrides.is_empty());
    }

    #[test]
    fn set_squad_env_overrides_persists_and_merges() {
        let mut store = Store::open_in_memory().unwrap();
        let toml = "[[task]]\nname=\"t\"\n[[task.cell]]\nid=\"s1\"\ncwd=\"/r\"\nprompt=\"p\"\n";
        let squad = store.insert_squad(&parse(toml), Some("r"), false).unwrap();

        let mut set = BTreeMap::new();
        set.insert("A".to_string(), "1".to_string());
        set.insert("B".to_string(), "2".to_string());
        let result = store.set_squad_env_overrides(&squad, &set, &[]).unwrap();
        assert_eq!(result.get("A").map(String::as_str), Some("1"));
        assert_eq!(result.get("B").map(String::as_str), Some("2"));

        // A second call merges into the existing map rather than replacing it.
        let mut set2 = BTreeMap::new();
        set2.insert("C".to_string(), "3".to_string());
        let result2 = store
            .set_squad_env_overrides(&squad, &set2, &["A".to_string()])
            .unwrap();
        assert!(!result2.contains_key("A"), "A was unset");
        assert_eq!(
            result2.get("B").map(String::as_str),
            Some("2"),
            "B untouched"
        );
        assert_eq!(result2.get("C").map(String::as_str), Some("3"));

        // Persisted across a fresh fetch, and reflected in the SquadView.
        assert_eq!(store.get_squad_env_overrides(&squad).unwrap(), result2);
        assert_eq!(store.get_squad(&squad).unwrap().env_overrides, result2);
    }

    #[test]
    fn set_squad_env_overrides_missing_squad_is_not_found() {
        let store = Store::open_in_memory().unwrap();
        let set = BTreeMap::new();
        assert!(matches!(
            store.set_squad_env_overrides("nope", &set, &[]),
            Err(StoreError::NotFound)
        ));
    }

    #[test]
    fn set_squad_env_overrides_set_wins_over_unset_for_same_key() {
        let mut store = Store::open_in_memory().unwrap();
        let toml = "[[task]]\nname=\"t\"\n[[task.cell]]\nid=\"s1\"\ncwd=\"/r\"\nprompt=\"p\"\n";
        let squad = store.insert_squad(&parse(toml), Some("r"), false).unwrap();
        let mut set = BTreeMap::new();
        set.insert("A".to_string(), "1".to_string());
        let result = store
            .set_squad_env_overrides(&squad, &set, &["A".to_string()])
            .unwrap();
        assert_eq!(result.get("A").map(String::as_str), Some("1"));
    }

    #[test]
    fn env_overrides_survive_retry_to_pending() {
        // Persistence (Q4): overrides must not be cleared by a plain retry.
        let mut store = Store::open_in_memory().unwrap();
        let toml = "[[task]]\nname=\"t\"\n[[task.cell]]\nid=\"s1\"\ncwd=\"/r\"\nprompt=\"p\"\n";
        let squad = store.insert_squad(&parse(toml), Some("r"), false).unwrap();
        let mut set = BTreeMap::new();
        set.insert("RALPHUS_RESOLVER_MODEL".to_string(), "qwen3:8b".to_string());
        store.set_squad_env_overrides(&squad, &set, &[]).unwrap();

        store.reset_squad_to_pending(&squad).unwrap();
        assert_eq!(
            store
                .get_squad_env_overrides(&squad)
                .unwrap()
                .get("RALPHUS_RESOLVER_MODEL"),
            Some(&"qwen3:8b".to_string())
        );
    }

    // ── hierarchical env overrides (RAL-150 extension) ─────────────────────

    fn two_task_two_cell_toml() -> &'static str {
        "[[task]]\nname=\"t0\"\n\
         [[task.cell]]\nid=\"s0\"\ncwd=\"/r\"\nprompt=\"p\"\n\
         [[task.cell]]\nid=\"s1\"\ncwd=\"/r\"\nprompt=\"p\"\n\
         [[task]]\nname=\"t1\"\n\
         [[task.cell]]\nid=\"s0\"\ncwd=\"/r\"\nprompt=\"p\"\n"
    }

    #[test]
    fn task_env_overrides_default_to_empty() {
        let mut store = Store::open_in_memory().unwrap();
        let squad = store
            .insert_squad(&parse(two_task_two_cell_toml()), Some("r"), false)
            .unwrap();
        assert!(store.get_task_env_overrides(&squad, 0).unwrap().is_empty());
        assert!(
            store
                .get_task_proof_env_overrides(&squad, 0)
                .unwrap()
                .is_empty()
        );
        assert!(
            store.get_squad(&squad).unwrap().tasks[0]
                .env_overrides
                .is_empty()
        );
        assert!(
            store.get_squad(&squad).unwrap().tasks[0]
                .proof_env_overrides
                .is_empty()
        );
    }

    #[test]
    fn toml_environment_seeds_task_and_cell_env_overrides() {
        // RAL-172: a task/cell's own `environment` table in the submitted
        // TOML seeds the same store columns `set_task_env_overrides`/
        // `set_cell_env_overrides` write to, so it merges into the
        // existing `squad < task < cell` layering (RAL-150) with zero
        // extra resolution logic.
        let src = "[[task]]\nname=\"t0\"\nenvironment={SHARED=\"from-task\", TASK_ONLY=\"1\"}\n\
                   [[task.cell]]\nid=\"s0\"\ncwd=\"/r\"\nprompt=\"p\"\n\
                   environment={SHARED=\"from-cell\", CELL_ONLY=\"2\"}\n";
        let mut store = Store::open_in_memory().unwrap();
        let squad = store.insert_squad(&parse(src), Some("r"), false).unwrap();

        let task_env = store.get_task_env_overrides(&squad, 0).unwrap();
        assert_eq!(
            task_env.get("SHARED").map(String::as_str),
            Some("from-task")
        );
        assert_eq!(task_env.get("TASK_ONLY").map(String::as_str), Some("1"));

        let cell_env = store.get_cell_env_overrides(&squad, 0, 0).unwrap();
        assert_eq!(
            cell_env.get("SHARED").map(String::as_str),
            Some("from-cell")
        );
        assert_eq!(cell_env.get("CELL_ONLY").map(String::as_str), Some("2"));

        // The cell's declared value wins over the task's for the shared
        // key once resolved, same precedence as a squad-time override.
        let resolved = store.resolve_cell_env_overrides(&squad, 0, 0).unwrap();
        assert_eq!(
            resolved.get("SHARED").map(String::as_str),
            Some("from-cell")
        );
        assert_eq!(resolved.get("TASK_ONLY").map(String::as_str), Some("1"));
        assert_eq!(resolved.get("CELL_ONLY").map(String::as_str), Some("2"));
    }

    #[test]
    fn toml_environment_seeds_each_proof_step_separately() {
        // RAL-191: the whole point of the per-step layer -- two proof steps
        // under one task set the same key to different values, and neither
        // clobbers the other.
        let src = "[[task]]\nname=\"t0\"\n\
                   [[task.proof]]\ncommand=\"cargo test\"\nenvironment={RUST_LOG=\"debug\"}\n\
                   [[task.proof]]\ncommand=\"cargo clippy\"\nenvironment={RUST_LOG=\"warn\"}\n\
                   [[task.cell]]\nid=\"s0\"\ncwd=\"/r\"\nprompt=\"p\"\n\
                   [[task.cell.proof]]\ncommand=\"npm test\"\nenvironment={CI=\"1\"}\n";
        let mut store = Store::open_in_memory().unwrap();
        let squad = store.insert_squad(&parse(src), Some("r"), false).unwrap();

        let step0 = store
            .get_proof_step_env_overrides(&squad, 0, "task", -1, 0)
            .unwrap();
        let step1 = store
            .get_proof_step_env_overrides(&squad, 0, "task", -1, 1)
            .unwrap();
        assert_eq!(step0.get("RUST_LOG").map(String::as_str), Some("debug"));
        assert_eq!(step1.get("RUST_LOG").map(String::as_str), Some("warn"));

        let sess_step = store
            .get_proof_step_env_overrides(&squad, 0, "cell", 0, 0)
            .unwrap();
        assert_eq!(sess_step.get("CI").map(String::as_str), Some("1"));
    }

    #[test]
    fn resolve_proof_step_env_precedence_step_wins_over_every_ancestor() {
        // RAL-191: squad < task < task.proof < step, and
        // squad < task < cell < cell.proof < step.
        let mut store = Store::open_in_memory().unwrap();
        let src = "[[task]]\nname=\"t0\"\n\
                   [[task.proof]]\ncommand=\"c\"\n\
                   [[task.cell]]\nid=\"s0\"\ncwd=\"/r\"\nprompt=\"p\"\n\
                   [[task.cell.proof]]\ncommand=\"d\"\n";
        let squad = store.insert_squad(&parse(src), Some("r"), false).unwrap();

        let mut m = BTreeMap::new();
        m.insert("A".to_string(), "squad".to_string());
        store.set_squad_env_overrides(&squad, &m, &[]).unwrap();
        // With nothing set below it, the step inherits the squad's value.
        assert_eq!(
            store
                .resolve_task_proof_step_env_overrides(&squad, 0, 0)
                .unwrap()
                .get("A"),
            Some(&"squad".to_string())
        );

        let mut m = BTreeMap::new();
        m.insert("A".to_string(), "task-proof".to_string());
        store
            .set_task_proof_env_overrides(&squad, 0, &m, &[])
            .unwrap();
        assert_eq!(
            store
                .resolve_task_proof_step_env_overrides(&squad, 0, 0)
                .unwrap()
                .get("A"),
            Some(&"task-proof".to_string())
        );

        // The step's own value beats the scope-wide proof layer above it.
        let mut m = BTreeMap::new();
        m.insert("A".to_string(), "step".to_string());
        store
            .set_proof_step_env_overrides(&squad, 0, "task", -1, 0, &m, &[])
            .unwrap();
        assert_eq!(
            store
                .resolve_task_proof_step_env_overrides(&squad, 0, 0)
                .unwrap()
                .get("A"),
            Some(&"step".to_string())
        );
        // ...and does not leak into the cell-scoped chain.
        assert_eq!(
            store
                .resolve_cell_proof_step_env_overrides(&squad, 0, 0, 0)
                .unwrap()
                .get("A"),
            Some(&"squad".to_string())
        );
    }

    #[test]
    fn set_task_env_overrides_persists_and_merges() {
        let mut store = Store::open_in_memory().unwrap();
        let squad = store
            .insert_squad(&parse(two_task_two_cell_toml()), Some("r"), false)
            .unwrap();

        let mut set = BTreeMap::new();
        set.insert("A".to_string(), "1".to_string());
        let result = store.set_task_env_overrides(&squad, 0, &set, &[]).unwrap();
        assert_eq!(result.get("A").map(String::as_str), Some("1"));

        let mut set2 = BTreeMap::new();
        set2.insert("B".to_string(), "2".to_string());
        let result2 = store
            .set_task_env_overrides(&squad, 0, &set2, &["A".to_string()])
            .unwrap();
        assert!(!result2.contains_key("A"));
        assert_eq!(result2.get("B").map(String::as_str), Some("2"));

        assert_eq!(store.get_task_env_overrides(&squad, 0).unwrap(), result2);
        assert_eq!(
            store.get_squad(&squad).unwrap().tasks[0].env_overrides,
            result2
        );
        // Task 1 is untouched.
        assert!(store.get_task_env_overrides(&squad, 1).unwrap().is_empty());
    }

    /// RAL-271: the board's inline "Edit" button re-`set`s an already-present
    /// key rather than unset-then-add -- verify that round trip replaces the
    /// value without changing the key count or disturbing a sibling key.
    #[test]
    fn set_task_env_overrides_editing_an_existing_key_replaces_its_value_in_place() {
        let mut store = Store::open_in_memory().unwrap();
        let squad = store
            .insert_squad(&parse(two_task_two_cell_toml()), Some("r"), false)
            .unwrap();

        let mut initial = BTreeMap::new();
        initial.insert("A".to_string(), "1".to_string());
        initial.insert("B".to_string(), "2".to_string());
        store
            .set_task_env_overrides(&squad, 0, &initial, &[])
            .unwrap();

        let mut edit = BTreeMap::new();
        edit.insert("A".to_string(), "edited".to_string());
        let result = store.set_task_env_overrides(&squad, 0, &edit, &[]).unwrap();

        assert_eq!(result.len(), 2);
        assert_eq!(result.get("A").map(String::as_str), Some("edited"));
        assert_eq!(result.get("B").map(String::as_str), Some("2"));
        assert_eq!(store.get_task_env_overrides(&squad, 0).unwrap(), result);
    }

    #[test]
    fn set_task_env_overrides_missing_task_is_not_found() {
        let mut store = Store::open_in_memory().unwrap();
        let squad = store
            .insert_squad(&parse(two_task_two_cell_toml()), Some("r"), false)
            .unwrap();
        let set = BTreeMap::new();
        assert!(matches!(
            store.set_task_env_overrides(&squad, 9, &set, &[]),
            Err(StoreError::NotFound)
        ));
    }

    #[test]
    fn set_task_proof_env_overrides_persists_independent_of_task_env() {
        let mut store = Store::open_in_memory().unwrap();
        let squad = store
            .insert_squad(&parse(two_task_two_cell_toml()), Some("r"), false)
            .unwrap();
        let mut set = BTreeMap::new();
        set.insert("A".to_string(), "task".to_string());
        store.set_task_env_overrides(&squad, 0, &set, &[]).unwrap();
        let mut vset = BTreeMap::new();
        vset.insert("A".to_string(), "task-proof".to_string());
        store
            .set_task_proof_env_overrides(&squad, 0, &vset, &[])
            .unwrap();

        assert_eq!(
            store.get_task_env_overrides(&squad, 0).unwrap().get("A"),
            Some(&"task".to_string())
        );
        assert_eq!(
            store
                .get_task_proof_env_overrides(&squad, 0)
                .unwrap()
                .get("A"),
            Some(&"task-proof".to_string())
        );
    }

    #[test]
    fn cell_env_overrides_default_to_empty() {
        let mut store = Store::open_in_memory().unwrap();
        let squad = store
            .insert_squad(&parse(two_task_two_cell_toml()), Some("r"), false)
            .unwrap();
        assert!(
            store
                .get_cell_env_overrides(&squad, 0, 0)
                .unwrap()
                .is_empty()
        );
        assert!(
            store
                .get_cell_proof_env_overrides(&squad, 0, 0)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn set_cell_env_overrides_persists_and_is_scoped_to_that_cell() {
        let mut store = Store::open_in_memory().unwrap();
        let squad = store
            .insert_squad(&parse(two_task_two_cell_toml()), Some("r"), false)
            .unwrap();
        let mut set = BTreeMap::new();
        set.insert("A".to_string(), "1".to_string());
        store
            .set_cell_env_overrides(&squad, 0, 0, &set, &[])
            .unwrap();

        assert_eq!(
            store.get_cell_env_overrides(&squad, 0, 0).unwrap().get("A"),
            Some(&"1".to_string())
        );
        // Sibling cell (t0/s1) and the other task's cell (t1/s0) are untouched.
        assert!(
            store
                .get_cell_env_overrides(&squad, 0, 1)
                .unwrap()
                .is_empty()
        );
        assert!(
            store
                .get_cell_env_overrides(&squad, 1, 0)
                .unwrap()
                .is_empty()
        );

        let view = store.get_squad(&squad).unwrap();
        assert_eq!(
            view.tasks[0].cells[0].env_overrides.get("A"),
            Some(&"1".to_string())
        );
    }

    #[test]
    fn set_cell_env_overrides_missing_cell_is_not_found() {
        let mut store = Store::open_in_memory().unwrap();
        let squad = store
            .insert_squad(&parse(two_task_two_cell_toml()), Some("r"), false)
            .unwrap();
        let set = BTreeMap::new();
        assert!(matches!(
            store.set_cell_env_overrides(&squad, 0, 9, &set, &[]),
            Err(StoreError::NotFound)
        ));
    }

    // ── "out of date" cascade badge (RAL-271) ───────────────────────────────

    /// One task (`t0`) with its own task-scoped proof step, a cell (`s0`)
    /// with two of its own cell-scoped proof steps, and a bare sibling cell
    /// (`s1`) with none -- plus an entirely unrelated second task (`t1`) with
    /// its own cell, used to assert a cascade never crosses into a sibling.
    fn env_out_of_date_fixture_toml() -> &'static str {
        "[[task]]\nname=\"t0\"\n\
         [[task.proof]]\ncommand=\"tp0\"\n\
         [[task.cell]]\nid=\"s0\"\ncwd=\"/r\"\nprompt=\"p\"\n\
         [[task.cell.proof]]\ncommand=\"cp0\"\n\
         [[task.cell.proof]]\ncommand=\"cp1\"\n\
         [[task.cell]]\nid=\"s1\"\ncwd=\"/r\"\nprompt=\"p\"\n\
         [[task]]\nname=\"t1\"\n\
         [[task.cell]]\nid=\"s0\"\ncwd=\"/r\"\nprompt=\"p\"\n"
    }

    #[test]
    fn env_out_of_date_defaults_to_false() {
        let mut store = Store::open_in_memory().unwrap();
        let squad = store
            .insert_squad(&parse(env_out_of_date_fixture_toml()), Some("r"), false)
            .unwrap();
        let view = store.get_squad(&squad).unwrap();
        assert!(!view.tasks[0].env_out_of_date);
        assert!(!view.tasks[0].proof[0].env_out_of_date);
        assert!(!view.tasks[0].cells[0].env_out_of_date);
        assert!(!view.tasks[0].cells[0].proof[0].env_out_of_date);
    }

    #[test]
    fn set_task_env_overrides_marks_task_and_its_cells_but_not_grandchildren_or_siblings() {
        let mut store = Store::open_in_memory().unwrap();
        let squad = store
            .insert_squad(&parse(env_out_of_date_fixture_toml()), Some("r"), false)
            .unwrap();
        let mut set = BTreeMap::new();
        set.insert("A".to_string(), "1".to_string());
        store.set_task_env_overrides(&squad, 0, &set, &[]).unwrap();

        let view = store.get_squad(&squad).unwrap();
        assert!(view.tasks[0].env_out_of_date);
        assert!(view.tasks[0].cells[0].env_out_of_date);
        assert!(view.tasks[0].cells[1].env_out_of_date);
        // Cascade stops one level down: grandchild proof steps (owned by the
        // task directly, or by one of its cells) are untouched.
        assert!(!view.tasks[0].proof[0].env_out_of_date);
        assert!(!view.tasks[0].cells[0].proof[0].env_out_of_date);
        // A sibling task and its cell are never touched.
        assert!(!view.tasks[1].env_out_of_date);
        assert!(!view.tasks[1].cells[0].env_out_of_date);
    }

    #[test]
    fn set_task_proof_env_overrides_marks_only_the_tasks_own_proof_steps() {
        let mut store = Store::open_in_memory().unwrap();
        let squad = store
            .insert_squad(&parse(env_out_of_date_fixture_toml()), Some("r"), false)
            .unwrap();
        let mut set = BTreeMap::new();
        set.insert("A".to_string(), "1".to_string());
        store
            .set_task_proof_env_overrides(&squad, 0, &set, &[])
            .unwrap();

        let view = store.get_squad(&squad).unwrap();
        assert!(view.tasks[0].proof[0].env_out_of_date);
        // Neither the task itself nor its cells are marked -- this layer
        // feeds the task's own proof steps directly.
        assert!(!view.tasks[0].env_out_of_date);
        assert!(!view.tasks[0].cells[0].env_out_of_date);
    }

    #[test]
    fn set_cell_env_overrides_marks_cell_and_its_proofs_but_not_sibling_cell_or_task() {
        let mut store = Store::open_in_memory().unwrap();
        let squad = store
            .insert_squad(&parse(env_out_of_date_fixture_toml()), Some("r"), false)
            .unwrap();
        let mut set = BTreeMap::new();
        set.insert("A".to_string(), "1".to_string());
        store
            .set_cell_env_overrides(&squad, 0, 0, &set, &[])
            .unwrap();

        let view = store.get_squad(&squad).unwrap();
        assert!(view.tasks[0].cells[0].env_out_of_date);
        assert!(view.tasks[0].cells[0].proof[0].env_out_of_date);
        assert!(view.tasks[0].cells[0].proof[1].env_out_of_date);
        // The sibling cell, the owning task, and its own proof step are
        // never touched.
        assert!(!view.tasks[0].cells[1].env_out_of_date);
        assert!(!view.tasks[0].env_out_of_date);
        assert!(!view.tasks[0].proof[0].env_out_of_date);
        // An unrelated task's cell is never touched.
        assert!(!view.tasks[1].cells[0].env_out_of_date);
    }

    #[test]
    fn set_cell_proof_env_overrides_marks_only_that_cells_proof_steps() {
        let mut store = Store::open_in_memory().unwrap();
        let squad = store
            .insert_squad(&parse(env_out_of_date_fixture_toml()), Some("r"), false)
            .unwrap();
        let mut set = BTreeMap::new();
        set.insert("A".to_string(), "1".to_string());
        store
            .set_cell_proof_env_overrides(&squad, 0, 0, &set, &[])
            .unwrap();

        let view = store.get_squad(&squad).unwrap();
        assert!(view.tasks[0].cells[0].proof[0].env_out_of_date);
        assert!(view.tasks[0].cells[0].proof[1].env_out_of_date);
        assert!(!view.tasks[0].cells[0].env_out_of_date);
    }

    #[test]
    fn set_proof_step_env_overrides_marks_only_that_one_step() {
        let mut store = Store::open_in_memory().unwrap();
        let squad = store
            .insert_squad(&parse(env_out_of_date_fixture_toml()), Some("r"), false)
            .unwrap();
        let mut set = BTreeMap::new();
        set.insert("A".to_string(), "1".to_string());
        store
            .set_proof_step_env_overrides(&squad, 0, "cell", 0, 0, &set, &[])
            .unwrap();

        let view = store.get_squad(&squad).unwrap();
        assert!(view.tasks[0].cells[0].proof[0].env_out_of_date);
        assert!(!view.tasks[0].cells[0].proof[1].env_out_of_date);
        assert!(!view.tasks[0].cells[0].env_out_of_date);
    }

    #[test]
    fn restart_cell_clears_env_out_of_date_for_that_cell_and_its_proofs() {
        let mut store = Store::open_in_memory().unwrap();
        let squad = store
            .insert_squad(&parse(env_out_of_date_fixture_toml()), Some("r"), false)
            .unwrap();
        let mut set = BTreeMap::new();
        set.insert("A".to_string(), "1".to_string());
        store
            .set_cell_env_overrides(&squad, 0, 0, &set, &[])
            .unwrap();

        store.restart_cell(&squad, 0, 0).unwrap();

        let view = store.get_squad(&squad).unwrap();
        assert!(!view.tasks[0].cells[0].env_out_of_date);
        assert!(!view.tasks[0].cells[0].proof[0].env_out_of_date);
        assert!(!view.tasks[0].cells[0].proof[1].env_out_of_date);
    }

    #[test]
    fn restart_task_clears_env_out_of_date_for_task_and_its_cells() {
        let mut store = Store::open_in_memory().unwrap();
        let squad = store
            .insert_squad(&parse(env_out_of_date_fixture_toml()), Some("r"), false)
            .unwrap();
        let mut set = BTreeMap::new();
        set.insert("A".to_string(), "1".to_string());
        store.set_task_env_overrides(&squad, 0, &set, &[]).unwrap();

        store.restart_task(&squad, 0).unwrap();

        let view = store.get_squad(&squad).unwrap();
        assert!(!view.tasks[0].env_out_of_date);
        assert!(!view.tasks[0].cells[0].env_out_of_date);
        assert!(!view.tasks[0].cells[1].env_out_of_date);
    }

    #[test]
    fn set_cell_state_clears_env_out_of_date_even_without_a_restart() {
        // Covers both triggers the ticket calls out: a manual `Set Status`
        // call (this test) and the scheduler's own dispatch into `running`
        // (same code path, since both go through `set_cell_state`).
        let mut store = Store::open_in_memory().unwrap();
        let squad = store
            .insert_squad(&parse(env_out_of_date_fixture_toml()), Some("r"), false)
            .unwrap();
        let mut set = BTreeMap::new();
        set.insert("A".to_string(), "1".to_string());
        store
            .set_cell_env_overrides(&squad, 0, 0, &set, &[])
            .unwrap();

        store.set_cell_state(&squad, 0, 0, NodeState::Done).unwrap();

        assert!(!store.get_squad(&squad).unwrap().tasks[0].cells[0].env_out_of_date);
    }

    #[test]
    fn set_task_state_clears_env_out_of_date() {
        let mut store = Store::open_in_memory().unwrap();
        let squad = store
            .insert_squad(&parse(env_out_of_date_fixture_toml()), Some("r"), false)
            .unwrap();
        let mut set = BTreeMap::new();
        set.insert("A".to_string(), "1".to_string());
        store.set_task_env_overrides(&squad, 0, &set, &[]).unwrap();

        store.set_task_state(&squad, 0, NodeState::Done).unwrap();

        assert!(!store.get_squad(&squad).unwrap().tasks[0].env_out_of_date);
    }

    #[test]
    fn set_proof_state_clears_env_out_of_date() {
        let mut store = Store::open_in_memory().unwrap();
        let squad = store
            .insert_squad(&parse(env_out_of_date_fixture_toml()), Some("r"), false)
            .unwrap();
        let mut set = BTreeMap::new();
        set.insert("A".to_string(), "1".to_string());
        store
            .set_proof_step_env_overrides(&squad, 0, "cell", 0, 0, &set, &[])
            .unwrap();

        store
            .set_proof_state(&squad, 0, "cell", 0, 0, NodeState::Done)
            .unwrap();

        let view = store.get_squad(&squad).unwrap();
        assert!(!view.tasks[0].cells[0].proof[0].env_out_of_date);
    }

    #[test]
    fn resolve_cell_env_overrides_precedence_squad_lt_task_lt_cell() {
        let mut store = Store::open_in_memory().unwrap();
        let squad = store
            .insert_squad(&parse(two_task_two_cell_toml()), Some("r"), false)
            .unwrap();

        // Only a squad-level value: flows straight through.
        let mut squad_set = BTreeMap::new();
        squad_set.insert("A".to_string(), "squad".to_string());
        squad_set.insert("B".to_string(), "squad".to_string());
        squad_set.insert("C".to_string(), "squad".to_string());
        store
            .set_squad_env_overrides(&squad, &squad_set, &[])
            .unwrap();
        let merged = store.resolve_cell_env_overrides(&squad, 0, 0).unwrap();
        assert_eq!(merged.get("A"), Some(&"squad".to_string()));

        // A task-level value for B wins over the squad's, but only for cells
        // under that task.
        let mut task_set = BTreeMap::new();
        task_set.insert("B".to_string(), "task".to_string());
        store
            .set_task_env_overrides(&squad, 0, &task_set, &[])
            .unwrap();
        let merged = store.resolve_cell_env_overrides(&squad, 0, 0).unwrap();
        assert_eq!(merged.get("A"), Some(&"squad".to_string()));
        assert_eq!(merged.get("B"), Some(&"task".to_string()));
        let other_task_merged = store.resolve_cell_env_overrides(&squad, 1, 0).unwrap();
        assert_eq!(other_task_merged.get("B"), Some(&"squad".to_string()));

        // A cell-level value for C wins over both the task's and the squad's,
        // but only for that one cell.
        let mut cell_set = BTreeMap::new();
        cell_set.insert("C".to_string(), "cell".to_string());
        store
            .set_cell_env_overrides(&squad, 0, 0, &cell_set, &[])
            .unwrap();
        let merged = store.resolve_cell_env_overrides(&squad, 0, 0).unwrap();
        assert_eq!(merged.get("C"), Some(&"cell".to_string()));
        let sibling_merged = store.resolve_cell_env_overrides(&squad, 0, 1).unwrap();
        assert_eq!(sibling_merged.get("C"), Some(&"squad".to_string()));
    }

    #[test]
    fn resolve_cell_env_overrides_batch_matches_the_single_cell_form() {
        let mut store = Store::open_in_memory().unwrap();
        let squad = store
            .insert_squad(&parse(two_task_two_cell_toml()), Some("r"), false)
            .unwrap();

        let mut squad_set = BTreeMap::new();
        squad_set.insert("A".to_string(), "squad".to_string());
        store
            .set_squad_env_overrides(&squad, &squad_set, &[])
            .unwrap();
        let mut task_set = BTreeMap::new();
        task_set.insert("B".to_string(), "task".to_string());
        store
            .set_task_env_overrides(&squad, 0, &task_set, &[])
            .unwrap();
        let mut cell_set = BTreeMap::new();
        cell_set.insert("C".to_string(), "cell".to_string());
        store
            .set_cell_env_overrides(&squad, 0, 0, &cell_set, &[])
            .unwrap();

        let refs = vec![
            (squad.clone(), 0, 0),
            (squad.clone(), 0, 1),
            (squad.clone(), 1, 0),
        ];
        let batched = store.resolve_cell_env_overrides_batch(&refs).unwrap();
        for r in &refs {
            let single = store.resolve_cell_env_overrides(&r.0, r.1, r.2).unwrap();
            assert_eq!(batched.get(r), Some(&single), "mismatch for {r:?}");
        }
        assert_eq!(
            batched.get(&(squad.clone(), 0, 0)).unwrap().get("C"),
            Some(&"cell".to_string())
        );

        // A squad/task/cell combo with no rows at all resolves to empty
        // rather than erroring, mirroring the per-cell version's behavior
        // for a deleted ancestor.
        let missing = vec![("no-such-squad".to_string(), 0, 0)];
        let batched_missing = store.resolve_cell_env_overrides_batch(&missing).unwrap();
        assert!(
            batched_missing
                .get(&("no-such-squad".to_string(), 0, 0))
                .unwrap()
                .is_empty()
        );

        assert!(
            store
                .resolve_cell_env_overrides_batch(&[])
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn resolve_task_proof_env_overrides_precedence_squad_lt_task_lt_task_proof() {
        let mut store = Store::open_in_memory().unwrap();
        let squad = store
            .insert_squad(&parse(two_task_two_cell_toml()), Some("r"), false)
            .unwrap();

        let mut squad_set = BTreeMap::new();
        squad_set.insert("A".to_string(), "squad".to_string());
        store
            .set_squad_env_overrides(&squad, &squad_set, &[])
            .unwrap();

        let mut task_set = BTreeMap::new();
        task_set.insert("A".to_string(), "task".to_string());
        store
            .set_task_env_overrides(&squad, 0, &task_set, &[])
            .unwrap();
        // Task-proof has no value of its own yet -- inherits the task's.
        assert_eq!(
            store
                .resolve_task_proof_env_overrides(&squad, 0)
                .unwrap()
                .get("A"),
            Some(&"task".to_string())
        );

        let mut proof_set = BTreeMap::new();
        proof_set.insert("A".to_string(), "task-proof".to_string());
        store
            .set_task_proof_env_overrides(&squad, 0, &proof_set, &[])
            .unwrap();
        assert_eq!(
            store
                .resolve_task_proof_env_overrides(&squad, 0)
                .unwrap()
                .get("A"),
            Some(&"task-proof".to_string())
        );
        // The plain (non-proof) task resolution is untouched by the
        // task-proof-only override.
        assert_eq!(
            store
                .resolve_cell_env_overrides(&squad, 0, 0)
                .unwrap()
                .get("A"),
            Some(&"task".to_string())
        );
    }

    #[test]
    fn resolve_cell_proof_env_overrides_precedence_full_chain() {
        let mut store = Store::open_in_memory().unwrap();
        let squad = store
            .insert_squad(&parse(two_task_two_cell_toml()), Some("r"), false)
            .unwrap();

        let mut squad_set = BTreeMap::new();
        squad_set.insert("A".to_string(), "squad".to_string());
        store
            .set_squad_env_overrides(&squad, &squad_set, &[])
            .unwrap();
        let mut task_set = BTreeMap::new();
        task_set.insert("A".to_string(), "task".to_string());
        store
            .set_task_env_overrides(&squad, 0, &task_set, &[])
            .unwrap();
        let mut cell_set = BTreeMap::new();
        cell_set.insert("A".to_string(), "cell".to_string());
        store
            .set_cell_env_overrides(&squad, 0, 0, &cell_set, &[])
            .unwrap();
        // No cell-proof value yet -- inherits the cell's.
        assert_eq!(
            store
                .resolve_cell_proof_env_overrides(&squad, 0, 0)
                .unwrap()
                .get("A"),
            Some(&"cell".to_string())
        );

        let mut proof_set = BTreeMap::new();
        proof_set.insert("A".to_string(), "cell-proof".to_string());
        store
            .set_cell_proof_env_overrides(&squad, 0, 0, &proof_set, &[])
            .unwrap();
        assert_eq!(
            store
                .resolve_cell_proof_env_overrides(&squad, 0, 0)
                .unwrap()
                .get("A"),
            Some(&"cell-proof".to_string())
        );
        // The plain cell resolution is untouched by the cell-proof-only
        // override, and a sibling cell's proof resolution never sees it.
        assert_eq!(
            store
                .resolve_cell_env_overrides(&squad, 0, 0)
                .unwrap()
                .get("A"),
            Some(&"cell".to_string())
        );
        assert_eq!(
            store
                .resolve_cell_proof_env_overrides(&squad, 0, 1)
                .unwrap()
                .get("A"),
            Some(&"task".to_string())
        );
    }

    #[test]
    fn get_task_cell_ids_returns_all_cells_ordered() {
        let mut store = Store::open_in_memory().unwrap();
        let toml = "[[task]]\nname=\"t\"\n\
            [[task.cell]]\nid=\"s1\"\ncwd=\"/r\"\nprompt=\"p\"\n\
            [[task.cell]]\nid=\"s2\"\ncwd=\"/r\"\nprompt=\"p\"\n\
            [[task]]\nname=\"u\"\n[[task.cell]]\nid=\"other\"\ncwd=\"/r\"\nprompt=\"p\"\n";
        let squad = store.insert_squad(&parse(toml), Some("r"), false).unwrap();
        assert_eq!(
            store.get_task_cell_ids(&squad, 0).unwrap(),
            vec![(0, "s1".to_string()), (1, "s2".to_string())]
        );
        assert_eq!(
            store.get_task_cell_ids(&squad, 1).unwrap(),
            vec![(0, "other".to_string())]
        );
    }

    #[test]
    fn get_task_cell_ids_empty_for_unknown_task() {
        let mut store = Store::open_in_memory().unwrap();
        let squad = store.insert_squad(&parse(SAMPLE), None, false).unwrap();
        assert_eq!(store.get_task_cell_ids(&squad, 99).unwrap(), Vec::new());
    }

    #[test]
    fn record_cell_result_does_not_clobber_a_manually_finalized_cell() {
        // RAL-163: a manual set-status override (server::capture_and_stop_node)
        // can finalize a cell's state while the scheduler's own runner call
        // for it is still in flight. When that call eventually unblocks and
        // reaches `record_cell_result`, it must not stomp the manual
        // override back to whatever the runner actually returned.
        // Deliberately no cell-level proof step here (unlike SAMPLE) --
        // `get_squad`'s view folds a raw `done` state through
        // `effective_cell_state`, which would otherwise mask the very
        // column this test is asserting on.
        let toml = "[[task]]\nname=\"t\"\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\n";
        let mut store = Store::open_in_memory().unwrap();
        let squad = store.insert_squad(&parse(toml), Some("r"), false).unwrap();
        store
            .set_cell_state(&squad, 0, 0, NodeState::Running)
            .unwrap();

        // The user manually finalizes it to `done` while the agent (unknown to
        // the store) is still actually running.
        store.set_cell_state(&squad, 0, 0, NodeState::Done).unwrap();

        // The scheduler's in-flight runner call finally returns -- too late,
        // the node is no longer `running`/`pending`, so this must be a no-op.
        let outcome = CellOutcome {
            state: NodeState::Failed,
            usage: RecordedUsage {
                tokens_in: 7,
                tokens_out: 9,
                cost_usd: 1.5,
                ..RecordedUsage::default()
            },
            error: Some("late result".to_string()),
            agent_session_id: None,
        };
        store.record_cell_result(&squad, 0, 0, &outcome).unwrap();

        assert_eq!(
            store.cell_state(&squad, 0, 0).unwrap(),
            Some(NodeState::Done),
            "the manual override must stick, not be overwritten by the late runner result"
        );
        let squad_view = store.get_squad(&squad).unwrap();
        let cell = &squad_view.tasks[0].cells[0];
        assert_eq!(
            cell.tokens_in, 0,
            "late outcome fields must not land either"
        );
        assert!(cell.error.is_none());
    }

    #[test]
    fn record_cell_result_applies_normally_when_cell_is_still_running() {
        let toml = "[[task]]\nname=\"t\"\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\n";
        let mut store = Store::open_in_memory().unwrap();
        let squad = store.insert_squad(&parse(toml), Some("r"), false).unwrap();
        store
            .set_cell_state(&squad, 0, 0, NodeState::Running)
            .unwrap();
        let outcome = CellOutcome {
            state: NodeState::Done,
            usage: RecordedUsage {
                tokens_in: 3,
                tokens_out: 4,
                cost_usd: 0.1,
                ..RecordedUsage::default()
            },
            error: None,
            agent_session_id: None,
        };
        store.record_cell_result(&squad, 0, 0, &outcome).unwrap();
        assert_eq!(
            store.cell_state(&squad, 0, 0).unwrap(),
            Some(NodeState::Done)
        );
        let squad_view = store.get_squad(&squad).unwrap();
        let cell = &squad_view.tasks[0].cells[0];
        assert_eq!(cell.tokens_in, 3);
    }

    /// RAL-326: prompt-cache tokens and the estimated-cost marker must survive
    /// the round trip through `cells` and reach the board's `CellView`. They
    /// are stored beside `tokens_in`, never folded into it -- an agentic cell
    /// bills most of its input through the cache tiers, and folding would
    /// silently redefine every existing `tokens_in` reading.
    #[test]
    fn record_cell_result_round_trips_cache_tokens_and_the_estimate_marker() {
        let toml = "[[task]]\nname=\"t\"\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\n";
        let mut store = Store::open_in_memory().unwrap();
        let squad = store.insert_squad(&parse(toml), Some("r"), false).unwrap();
        store
            .set_cell_state(&squad, 0, 0, NodeState::Running)
            .unwrap();
        let outcome = CellOutcome {
            state: NodeState::Failed,
            usage: RecordedUsage {
                tokens_in: 34,
                tokens_out: 5374,
                cache_creation_tokens: 320_114,
                cache_read_tokens: 7_204_990,
                compaction_input_tokens: 115_000,
                compaction_count: 1,
                cost_usd: 0.6807,
                cost_is_estimated: true,
            },
            error: Some("lost pane".to_string()),
            agent_session_id: None,
        };
        store.record_cell_result(&squad, 0, 0, &outcome).unwrap();

        let squad_view = store.get_squad(&squad).unwrap();
        let cell = &squad_view.tasks[0].cells[0];
        assert_eq!(cell.tokens_in, 34, "uncached input is unchanged in meaning");
        assert_eq!(cell.tokens_out, 5374);
        assert_eq!(cell.cache_creation_tokens, 320_114);
        assert_eq!(cell.cache_read_tokens, 7_204_990);
        // RAL-373: the same round trip, for the columns this ticket adds.
        assert_eq!(cell.compaction_input_tokens, 115_000);
        assert_eq!(cell.compaction_count, 1);
        assert!(
            cell.cost_is_estimated,
            "a snapshot-derived figure must not read as a settled bill"
        );
    }

    /// The proof-step twin of the round trip above -- `set_proof_result` is a
    /// separate write path with its own columns, so it needs its own proof.
    #[test]
    fn set_proof_result_round_trips_cache_tokens_and_the_estimate_marker() {
        let toml = concat!(
            "[[task]]\nname=\"t\"\n",
            "[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\n",
            "[[task.cell.proof]]\nkind=\"prompt\"\nprompt=\"check\"\n"
        );
        let mut store = Store::open_in_memory().unwrap();
        let squad = store.insert_squad(&parse(toml), Some("r"), false).unwrap();
        store
            .set_proof_result(
                &squad,
                0,
                "cell",
                0,
                0,
                NodeState::Done,
                "PASS",
                None,
                RecordedUsage {
                    tokens_in: 11,
                    tokens_out: 22,
                    cache_creation_tokens: 33,
                    cache_read_tokens: 44,
                    compaction_input_tokens: 55,
                    compaction_count: 2,
                    cost_usd: 0.5,
                    cost_is_estimated: true,
                },
            )
            .unwrap();

        let steps = store.proofs_for(&squad, 0, "cell", 0).unwrap();
        let step = &steps[0];
        assert_eq!(step.tokens_in, 11);
        assert_eq!(step.tokens_out, 22);
        assert_eq!(step.cache_creation_tokens, 33);
        assert_eq!(step.cache_read_tokens, 44);
        // RAL-373: the same round trip, for the columns this ticket adds.
        assert_eq!(step.compaction_input_tokens, 55);
        assert_eq!(step.compaction_count, 2);
        assert!(step.cost_is_estimated);
    }

    #[test]
    fn record_cell_result_applies_to_a_never_started_pending_cell() {
        // Mirrors the "blocked by a failed dependency" scheduler path: the
        // cell never left `pending` before its outcome is recorded.
        let mut store = Store::open_in_memory().unwrap();
        let squad = store
            .insert_squad(&parse(SAMPLE), Some("r"), false)
            .unwrap();
        let outcome = CellOutcome {
            state: NodeState::Failed,
            usage: crate::store::RecordedUsage::default(),
            error: Some("blocked by a failed dependency".to_string()),
            agent_session_id: None,
        };
        store.record_cell_result(&squad, 0, 0, &outcome).unwrap();
        let squad_view = store.get_squad(&squad).unwrap();
        let cell = &squad_view.tasks[0].cells[0];
        assert_eq!(cell.state, "failed");
        assert_eq!(
            cell.error.as_deref(),
            Some("blocked by a failed dependency")
        );
    }

    #[test]
    fn done_cells_excludes_cells_with_pending_cell_proofs() {
        // RAL-64: if the daemon stopped between record_cell_result and
        // run_proofs, the cell is 'done' in the DB but its proofs are
        // still 'pending'. done_cells must NOT return such a cell, so that
        // the scheduler re-runs the worker and the proofs actually execute.
        let mut store = Store::open_in_memory().unwrap();
        let toml = "[[task]]\nname=\"t\"\n\
            [[task.cell]]\nid=\"s1\"\ncwd=\"/r\"\nprompt=\"p\"\n\
            [[task.cell.proof]]\ncommand=\"exit 0\"\n";
        let squad = store.insert_squad(&parse(toml), Some("r"), false).unwrap();

        // Simulate the crash window: cell is Done, proof is still Pending.
        store.set_cell_state(&squad, 0, 0, NodeState::Done).unwrap();
        // Proof is inserted as 'pending' by insert_squad — leave it as-is.

        let done = store.done_cells(&squad).unwrap();
        assert!(
            !done.contains(&(0, 0)),
            "cell with pending proof must not be in done_cells"
        );

        // After completing the proof, the cell is returned.
        store
            .set_proof_state(&squad, 0, "cell", 0, 0, NodeState::Done)
            .unwrap();
        let done = store.done_cells(&squad).unwrap();
        assert!(
            done.contains(&(0, 0)),
            "cell with completed proof must be in done_cells"
        );
    }

    #[test]
    fn failed_cells_returns_only_cells_in_failed_state() {
        let mut store = Store::open_in_memory().unwrap();
        let toml = "[[task]]\nname=\"a\"\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\n\
            [[task]]\nname=\"b\"\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\n\
            [[task]]\nname=\"c\"\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\n";
        let squad = store.insert_squad(&parse(toml), Some("r"), false).unwrap();
        store
            .set_cell_state(&squad, 0, 0, NodeState::Failed)
            .unwrap();
        store.set_cell_state(&squad, 1, 0, NodeState::Done).unwrap();
        // task_idx 2 stays Pending (insert_squad's default).

        let failed = store.failed_cells(&squad).unwrap();
        assert_eq!(failed, HashSet::from([(0, 0)]));
    }

    /// RAL-185: the seed the scheduler needs so a squad-level cancel's leftovers
    /// stay terminal. Mirrors `failed_cells_returns_only_cells_in_failed_state`.
    #[test]
    fn cancelled_cells_and_tasks_return_only_rows_in_cancelled_state() {
        let mut store = Store::open_in_memory().unwrap();
        let toml = "[[task]]\nname=\"a\"\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\n\
            [[task]]\nname=\"b\"\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\n\
            [[task]]\nname=\"c\"\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\n";
        let squad = store.insert_squad(&parse(toml), Some("r"), false).unwrap();
        // Task 0 finished for real before the cancel; task 1 was still in
        // flight and got flipped; task 2 never started and got flipped too.
        store.set_cell_state(&squad, 0, 0, NodeState::Done).unwrap();
        store.set_task_state(&squad, 0, NodeState::Done).unwrap();
        store
            .set_cell_state(&squad, 1, 0, NodeState::Running)
            .unwrap();
        store.set_task_state(&squad, 1, NodeState::Running).unwrap();

        store.cancel(&squad).unwrap();

        assert_eq!(
            store.cancelled_cells(&squad).unwrap(),
            HashSet::from([(1, 0), (2, 0)]),
            "the already-Done cell must be left alone by a squad-level cancel"
        );
        assert_eq!(
            store.cancelled_tasks(&squad).unwrap(),
            HashSet::from([1, 2]),
            "cancel_nonterminal_nodes flips tasks alongside cells"
        );
    }

    #[test]
    fn squad_terminal_state_precedence() {
        assert_eq!(Store::squad_terminal_state(false, false), SquadState::Done);
        assert_eq!(
            Store::squad_terminal_state(false, true),
            SquadState::Cancelled
        );
        assert_eq!(Store::squad_terminal_state(true, false), SquadState::Failed);
        assert_eq!(Store::squad_terminal_state(true, true), SquadState::Failed);
    }

    /// RAL-315: cancelling a squad's tasks one at a time via `set_status`
    /// (outside the scheduler's own dispatch loop) must still reach
    /// `cancelled` once every task has landed there -- matching what a
    /// whole-squad cancel or the scheduler's own end-of-dispatch aggregation
    /// would report.
    #[test]
    fn reconcile_squad_cancellation_flips_squad_once_every_task_is_terminal() {
        let mut store = Store::open_in_memory().unwrap();
        let toml = "[[task]]\nname=\"a\"\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\n\
            [[task]]\nname=\"b\"\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\n";
        let squad = store.insert_squad(&parse(toml), Some("r"), false).unwrap();
        store.set_squad_state(&squad, SquadState::Running).unwrap();

        store
            .set_task_state(&squad, 0, NodeState::Cancelled)
            .unwrap();
        store.reconcile_squad_cancellation(&squad).unwrap();
        assert_eq!(
            store.squad_state(&squad).unwrap(),
            SquadState::Running,
            "a still-pending sibling task must block reconciliation"
        );

        store
            .set_task_state(&squad, 1, NodeState::Cancelled)
            .unwrap();
        store.reconcile_squad_cancellation(&squad).unwrap();
        assert_eq!(store.squad_state(&squad).unwrap(), SquadState::Cancelled);
    }

    #[test]
    fn reconcile_squad_cancellation_lets_failed_task_win_over_cancelled() {
        let mut store = Store::open_in_memory().unwrap();
        let toml = "[[task]]\nname=\"a\"\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\n\
            [[task]]\nname=\"b\"\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\n";
        let squad = store.insert_squad(&parse(toml), Some("r"), false).unwrap();
        store.set_squad_state(&squad, SquadState::Running).unwrap();
        store.set_task_state(&squad, 0, NodeState::Failed).unwrap();
        store
            .set_task_state(&squad, 1, NodeState::Cancelled)
            .unwrap();

        store.reconcile_squad_cancellation(&squad).unwrap();
        assert_eq!(store.squad_state(&squad).unwrap(), SquadState::Failed);
    }

    #[test]
    fn reconcile_squad_cancellation_is_a_noop_without_a_cancelled_task() {
        let mut store = Store::open_in_memory().unwrap();
        let toml = "[[task]]\nname=\"a\"\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\n";
        let squad = store.insert_squad(&parse(toml), Some("r"), false).unwrap();
        store.set_squad_state(&squad, SquadState::Running).unwrap();
        store.set_task_state(&squad, 0, NodeState::Done).unwrap();

        store.reconcile_squad_cancellation(&squad).unwrap();
        assert_eq!(
            store.squad_state(&squad).unwrap(),
            SquadState::Running,
            "an ordinary completion is the scheduler's own job to report, not this reconciliation"
        );
    }

    /// RAL-185 AC: a whole-squad restart must still revive cancelled cells.
    /// `reset_squad_to_pending` rewrites *every* row, so by the time the
    /// scheduler reads its seeds there is nothing left in `cancelled` state and
    /// the new `cancelled_cells`/`cancelled_tasks` seeds are inert.
    #[test]
    fn restart_squad_clears_cancelled_state_so_the_scheduler_seeds_are_empty() {
        let mut store = Store::open_in_memory().unwrap();
        let toml = "[[task]]\nname=\"a\"\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\n\
            [[task]]\nname=\"b\"\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\n";
        let squad = store.insert_squad(&parse(toml), Some("r"), false).unwrap();
        store
            .set_cell_state(&squad, 0, 0, NodeState::Running)
            .unwrap();
        store.cancel(&squad).unwrap();
        assert!(!store.cancelled_cells(&squad).unwrap().is_empty());

        store.restart_squad(&squad).unwrap();

        assert!(
            store.cancelled_cells(&squad).unwrap().is_empty(),
            "restart_squad must leave no cell cancelled, or the scheduler \
             would refuse to dispatch it"
        );
        assert!(store.cancelled_tasks(&squad).unwrap().is_empty());
    }

    #[test]
    fn restart_squad_on_missing_squad_is_not_found() {
        let store = Store::open_in_memory().unwrap();
        assert!(matches!(
            store.restart_squad("nope"),
            Err(StoreError::NotFound)
        ));
    }

    #[test]
    fn delete_squad_removes_it_and_children() {
        let mut store = Store::open_in_memory().unwrap();
        let id = store
            .insert_squad(&parse(SAMPLE), Some("gone"), false)
            .unwrap();
        store.delete_squad(&id).unwrap();
        assert!(matches!(store.get_squad(&id), Err(StoreError::NotFound)));
        // The child rows are gone too, so a re-fetch of cells is empty.
        assert!(store.cells_of(&id).unwrap().is_empty());
        // Deleting again is a NotFound.
        assert!(matches!(store.delete_squad(&id), Err(StoreError::NotFound)));
    }

    #[test]
    fn cell_lists_the_reviews_its_branch_participates_in() {
        // RAL-17: a cell whose review branch is in a guardian's stack (both
        // tied to the same squad) lists that review in its board view.
        let mut store = Store::open_in_memory().unwrap();
        let squad = store
            .insert_squad(&parse(SAMPLE), Some("r"), false)
            .unwrap();
        let gid = store
            .create_guardian_for_squad("Backend review", "main", "/repo", Some(&squad))
            .unwrap();
        store.add_guardian_branch(&gid, "feature/a").unwrap();
        // The single SAMPLE cell is task 0, cell 0.
        store
            .set_cell_review_branch(&squad, 0, 0, "feature/a")
            .unwrap();

        let view = store.get_squad(&squad).unwrap();
        let cell = &view.tasks[0].cells[0];
        assert_eq!(cell.reviews.len(), 1);
        assert_eq!(cell.reviews[0].id, gid);
        assert_eq!(cell.reviews[0].name, "Backend review");
        // A cell with no review branch lists nothing.
        let squad2 = store
            .insert_squad(&parse(SAMPLE), Some("r2"), false)
            .unwrap();
        let view2 = store.get_squad(&squad2).unwrap();
        assert!(view2.tasks[0].cells[0].reviews.is_empty());
    }

    #[test]
    fn cell_lists_a_review_whose_guardian_was_created_by_a_different_squad() {
        // A guardian created by one squad's submission (its `squad_id` column) can
        // later be *found* rather than created for a second squad that shares a
        // `ralphus:new-review/<key>` link (or was attached to manually) — its
        // `squad_id` still points at the first squad, but the second squad's cell
        // branch is appended to it. The "in reviews" lookup must match by
        // `cells.review_branch = guardian_branches.branch`, not by
        // `guardians.squad_id`, or the second squad's cell sees no review at all.
        let mut store = Store::open_in_memory().unwrap();
        let squad_a = store
            .insert_squad(&parse(SAMPLE), Some("a"), false)
            .unwrap();
        let gid = store
            .create_guardian_for_squad("Shared review", "main", "/repo", Some(&squad_a))
            .unwrap();
        store.add_guardian_branch(&gid, "feature/b").unwrap();

        let squad_b = store
            .insert_squad(&parse(SAMPLE), Some("b"), false)
            .unwrap();
        store
            .set_cell_review_branch(&squad_b, 0, 0, "feature/b")
            .unwrap();

        let view_b = store.get_squad(&squad_b).unwrap();
        let cell = &view_b.tasks[0].cells[0];
        assert_eq!(cell.reviews.len(), 1);
        assert_eq!(cell.reviews[0].id, gid);
        assert_eq!(cell.reviews[0].name, "Shared review");
    }

    #[test]
    fn list_squads_newest_first() {
        let mut store = Store::open_in_memory().unwrap();
        let _a = store
            .insert_squad(&parse(SAMPLE), Some("a"), false)
            .unwrap();
        let b = store
            .insert_squad(&parse(SAMPLE), Some("b"), false)
            .unwrap();
        let squads = store.list_squads().unwrap();
        assert_eq!(squads.len(), 2);
        assert_eq!(squads[0].id, b); // newest first
    }

    #[test]
    fn state_transitions_are_logged_and_cleaned_up() {
        let mut store = Store::open_in_memory().unwrap();
        let id = store.insert_squad(&parse(SAMPLE), None, false).unwrap();
        store.set_squad_state(&id, SquadState::Running).unwrap();
        store.set_squad_state(&id, SquadState::Done).unwrap();
        let events = store.events_for_squad(&id, 100).unwrap();
        // Oldest-first, and the last transition is "done".
        assert!(
            events
                .iter()
                .any(|e| e.scope == "squad" && e.message.contains("running"))
        );
        assert_eq!(events.last().unwrap().message, "squad → done");
        // Deleting the squad removes its events.
        store.delete_squad(&id).unwrap();
        assert!(store.events_for_squad(&id, 100).unwrap().is_empty());
    }

    #[test]
    fn squad_state_stamps_started_and_finished_at() {
        let mut store = Store::open_in_memory().unwrap();
        let id = store.insert_squad(&parse(SAMPLE), None, false).unwrap();
        let squad = store.get_squad(&id).unwrap();
        assert!(squad.started_at_ms.is_none());
        assert!(squad.finished_at_ms.is_none());

        store.set_squad_state(&id, SquadState::Running).unwrap();
        let squad = store.get_squad(&id).unwrap();
        let started = squad.started_at_ms.expect("started_at_ms set on running");
        assert!(squad.finished_at_ms.is_none());

        store.set_squad_state(&id, SquadState::Done).unwrap();
        let squad = store.get_squad(&id).unwrap();
        // started_at_ms is untouched by the terminal transition.
        assert_eq!(squad.started_at_ms, Some(started));
        assert!(squad.finished_at_ms.is_some());
    }

    #[test]
    fn squad_state_started_at_is_not_overwritten_by_re_entering_running() {
        let mut store = Store::open_in_memory().unwrap();
        let id = store.insert_squad(&parse(SAMPLE), None, false).unwrap();
        store.set_squad_state(&id, SquadState::Running).unwrap();
        let first_started = store.get_squad(&id).unwrap().started_at_ms.unwrap();

        // A squad doesn't normally re-enter `running` without a restart in
        // between, but the setter must be idempotent regardless.
        store.set_squad_state(&id, SquadState::Running).unwrap();
        assert_eq!(
            store.get_squad(&id).unwrap().started_at_ms,
            Some(first_started)
        );
    }

    #[test]
    fn old_timestamps_do_not_make_running_work_look_unhealthy() {
        let mut store = Store::open_in_memory().unwrap();
        let id = store
            .insert_squad(&parse(SAMPLE), Some("slow"), false)
            .unwrap();
        store.set_squad_state(&id, SquadState::Running).unwrap();
        store.set_task_state(&id, 0, NodeState::Running).unwrap();
        store.set_cell_state(&id, 0, 0, NodeState::Running).unwrap();

        // Simulate a legitimately long-running task entirely by rewriting the
        // persisted clocks, rather than by waiting in real time.
        store
            .conn
            .execute(
                "UPDATE squads SET created_at_ms=0, updated_at_ms=0 WHERE id=?",
                params![id],
            )
            .unwrap();
        store
            .conn
            .execute("UPDATE events SET at_ms=0 WHERE squad_id=?", params![id])
            .unwrap();
        store
            .conn
            .execute(
                "UPDATE cartographer_events SET at_ms=0 WHERE squad_id=?",
                params![id],
            )
            .unwrap();

        let squad = store.get_squad(&id).unwrap();
        assert_eq!(squad.created_at_ms, 0);
        assert_eq!(squad.state, "running");
        assert_eq!(squad.tasks[0].state, "running");
        assert_eq!(squad.tasks[0].cells[0].state, "running");
        assert!(squad.tasks[0].cells[0].error.is_none());

        let events = store.events_for_squad(&id, 100).unwrap();
        assert!(
            !events.is_empty(),
            "running work should still have event history"
        );
        assert!(events.iter().all(|e| e.at_ms == 0));

        let carto = store
            .cartographer_query(&crate::cartographer::CartographerFilter {
                squad_id: Some(id.clone()),
                limit: 100,
                ..Default::default()
            })
            .unwrap();
        assert!(
            carto.total > 0,
            "running work should still surface in Cartographer"
        );
        assert!(carto.rows.iter().all(|row| row.at_ms == 0));
        assert!(
            carto.rows.iter().any(|row| row.message.contains("running")),
            "the synthetic age must not erase the underlying running transition"
        );
    }

    #[test]
    fn task_and_cell_state_stamp_timestamps() {
        let mut store = Store::open_in_memory().unwrap();
        let id = store.insert_squad(&parse(SAMPLE), None, false).unwrap();

        store.set_task_state(&id, 0, NodeState::Running).unwrap();
        store.set_cell_state(&id, 0, 0, NodeState::Running).unwrap();
        let squad = store.get_squad(&id).unwrap();
        assert!(squad.tasks[0].started_at_ms.is_some());
        assert!(squad.tasks[0].finished_at_ms.is_none());
        assert!(squad.tasks[0].cells[0].started_at_ms.is_some());
        assert!(squad.tasks[0].cells[0].finished_at_ms.is_none());

        store.set_task_state(&id, 0, NodeState::Done).unwrap();
        store.set_cell_state(&id, 0, 0, NodeState::Done).unwrap();
        let squad = store.get_squad(&id).unwrap();
        assert!(squad.tasks[0].finished_at_ms.is_some());
        assert!(squad.tasks[0].cells[0].finished_at_ms.is_some());
    }

    #[test]
    fn record_cell_result_stamps_finished_at() {
        let mut store = Store::open_in_memory().unwrap();
        let id = store.insert_squad(&parse(SAMPLE), None, false).unwrap();
        store.set_cell_state(&id, 0, 0, NodeState::Running).unwrap();

        store
            .record_cell_result(
                &id,
                0,
                0,
                &CellOutcome {
                    state: NodeState::Done,
                    usage: RecordedUsage {
                        tokens_in: 1,
                        tokens_out: 2,
                        cost_usd: 0.0,
                        ..RecordedUsage::default()
                    },
                    error: None,
                    agent_session_id: None,
                },
            )
            .unwrap();
        let squad = store.get_squad(&id).unwrap();
        assert!(squad.tasks[0].cells[0].started_at_ms.is_some());
        assert!(squad.tasks[0].cells[0].finished_at_ms.is_some());
    }

    #[test]
    fn restart_squad_clears_started_and_finished_at() {
        let mut store = Store::open_in_memory().unwrap();
        let id = store.insert_squad(&parse(SAMPLE), None, false).unwrap();
        store.set_squad_state(&id, SquadState::Running).unwrap();
        store.set_task_state(&id, 0, NodeState::Running).unwrap();
        store.set_cell_state(&id, 0, 0, NodeState::Running).unwrap();
        store.set_cell_state(&id, 0, 0, NodeState::Done).unwrap();
        store.set_task_state(&id, 0, NodeState::Done).unwrap();
        store.set_squad_state(&id, SquadState::Done).unwrap();

        let squad = store.get_squad(&id).unwrap();
        assert!(squad.started_at_ms.is_some());
        assert!(squad.finished_at_ms.is_some());

        store.restart_squad(&id).unwrap();
        let squad = store.get_squad(&id).unwrap();
        assert!(squad.started_at_ms.is_none());
        assert!(squad.finished_at_ms.is_none());
        assert!(squad.tasks[0].started_at_ms.is_none());
        assert!(squad.tasks[0].finished_at_ms.is_none());
        assert!(squad.tasks[0].cells[0].started_at_ms.is_none());
        assert!(squad.tasks[0].cells[0].finished_at_ms.is_none());
    }

    #[test]
    fn restart_cell_clears_its_own_timestamps_but_not_the_squads_started_at() {
        let mut store = Store::open_in_memory().unwrap();
        let id = store.insert_squad(&parse(SAMPLE), None, false).unwrap();
        store.set_squad_state(&id, SquadState::Running).unwrap();
        store.set_task_state(&id, 0, NodeState::Running).unwrap();
        store.set_cell_state(&id, 0, 0, NodeState::Running).unwrap();
        store.set_cell_state(&id, 0, 0, NodeState::Done).unwrap();
        store.set_task_state(&id, 0, NodeState::Done).unwrap();
        store.set_squad_state(&id, SquadState::Done).unwrap();

        let squad_started = store.get_squad(&id).unwrap().started_at_ms.unwrap();

        store.restart_cell(&id, 0, 0).unwrap();
        let squad = store.get_squad(&id).unwrap();
        // The overall squad already started earlier and isn't restarting from
        // scratch, so its own started_at_ms survives...
        assert_eq!(squad.started_at_ms, Some(squad_started));
        // ...but it's no longer finished, and the restarted cell/task
        // genuinely are starting over.
        assert!(squad.finished_at_ms.is_none());
        assert!(squad.tasks[0].started_at_ms.is_none());
        assert!(squad.tasks[0].finished_at_ms.is_none());
        assert!(squad.tasks[0].cells[0].started_at_ms.is_none());
        assert!(squad.tasks[0].cells[0].finished_at_ms.is_none());
    }

    #[test]
    fn restart_cell_proof_keeps_cell_done_but_resets_its_proofs() {
        let mut store = Store::open_in_memory().unwrap();
        let id = store.insert_squad(&parse(SAMPLE), None, false).unwrap();
        // Simulate a completed cell + proof.
        store.set_cell_state(&id, 0, 0, NodeState::Done).unwrap();
        store
            .set_proof_state(&id, 0, "cell", 0, 0, NodeState::Done)
            .unwrap();
        store.set_squad_state(&id, SquadState::Done).unwrap();

        store.restart_cell_proof(&id, 0, 0, 0).unwrap();

        let squad = store.get_squad(&id).unwrap();
        assert_eq!(
            squad.state, "pending",
            "squad must be pending after restart"
        );
        assert_eq!(squad.tasks[0].state, "pending", "task must be pending");
        // The persisted cell body stays Done (only the proof row is reset —
        // see `done_cells`, which relies on this for crash-recovery), but the
        // displayed state folds proof progress back in so the board doesn't
        // show the cell as finished while its proof re-runs.
        assert_eq!(
            squad.tasks[0].cells[0].state, "running",
            "displayed cell state must reflect its pending proof"
        );
        assert_eq!(
            squad.tasks[0].cells[0].proof[0].state, "pending",
            "cell proof must be pending"
        );
    }

    #[test]
    fn squad_view_shows_running_when_a_deferred_restart_leaves_pending_but_a_sibling_task_is_live()
    {
        // Reproduces the squad-000000000148/ral-169+ral-170 case: restarting
        // ral-169's terminal `test` proof resets the *squad* row to "pending"
        // (see `restart_cell_proof`) purely as a "reclaim me later"
        // signal — `scheduler::claim_ready` deliberately leaves ral-170's
        // still-live worker alone rather than cancelling it. From the
        // outside the squad plainly has a live child (ral-170's cell is
        // genuinely `running`), so the board must not show "pending".
        let mut store = Store::open_in_memory().unwrap();
        let toml = "[[task]]\nname=\"a\"\n\
            [[task.cell]]\nid=\"work\"\ncwd=\"/r\"\nprompt=\"p\"\n[[task.cell.proof]]\nid=\"test\"\ncommand=\"true\"\n\
            [[task]]\nname=\"b\"\n\
            [[task.cell]]\nid=\"work\"\ncwd=\"/r\"\nprompt=\"p\"\n";
        let id = store.insert_squad(&parse(toml), Some("r"), false).unwrap();

        // Task a: completed once, its `test` proof failed, squad finished Failed.
        store.set_cell_state(&id, 0, 0, NodeState::Done).unwrap();
        store
            .set_proof_state(&id, 0, "cell", 0, 0, NodeState::Failed)
            .unwrap();
        store.set_task_state(&id, 0, NodeState::Failed).unwrap();
        store.set_squad_state(&id, SquadState::Failed).unwrap();

        // Task b: its worker is still actively driving this cell (the
        // sibling task the deferred-claim mechanism refuses to interrupt).
        store.set_cell_state(&id, 1, 0, NodeState::Running).unwrap();

        // Restart task a's `test` proof. Per `restart_cell_proof`, this
        // writes the squad row back to "pending" even though task b's cell
        // is still genuinely running.
        store.restart_cell_proof(&id, 0, 0, 0).unwrap();

        let squad = store.get_squad(&id).unwrap();
        assert_eq!(
            squad.state, "running",
            "a squad with a genuinely live sibling cell must not display as pending, \
             even though the scheduler's own squads.state column reads pending while it \
             waits to reclaim the restarted task"
        );
    }

    #[test]
    fn restart_cell_proof_on_missing_cell_is_not_found() {
        let mut store = Store::open_in_memory().unwrap();
        let id = store.insert_squad(&parse(SAMPLE), None, false).unwrap();
        assert!(matches!(
            store.restart_cell_proof(&id, 0, 99, 0),
            Err(StoreError::NotFound)
        ));
    }

    #[test]
    fn restart_cell_proof_revives_downstream_cell_left_failed_by_cascade() {
        // Mirrors RAL-159/squad-000000000147: "work" -> "finalize" in one task.
        // work's own checks proof failed, which cascaded finalize (its
        // dependent) to Failed with "blocked by a failed dependency". Only
        // work's proof then gets retried (not a full cell restart) --
        // finalize must come back to Pending too, or a now-passing proof
        // can never actually unstick the task (RAL-165).
        let mut store = Store::open_in_memory().unwrap();
        let toml = "[[task]]\nname=\"t\"\n\
            [[task.cell]]\nid=\"work\"\ncwd=\"/r\"\nprompt=\"p\"\n[[task.cell.proof]]\nid=\"checks\"\ncommand=\"true\"\n\
            [[task.cell]]\nid=\"finalize\"\ncwd=\"/r\"\nprompt=\"p\"\ndepends_on=[\"work\"]\n";
        let squad = store.insert_squad(&parse(toml), Some("r"), false).unwrap();
        store.set_cell_state(&squad, 0, 0, NodeState::Done).unwrap(); // work's body succeeded
        store
            .set_proof_state(&squad, 0, "cell", 0, 0, NodeState::Failed)
            .unwrap(); // work's checks proof failed
        store
            .record_cell_result(
                &squad,
                0,
                1,
                &CellOutcome {
                    state: NodeState::Failed,
                    usage: crate::store::RecordedUsage::default(),
                    error: Some("blocked by a failed dependency".to_string()),
                    agent_session_id: None,
                },
            )
            .unwrap(); // finalize cascaded to Failed
        store.set_task_state(&squad, 0, NodeState::Failed).unwrap();
        store.set_squad_state(&squad, SquadState::Failed).unwrap();

        store.restart_cell_proof(&squad, 0, 0, 0).unwrap();

        let squad_view = store.get_squad(&squad).unwrap();
        let finalize = &squad_view.tasks[0].cells[1];
        assert_eq!(
            finalize.state, "pending",
            "finalize must be revived to pending, not left stuck failed"
        );
        assert_eq!(
            finalize.error, None,
            "finalize's stale cascade error must be cleared"
        );
    }

    #[test]
    fn restart_cell_proof_does_not_touch_a_done_downstream_cell() {
        // If the downstream cell already succeeded, a proof retry on its
        // upstream must not force it to redo work (e.g. re-commit).
        let mut store = Store::open_in_memory().unwrap();
        let toml = "[[task]]\nname=\"t\"\n\
            [[task.cell]]\nid=\"work\"\ncwd=\"/r\"\nprompt=\"p\"\n[[task.cell.proof]]\nid=\"checks\"\ncommand=\"true\"\n\
            [[task.cell]]\nid=\"finalize\"\ncwd=\"/r\"\nprompt=\"p\"\ndepends_on=[\"work\"]\n";
        let squad = store.insert_squad(&parse(toml), Some("r"), false).unwrap();
        store.set_cell_state(&squad, 0, 0, NodeState::Done).unwrap();
        store
            .set_proof_state(&squad, 0, "cell", 0, 0, NodeState::Done)
            .unwrap();
        store.set_cell_state(&squad, 0, 1, NodeState::Done).unwrap();
        store.set_task_state(&squad, 0, NodeState::Done).unwrap();
        store.set_squad_state(&squad, SquadState::Done).unwrap();

        store.restart_cell_proof(&squad, 0, 0, 0).unwrap();

        let squad_view = store.get_squad(&squad).unwrap();
        assert_eq!(
            squad_view.tasks[0].cells[1].state, "done",
            "an already-done downstream cell must not be reset"
        );
    }

    #[test]
    fn restart_task_proof_resets_only_task_proofs_cells_remain_done() {
        let mut store = Store::open_in_memory().unwrap();
        let id = store.insert_squad(&parse(SAMPLE), None, false).unwrap();
        // Simulate a task where cells passed but the task-level proof failed.
        store.set_cell_state(&id, 0, 0, NodeState::Done).unwrap();
        store
            .set_proof_state(&id, 0, "cell", 0, 0, NodeState::Done)
            .unwrap();
        store
            .set_proof_state(&id, 0, "task", -1, 0, NodeState::Failed)
            .unwrap();
        store.set_task_state(&id, 0, NodeState::Failed).unwrap();
        store.set_squad_state(&id, SquadState::Failed).unwrap();

        store.restart_task_proof(&id, 0, 0).unwrap();

        let squad = store.get_squad(&id).unwrap();
        assert_eq!(squad.state, "pending");
        assert_eq!(squad.tasks[0].state, "pending");
        // Cells and cell-level proofs are NOT reset — only task-level proofs are.
        assert_eq!(
            squad.tasks[0].cells[0].state, "done",
            "cell must remain done"
        );
        assert_eq!(
            squad.tasks[0].cells[0].proof[0].state, "done",
            "cell-level proof must remain done"
        );
        assert_eq!(squad.tasks[0].proof[0].state, "pending");
    }

    #[test]
    fn restart_task_proof_on_missing_task_is_not_found() {
        let mut store = Store::open_in_memory().unwrap();
        let id = store.insert_squad(&parse(SAMPLE), None, false).unwrap();
        assert!(matches!(
            store.restart_task_proof(&id, 99, 0),
            Err(StoreError::NotFound)
        ));
    }

    #[test]
    fn restart_task_proof_revives_downstream_cell_left_failed_by_cascade() {
        // Same RAL-165 gap as restart_cell_proof, but for a task-level
        // proof: task "t" -> cell "downstream" in a separate task,
        // dependent on t's own cell. t's task-level proof failed,
        // cascading "downstream" to Failed; retrying only t's task proof
        // must revive "downstream" too.
        let mut store = Store::open_in_memory().unwrap();
        let toml = "[[task]]\nname=\"t\"\n\
            [[task.cell]]\nid=\"work\"\ncwd=\"/r\"\nprompt=\"p\"\n[[task.proof]]\ncommand=\"true\"\n\
            [[task]]\nname=\"u\"\ndepends_on=[\"t\"]\n\
            [[task.cell]]\nid=\"downstream\"\ncwd=\"/r\"\nprompt=\"p\"\n";
        let squad = store.insert_squad(&parse(toml), Some("r"), false).unwrap();
        store.set_cell_state(&squad, 0, 0, NodeState::Done).unwrap();
        store
            .set_proof_state(&squad, 0, "task", -1, 0, NodeState::Failed)
            .unwrap();
        store.set_task_state(&squad, 0, NodeState::Failed).unwrap();
        store
            .record_cell_result(
                &squad,
                1,
                0,
                &CellOutcome {
                    state: NodeState::Failed,
                    usage: crate::store::RecordedUsage::default(),
                    error: Some("blocked by a failed dependency".to_string()),
                    agent_session_id: None,
                },
            )
            .unwrap();
        store.set_task_state(&squad, 1, NodeState::Failed).unwrap();
        store.set_squad_state(&squad, SquadState::Failed).unwrap();

        store.restart_task_proof(&squad, 0, 0).unwrap();

        let squad_view = store.get_squad(&squad).unwrap();
        let downstream = &squad_view.tasks[1].cells[0];
        assert_eq!(
            downstream.state, "pending",
            "downstream cell must be revived to pending"
        );
        assert_eq!(
            downstream.error, None,
            "downstream cell's stale cascade error must be cleared"
        );
    }

    // Three cell-level proof steps: restart from vi=1 leaves vi=0 Done,
    // resets vi=1 and vi=2 to Pending.
    const THREE_CELL_PROOFS: &str = r#"
[[task]]
name = "t"
[[task.cell]]
cwd = "."
command = "build"
[[task.cell.proof]]
command = "check-a"
[[task.cell.proof]]
command = "check-b"
[[task.cell.proof]]
command = "check-c"
"#;

    // Three task-level proof steps (no cell-level proofs).
    const THREE_TASK_PROOFS: &str = r#"
[[task]]
name = "t"
[[task.cell]]
cwd = "."
command = "build"
[[task.proof]]
command = "check-a"
[[task.proof]]
command = "check-b"
[[task.proof]]
command = "check-c"
"#;

    #[test]
    fn restart_cell_proof_from_middle_leaves_earlier_step_intact() {
        let mut store = Store::open_in_memory().unwrap();
        let id = store
            .insert_squad(&parse(THREE_CELL_PROOFS), None, false)
            .unwrap();
        // Simulate: cell done, all three proofs done.
        store.set_cell_state(&id, 0, 0, NodeState::Done).unwrap();
        for vi in 0..3i64 {
            store
                .set_proof_state(&id, 0, "cell", 0, vi, NodeState::Done)
                .unwrap();
        }
        store.set_squad_state(&id, SquadState::Done).unwrap();

        // Restart from vi=1 — only steps 1 and 2 should reset.
        store.restart_cell_proof(&id, 0, 0, 1).unwrap();

        let squad = store.get_squad(&id).unwrap();
        let vs = &squad.tasks[0].cells[0].proof;
        assert_eq!(vs[0].state, "done", "vi=0 must stay done");
        assert_eq!(vs[1].state, "pending", "vi=1 must be reset to pending");
        assert_eq!(vs[2].state, "pending", "vi=2 must be reset to pending");
        assert_eq!(squad.tasks[0].state, "pending");
        assert_eq!(squad.state, "pending");
    }

    #[test]
    fn restart_cell_proof_from_last_only_resets_that_step() {
        let mut store = Store::open_in_memory().unwrap();
        let id = store
            .insert_squad(&parse(THREE_CELL_PROOFS), None, false)
            .unwrap();
        store.set_cell_state(&id, 0, 0, NodeState::Done).unwrap();
        for vi in 0..3i64 {
            store
                .set_proof_state(&id, 0, "cell", 0, vi, NodeState::Done)
                .unwrap();
        }
        store.set_squad_state(&id, SquadState::Done).unwrap();

        // Restart from vi=2 — only the last step resets.
        store.restart_cell_proof(&id, 0, 0, 2).unwrap();

        let squad = store.get_squad(&id).unwrap();
        let vs = &squad.tasks[0].cells[0].proof;
        assert_eq!(vs[0].state, "done", "vi=0 must stay done");
        assert_eq!(vs[1].state, "done", "vi=1 must stay done");
        assert_eq!(vs[2].state, "pending", "vi=2 must be reset to pending");
    }

    #[test]
    fn restart_task_proof_from_middle_leaves_earlier_step_intact() {
        let mut store = Store::open_in_memory().unwrap();
        let id = store
            .insert_squad(&parse(THREE_TASK_PROOFS), None, false)
            .unwrap();
        // Simulate: cell done, all three task-level proofs done.
        store.set_cell_state(&id, 0, 0, NodeState::Done).unwrap();
        for vi in 0..3i64 {
            store
                .set_proof_state(&id, 0, "task", -1, vi, NodeState::Done)
                .unwrap();
        }
        store.set_task_state(&id, 0, NodeState::Done).unwrap();
        store.set_squad_state(&id, SquadState::Done).unwrap();

        // Restart from vi=1 — only steps 1 and 2 should reset.
        store.restart_task_proof(&id, 0, 1).unwrap();

        let squad = store.get_squad(&id).unwrap();
        let vs = &squad.tasks[0].proof;
        assert_eq!(vs[0].state, "done", "vi=0 must stay done");
        assert_eq!(vs[1].state, "pending", "vi=1 must be reset to pending");
        assert_eq!(vs[2].state, "pending", "vi=2 must be reset to pending");
        assert_eq!(squad.tasks[0].state, "pending");
        assert_eq!(squad.state, "pending");
        // Cell must be untouched.
        assert_eq!(squad.tasks[0].cells[0].state, "done");
    }

    // ── RAL Queue ────────────────────────────────────────────────────────────

    const TWO_CELL_CHAIN: &str = "[[task]]\nname=\"t\"\n\
        [[task.cell]]\nid=\"a\"\ncwd=\"/r\"\nprompt=\"p\"\n\
        [[task.cell]]\nid=\"b\"\ncwd=\"/r\"\nprompt=\"p\"\ndepends_on=[\"a\"]\n";

    #[test]
    fn ignored_state_round_trips() {
        assert_eq!(NodeState::parse("ignored"), Some(NodeState::Ignored));
        assert_eq!(NodeState::Ignored.as_str(), "ignored");
        assert_eq!(SquadState::parse("ignored"), Some(SquadState::Ignored));
        assert_eq!(SquadState::Ignored.as_str(), "ignored");
        assert!(NodeState::Ignored.satisfies_dependents());
        assert!(SquadState::Ignored.satisfies_dependents());
        assert!(!SquadState::Ignored.is_terminal(), "ignored is reversible");
    }

    #[test]
    fn ignored_upstream_squad_satisfies_dependents() {
        let mut store = Store::open_in_memory().unwrap();
        let a = store
            .insert_squad(&parse(SAMPLE), Some("a"), false)
            .unwrap();
        let dep = format!(
            "[[default]]\ndepends_on = [\"{a}\"]\n[[task]]\nname=\"b\"\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\n"
        );
        let b = store.insert_squad(&parse(&dep), Some("b"), false).unwrap();
        assert!(!store.list_ready().unwrap().contains(&b));
        // Ignoring the upstream squad unblocks the dependent, exactly like done.
        store.set_squad_state(&a, SquadState::Ignored).unwrap();
        assert!(store.list_ready().unwrap().contains(&b));
    }

    #[test]
    fn queue_lists_ready_and_blocked_cells() {
        let mut store = Store::open_in_memory().unwrap();
        let squad = store
            .insert_squad(&parse(TWO_CELL_CHAIN), Some("r"), false)
            .unwrap();
        let q = store.queue().unwrap();
        let a = q.iter().find(|i| i.path.ends_with("/s0")).unwrap();
        let b = q.iter().find(|i| i.path.ends_with("/s1")).unwrap();
        assert_eq!(a.readiness, "ready", "cell a has no deps");
        assert_eq!(b.readiness, "blocked", "cell b waits on a");
        assert_eq!(b.deps_paths, vec![cell_path(&squad, 0, 0)]);
    }

    #[test]
    fn queue_ignored_upstream_makes_downstream_ready() {
        let mut store = Store::open_in_memory().unwrap();
        let squad = store
            .insert_squad(&parse(TWO_CELL_CHAIN), None, false)
            .unwrap();
        // Ignore cell a → cell b becomes ready.
        store
            .set_cell_state(&squad, 0, 0, NodeState::Ignored)
            .unwrap();
        let q = store.queue().unwrap();
        // a is ignored (terminal-like) so it drops out of the queue; b is ready.
        assert!(q.iter().all(|i| !i.path.ends_with("/s0")));
        let b = q.iter().find(|i| i.path.ends_with("/s1")).unwrap();
        assert_eq!(b.readiness, "ready");
    }

    #[test]
    fn queue_exposes_task_depends_on() {
        let toml = "[[task]]\nname=\"a\"\n[[task.cell]]\ncwd=\"/r\"\ncommand=\"x\"\n\
                    [[task]]\nname=\"b\"\ndepends_on=[\"a\"]\n[[task.cell]]\ncwd=\"/r\"\ncommand=\"y\"\n";
        let mut store = Store::open_in_memory().unwrap();
        let _squad = store.insert_squad(&parse(toml), None, false).unwrap();
        let q = store.queue().unwrap();
        let b = q.iter().find(|i| i.task_name == "b").unwrap();
        assert_eq!(
            b.task_depends_on,
            vec!["a".to_string()],
            "task header shows its dep"
        );
        assert_eq!(b.readiness, "blocked");
        let a = q.iter().find(|i| i.task_name == "a").unwrap();
        assert!(a.task_depends_on.is_empty());
        assert_eq!(a.readiness, "ready");
    }

    #[test]
    fn reorder_pulls_dependency_along() {
        // b depends on a. Asking for [b, a] must be repaired to [a, b].
        let mut store = Store::open_in_memory().unwrap();
        let squad = store
            .insert_squad(&parse(TWO_CELL_CHAIN), None, false)
            .unwrap();
        let pa = cell_path(&squad, 0, 0);
        let pb = cell_path(&squad, 0, 1);
        let order = store.reorder_queue(&[pb.clone(), pa.clone()]).unwrap();
        assert_eq!(order, vec![pa, pb], "dependency dragged along");
    }

    #[test]
    fn set_position_absolute_clamps_to_bottom() {
        let mut store = Store::open_in_memory().unwrap();
        let squad = store
            .insert_squad(&parse(TWO_CELL_CHAIN), None, false)
            .unwrap();
        let pa = cell_path(&squad, 0, 0);
        let pb = cell_path(&squad, 0, 1);
        // Send a to an enormous absolute position → clamps to the bottom, but the
        // b→a dependency repair pulls a back above b, so the order stays [a, b].
        let order = store
            .set_queue_position(std::slice::from_ref(&pa), 1_000_000, true)
            .unwrap();
        assert_eq!(order, vec![pa, pb]);
    }

    #[test]
    fn move_block_relative_moves_up() {
        let cur: Vec<String> = ["x", "y", "z"].iter().map(|s| s.to_string()).collect();
        let got = move_block(&cur, &["z".to_string()], 2, false);
        assert_eq!(got, vec!["z", "x", "y"]);
    }

    #[test]
    fn restart_task_proof_from_last_only_resets_that_step() {
        let mut store = Store::open_in_memory().unwrap();
        let id = store
            .insert_squad(&parse(THREE_TASK_PROOFS), None, false)
            .unwrap();
        store.set_cell_state(&id, 0, 0, NodeState::Done).unwrap();
        for vi in 0..3i64 {
            store
                .set_proof_state(&id, 0, "task", -1, vi, NodeState::Done)
                .unwrap();
        }
        store.set_task_state(&id, 0, NodeState::Done).unwrap();
        store.set_squad_state(&id, SquadState::Done).unwrap();

        // Restart from vi=2 — only the last step resets.
        store.restart_task_proof(&id, 0, 2).unwrap();

        let squad = store.get_squad(&id).unwrap();
        let vs = &squad.tasks[0].proof;
        assert_eq!(vs[0].state, "done", "vi=0 must stay done");
        assert_eq!(vs[1].state, "done", "vi=1 must stay done");
        assert_eq!(vs[2].state, "pending", "vi=2 must be reset to pending");
    }

    // ── project registry (RAL-100) ────────────────────────────────────────────

    #[test]
    fn register_and_get_project_round_trips() {
        let store = Store::open_in_memory().unwrap();
        store
            .register_project(
                "ralphus",
                "the ralphus repo itself",
                "C:/repos/ralphus",
                "git",
            )
            .unwrap();
        let p = store.get_project("ralphus").unwrap().expect("registered");
        assert_eq!(p.name, "ralphus");
        assert_eq!(p.description, "the ralphus repo itself");
        assert_eq!(p.path, "C:/repos/ralphus");
        assert_eq!(p.clone_url, None);
        assert_eq!(p.vcs, "git");
    }

    #[test]
    fn project_clone_url_round_trips_and_omission_does_not_erase_it() {
        let store = Store::open_in_memory().unwrap();
        store
            .register_project_with_clone_url_ex(
                "ralphus",
                "remote capable",
                "C:/repos/ralphus",
                "git",
                Some("git@example.invalid:team/ralphus.git"),
                None,
            )
            .unwrap();
        assert_eq!(
            store
                .get_project("ralphus")
                .unwrap()
                .unwrap()
                .clone_url
                .as_deref(),
            Some("git@example.invalid:team/ralphus.git")
        );

        store
            .register_project(
                "ralphus",
                "updated by an older client",
                "C:/repos/ralphus-new",
                "git",
            )
            .unwrap();
        let project = store.get_project("ralphus").unwrap().unwrap();
        assert_eq!(
            project.clone_url.as_deref(),
            Some("git@example.invalid:team/ralphus.git")
        );
        assert_eq!(project.path, "C:/repos/ralphus-new");
    }

    #[test]
    fn clear_project_clone_url_erases_a_previously_registered_url() {
        let store = Store::open_in_memory().unwrap();
        store
            .register_project_with_clone_url_ex(
                "ralphus",
                "",
                "C:/repos/ralphus",
                "git",
                Some("git@example.invalid:team/ralphus.git"),
                None,
            )
            .unwrap();
        store.clear_project_clone_url("ralphus").unwrap();
        let project = store.get_project("ralphus").unwrap().unwrap();
        assert_eq!(project.clone_url, None);
        // Clearing must not touch any other field.
        assert_eq!(project.path, "C:/repos/ralphus");
    }

    #[test]
    fn clear_project_clone_url_on_an_unregistered_project_is_not_found() {
        let store = Store::open_in_memory().unwrap();
        assert!(matches!(
            store.clear_project_clone_url("nope"),
            Err(StoreError::NotFound)
        ));
    }

    #[test]
    fn get_project_missing_is_none() {
        let store = Store::open_in_memory().unwrap();
        assert!(store.get_project("nope").unwrap().is_none());
    }

    #[test]
    fn register_project_is_upsert_by_name() {
        let store = Store::open_in_memory().unwrap();
        store
            .register_project("ralphus", "old description", "C:/old/path", "git")
            .unwrap();
        store
            .register_project("ralphus", "new description", "C:/new/path", "git")
            .unwrap();
        let all = store.list_projects().unwrap();
        assert_eq!(
            all.len(),
            1,
            "re-registering the same name must not duplicate it"
        );
        assert_eq!(all[0].description, "new description");
        assert_eq!(all[0].path, "C:/new/path");
    }

    #[test]
    fn register_project_stamps_global_skip_base_updates_on_first_insert() {
        let store = Store::open_in_memory().unwrap();
        // The global value at first registration (here `true`) is stamped into
        // the new project row via the injectable seam -- `agent_profiles.rs`
        // documents why the process env can't be mutated in-test to supply it.
        store
            .register_project_with_stamp(
                "ralphus",
                "",
                "C:/repos/ralphus",
                "git",
                Some(true),
                None,
                None,
                None,
            )
            .unwrap();
        assert_eq!(
            store.project_skip_base_updates_stamp("C:/repos/ralphus"),
            Some(true)
        );
        // A subdirectory of the project also resolves to the same stamp.
        assert_eq!(
            store.project_skip_base_updates_stamp("C:/repos/ralphus/daemon"),
            Some(true)
        );
    }

    #[test]
    fn reregistering_existing_project_does_not_restamp() {
        let store = Store::open_in_memory().unwrap();
        store
            .register_project_with_stamp(
                "ralphus",
                "",
                "C:/repos/ralphus",
                "git",
                Some(true),
                None,
                None,
                None,
            )
            .unwrap();

        // Re-register the same name with a *different* global value (`false`):
        // registration is an upsert, so the existing row's stamp is preserved
        // rather than being restamped from the now-changed global (RAL-250's
        // "no backfill for existing projects").
        store
            .register_project_with_stamp(
                "ralphus",
                "updated",
                "C:/repos/ralphus",
                "git",
                Some(false),
                None,
                None,
                None,
            )
            .unwrap();
        assert_eq!(
            store.project_skip_base_updates_stamp("C:/repos/ralphus"),
            Some(true),
            "re-registering must not overwrite the original creation-time stamp"
        );
    }

    #[test]
    fn stamp_is_none_only_for_pre_ral250_unstamped_project() {
        let store = Store::open_in_memory().unwrap();
        // A project registered with an explicit value resolves to that value.
        store
            .register_project_with_stamp(
                "ralphus",
                "",
                "C:/repos/ralphus",
                "git",
                Some(false),
                None,
                None,
                None,
            )
            .unwrap();
        assert_eq!(
            store.project_skip_base_updates_stamp("C:/repos/ralphus"),
            Some(false),
            "an explicit false stamp must resolve to Some(false)"
        );
        // Unregistered path has no stamp.
        assert_eq!(store.project_skip_base_updates_stamp("C:/unrelated"), None);
        // A pre-RAL-250 project (never stamped, column NULL) deliberately has
        // no stamp -- the ticket's "existing projects are not backfilled".
        store
            .conn
            .execute(
                "INSERT INTO projects(name, description, path, vcs, created_at_ms, skip_base_updates)
                 VALUES('legacy','','C:/legacy','git',1,NULL)",
                [],
            )
            .unwrap();
        assert_eq!(
            store.project_skip_base_updates_stamp("C:/legacy"),
            None,
            "an unstamped legacy project must resolve to None (not backfilled)"
        );
    }

    #[test]
    fn list_guardians_stamp_resolution_matches_get_guardian_across_shared_and_distinct_git_roots() {
        // GUARDIAN_PERF.local.md: `list_guardians()` now resolves every
        // guardian's project-stamp fields via one shared, call-scoped
        // context (`GuardianHydrationCtx`) instead of `hydrate_guardian`
        // re-querying `projects` per guardian. This proves that batching
        // didn't introduce cross-guardian contamination: two guardians
        // sharing a git_root must resolve identically to each other AND to
        // an independent `get_guardian` call, while a guardian at a
        // *different* registered project (with a different stamp) and one
        // at an *unregistered* path must each resolve their own, distinct
        // value -- not leak another guardian's cached config.
        let store = Store::open_in_memory().unwrap();
        store
            .register_project_with_stamp(
                "proj-a",
                "",
                "C:/repos/proj-a",
                "git",
                Some(true),
                None,
                None,
                None,
            )
            .unwrap();
        store
            .register_project_with_stamp(
                "proj-b",
                "",
                "C:/repos/proj-b",
                "git",
                Some(false),
                None,
                None,
                None,
            )
            .unwrap();

        // Two guardians share proj-a's git_root (exercises the per-git_root
        // memoization); one is at proj-b; one is at an unregistered path.
        let id_a1 = store
            .create_guardian("a1", "main", "C:/repos/proj-a")
            .unwrap();
        let id_a2 = store
            .create_guardian("a2", "main", "C:/repos/proj-a")
            .unwrap();
        let id_b = store
            .create_guardian("b", "main", "C:/repos/proj-b")
            .unwrap();
        let id_unregistered = store
            .create_guardian("u", "main", "C:/repos/unregistered")
            .unwrap();

        let list = store.list_guardians().unwrap();
        let find = |id: &str| list.iter().find(|g| g.id == id).unwrap();

        assert!(find(&id_a1).effective_skip_base_updates);
        assert!(find(&id_a2).effective_skip_base_updates);
        assert!(!find(&id_b).effective_skip_base_updates);
        // Unregistered path: no project stamp, live global default (false).
        assert!(!find(&id_unregistered).effective_skip_base_updates);

        // The batched list path must agree with the unbatched single-guardian
        // path for every guardian, not just happen to match by coincidence.
        for id in [&id_a1, &id_a2, &id_b, &id_unregistered] {
            let listed = find(id);
            let fetched = store.get_guardian(id).unwrap();
            assert_eq!(
                listed.effective_skip_base_updates, fetched.effective_skip_base_updates,
                "list_guardians and get_guardian disagree for {id}"
            );
        }
    }

    #[test]
    fn register_project_ex_uses_explicit_match_pr_branch_name_over_global() {
        let store = Store::open_in_memory().unwrap();
        // An explicit `Some` override (the new CLI flag) wins regardless of
        // what the live global config would otherwise stamp.
        store
            .register_project_ex("ralphus", "", "C:/repos/ralphus", "git", Some(true))
            .unwrap();
        assert_eq!(
            store.project_match_pr_branch_name_stamp("C:/repos/ralphus"),
            Some(true)
        );
    }

    #[test]
    fn register_project_ex_falls_back_to_global_when_unset() {
        let store = Store::open_in_memory().unwrap();
        // `register_project` (no explicit override) stamps from the live
        // global config, which defaults to `false` when unconfigured.
        store
            .register_project("ralphus", "", "C:/repos/ralphus", "git")
            .unwrap();
        assert_eq!(
            store.project_match_pr_branch_name_stamp("C:/repos/ralphus"),
            Some(false)
        );
    }

    #[test]
    fn reregistering_existing_project_does_not_restamp_match_pr_branch_name() {
        let store = Store::open_in_memory().unwrap();
        store
            .register_project_ex("ralphus", "", "C:/repos/ralphus", "git", Some(true))
            .unwrap();
        store
            .register_project_ex("ralphus", "updated", "C:/repos/ralphus", "git", Some(false))
            .unwrap();
        assert_eq!(
            store.project_match_pr_branch_name_stamp("C:/repos/ralphus"),
            Some(true),
            "re-registering must not overwrite the original creation-time stamp"
        );
    }

    #[test]
    fn register_project_stamps_global_auto_submit_pr_stack_on_first_insert() {
        let store = Store::open_in_memory().unwrap();
        // No explicit per-registration override exists for this one (unlike
        // `match_pr_branch_name`) -- it always stamps from the live global
        // config, same shape as `skip_base_updates`.
        store
            .register_project_with_stamp(
                "ralphus",
                "",
                "C:/repos/ralphus",
                "git",
                None,
                None,
                Some(true),
                None,
            )
            .unwrap();
        assert_eq!(
            store.project_auto_submit_pr_stack_stamp("C:/repos/ralphus"),
            Some(true)
        );
        // Unregistered path has no stamp.
        assert_eq!(
            store.project_auto_submit_pr_stack_stamp("C:/unrelated"),
            None
        );
    }

    #[test]
    fn reregistering_existing_project_does_not_restamp_auto_submit_pr_stack() {
        let store = Store::open_in_memory().unwrap();
        store
            .register_project_with_stamp(
                "ralphus",
                "",
                "C:/repos/ralphus",
                "git",
                None,
                None,
                Some(true),
                None,
            )
            .unwrap();
        store
            .register_project_with_stamp(
                "ralphus",
                "updated",
                "C:/repos/ralphus",
                "git",
                None,
                None,
                Some(false),
                None,
            )
            .unwrap();
        assert_eq!(
            store.project_auto_submit_pr_stack_stamp("C:/repos/ralphus"),
            Some(true),
            "re-registering must not overwrite the original creation-time stamp"
        );
    }

    #[test]
    fn project_auto_submit_pr_stack_stamp_flows_into_new_guardians() {
        // End-to-end: a project's stamped `auto_submit_pr_stack` default
        // (RAL-317) is what a *new* guardian under that project's path
        // freezes onto itself at creation time -- see
        // `Store::create_guardian`'s `auto_submit_pr_stack_stamp` lookup in
        // `guardian.rs`.
        let store = Store::open_in_memory().unwrap();
        store
            .register_project_with_stamp(
                "ralphus",
                "",
                "C:/repos/ralphus",
                "git",
                None,
                None,
                Some(true),
                None,
            )
            .unwrap();
        let gid = store
            .create_guardian("r", "main", "C:/repos/ralphus")
            .unwrap();
        let g = store.get_guardian(&gid).unwrap();
        assert_eq!(
            g.auto_submit_pr_stack,
            Some(true),
            "the project's stamped default is frozen onto the new review"
        );
        assert!(g.effective_auto_submit_pr_stack);
    }

    #[test]
    fn list_projects_returns_all_registered() {
        let store = Store::open_in_memory().unwrap();
        store.register_project("a", "", "C:/a", "git").unwrap();
        store.register_project("b", "", "C:/b", "git").unwrap();
        let names: Vec<String> = store
            .list_projects()
            .unwrap()
            .into_iter()
            .map(|p| p.name)
            .collect();
        assert_eq!(names.len(), 2);
        assert!(names.contains(&"a".to_string()));
        assert!(names.contains(&"b".to_string()));
    }

    #[test]
    fn project_name_for_path_matches_the_path_itself_and_descendants() {
        let store = Store::open_in_memory().unwrap();
        store
            .register_project("ralphus", "orchestrator", "C:/repos/ralphus", "git")
            .unwrap();
        assert_eq!(
            store.project_name_for_path("C:/repos/ralphus"),
            Some("ralphus".to_string())
        );
        assert_eq!(
            store.project_name_for_path("C:/repos/ralphus/"),
            Some("ralphus".to_string())
        );
        assert_eq!(store.project_name_for_path("C:/repos/other"), None);
    }

    /// A registered project's path may be typed with backslashes (Windows
    /// native form, e.g. via `ralphus project git`) while a guardian's
    /// `git_root` is git-reported and forward-slashed. Without normalizing
    /// slash direction before comparing, these never match -- which
    /// silently defeats fork-aware PR routing
    /// (`crate::pr::resolve_pr_repo_routing`) and sends a review's PR stack
    /// to the wrong remote (the bug behind guardian-000000000065's PR stack
    /// landing on `origin` instead of its project's registered fork).
    #[test]
    fn project_name_for_path_matches_regardless_of_slash_direction() {
        let store = Store::open_in_memory().unwrap();
        store
            .register_project("ralphus", "orchestrator", r"C:\repos\ralphus", "git")
            .unwrap();
        assert_eq!(
            store.project_name_for_path("C:/repos/ralphus"),
            Some("ralphus".to_string()),
            "a forward-slashed (git-reported) lookup path must match a backslashed registered path"
        );
        assert_eq!(
            store.project_name_for_path("C:/repos/ralphus/worktrees/w1"),
            Some("ralphus".to_string()),
            "descendant matching must also survive the slash-direction mismatch"
        );
    }

    #[test]
    fn resolve_project_exact_name_match() {
        let store = Store::open_in_memory().unwrap();
        store
            .register_project("ralphus", "orchestrator", "C:/repos/ralphus", "git")
            .unwrap();
        let p = store
            .resolve_project("ralphus")
            .unwrap()
            .expect("exact match");
        assert_eq!(p.name, "ralphus");
    }

    #[test]
    fn resolve_project_unregistered_is_none() {
        let store = Store::open_in_memory().unwrap();
        store
            .register_project("ralphus", "", "C:/repos/ralphus", "git")
            .unwrap();
        assert!(
            store
                .resolve_project("totally-unrelated-name")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn resolve_project_empty_registry_is_none() {
        let store = Store::open_in_memory().unwrap();
        assert!(store.resolve_project("anything").unwrap().is_none());
    }

    #[test]
    fn resolve_project_fuzzy_near_miss_name() {
        // A speech-to-text-style near miss (one transposed letter) should still
        // resolve when it's close enough relative to the name's length.
        let store = Store::open_in_memory().unwrap();
        store
            .register_project("ralphus", "orchestrator", "C:/repos/ralphus", "git")
            .unwrap();
        let p = store
            .resolve_project("ralphuz")
            .unwrap()
            .expect("fuzzy match");
        assert_eq!(p.name, "ralphus");
    }

    #[test]
    fn resolve_project_description_substring_match() {
        let store = Store::open_in_memory().unwrap();
        store
            .register_project(
                "ralphus",
                "autonomous agent orchestrator",
                "C:/repos/ralphus",
                "git",
            )
            .unwrap();
        store
            .register_project("other", "unrelated project", "C:/repos/other", "git")
            .unwrap();
        let p = store
            .resolve_project("orchestrator")
            .unwrap()
            .expect("description substring match");
        assert_eq!(p.name, "ralphus");
    }

    #[test]
    fn resolve_project_ambiguous_substring_falls_through_to_fuzzy() {
        // Two projects both match the substring "repo" in their description, so
        // the substring branch can't disambiguate; distance-based fallback then
        // decides (or returns None if nothing is close enough by name).
        let store = Store::open_in_memory().unwrap();
        store
            .register_project("alpha", "a repo for alpha work", "C:/a", "git")
            .unwrap();
        store
            .register_project("beta", "a repo for beta work", "C:/b", "git")
            .unwrap();
        // Neither name is close to "repo" by edit distance, so this must be None
        // rather than silently picking one of the two ambiguous matches.
        assert!(store.resolve_project("repo").unwrap().is_none());
    }

    #[test]
    fn set_cell_cwd_rewrites_placeholder_to_real_path() {
        let mut store = Store::open_in_memory().unwrap();
        let id = store.insert_squad(&parse(SAMPLE), None, false).unwrap();
        store
            .set_cell_cwd(&id, 0, 0, "C:/repos/ralphus/.git/.ralphus_worktrees/feat")
            .unwrap();
        let squad = store.get_squad(&id).unwrap();
        assert_eq!(
            squad.tasks[0].cells[0].cwd.as_deref(),
            Some("C:/repos/ralphus/.git/.ralphus_worktrees/feat")
        );
    }

    // -----------------------------------------------------------------------
    // RAL-208 — debounced final change-summary regen requests
    // -----------------------------------------------------------------------

    #[test]
    fn claim_final_summary_repair_fires_once_per_guardian() {
        let mut store = Store::open_in_memory().unwrap();
        assert!(store.claim_final_summary_repair("g1"));
        assert!(!store.claim_final_summary_repair("g1"));
        // Independent per guardian.
        assert!(store.claim_final_summary_repair("g2"));
    }

    #[test]
    fn claim_final_summary_repair_declines_a_guardian_already_summarized() {
        let mut store = Store::open_in_memory().unwrap();
        store.mark_final_summary_generated("g1", "sig-a");
        assert!(!store.claim_final_summary_repair("g1"));
    }

    #[test]
    fn request_final_summary_is_noop_when_signature_already_generated() {
        let mut store = Store::open_in_memory().unwrap();
        store.mark_final_summary_generated("g1", "sig-a");
        // A rebuild that didn't change the enabled-branch set (a feedback
        // restack, a manual-push rebase, a base-branch shift) requests the
        // same signature again -- this must not queue anything.
        store.request_final_summary("g1", "sig-a", 1_000, false);
        assert!(
            store
                .take_due_final_summary_requests(1_000 + 60_000, 0)
                .is_empty()
        );
    }

    /// RAL-303: the handoff from the deterministic git-log summary to the LLM
    /// one happens when the stack finishes rebuilding, which usually leaves
    /// the enabled-branch set untouched -- so the signature check alone would
    /// deny it and the review would keep showing raw commit subjects forever.
    #[test]
    fn request_final_summary_forced_queues_even_for_an_already_generated_signature() {
        let mut store = Store::open_in_memory().unwrap();
        store.mark_final_summary_generated("g1", "sig-a");
        store.request_final_summary("g1", "sig-a", 1_000, true);
        let due = store.take_due_final_summary_requests(1_000 + 60_000, 0);
        assert_eq!(due, vec![("g1".to_string(), "sig-a".to_string())]);
    }

    #[test]
    fn request_final_summary_queues_when_signature_differs_from_generated() {
        let mut store = Store::open_in_memory().unwrap();
        store.mark_final_summary_generated("g1", "sig-a");
        store.request_final_summary("g1", "sig-b", 1_000, false);
        let due = store.take_due_final_summary_requests(1_000 + 5_000, 5_000);
        assert_eq!(due, vec![("g1".to_string(), "sig-b".to_string())]);
    }

    #[test]
    fn take_due_final_summary_requests_respects_debounce_window() {
        let mut store = Store::open_in_memory().unwrap();
        store.request_final_summary("g1", "sig-a", 1_000, false);
        // Not due yet -- the quiet period hasn't elapsed.
        assert!(
            store
                .take_due_final_summary_requests(1_000 + 2_000, 5_000)
                .is_empty()
        );
        // Due once the full debounce window has elapsed.
        let due = store.take_due_final_summary_requests(1_000 + 5_000, 5_000);
        assert_eq!(due, vec![("g1".to_string(), "sig-a".to_string())]);
    }

    #[test]
    fn repeated_requests_restart_the_debounce_clock_and_only_the_latest_signature_survives() {
        let mut store = Store::open_in_memory().unwrap();
        // Simulates rapid enable/disable toggling: each toggle rebuilds and
        // requests a different enabled-branch signature before the previous
        // request's debounce window has elapsed.
        store.request_final_summary("g1", "sig-a", 0, false);
        store.request_final_summary("g1", "sig-b", 1_000, false);
        store.request_final_summary("g1", "sig-c", 2_000, false);
        // 5s after the FIRST request, but only 3s after the last -- still
        // not due, proving the clock restarted rather than accumulating from
        // the first request.
        assert!(
            store
                .take_due_final_summary_requests(5_000, 5_000)
                .is_empty()
        );
        // 5s after the last request, only the final signature is due -- the
        // intermediate toggles never fired their own LLM call.
        let due = store.take_due_final_summary_requests(2_000 + 5_000, 5_000);
        assert_eq!(due, vec![("g1".to_string(), "sig-c".to_string())]);
    }

    #[test]
    fn take_due_final_summary_requests_clears_pending_so_it_is_claimed_once() {
        let mut store = Store::open_in_memory().unwrap();
        store.request_final_summary("g1", "sig-a", 0, false);
        let first = store.take_due_final_summary_requests(10_000, 5_000);
        assert_eq!(first, vec![("g1".to_string(), "sig-a".to_string())]);
        // A concurrent/subsequent sweep at the same instant finds nothing
        // left to claim for this guardian.
        assert!(
            store
                .take_due_final_summary_requests(10_000, 5_000)
                .is_empty()
        );
    }

    fn any_guardian(store: &Store) -> String {
        store.create_guardian("r", "main", "/repo").unwrap()
    }

    #[test]
    fn take_due_auto_submits_respects_debounce_window() {
        let store = Store::open_in_memory().unwrap();
        let id = any_guardian(&store);
        store.request_auto_submit_branch(&id, 1_000).unwrap();
        assert!(store.take_due_auto_submits(1_200, 400).unwrap().is_empty());
        assert_eq!(store.take_due_auto_submits(1_400, 400).unwrap(), vec![id]);
    }

    #[test]
    fn a_second_auto_submit_request_restarts_the_debounce_clock() {
        let store = Store::open_in_memory().unwrap();
        let id = any_guardian(&store);
        store.request_auto_submit_branch(&id, 1_000).unwrap();
        store.request_auto_submit_branch(&id, 1_200).unwrap();
        assert!(store.take_due_auto_submits(1_400, 400).unwrap().is_empty());
        assert_eq!(store.take_due_auto_submits(1_600, 400).unwrap(), vec![id]);
    }

    #[test]
    fn due_auto_submit_is_claimed_once() {
        let store = Store::open_in_memory().unwrap();
        let id = any_guardian(&store);
        store.request_auto_submit_branch(&id, 0).unwrap();
        assert_eq!(store.take_due_auto_submits(400, 400).unwrap(), vec![id]);
        assert!(store.take_due_auto_submits(400, 400).unwrap().is_empty());
    }

    #[test]
    fn auto_submit_requests_survive_store_reopen() {
        let dir = std::env::temp_dir().join(format!(
            "ralphus-auto-submit-durability-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        let db_path = dir.join("tasks.db");
        let id = {
            let store = Store::open(&db_path).unwrap();
            let id = any_guardian(&store);
            store.request_auto_submit_branch(&id, 1_000).unwrap();
            id
        };
        let reopened = Store::open(&db_path).unwrap();
        assert_eq!(
            reopened.take_due_auto_submits(1_400, 400).unwrap(),
            vec![id]
        );
        drop(reopened);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn guardian_restack_waits_for_all_parallel_branch_leases_and_coalesces() {
        let mut store = Store::open_in_memory().unwrap();
        assert!(store.try_acquire_guardian_worktree_lease("g", "a", "feedback:a"));
        assert!(store.try_acquire_guardian_worktree_lease("g", "b", "feedback:b"));
        store.request_guardian_restack("g", 4);
        store.request_guardian_restack("g", 2);
        store.request_guardian_restack("g", 3);
        assert_eq!(store.try_claim_guardian_restack("g"), None);
        assert!(store.release_guardian_worktree_lease("g", "a", "feedback:a"));
        assert_eq!(store.try_claim_guardian_restack("g"), None);
        assert!(store.release_guardian_worktree_lease("g", "b", "feedback:b"));
        assert_eq!(store.try_claim_guardian_restack("g"), Some(2));
        assert_eq!(store.try_claim_guardian_restack("g"), None);
        assert!(!store.try_acquire_guardian_worktree_lease("g", "a", "feedback:a"));
        store.finish_guardian_restack("g");
        assert!(store.try_acquire_guardian_worktree_lease("g", "a", "feedback:a"));
    }

    #[test]
    fn guardian_distinct_branch_feedback_leases_are_concurrent() {
        let mut store = Store::open_in_memory().unwrap();
        assert!(store.try_acquire_guardian_worktree_lease("g", "a", "feedback:a"));
        assert!(store.try_acquire_guardian_worktree_lease("g", "b", "feedback:b"));
        assert!(!store.try_acquire_guardian_worktree_lease("g", "a", "feedback:a2"));
    }

    /// RAL-230: the DB file (and its WAL/SHM siblings, when SQLite has
    /// already created them) must be owner-only on Unix -- the DB holds
    /// stored env-var overrides and task/cell prompts.
    #[cfg(unix)]
    #[test]
    fn open_sets_owner_only_permissions_on_the_db_file() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("ral230-db-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create dir");
        let db_path = dir.join("tasks.db");

        let store = Store::open(&db_path).expect("open store");
        drop(store);

        let mode = std::fs::metadata(&db_path)
            .expect("stat db")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "expected 0o600 on db file, got {mode:o}");

        // SQLite creates the WAL/SHM siblings lazily -- only assert on ones
        // that actually exist by the time `open` returns.
        for suffix in ["-wal", "-shm"] {
            let sibling = dir.join(format!("tasks.db{suffix}"));
            if sibling.exists() {
                let mode = std::fs::metadata(&sibling)
                    .expect("stat sibling")
                    .permissions()
                    .mode()
                    & 0o777;
                assert_eq!(mode, 0o600, "expected 0o600 on {sibling:?}, got {mode:o}");
            }
        }

        let _ = std::fs::remove_dir_all(&dir);
    }
}
