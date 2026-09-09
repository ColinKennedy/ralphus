//! Guardian: the review + stacked-rebase system.
//!
//! A *guardian* (a review) collects a set of feature branches, rebases them into
//! a single linear stack on top of a base branch (resolving conflicts along the
//! way), and exposes the result for approval. This module holds the persistence
//! (guardian + branch rows) as methods on [`Store`]; the git mechanics live in
//! `guardian_git.rs` and the orchestration in the server/merge path.

use std::collections::BTreeMap;
use std::path::Path;

use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::store::{ProofView, Result, Store, StoreError};

/// Return type of [`Store::proof_steps_for_review_branch`]:
/// `(cell_proofs, task_proofs, cell_system_prompt)`.
pub type BranchProofInfo = (Vec<ProofView>, Vec<ProofView>, Option<String>);

/// A named, defaulted input referenced by a [`GuardianCheck`]'s
/// `command`/`cleanup_command` as a `{name}` placeholder (RAL-164), e.g. a
/// port number that would otherwise be hardcoded and collide across
/// concurrent reviews.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckInput {
    /// Placeholder key, e.g. `"port"` for a `{port}` placeholder.
    pub name: String,
    /// Shown to the user next to the input field, e.g. "Port for the daemon".
    pub message: String,
    /// Pre-filled default value, offered until the user (or the resolver
    /// agent, via "set it for me") submits a different one.
    #[serde(default)]
    pub default: String,
    /// Declared value type (RAL-221), enforced against every
    /// submitted/stored value for this input before it is substituted into
    /// a check's `command`/`cleanup_command` -- see
    /// `server::check_input_type_failures`. Defaults to `String`
    /// (unconstrained) so an input declared before this field existed keeps
    /// behaving exactly as before.
    #[serde(default)]
    pub r#type: CheckInputType,
}

/// Declared value type for a [`CheckInput`] (RAL-221). A submitted or stored
/// value that doesn't match its input's declared type is rejected before
/// substitution -- the fix for a command-injection path where a value like
/// `1.0 & calc.exe & rem` submitted for a should-be-numeric input flowed
/// straight into a `cmd /K` spawn unchecked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CheckInputType {
    /// No constraint on the value's shape -- the default, matching every
    /// input declared before this field existed.
    #[default]
    String,
    /// Value must parse as a base-10 signed integer (surrounding whitespace
    /// tolerated, nothing else -- in particular no shell metacharacters).
    Int,
}

impl CheckInputType {
    /// Whether `value` is well-formed for this declared type.
    #[must_use]
    pub fn accepts(self, value: &str) -> bool {
        match self {
            Self::String => true,
            Self::Int => value.trim().parse::<i64>().is_ok(),
        }
    }

    /// Human-readable name for error messages.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::String => "string",
            Self::Int => "int",
        }
    }
}

/// Status of a "set it for me" AI resolution for one named [`CheckInput`]
/// (RAL-164). `value` is `None` while `status == "resolving"`.
#[derive(Debug, Clone, Serialize)]
pub struct InputResolutionView {
    /// `"resolving"` | `"ready"` | `"failed"`.
    pub status: String,
    /// The resolved value, once `status == "ready"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
}

/// One runnable review check (RAL-164): either a user-declared test/action
/// hint from `[[review.action]]` (RAL-77, has a `label`) or an LLM-synthesized
/// manual review command (RAL-27, no `label`). Unified onto one shape so both
/// can carry an optional `cleanup_command` and named, defaulted `inputs`.
/// Exactly one of `command` (verbatim shell) or `prompt` (forwarded to LLM)
/// is set.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GuardianCheck {
    /// Button label shown in the UI. `None` for AI-synthesized manual checks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Verbatim shell command to run (mutually exclusive with `prompt`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// Prompt forwarded to the LLM to expand into a command (mutually exclusive with `command`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    /// Optional command run before `command`/the expanded `prompt`, e.g. to
    /// stop a stale process from a previous run. Opt-in at run time via a UI
    /// checkbox.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cleanup_command: Option<String>,
    /// Named, defaulted values referenced in `command`/`cleanup_command` as
    /// `{name}` placeholders.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inputs: Vec<CheckInput>,
}

/// This review's own declared build step (RAL-342), authored via
/// `[[review.auto_build]]` and resolved once at submit time
/// (`reviews::derive_reviews`, converted from `ralphus_core::schema::AutoBuildDef`)
/// into the JSON blob stored in the `guardians.auto_build_json` column. Either
/// a static shell `command`, or an agent invocation described by the
/// remaining fields -- exactly one of the two shapes is populated, enforced
/// by `core::validate` at parse time.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GuardianAutoBuild {
    /// Verbatim shell command to run against the combined worktree (mutually
    /// exclusive with the agent-invocation fields below).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// Prompt forwarded to the build agent (mutually exclusive with `command`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    /// Optional system prompt for the build agent invocation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    /// Where `system_prompt` is spliced relative to the agent's own default
    /// system prompt, e.g. `"prepend"` / `"append"` / `"replace"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt_position: Option<String>,
    /// Backend that runs the build agent invocation. `None` falls back to
    /// this review's resolver agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// Model override for the build agent invocation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

/// Lifecycle state of a guardian/review.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GuardianStatus {
    /// Accepting branches; not yet merged.
    Collecting,
    /// Building the stacked rebase.
    Merging,
    /// A merge/rebase step failed and needs attention.
    MergeFailed,
    /// The user halted a mid-rebase merge (RAL-249) — a recoverable pause,
    /// distinct from [`Self::Cancelled`]: the review and its branches are kept
    /// and the rebase can be started again from its next checkpoint.
    MergeStopped,
    /// Stack built; awaiting human review.
    InReview,
    /// Approved by a human.
    Approved,
    /// Cancelled by the user — the current run is discarded; can be restarted.
    Cancelled,
    /// Deployed (terminal; deploy itself is a stub).
    Deployed,
}

impl GuardianStatus {
    /// The stored lowercase string.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Collecting => "collecting",
            Self::Merging => "merging",
            Self::MergeFailed => "merge_failed",
            Self::MergeStopped => "merge_stopped",
            Self::InReview => "in_review",
            Self::Approved => "approved",
            Self::Cancelled => "cancelled",
            Self::Deployed => "deployed",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "collecting" => Self::Collecting,
            "merging" => Self::Merging,
            "merge_failed" => Self::MergeFailed,
            "merge_stopped" => Self::MergeStopped,
            "in_review" => Self::InReview,
            "approved" => Self::Approved,
            "cancelled" => Self::Cancelled,
            "deployed" => Self::Deployed,
            _ => return None,
        })
    }
}

/// Per-branch merge state within a guardian.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MergeStatus {
    /// Waiting for linked task cells to complete.
    Pending,
    /// All linked task cells are done; branch is waiting for the rebase to start.
    Ready,
    /// Being rebased onto the stack.
    InProgress,
    /// The review rebase was explicitly stopped while this branch was active.
    /// Its worktree is left for the next merge attempt to clean up or resume.
    Stopped,
    /// Reviewer feedback is being applied: the resolver agent is editing the
    /// worktree in response to `review feedback` (and, once it finishes, the
    /// target-branch proof/commit/push steps run). Transient — set right
    /// before the resolver agent runs and cleared to [`Self::Done`] or
    /// [`Self::Failed`] once the whole feedback pass for this branch
    /// finishes. Distinct from [`Self::InProgress`], which is a stack
    /// rebase, not a feedback revision.
    Actioning,
    /// Rebased cleanly.
    Done,
    /// All conflict markers for this branch have been resolved and committed,
    /// but the dedicated final-proof agent call (RAL-149) has not yet
    /// run. Transient: set right before that call and cleared (to
    /// [`Self::ConflictResolved`]) once it completes, pass or fail.
    ProofPending,
    /// Rebased after resolving conflicts (and, when the fix pass hit
    /// conflicts, after the RAL-149 final-proof call has run).
    ConflictResolved,
    /// Failed to rebase.
    Failed,
}

impl MergeStatus {
    /// The stored lowercase string.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Ready => "ready",
            Self::InProgress => "in_progress",
            Self::Stopped => "stopped",
            Self::Actioning => "actioning",
            Self::Done => "done",
            Self::ProofPending => "proof_pending",
            Self::ConflictResolved => "conflict_resolved",
            Self::Failed => "failed",
        }
    }
}

/// RAL-380: durable, read-only completion status for one "reviewer"-role
/// feedback message -- rendered by the board as a checkmark on that message's
/// chat bubble. Distinct from [`MergeStatus::Actioning`]/[`MergeStatus::Done`],
/// which describe the *branch's* current rebase/feedback state as a whole;
/// this tracks a single message's own outcome so an older bubble's checkmark
/// can't be mistaken for a newer request's progress. `None` (no column value)
/// on a "guardian"-role message, which isn't actionable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FeedbackActionStatus {
    /// Ralphus accepted the feedback and started applying it (one checkmark).
    Received,
    /// The resolver agent's pass for this feedback finished successfully
    /// (two checkmarks).
    Done,
    /// The resolver agent's pass (or a preceding validation step) failed.
    Failed,
    /// A newer feedback message was submitted on the same branch before this
    /// one's action finished -- its eventual outcome, if any, is stale and
    /// must not be shown as completed.
    Superseded,
}

impl FeedbackActionStatus {
    /// The stored lowercase string.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Received => "received",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Superseded => "superseded",
        }
    }
}

/// Parse a review branch's stored env-override map (RAL-191). Unlike every
/// other env layer this one is `{key: value|null}`, where `null` is a
/// tombstone meaning "remove this inherited key". Malformed JSON degrades to
/// "no overrides" rather than failing the whole guardian view.
pub(crate) fn branch_env_from_json(s: &str) -> BTreeMap<String, Option<String>> {
    serde_json::from_str(s).unwrap_or_default()
}

/// Serialize a review branch's env-override map back to storage.
pub(crate) fn branch_env_to_json(m: &BTreeMap<String, Option<String>>) -> String {
    serde_json::to_string(m).unwrap_or_else(|_| "{}".to_string())
}

/// Apply a review branch's own overrides on top of the environment it
/// inherited from its source cell (RAL-191): `Some(v)` replaces the
/// inherited value (or adds a new one), `None` removes the key entirely.
#[must_use]
pub(crate) fn apply_branch_env(
    inherited: &BTreeMap<String, String>,
    overrides: &BTreeMap<String, Option<String>>,
) -> BTreeMap<String, String> {
    let mut out = inherited.clone();
    for (k, v) in overrides {
        match v {
            Some(value) => {
                out.insert(k.clone(), value.clone());
            }
            None => {
                out.remove(k);
            }
        }
    }
    out
}

/// The resolved environment of the last *enabled* branch in the stack
/// (RAL-203, highest `position`) -- the combined worktree's tip is that
/// branch's code rebased on top of everything beneath it, so its own
/// environment is the one actually in effect there. Disabled branches are
/// never candidates: their commits are not in the combined worktree.
///
/// This is the shared "last worktree in the branch chain" baseline both
/// [`GuardianView::build_env`] and [`GuardianView::manual_checks_env`]
/// layer their own section-specific overrides on top of -- the combined
/// worktree has no upstream task cell of its own to inherit from, so it
/// borrows this instead.
#[must_use]
pub(crate) fn combined_env_from_branches(branches: &[BranchView]) -> BTreeMap<String, String> {
    branches
        .iter()
        .filter(|b| b.enabled)
        .max_by_key(|b| b.position)
        .map(|b| b.resolved_env.clone())
        .unwrap_or_default()
}

/// A branch row in a guardian, for display.
#[derive(Debug, Clone, Serialize)]
pub struct BranchView {
    /// Stable, globally-unique branch id (RAL-122), e.g. `branch-000000000001`.
    /// Survives reorders and cross-guardian moves; the correct addressing key
    /// for routes/state, unlike `position`.
    pub id: String,
    /// Order position (0-based).
    pub position: i64,
    /// The feature branch name.
    pub branch: String,
    /// Merge status string.
    pub merge_status: String,
    /// Optional detail (e.g. conflict summary).
    pub detail: Option<String>,
    /// The review-owned branch stacked for this feature (copy; never the task's).
    pub review_branch: Option<String>,
    /// RAL-378: `true` once this branch's review branch is named readably
    /// (`<task branch>-review`) rather than as the internal
    /// `guardian/<id>/wt-<task branch>` ref. `false` for every branch
    /// registered before readable naming landed, which keeps the internal ref
    /// for the rest of its life so no PR already open against one ever moves.
    pub readable_review_branch: bool,
    /// RAL-378: the readable review-branch name claimed for this branch, once
    /// its first build has resolved it. Sticky -- unlike [`Self::review_branch`]
    /// this survives a reset, so a rebuild reuses the name rather than walking
    /// the collision suffix forward. Always `None` when
    /// [`Self::readable_review_branch`] is `false`.
    pub review_branch_name: Option<String>,
    /// The review worktree this branch is assembled in (read/write for feedback).
    pub worktree: Option<String>,
    /// Total conflict marker blocks detected when this branch was being resolved, if any.
    pub conflicts_found: Option<i64>,
    /// Files where markers are resolved in the working tree, not yet staged.
    pub conflicts_fixed: Option<i64>,
    /// Files whose conflict resolutions are staged and committed to the branch.
    pub conflicts_committed: Option<i64>,
    /// `true` when this branch introduced no diff over the stack tip beneath it
    /// (RAL-190) — it rebased cleanly but contributed nothing.
    ///
    /// Almost always means the owning task never committed its work: the review
    /// would otherwise look perfectly healthy while containing none of that
    /// task's changes. A *task* is allowed to produce no changes, but a branch
    /// in a review stack is there to contribute something — so this **fails the
    /// merge** (see `guardian_merge::note_if_branch_is_empty` and its
    /// `fail_branch` call site), leaving the branch `failed` and the guardian
    /// `merge_failed`. The escape hatch for a deliberately-empty branch is to
    /// disable it, which drops it from the stack while keeping it visible.
    ///
    /// The flag is kept separate from the `failed` status so the board can say
    /// *why* it failed — `board.html`'s `⌀ empty` badge takes precedence over
    /// the generic conflict badge, since the fix here is to go look at the
    /// task's cell rather than at a diff.
    pub is_empty: bool,
    /// The machine the cell that produced this branch ran on (RAL-185).
    /// `None` means the daemon's own host — every pre-RAL-185 branch, and any
    /// branch whose work was done locally.
    ///
    /// When this is set, the branch's commits live on *that* machine, so the
    /// review must fetch them from the project's shared remote before it can
    /// stack them (see `guardian_merge::fetch_branch_for_remote_cell`).
    pub source_cell_machine: Option<String>,
    /// Whether this branch is included in the rebase stack (RAL-43). Disabled
    /// branches are skipped during merge but remain visible in the branch list.
    pub enabled: bool,
    /// Which git project root this branch lives in (RAL-29). `None` means the
    /// guardian's primary `git_root` (backward compatible with single-project).
    pub project: Option<String>,
    /// State of the cell whose work lives in this branch's worktree (RAL-69).
    /// `None` when no cell has `review_branch = branch` (branch was never
    /// submitted or was added manually). Used to determine force-start readiness.
    pub source_cell_state: Option<String>,
    /// `true` when this branch is disabled, all its source cells are `done`,
    /// and the "can re-enable" notification has not been dismissed (RAL-69).
    /// TODO(RAL-73): wire to the dedicated `ready` signal when that lands.
    pub can_reenable: bool,
    /// Squad ID of the most recent cell submitted for this branch (for board navigation).
    pub source_squad_id: Option<String>,
    /// Task index within the squad for the source cell.
    pub source_task_idx: Option<i64>,
    /// Cell index within the task for the source cell.
    pub source_cell_idx: Option<i64>,
    /// agent_session_id from the conflict-resolver run on this branch.
    /// Only populated when resolver_agent is "claude-code". Enables terminal resume.
    pub resolver_agent_session_id: Option<String>,
    /// `true` when this branch's stacked commit is staged and ready to merge
    /// as-is (`merge_status == "ready"`). Ported from board.html's
    /// `branchBadge` (CLI_PARITY_PLAN.local.md Phase 5).
    pub ready: bool,
    /// Which terminal-resume modes are available for this branch's conflict
    /// resolver: `"readonly"`/`"open"` once a resolver cell exists, or
    /// `"worktree"` for a CLI agent with a worktree but no cell yet. Empty
    /// when none apply. Ported from board.html's `resolverTerminalBtns`
    /// (button *labels*/tooltips are UI-only and not reproduced here).
    pub terminal_modes: Vec<&'static str>,
    /// RAL-118: id of the guardian this branch *originally* belonged to,
    /// set the first time it is moved into a different review via
    /// [`Store::move_guardian_branch`] and preserved across any later moves.
    /// `None` for a branch that has never been moved.
    pub moved_from_guardian_id: Option<String>,
    /// RAL-145: git's own interactive-rebase todo-list "commands done" count
    /// (`rebase-merge/done`), read live from the worktree. Populated only for
    /// the branch currently `merge_status == "in_progress"` with a worktree;
    /// `None` otherwise, or if no rebase is actually paused/running there.
    pub rebase_commands_done: Option<i64>,
    /// RAL-145: total rebase-todo commands (done + remaining, from
    /// `rebase-merge/git-rebase-todo`). See [`Self::rebase_commands_done`].
    pub rebase_commands_total: Option<i64>,
    /// RAL-191: this branch's *own* environment-variable overrides, layered on
    /// top of whatever its source cell resolves to. A `Some(value)` entry
    /// overrides the inherited value; a `None` entry is a tombstone meaning
    /// "remove this inherited variable entirely". A key absent from this map
    /// is simply inherited. Set via
    /// `POST /api/guardians/{id}/branches/{bid}/env`.
    pub env_overrides: BTreeMap<String, Option<String>>,
    /// RAL-191: the *effective* environment this branch's review worktree runs
    /// under — the source cell's resolved overrides with this branch's own
    /// [`Self::env_overrides`] applied (values replaced, tombstones removed).
    /// This is exactly what the conflict resolver, feedback routing, and check
    /// gates are spawned with.
    pub resolved_env: BTreeMap<String, String>,
    /// RAL-191: the environment inherited from the source cell *before*
    /// this branch's own overrides are applied. Lets the board show which keys
    /// are inherited, overridden, or tombstoned without recomputing the merge.
    pub inherited_env: BTreeMap<String, String>,
    /// RAL-259: when this branch's conflict-resolver agent (fix pass or
    /// final-proof call) most recently began running — the Review Live View's
    /// "started" timestamp (epoch ms). `None` until a resolver session
    /// actually starts, or for a branch that never needed one. Cleared and
    /// re-stamped per merge attempt (mirrors cell `started_at_ms`, RAL-210);
    /// persists after the resolver finishes so completed reviews still show it.
    pub started_at_ms: Option<i64>,
    /// RAL-317: a one-shot failure marker for this branch's most recent
    /// auto-submit-PR-stack attempt (best-effort side channel -- never blocks
    /// a Guardian merge transition). `None` means no failure to report;
    /// cleared again by the next successful auto-submit attempt on this
    /// branch. Rendered as a per-branch badge next to the branch's PR link.
    pub auto_submit_error: Option<String>,
}

/// One message in a guardian's feedback thread (RAL-22, scoped per-branch by RAL-272).
#[derive(Debug, Clone, Serialize)]
pub struct MessageView {
    /// Auto-increment primary key — used by the client to reference a specific
    /// message for conversation branching (RAL-59).
    pub seq: i64,
    /// `"reviewer"` (human) or `"guardian"` (triage agent).
    pub role: String,
    /// Message body.
    pub text: String,
    /// When it was posted (Unix epoch milliseconds).
    pub at_ms: i64,
    /// Base64 data-URI of an image attached to this message (RAL-59). `None`
    /// for text-only messages; omitted from JSON when absent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// RAL-379: the registered user this feedback is attributed to -- the
    /// only identity the UI shows. Defaults to `submitted_by` when a caller
    /// doesn't name one explicitly. `None` for a "guardian"-role message and
    /// for any row predating this field.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    /// RAL-379: the authenticated/default requester who actually submitted
    /// this message, resolved server-side and never overridable by request
    /// data. Kept for audit/provenance only -- never shown in the UI. Still
    /// caller-claimed (via `X-Ralphus-User`) until RAL-252 makes
    /// authentication authoritative. `None` for any row predating this field.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub submitted_by: Option<String>,
    /// RAL-380: this message's completion status (see
    /// [`FeedbackActionStatus`]), the source of its bubble's checkmark.
    /// `None` for a "guardian"-role message and for any row predating this
    /// field.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action_status: Option<String>,
}

/// [`GuardianView::origin`] value for a review created from an authored
/// `[[review]]` block (or any other non-Arbiter path, e.g. the board's
/// "+ Create Review" / `ralphus review create`). The default for every
/// existing row and every fresh `INSERT INTO guardians` that doesn't name the
/// column explicitly (RAL-318).
pub const GUARDIAN_ORIGIN_EXPLICIT: &str = "explicit";

/// [`GuardianView::origin`] value for a review the Arbiter created by
/// draining a Triage pool once its count threshold or a cron schedule fired
/// (RAL-318). See `crate::reviews::derive_triage_pools`.
pub const GUARDIAN_ORIGIN_ARBITER: &str = "arbiter";

/// A guardian/review, for display.
#[derive(Debug, Clone, Serialize)]
pub struct GuardianView {
    /// Guardian id, e.g. `guardian-000000000001`.
    pub id: String,
    /// Human name.
    pub name: String,
    /// Base branch the stack rebases onto.
    pub base_branch: String,
    /// The base-branch commit the stack was last built against. Used to detect a
    /// base-branch shift and auto-rebuild the review. `None` until first built.
    pub base_commit: Option<String>,
    /// Absolute path to the git repository.
    pub git_root: String,
    /// The resulting review branch, once built.
    pub review_branch: Option<String>,
    /// Status string.
    pub status: String,
    /// Optional detail (e.g. merge failure reason, a check-gate opt-out note,
    /// or the RAL-101 auto-build note recorded on reaching `in_review`).
    pub detail: Option<String>,
    /// The squad this review was derived from, if any (manual reviews have none).
    pub squad_id: Option<String>,
    /// The stable, read-only combined review worktree (head of the last branch).
    pub combined_worktree: Option<String>,
    /// Total conflict marker blocks detected across all files when last resolving.
    pub conflicts_found: Option<i64>,
    /// Files where markers are resolved in the working tree, not yet staged.
    pub conflicts_fixed: Option<i64>,
    /// Files whose conflict resolutions are staged and committed to the branch.
    pub conflicts_committed: Option<i64>,
    /// When true, the finalize-time build/check step against the combined
    /// worktree is skipped entirely: explicit `checks`, the project's
    /// `.ralphus.toml [review] auto_build`, and the AI-inferred build command
    /// (RAL-110) are all skipped. Independent of [`Self::effective_proof_scope`].
    pub skip_auto_build: bool,
    /// The machine this review's worktrees, rebase and conflict resolution run
    /// on (RAL-185). `None` means the daemon's own host — every pre-RAL-185
    /// review, and any review that never declared one.
    pub machine: Option<String>,
    /// The review source type. `git` (the default and only fully-implemented
    /// type) drives the branch-stacking flow; other values are placeholders for
    /// future non-git review kinds (see CCTL-112). Existing/derived reviews are
    /// `git`.
    pub review_type: String,
    /// When true, the merge builds the stack in a single shared worktree instead
    /// of one git worktree per branch (CCTL-156), for large repos.
    pub skip_worktrees: bool,
    /// Backend that resolves merge conflicts / applies feedback for this review
    /// (from the top-level `[[review]]` block's `agent`). `None` falls back to the
    /// `RALPHUS_RESOLVER_AGENT` env override, then `ollama`.
    pub resolver_agent: Option<String>,
    /// Model the resolver agent runs. `None` falls back to `RALPHUS_RESOLVER_MODEL`,
    /// then `qwen3:8b` for the ollama backend.
    pub resolver_model: Option<String>,
    /// Creation time (epoch ms).
    pub created_at_ms: i64,
    /// Shell check commands run in the review worktree before it is ready.
    pub checks: Vec<String>,
    /// The ordered branches.
    pub branches: Vec<BranchView>,
    /// Agent-generated summary of changes across all stacked branches (RAL-39).
    pub change_summary: Option<String>,
    /// Ordered distinct project roots spanned by this guardian's branches (RAL-29).
    /// Single-project guardians have exactly one entry (same as `git_root`). The
    /// merge engine processes each project independently.
    pub projects: Vec<String>,
    /// Per-project base commits: JSON map of `{project_root: sha}`. Records the
    /// base-branch tip each project was last built against, for base-shift detection.
    pub base_commits: std::collections::HashMap<String, String>,
    /// LLM-generated shell commands for manual review verification (RAL-27).
    /// Regenerated each time the review branch is rebuilt.
    pub manual_commands: Vec<GuardianCheck>,
    /// User-declared test/action hints from `[[review.action]]` (RAL-77).
    /// Persisted at submit time; not regenerated by the merge engine.
    pub action_hints: Vec<GuardianCheck>,
    /// Resolved/submitted values for any [`CheckInput`] referenced by
    /// `manual_commands`/`action_hints`, keyed by [`CheckInput::name`] and
    /// scoped to this guardian/review (RAL-164). A value here overrides that
    /// input's own literal `default` -- this is what makes a submitted or
    /// AI-resolved value "the new default" the next time the check is viewed.
    pub input_values: std::collections::HashMap<String, String>,
    /// In-flight/completed "set it for me" AI resolutions, keyed by
    /// [`CheckInput::name`] (RAL-164). Absent entries have never had a
    /// resolution requested.
    pub input_resolutions: std::collections::HashMap<String, InputResolutionView>,
    /// RAL-88: the resolved agent/model that produced `change_summary`, recorded
    /// at generation time. `None` for guardians whose summary predates provenance
    /// tracking or that have not generated one yet.
    pub summary_agent: Option<String>,
    /// Model behind `change_summary` (see [`Self::summary_agent`]). May be `None`
    /// even when the agent is known (the backend used its own default model).
    pub summary_model: Option<String>,
    /// RAL-88: the resolved agent/model that produced `manual_commands`.
    pub manual_commands_agent: Option<String>,
    /// Model behind `manual_commands` (see [`Self::manual_commands_agent`]).
    pub manual_commands_model: Option<String>,
    /// RAL-88 follow-up: agent_session_id from the most recent manual-checks
    /// generation pass, for its "Open Agent" terminal action. `None` when the
    /// resolved agent isn't claude-code, or generation hasn't run yet.
    pub manual_commands_agent_session_id: Option<String>,
    /// Git project roots whose task branches are squashed to a single commit in
    /// the review worktree during the stacked rebase (RAL-91). Scoped per-project:
    /// a review spanning N projects honours each project's setting independently.
    /// A project absent from this list keeps its individual working commits.
    pub squash_projects: Vec<String>,
    /// RAL-117: when true, this review opts into automatically incorporating PR
    /// feedback comments instead of requiring the manual "Pull in PR feedback"
    /// action. Data-model only for v1 -- no background poller reads this flag
    /// yet; it exists so a future automatic mode has somewhere to persist to.
    pub auto_pr_feedback: bool,
    /// RAL-168: this review's own Proof-scope override -- `"each_branch"`,
    /// `"final_branch"`, or `"nothing"`. `None` means "inherit the
    /// project-level default" (`.ralphus.toml [review] proof_scope`,
    /// resolved into [`Self::effective_proof_scope`] at hydration time).
    /// Governs whether/how often the dedicated LLM-based final-proof call
    /// ([`crate::guardian_merge::run_final_proof`]) fires -- replaces the
    /// old `verify_mid_resolution` flag outright, not layered alongside it.
    pub proof_scope: Option<String>,
    /// RAL-168: this review's own override for whether `"each_branch"` scope
    /// additionally skips proving on branches whose rebase applied
    /// cleanly with no conflict (an "auto-clean" branch). `None` means
    /// "inherit the project-level default".
    pub proof_skip_auto_clean: Option<bool>,
    /// RAL-168: [`Self::proof_scope`] resolved against the project-level
    /// `.ralphus.toml [review] proof_scope` default -- always one of
    /// `"each_branch"`/`"final_branch"`/`"nothing"`, never empty. This is
    /// what the merge engine actually gates on; the raw field above is only
    /// for the UI to distinguish "explicit override" from "inherited".
    pub effective_proof_scope: String,
    /// RAL-168: [`Self::proof_skip_auto_clean`] resolved against the
    /// project-level default.
    pub effective_proof_skip_auto_clean: bool,
    /// RAL-250: this review's own override for whether the automatic
    /// base-branch auto-update rebuild (`review_maintenance`'s base-shift pass)
    /// is skipped. `None` means "inherit the project/global default"
    /// (resolved into [`Self::effective_skip_base_updates`] at hydration time).
    pub skip_base_updates: Option<bool>,
    /// RAL-250: [`Self::skip_base_updates`] resolved against the
    /// project-level `.ralphus.toml [review] skip_base_updates` default,
    /// this project's creation-time stamp, and the live global config -- the
    /// value `rebuild_on_base_shift` actually gates on.
    pub effective_skip_base_updates: bool,
    /// RAL-307: this review's own override for whether a newly submitted
    /// PR's branch defaults to the exact worktree/feature branch name
    /// instead of the convention-derived alias. `None` means "inherit the
    /// project/global default" (resolved into
    /// [`Self::effective_match_pr_branch_name`] at hydration time). Stamped
    /// from the owning project's effective value at review creation, then
    /// editable per-review afterward (board checkbox / `review settings`).
    pub match_pr_branch_name: Option<bool>,
    /// RAL-307: [`Self::match_pr_branch_name`] resolved against the
    /// project-level `.ralphus.toml [review] match_pr_branch_name` default,
    /// this project's creation-time stamp, and the live global config -- the
    /// value PR submission actually gates on unless a per-submission
    /// `PrRequest::use_worktree_branch_name` overrides it.
    pub effective_match_pr_branch_name: bool,
    /// RAL-378: this review's own override for whether its pull request is
    /// pushed to a branch separate from its review branch. `None` means
    /// "inherit the project/global default" (resolved into
    /// [`Self::effective_separate_pr_branch`] at hydration time). Stamped from
    /// the owning project's effective value at review creation, then editable
    /// per-review afterward (board checkbox / `review settings`), the same
    /// shape as [`Self::match_pr_branch_name`].
    pub separate_pr_branch: Option<bool>,
    /// RAL-378: [`Self::separate_pr_branch`] resolved against the
    /// project-level `.ralphus.toml [review] separate_pr_branch` default, this
    /// project's creation-time stamp, and the live global config.
    ///
    /// `false` -- the default -- means the PR is opened from the review branch
    /// itself, so `pr::resolve_pr_alias` returns that branch's own name and
    /// neither `forge.pull_request_branch_convention` nor
    /// [`Self::effective_match_pr_branch_name`] is read. Those two only take
    /// effect when this is `true`.
    pub effective_separate_pr_branch: bool,
    /// RAL-378: `true` once this review's *combined* worktree branch is named
    /// readably rather than as the internal `guardian/<id>/review` ref.
    /// `false` for every review created before readable naming landed. Only
    /// meaningful for a combined-worktree review; a stacked one names each
    /// branch individually (see [`BranchView::readable_review_branch`]).
    pub readable_review_branch: bool,
    /// RAL-378: the readable name claimed for this review's combined worktree
    /// branch, derived from [`Self::name`] at the first combined build.
    /// Sticky, so renaming the review afterwards does not move the branch.
    pub review_branch_name: Option<String>,
    /// RAL-317: this review's own override for whether the PR stack is
    /// auto-submitted/grown as each branch reaches a terminal
    /// (`done`/`conflict_resolved`) merge state, instead of requiring the
    /// manual `review pr submit` call. `None` means "inherit the
    /// project/global default" (resolved into
    /// [`Self::effective_auto_submit_pr_stack`] at hydration time). Stamped
    /// from the owning project's effective value at review creation, then
    /// editable per-review afterward (board checkbox / `review settings`),
    /// same shape as [`Self::match_pr_branch_name`].
    pub auto_submit_pr_stack: Option<bool>,
    /// RAL-317: [`Self::auto_submit_pr_stack`] resolved against the
    /// project-level `.ralphus.toml [review] auto_submit_pr_stack` default,
    /// this project's creation-time stamp, and the live global config -- the
    /// value the per-branch auto-submit trigger actually gates on.
    pub effective_auto_submit_pr_stack: bool,
    /// RAL-318: `"explicit"` for a review created from an authored
    /// `[[review]]` block (or the board's "+ Create Review"), `"arbiter"` for
    /// one the Arbiter created by draining a Triage pool. See
    /// [`GUARDIAN_ORIGIN_EXPLICIT`] / [`GUARDIAN_ORIGIN_ARBITER`]. The
    /// board's Reviews sidebar filter and Arbiter badge key off this.
    pub origin: String,
    /// `true` once the review is built and awaiting human approval
    /// (`status == "in_review"`). Ported from board.html's "ready to act on"
    /// banner condition (`renderReadyBanner`, minus its client-only dismissed
    /// set) -- CLI_PARITY_PLAN.local.md Phase 5.
    pub ready: bool,
    /// Aggregated merge progress across `branches`. Ported from board.html's
    /// `mergeProgress`.
    pub merge_progress: MergeProgress,
    /// `change_summary` display state (RAL-103): `"ready"` (has content --
    /// either a preliminary git-log summary or the LLM-authored final one) or
    /// `"waiting"` (no branch has reached `Ready` yet). `change_summary` is
    /// never intentionally cleared mid-rebuild (see
    /// `recompute_preliminary_summary`/`generate_final_summary` in
    /// `guardian_merge.rs`) -- the final summary's regeneration is debounced
    /// and asynchronous (RAL-208), but the last-computed value (preliminary or
    /// final) stays visible throughout, so there is no meaningful in-between
    /// "generating" state to report here.
    pub summary_state: &'static str,
    /// `manual_commands` display state (RAL-103): `"ready"` (commands are
    /// available), `"generating"` (every enabled branch has finished rebasing
    /// cleanly and the manual-checks LLM call is expected to be in flight --
    /// see `generate_manual_commands`'s call sites, always after the stack
    /// fully rebuilds), or `"waiting"` (branches are still being collected or
    /// rebased, so generation has not started).
    pub checks_state: &'static str,
    /// RAL-203: this review's own environment-variable overrides for the
    /// finalize-time build/check-gate step (`final_checks`, run against the
    /// combined worktree) -- not yet merged with [`Self::combined_env`].
    /// `Some(v)` overrides an inherited value, `None` is a tombstone. Set via
    /// `POST /api/guardians/{id}/build-env`. Independent of
    /// [`Self::manual_checks_env_overrides`] -- setting one never affects
    /// the other.
    pub build_env_overrides: BTreeMap<String, Option<String>>,
    /// RAL-203: this review's own environment-variable overrides for the
    /// manual-checks step -- the LLM-suggested commands run via `ralphus
    /// review checks run`/the board's "Run all". Set via
    /// `POST /api/guardians/{id}/manual-checks-env`.
    pub manual_checks_env_overrides: BTreeMap<String, Option<String>>,
    /// RAL-203: the environment the combined worktree inherits by default --
    /// the last enabled branch's own [`BranchView::resolved_env`] (see
    /// [`combined_env_from_branches`]). This is the shared baseline
    /// [`Self::build_env`] and [`Self::manual_checks_env`] each layer their
    /// own overrides on top of.
    pub combined_env: BTreeMap<String, String>,
    /// RAL-203: the effective environment the finalize-time build/check-gate
    /// step runs under -- [`Self::combined_env`] with
    /// [`Self::build_env_overrides`] applied.
    pub build_env: BTreeMap<String, String>,
    /// RAL-203: the effective environment the manual-checks step runs under
    /// -- [`Self::combined_env`] with [`Self::manual_checks_env_overrides`]
    /// applied.
    pub manual_checks_env: BTreeMap<String, String>,
    /// RAL-193: this review's own USD spend cap (from `[[review]]`'s
    /// `maximum_budget_usd`), enforced against [`Self::cumulative_cost_usd`].
    /// `None` means no cap.
    pub maximum_budget_usd: Option<f64>,
    /// RAL-193: current merge-attempt counter, bumped once per rebase/re-merge
    /// (`guardian_merge::run_merge`). [`Self::attempt_tokens_in`]/
    /// [`Self::attempt_tokens_out`]/[`Self::attempt_cost_usd`] are scoped to
    /// this attempt.
    pub merge_attempt: i64,
    /// RAL-259: when this review's manual-checks generation agent most
    /// recently began work (epoch ms), for the manual-checks Live View panel's
    /// "started" timestamp. `None` until generation starts. Persists after
    /// generation finishes so a completed generation still shows it; re-stamped
    /// fresh on every regeneration.
    pub manual_checks_started_at_ms: Option<i64>,
    /// RAL-193: input tokens spent on this review's own conflict-resolution
    /// and prover agent calls during the current merge attempt only --
    /// excludes the tasks/cells that fed into the review.
    pub attempt_tokens_in: i64,
    /// RAL-193: output tokens, current merge attempt only. See
    /// [`Self::attempt_tokens_in`].
    pub attempt_tokens_out: i64,
    /// RAL-193: USD cost, current merge attempt only. See
    /// [`Self::attempt_tokens_in`].
    pub attempt_cost_usd: f64,
    /// RAL-193: input tokens spent on this review's own conflict-resolution
    /// and prover agent calls, cumulative across every rebase/re-merge
    /// attempt this review has gone through.
    pub cumulative_tokens_in: i64,
    /// RAL-193: output tokens, cumulative across every attempt. See
    /// [`Self::cumulative_tokens_in`].
    pub cumulative_tokens_out: i64,
    /// RAL-193: USD cost, cumulative across every attempt -- the value
    /// [`Self::maximum_budget_usd`] is enforced against. See
    /// [`Self::cumulative_tokens_in`].
    pub cumulative_cost_usd: f64,
    /// RAL-273: a one-shot, GUI-facing notice, e.g. `"forge_reorder_interrupted_local"`
    /// when an incoming GitHub/GitLab stack reorder interrupted a local
    /// reorder in flight. `None` when there is nothing to show. The board
    /// shows [`Self::notice_message`] as a toast the first time it observes
    /// [`Self::notice_at_ms`] newer than what it last displayed for this
    /// guardian -- there is no server-side "seen" tracking or expiry.
    pub notice_kind: Option<String>,
    /// Human-readable text for [`Self::notice_kind`].
    pub notice_message: Option<String>,
    /// When [`Self::notice_kind`] was recorded (epoch ms). `None` alongside
    /// `notice_kind: None`.
    pub notice_at_ms: Option<i64>,
    /// This review's declared build step (RAL-342), from `[[review.auto_build]]`.
    /// `None` means the review declared `skip_auto_build = true` instead --
    /// unlike [`Self::resolver_agent`]-style overrides, `None` here is never
    /// "inherit the project config default": every guardian created after
    /// the RAL-342 migration has exactly one of this field or
    /// [`Self::skip_auto_build`] set, enforced at submit time
    /// (`reviews::require_auto_build_declaration`). A guardian created
    /// before that migration shipped simply has no auto_build tier at
    /// finalize time (see `guardian_merge::final_checks`).
    pub auto_build: Option<GuardianAutoBuild>,
}

/// Aggregated merge progress across a guardian's branches, ported from
/// board.html's `mergeProgress` (CLI_PARITY_PLAN.local.md Phase 5). "merged"
/// counts `done` + `conflict_resolved` branches; `pct` is `merged/total*100`
/// (`0.0` when there are no branches).
#[derive(Debug, Clone, Copy, Serialize)]
pub struct MergeProgress {
    /// Branches whose `merge_status` is `done` or `conflict_resolved`.
    pub done: usize,
    /// Total branch count.
    pub total: usize,
    /// `done / total * 100`, `0.0` when `total == 0`.
    pub pct: f64,
    /// Branches whose `merge_status` is `failed`.
    pub failed: usize,
}

impl MergeProgress {
    fn compute(branches: &[BranchView]) -> Self {
        let total = branches.len();
        let done = branches
            .iter()
            .filter(|b| matches!(b.merge_status.as_str(), "done" | "conflict_resolved"))
            .count();
        let failed = branches
            .iter()
            .filter(|b| b.merge_status == "failed")
            .count();
        let pct = if total == 0 {
            0.0
        } else {
            100.0 * done as f64 / total as f64
        };
        Self {
            done,
            total,
            pct,
            failed,
        }
    }
}

/// Cell state priority for picking the "worst" state among several linked
/// cells, worst-first order (NOT a numeric scale -- first match in this
/// list wins). Ported verbatim from board.html's `CELL_STATE_RANK`
/// (CLI_PARITY_PLAN.local.md Phase 5).
pub const CELL_STATE_RANK: [&str; 6] = [
    "running",
    "failed",
    "pending",
    "queued",
    "cancelled",
    "done",
];

/// The first state in [`CELL_STATE_RANK`] present in `states`, or `states[0]`
/// if none match (mirrors the JS `CELL_STATE_RANK.find(...) || cells[0].state`
/// fallback). `None` when `states` is empty.
#[must_use]
pub fn worst_cell_state<'a>(states: &[&'a str]) -> Option<&'a str> {
    for rank in CELL_STATE_RANK {
        if let Some(s) = states.iter().find(|s| **s == rank) {
            return Some(*s);
        }
    }
    states.first().copied()
}

/// Which terminal-resume modes are available for a review branch's conflict
/// resolver. Ported from board.html's `resolverTerminalBtns` gating decision
/// (button labels/tooltips are UI-only and intentionally not reproduced here)
/// -- CLI_PARITY_PLAN.local.md Phase 5.
#[must_use]
pub fn terminal_modes_for(
    resolver_agent: Option<&str>,
    has_session_id: bool,
    has_worktree: bool,
    cwd: &Path,
) -> Vec<&'static str> {
    if has_session_id {
        return vec!["readonly", "open"];
    }
    let default_agent = crate::config::resolve(cwd)
        .default_resolver_agent()
        .to_string();
    let agent = resolver_agent.unwrap_or(&default_agent);
    let is_cli_agent = matches!(agent, "claude-code" | "codex" | "codex-cli" | "pi");
    if is_cli_agent && has_worktree {
        return vec!["worktree"];
    }
    Vec::new()
}

/// The ordered `(position, branch)` list a merge run consumes.
#[derive(Debug, Clone)]
pub struct OrderedBranch {
    /// Stable, globally-unique branch id (RAL-122) -- the correct addressing
    /// key for `Store` setters/getters, unlike `position`.
    pub id: String,
    /// Order position.
    pub position: i64,
    /// Branch name.
    pub branch: String,
    /// Whether this branch is enabled in the stack (RAL-43).
    pub enabled: bool,
    /// RAL-378: see [`BranchView::readable_review_branch`].
    pub readable_review_branch: bool,
    /// RAL-378: see [`BranchView::review_branch_name`].
    pub review_branch_name: Option<String>,
}

impl Store {
    /// Create a guardian in the `Collecting` state; returns its id.
    pub fn create_guardian(&self, name: &str, base_branch: &str, git_root: &str) -> Result<String> {
        self.create_guardian_for_squad(name, base_branch, git_root, None)
    }

    /// Create a guardian, optionally tagged with the squad it was derived from.
    pub fn create_guardian_for_squad(
        &self,
        name: &str,
        base_branch: &str,
        git_root: &str,
        squad_id: Option<&str>,
    ) -> Result<String> {
        self.create_guardian_keyed(name, base_branch, git_root, squad_id, None)
    }

    /// Like [`Store::create_guardian_for_squad`] but also stores a stable
    /// `review_key` (from a `ralphus:new-review/<key>` link id) so later
    /// submissions can find this guardian and append their branches to it.
    pub fn create_guardian_keyed(
        &self,
        name: &str,
        base_branch: &str,
        git_root: &str,
        squad_id: Option<&str>,
        review_key: Option<&str>,
    ) -> Result<String> {
        let id = self.next_id("guardian_seq", "guardian")?;
        let now = crate::store::now_ms();
        // RAL-307: stamp this project's *effective* `match_pr_branch_name`
        // (explicit `.ralphus.toml [review]` value > this project's
        // registration-time stamp > the live global config > `false`) onto
        // the new review at creation time -- unlike `skip_base_updates`
        // (left `NULL`/"inherit" forever), this setting is meant to be a
        // per-review starting point the board checkbox then edits directly,
        // so it must be a concrete value from the start, not a perpetual
        // fallback chain.
        let explicit_project = crate::config::project_review_config(Path::new(git_root));
        let stamp = self.project_match_pr_branch_name_stamp(git_root);
        let live_global = crate::config::global_review_config();
        let match_pr_branch_name = explicit_project
            .match_pr_branch_name
            .or(stamp)
            .or(live_global.match_pr_branch_name)
            .unwrap_or(false);
        // RAL-317: same stamping shape as `match_pr_branch_name` above -- a
        // concrete value from the start, not a perpetual "inherit" fallback.
        let auto_submit_pr_stack_stamp = self.project_auto_submit_pr_stack_stamp(git_root);
        let auto_submit_pr_stack = explicit_project
            .auto_submit_pr_stack
            .or(auto_submit_pr_stack_stamp)
            .or(live_global.auto_submit_pr_stack)
            .unwrap_or(false);
        // RAL-378: same stamping shape again. `readable_review_branch` is set
        // unconditionally here -- every review created from now on names its
        // combined branch readably; only reviews that predate the column keep
        // the internal `guardian/<id>/review` ref.
        let separate_pr_branch_stamp = self.project_separate_pr_branch_stamp(git_root);
        let separate_pr_branch = explicit_project
            .separate_pr_branch
            .or(separate_pr_branch_stamp)
            .or(live_global.separate_pr_branch)
            .unwrap_or(false);
        self.conn.execute(
            "INSERT INTO guardians(id, name, base_branch, git_root, review_branch, status, detail, squad_id, review_key, created_at_ms, updated_at_ms, base_changed_at_ms, match_pr_branch_name, auto_submit_pr_stack, separate_pr_branch, readable_review_branch)
             VALUES(?,?,?,?,NULL,?,NULL,?,?,?,?,?,?,?,?,1)",
            params![id, name, base_branch, git_root, GuardianStatus::Collecting.as_str(), squad_id, review_key, now, now, now, i64::from(match_pr_branch_name), i64::from(auto_submit_pr_stack), i64::from(separate_pr_branch)],
        )?;
        Ok(id)
    }

    /// The id of the guardian that owns `review_key`, if one exists. Used to link
    /// a `ralphus:new-review/<key>` review across separate submissions.
    /// Atomically transition a guardian from `collecting`, `merge_failed`, or
    /// `in_review` to `merging`. The `in_review` case (RAL-108) is what makes
    /// pressing "Merge / rebase" on an already-done review force a full rebase
    /// walk instead of silently 409ing: `run_merge` resets every enabled branch
    /// back to `pending` before it starts, so the board visibly steps them back
    /// through to `done`. Returns `true` when this call won the transition (the
    /// caller should proceed with the merge), `false` when the guardian was
    /// already `merging`, or in a state that must never be reopened implicitly
    /// (`approved`, `deployed`, `cancelled`) — another caller claimed it first,
    /// or an explicit cancel is required. `merge_stopped` (RAL-249) is
    /// claimable so a stopped rebase can be resumed. States this transitions
    /// into `merging`: `collecting`, `merge_failed`, `merge_stopped`,
    /// `in_review`.
    pub fn claim_guardian_merge(&self, id: &str) -> Result<bool> {
        let n = self.conn.execute(
            "UPDATE guardians SET status='merging', updated_at_ms=? \
             WHERE id=? AND status IN ('collecting','merge_failed','merge_stopped','in_review')",
            params![crate::store::now_ms(), id],
        )?;
        Ok(n > 0)
    }

    /// Atomically claim the right to resolve `input_name` for `guardian_id`
    /// (RAL-164) via "set it for me". Returns `true` when this call won the
    /// claim (the caller should proceed to spawn the resolver), `false` when
    /// a resolution for this exact (guardian, input) pair is already in
    /// flight. This -- not a disabled button -- is what makes "set it for
    /// me" spam-proof: a double-click, a second browser tab, or a direct API
    /// call all race the same atomic UPSERT and only one can win.
    pub fn claim_guardian_input_resolution(
        &self,
        guardian_id: &str,
        input_name: &str,
    ) -> Result<bool> {
        let n = self.conn.execute(
            "INSERT INTO guardian_input_resolutions (guardian_id, input_name, status, value, updated_at_ms)
             VALUES (?, ?, 'resolving', NULL, ?)
             ON CONFLICT(guardian_id, input_name) DO UPDATE SET status='resolving', value=NULL, updated_at_ms=excluded.updated_at_ms
             WHERE guardian_input_resolutions.status != 'resolving'",
            params![guardian_id, input_name, crate::store::now_ms()],
        )?;
        Ok(n > 0)
    }

    /// Record a resolved value once the resolver LLM call completes
    /// successfully (RAL-164). Does not itself update
    /// [`GuardianView::input_values`] -- callers that want the resolved
    /// value to become the new default also call
    /// [`Self::merge_guardian_input_values`].
    pub fn set_guardian_input_resolution_ready(
        &self,
        guardian_id: &str,
        input_name: &str,
        value: &str,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE guardian_input_resolutions SET status='ready', value=?, updated_at_ms=? \
             WHERE guardian_id=? AND input_name=?",
            params![value, crate::store::now_ms(), guardian_id, input_name],
        )?;
        Ok(())
    }

    /// Record that a resolution attempt failed (the LLM call errored, timed
    /// out, or returned nothing usable) (RAL-164).
    pub fn set_guardian_input_resolution_failed(
        &self,
        guardian_id: &str,
        input_name: &str,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE guardian_input_resolutions SET status='failed', updated_at_ms=? \
             WHERE guardian_id=? AND input_name=?",
            params![crate::store::now_ms(), guardian_id, input_name],
        )?;
        Ok(())
    }

    /// Every input-resolution row for `guardian_id` (RAL-164), for
    /// [`GuardianView::input_resolutions`].
    fn guardian_input_resolutions(
        &self,
        guardian_id: &str,
    ) -> Result<std::collections::HashMap<String, InputResolutionView>> {
        let mut stmt = self.conn.prepare(
            "SELECT input_name, status, value FROM guardian_input_resolutions WHERE guardian_id=?",
        )?;
        let rows = stmt
            .query_map(params![guardian_id], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    InputResolutionView {
                        status: r.get(1)?,
                        value: r.get(2)?,
                    },
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows.into_iter().collect())
    }

    /// Crash recovery: resolutions left `resolving` after an unclean
    /// shutdown have no background thread to complete them. Reset each to
    /// `failed` so the UI never shows a permanently-stuck spinner (RAL-164).
    /// Run at daemon startup, mirrors [`Self::recover_orphaned_merges`].
    /// Returns the recovered `(guardian_id, input_name)` pairs.
    pub fn recover_orphaned_input_resolutions(&self) -> Result<Vec<(String, String)>> {
        let pairs: Vec<(String, String)> = {
            let mut stmt = self.conn.prepare(
                "SELECT guardian_id, input_name FROM guardian_input_resolutions WHERE status='resolving'",
            )?;
            stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
                .collect::<std::result::Result<Vec<_>, _>>()?
        };
        if !pairs.is_empty() {
            self.conn.execute(
                "UPDATE guardian_input_resolutions SET status='failed', updated_at_ms=? \
                 WHERE status='resolving'",
                params![crate::store::now_ms()],
            )?;
        }
        Ok(pairs)
    }

    /// Ids of the guardians derived from a squad, oldest first.
    pub fn guardians_for_squad(&self, squad_id: &str) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id FROM guardians WHERE squad_id=? ORDER BY created_at_ms, id")?;
        let ids = stmt
            .query_map(params![squad_id], |r| r.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(ids)
    }

    /// Ids of collecting guardians this squad's cells contribute to.
    ///
    /// Prefers each cell's direct `review_guardian_id` (RAL-314: set at
    /// submit time by `reviews::derive_reviews`, alongside `review_branch`)
    /// so two unrelated squads' cells that happen to record the identical
    /// branch *string* (e.g. repeat submissions against the same worktree,
    /// each minting its own fresh guardian) are never conflated -- this
    /// matters here specifically because the scheduler's stack-readiness
    /// gating (`scheduler::mark_ready_...`) reads this list to decide which
    /// guardian's branch-readiness a squad's cells are contributing to, not
    /// just which reviews cosmetically show up on the board.
    ///
    /// Falls back to the old `cells.review_branch = guardian_branches.branch`
    /// string join for a cell with no `review_guardian_id` -- a pre-RAL-314
    /// row, or one whose review linkage came from the manual
    /// `POST /api/guardians/{id}/branches` attach path, which has no
    /// submission-time cell membership to record one against. This also
    /// covers a guardian *found* (not created) by a later submission that
    /// shares a `ralphus:new-review/<key>` link, whose `squad_id` still
    /// points at whichever squad created it.
    pub fn collecting_guardians_for_cells(&self, squad_id: &str) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT g.id FROM guardians g
             JOIN cells s ON (
                 s.review_guardian_id = g.id
                 OR (
                     s.review_guardian_id IS NULL
                     AND EXISTS (
                         SELECT 1 FROM guardian_branches gb
                         WHERE gb.guardian_id = g.id AND gb.branch = s.review_branch
                     )
                 )
             )
             WHERE s.squad_id = ? AND g.status = 'collecting'
             ORDER BY g.created_at_ms, g.id",
        )?;
        let ids = stmt
            .query_map(params![squad_id], |r| r.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(ids)
    }

    /// Ids of collecting guardians that are ready to start: every enabled branch
    /// that has a contributing cell (matched by `cells.review_branch =
    /// guardian_branches.branch`) has its *most recently created* such cell in
    /// the `done` state. Guardians with no cell-linked branches are excluded
    /// (they haven't been triggered yet). Used at daemon startup to recover
    /// guardians that were left `collecting` because the daemon was restarted
    /// after the squad completed.
    ///
    /// This requires the latest matching cell done, not just one — which,
    /// since RAL-159, includes implicit worktree-sharing siblings alongside the
    /// explicitly review-linked cell (see `mark_ready_branches_with_done_cells`).
    /// "Latest" (highest `rowid`, mirroring `force_start_disable_branches`) matters
    /// because a branch name is stable across resubmissions: a squad retried
    /// under a new squad id reuses the same `review_branch`, leaving the earlier
    /// attempt's non-`done` cell row (failed, or superseded mid-run) still in the
    /// table. Requiring *every* historical row done, as a plain `!= 'done'`
    /// filter over the full join would, means a stale row from a superseded
    /// attempt blocks readiness forever even after a fresh attempt succeeds.
    pub fn collecting_guardians_ready(&self) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT id FROM guardians WHERE status = 'collecting'
             AND EXISTS (
                 SELECT 1 FROM guardian_branches gb
                 JOIN cells s ON s.review_branch = gb.branch
                 WHERE gb.guardian_id = guardians.id AND gb.enabled = 1
             )
             AND NOT EXISTS (
                 SELECT 1 FROM guardian_branches gb
                 WHERE gb.guardian_id = guardians.id AND gb.enabled = 1
                   AND (
                       SELECT s.state FROM cells s
                       WHERE s.review_branch = gb.branch
                       ORDER BY s.rowid DESC LIMIT 1
                   ) != 'done'
             )
             ORDER BY created_at_ms, id",
        )?;
        let ids = stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(ids)
    }

    /// Names of `guardian_id`'s enabled branches whose *most recently created*
    /// linked cell (`cells.review_branch = guardian_branches.branch`, highest
    /// `rowid`) is not `done` yet -- i.e. branches a merge started right now
    /// would run ahead of, because they're still waiting on their upstream Cell
    /// (RAL-255). Mirrors [`Self::collecting_guardians_ready`]'s own `NOT
    /// EXISTS` clause but as a single-guardian query callable from
    /// [`crate::guardian_merge::start_merge`] itself, so every caller is
    /// protected, not just the ones that already route through
    /// `collecting_guardians_ready`.
    ///
    /// Uses the latest cell row per branch, not "all matching rows done", for
    /// the same reason [`Self::force_start_disable_branches`] already does:
    /// a branch name is stable across resubmissions (a squad retried under a
    /// new squad id reuses the same `review_branch`), so an older, superseded
    /// attempt's non-`done` cell row must not block a branch whose current
    /// attempt has actually finished.
    ///
    /// A branch with no linked cell at all (e.g. manually added, never
    /// wired to a task) never appears here -- there's nothing to wait for.
    pub fn guardian_unfinished_linked_branches(&self, guardian_id: &str) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT branch FROM (
                 SELECT gb.branch AS branch,
                        (
                            SELECT s.state FROM cells s
                            WHERE s.review_branch = gb.branch
                            ORDER BY s.rowid DESC LIMIT 1
                        ) AS latest_state
                 FROM guardian_branches gb
                 WHERE gb.guardian_id = ?1 AND gb.enabled = 1
             )
             WHERE latest_state != 'done'
             ORDER BY branch",
        )?;
        let branches = stmt
            .query_map(params![guardian_id], |r| r.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(branches)
    }

    /// Ids of guardians that have already left `collecting` (`in_review` or
    /// `merge_failed`) but still have an enabled branch stuck at `pending` whose
    /// contributing cell has since finished. This is the straggler case: a
    /// linked review (RAL-97/98) whose branches arrive from separate squads, where
    /// the guardian moved on after its first squad's task finished, before the
    /// second squad's task — and therefore `collecting_guardians_for_cells`,
    /// which only matches `status = 'collecting'` — ever saw it. Picked up by
    /// [`crate::guardian_merge::review_maintenance`]'s periodic sweep so the
    /// branch is not stuck `pending` forever.
    pub fn guardians_with_ready_stragglers(&self) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT g.id FROM guardians g
             JOIN guardian_branches gb ON g.id = gb.guardian_id
             JOIN cells s ON s.review_branch = gb.branch
             WHERE g.status IN ('in_review','merge_failed')
               AND gb.enabled = 1 AND gb.merge_status = 'pending' AND s.state = 'done'
             ORDER BY g.created_at_ms, g.id",
        )?;
        let ids = stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(ids)
    }

    /// Promote a guardian's enabled `pending` branches to `ready`, but only those
    /// whose contributing cell(s) have ALL actually finished — unlike
    /// [`Self::mark_guardian_branches_ready`], which blindly promotes every
    /// pending branch and is only safe to call once the caller has separately
    /// verified every blocking task is done. Used for the straggler sweep, where
    /// a guardian may still have other, genuinely-unfinished pending branches
    /// that must not be promoted early.
    ///
    /// RAL-159: a branch's `cells.review_branch` set can now contain more
    /// than one row — an explicitly review-linked cell plus any sibling
    /// cells that merely share its git worktree (e.g. a nested cwd
    /// subfolder), attached by `reviews::derive_reviews`. Requiring `NOT
    /// EXISTS` a non-done contributor (rather than the old `EXISTS` a done
    /// one) means the branch is only marked `ready` once every one of them —
    /// explicit or implicit — has finished, not just the first.
    pub fn mark_ready_branches_with_done_cells(&self, guardian_id: &str) -> Result<usize> {
        let n = self.conn.execute(
            "UPDATE guardian_branches
             SET merge_status='ready'
             WHERE guardian_id=? AND enabled=1 AND merge_status='pending'
               AND EXISTS (
                   SELECT 1 FROM cells s
                   WHERE s.review_branch = guardian_branches.branch
               )
               AND NOT EXISTS (
                   SELECT 1 FROM cells s
                   WHERE s.review_branch = guardian_branches.branch AND s.state != 'done'
               )",
            params![guardian_id],
        )?;
        Ok(n)
    }

    /// RAL-280 dispatch-priority signal for one cell: is it the first
    /// not-yet-contributed branch of a Review it feeds, and if so how many
    /// enabled branches does that Review have? Reuses the same "enabled +
    /// contributing cell not done" notion as
    /// [`Self::mark_ready_branches_with_done_cells`]/
    /// [`Self::guardian_unfinished_linked_branches`] — an earlier-position
    /// enabled branch with no linked cell at all never blocks (nothing to
    /// wait for), matching those.
    ///
    /// Returns `None` when the cell has no `review_branch`, or its branch
    /// isn't (yet) the earliest unfinished one in any guardian stack it
    /// belongs to — callers treat that as "no scheduling boost", never as
    /// "unschedulable". When the branch is the first-in-line for more than
    /// one guardian (a branch name reused across stacks), the largest
    /// enabled-branch count wins, so the scheduler always front-loads the
    /// longest critical path it can unblock.
    ///
    /// The caller (`scheduler.rs`'s dispatch loop) computes this once, at the
    /// moment a cell becomes ready to dispatch, and never recomputes it later
    /// as branches are added/reordered/dropped — deliberately, so the
    /// scheduler doesn't pay for a recompute on every tick.
    pub fn cell_review_dispatch_priority(
        &self,
        squad_id: &str,
        task_idx: i64,
        idx: i64,
    ) -> Result<Option<usize>> {
        let branch: Option<String> = self
            .conn
            .query_row(
                "SELECT review_branch FROM cells WHERE squad_id=? AND task_idx=? AND idx=?",
                params![squad_id, task_idx, idx],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        let Some(branch) = branch else {
            return Ok(None);
        };

        let mut stmt = self.conn.prepare(
            "SELECT guardian_id, position FROM guardian_branches WHERE branch=? AND enabled=1",
        )?;
        let candidates: Vec<(String, i64)> = stmt
            .query_map(params![branch], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        let mut best: Option<usize> = None;
        for (guardian_id, position) in candidates {
            let has_unfinished_earlier: i64 = self.conn.query_row(
                "SELECT EXISTS(
                     SELECT 1 FROM guardian_branches gb
                     JOIN cells s ON s.review_branch = gb.branch
                     WHERE gb.guardian_id = ?1 AND gb.enabled = 1 AND gb.position < ?2
                       AND s.state != 'done'
                 )",
                params![guardian_id, position],
                |r| r.get(0),
            )?;
            if has_unfinished_earlier != 0 {
                continue;
            }
            let enabled_count: i64 = self.conn.query_row(
                "SELECT COUNT(*) FROM guardian_branches WHERE guardian_id=? AND enabled=1",
                params![guardian_id],
                |r| r.get(0),
            )?;
            let enabled_count = enabled_count.max(0) as usize;
            best = Some(best.map_or(enabled_count, |b| b.max(enabled_count)));
        }
        Ok(best)
    }

    /// Record the stack configuration a staged merge pass built against (RAL-265):
    /// a hash of the enabled-branch order/identity plus each project's resolved
    /// base commit. Written after every (partial or full) build pass so the next
    /// pass can tell a still-valid `Done` prefix (same signature → resume) from a
    /// changed base or branch config (different signature → rebuild the prefix).
    pub fn set_guardian_build_signature(&self, id: &str, sig: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE guardians SET build_signature=?, updated_at_ms=? WHERE id=?",
            params![sig, crate::store::now_ms(), id],
        )?;
        Ok(())
    }

    /// The last recorded build signature for `id`, if any. `None` means no prior
    /// (partial or full) build pass has completed, so there is nothing to resume.
    pub fn guardian_build_signature(&self, id: &str) -> Result<Option<String>> {
        let value: Option<Option<String>> = self
            .conn
            .query_row(
                "SELECT build_signature FROM guardians WHERE id=?",
                params![id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(value.flatten())
    }

    /// Ids of collecting guardians that are mid-incremental (RAL-265) and can
    /// still make staged progress: at least one enabled branch is in a
    /// buildable-but-not-terminal state (`ready` — its cell done, waiting to
    /// rebase — or `in_progress`/`proof_pending`/`actioning`, a branch a crashed
    /// pass left half-built) while other enabled branches are still `pending`.
    /// Used at daemon startup so a restart doesn't strand a partially rebuilt
    /// stack waiting for a task completion that never comes.
    pub fn collecting_guardians_resumable(&self) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT g.id FROM guardians g
             JOIN guardian_branches gb ON gb.guardian_id = g.id
             WHERE g.status = 'collecting'
               AND gb.enabled = 1
               AND gb.merge_status IN ('ready', 'in_progress', 'proof_pending', 'actioning')
               AND EXISTS (
                   SELECT 1 FROM guardian_branches other
                   WHERE other.guardian_id = g.id AND other.enabled = 1
                     AND other.merge_status = 'pending'
               )
             ORDER BY g.created_at_ms, g.id",
        )?;
        let ids = stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(ids)
    }

    /// The `cwd` of the most recent cell that contributed to `branch` (matched
    /// via `cells.review_branch`), if any (RAL-103). Used to compute a
    /// preliminary, git-log-only change summary from the task's own worktree
    /// before any review worktree has been built for that branch.
    pub fn cell_cwd_for_branch(&self, branch: &str) -> Result<Option<String>> {
        let cwd: Option<Option<String>> = self
            .conn
            .query_row(
                "SELECT cwd FROM cells WHERE review_branch=? ORDER BY rowid DESC LIMIT 1",
                params![branch],
                |r| r.get(0),
            )
            .optional()?;
        Ok(cwd.flatten())
    }

    /// Atomically reopen a guardian already out of `collecting` (`in_review` or
    /// `merge_failed`) back to `merging`, to pick up a straggler branch. Since
    /// RAL-108, [`Self::claim_guardian_merge`] also claims `in_review` guardians
    /// (for the user-facing "Merge / rebase" button), so the two now overlap on
    /// `in_review`/`merge_failed`; this one stays separate because it omits
    /// `collecting`, which the straggler sweep never targets. Guardians that
    /// are `approved`, `deployed`, or `cancelled` are never matched and therefore
    /// never reopened.
    pub fn reopen_guardian_merge(&self, id: &str) -> Result<bool> {
        let n = self.conn.execute(
            "UPDATE guardians SET status='merging', updated_at_ms=? \
             WHERE id=? AND status IN ('in_review','merge_failed')",
            params![crate::store::now_ms(), id],
        )?;
        Ok(n > 0)
    }

    /// Ids of guardians whose merge was interrupted by a daemon restart (status =
    /// `merging` with no live background thread). Called at startup to resume them.
    pub fn interrupted_merges(&self) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT id FROM guardians WHERE status='merging' ORDER BY created_at_ms, id",
        )?;
        let ids = stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(ids)
    }

    /// Append a branch to a guardian at the next position. Returns its position.
    /// The branch is assigned to the guardian's primary `git_root` (single-project).
    pub fn add_guardian_branch(&self, guardian_id: &str, branch: &str) -> Result<i64> {
        self.add_guardian_branch_with_project(guardian_id, branch, None)
    }

    /// Append a branch to a guardian, explicitly naming which project (git root)
    /// this branch lives in. `project = None` means "same as guardian's git_root".
    /// Used for multi-project link-group guardians (RAL-29).
    pub fn add_guardian_branch_with_project(
        &self,
        guardian_id: &str,
        branch: &str,
        project: Option<&str>,
    ) -> Result<i64> {
        self.guardian_exists(guardian_id)?;
        let next: i64 = self.conn.query_row(
            "SELECT COALESCE(MAX(position) + 1, 0) FROM guardian_branches WHERE guardian_id=?",
            params![guardian_id],
            |r| r.get(0),
        )?;
        // RAL-122: mint a stable, globally-unique branch id (independent of
        // `position`, which is a mutable display/order attribute only).
        let branch_id = self.next_id("branch_seq", "branch")?;
        self.conn.execute(
            "INSERT INTO guardian_branches(guardian_id, position, branch, merge_status, detail, project, id, readable_review_branch)
             VALUES(?,?,?,?,NULL,?,?,1)",
            params![
                guardian_id,
                next,
                branch,
                MergeStatus::Pending.as_str(),
                project,
                branch_id
            ],
        )?;
        Ok(next)
    }

    /// Move a branch from one guardian (review) to another (RAL-118). This is
    /// a *move*, not a copy (RAL-118 Q3): the branch leaves `from_guardian_id`'s
    /// stack entirely and is appended to the end of `to_guardian_id`'s stack.
    /// The source guardian's remaining branches are renumbered to stay
    /// contiguous, mirroring [`Self::reorder_guardian_branches`]'s compaction.
    /// The branch's merge state is reset to `Pending` with its review-branch/
    /// worktree/conflict/resolver-session fields cleared, since its rebase
    /// base changes along with its new position in the destination's stack --
    /// callers must re-run `start_merge` on both guardians afterward (see
    /// `server::guardian_move_branch`) for the new stacked rebase to actually
    /// be built.
    ///
    /// `moved_from_guardian_id` records the *original* owning guardian (RAL-118
    /// Q2 provenance pointer) and is preserved across repeated moves, so a
    /// branch moved more than once still identifies where it truly started.
    ///
    /// Returns [`StoreError::NotFound`] if either guardian, or the branch
    /// identified by `branch_id` in the source guardian, does not exist.
    /// Returns [`StoreError::InvalidTransition`] if source and destination are
    /// the same guardian, if they are not in the same git repository (a branch
    /// cannot move between unrelated repos), or if either guardian is
    /// currently `merging` (RAL-118 Q4: moving is blocked while a review has
    /// an active merge/rebase in flight; the caller must let it finish, or
    /// cancel it, first). Returns the branch's new position in the
    /// destination guardian on success. The branch's stable `id` (RAL-122)
    /// is carried forward unchanged onto the re-inserted row, so its identity
    /// survives the move even though its `position` is recomputed.
    pub fn move_guardian_branch(
        &mut self,
        from_guardian_id: &str,
        branch_id: &str,
        to_guardian_id: &str,
    ) -> Result<i64> {
        if from_guardian_id == to_guardian_id {
            return Err(StoreError::InvalidTransition(
                "source and destination review are the same".to_string(),
            ));
        }
        let (from_status, from_root): (String, String) = self
            .conn
            .query_row(
                "SELECT status, git_root FROM guardians WHERE id=?",
                params![from_guardian_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?
            .ok_or(StoreError::NotFound)?;
        let (to_status, to_root): (String, String) = self
            .conn
            .query_row(
                "SELECT status, git_root FROM guardians WHERE id=?",
                params![to_guardian_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?
            .ok_or(StoreError::NotFound)?;
        if (from_status == "merging" || from_status == "merge_stopped")
            || (to_status == "merging" || to_status == "merge_stopped")
        {
            return Err(StoreError::InvalidTransition(
                "cannot move a branch while the source or destination review has a merge/rebase in progress or stopped mid-rebase"
                    .to_string(),
            ));
        }
        if from_root != to_root {
            return Err(StoreError::InvalidTransition(
                "source and destination review are not in the same git repository".to_string(),
            ));
        }
        let (branch, existing_provenance): (String, Option<String>) = self
            .conn
            .query_row(
                "SELECT branch, moved_from_guardian_id FROM guardian_branches
                 WHERE guardian_id=? AND id=?",
                params![from_guardian_id, branch_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?
            .ok_or(StoreError::NotFound)?;
        let provenance = existing_provenance.unwrap_or_else(|| from_guardian_id.to_string());

        let tx = self.conn.transaction()?;
        tx.execute(
            "DELETE FROM guardian_branches WHERE guardian_id=? AND id=?",
            params![from_guardian_id, branch_id],
        )?;
        // Compact the source guardian's remaining positions (same shift-then-
        // rewrite trick as `reorder_guardian_branches`, to avoid colliding with
        // the `(guardian_id, position)` primary key mid-transaction).
        const SHIFT: i64 = 1_000_000;
        tx.execute(
            "UPDATE guardian_branches SET position = position + ?1 WHERE guardian_id=?2",
            params![SHIFT, from_guardian_id],
        )?;
        {
            let mut stmt = tx.prepare(
                "SELECT position FROM guardian_branches
                 WHERE guardian_id=?1 AND position >= ?2 ORDER BY position",
            )?;
            let leftover: Vec<i64> = stmt
                .query_map(params![from_guardian_id, SHIFT], |r| r.get(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            drop(stmt);
            for (next, old_pos) in leftover.into_iter().enumerate() {
                tx.execute(
                    "UPDATE guardian_branches SET position=?1 WHERE guardian_id=?2 AND position=?3",
                    params![next as i64, from_guardian_id, old_pos],
                )?;
            }
        }
        let new_position: i64 = tx.query_row(
            "SELECT COALESCE(MAX(position) + 1, 0) FROM guardian_branches WHERE guardian_id=?",
            params![to_guardian_id],
            |r| r.get(0),
        )?;
        tx.execute(
            // RAL-378: a moved branch is a fresh row under a different
            // review, and the move already discards its old review ref, so it
            // opts into readable naming even when the branch it came from
            // predates it. Its first build in the destination claims a name of
            // its own, collision-checked against the source's open PR alias
            // like any other.
            "INSERT INTO guardian_branches
                 (guardian_id, position, branch, merge_status, detail, enabled, moved_from_guardian_id, id, readable_review_branch)
             VALUES (?,?,?,?,NULL,1,?,?,1)",
            params![
                to_guardian_id,
                new_position,
                branch,
                MergeStatus::Pending.as_str(),
                provenance,
                branch_id,
            ],
        )?;
        tx.commit()?;
        Ok(new_position)
    }

    fn guardian_exists(&self, id: &str) -> Result<()> {
        let found: Option<i64> = self
            .conn
            .query_row("SELECT 1 FROM guardians WHERE id=?", params![id], |r| {
                r.get(0)
            })
            .optional()?;
        found.map(|_| ()).ok_or(StoreError::NotFound)
    }

    /// Rename a guardian. Returns [`StoreError::NotFound`] if it does not exist.
    pub fn rename_guardian(&self, id: &str, name: &str) -> Result<()> {
        let n = self.conn.execute(
            "UPDATE guardians SET name=?, updated_at_ms=? WHERE id=?",
            params![name, crate::store::now_ms(), id],
        )?;
        if n == 0 {
            Err(StoreError::NotFound)
        } else {
            Ok(())
        }
    }

    /// Delete a guardian and its branch rows. Returns [`StoreError::NotFound`] if
    /// it does not exist. Branches are removed explicitly so the delete also holds
    /// on connections without `ON DELETE CASCADE` enforcement (in-memory tests).
    pub fn delete_guardian(&self, id: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM guardian_branches WHERE guardian_id=?",
            params![id],
        )?;
        self.conn
            .execute("DELETE FROM events WHERE guardian_id=?", params![id])?;
        self.conn.execute(
            "DELETE FROM guardian_messages WHERE guardian_id=?",
            params![id],
        )?;
        self.conn
            .execute("DELETE FROM ghosts WHERE guardian_id=?", params![id])?;
        self.conn
            .execute("DELETE FROM hidden_items WHERE guardian_id=?", params![id])?;
        self.conn.execute(
            "DELETE FROM guardian_input_resolutions WHERE guardian_id=?",
            params![id],
        )?;
        // RAL-385: retired-worktree history lives exactly as long as its
        // review does, so delete it with the review. (The FK also cascades;
        // this explicit sweep matches `delete_guardian`'s other child tables,
        // which must not rely on cascade enforcement.)
        self.conn.execute(
            "DELETE FROM guardian_worktree_retirements WHERE guardian_id=?",
            params![id],
        )?;
        // RAL-320: watches are keyed by `EntityUri` string, not a `guardian_id`
        // FK column, so a deleted review's watches need an explicit sweep.
        self.conn.execute(
            "DELETE FROM watches WHERE entity_uri = 'guardian:'||?1",
            params![id],
        )?;
        let n = self
            .conn
            .execute("DELETE FROM guardians WHERE id=?", params![id])?;
        if n == 0 {
            Err(StoreError::NotFound)
        } else {
            Ok(())
        }
    }

    /// Append a message to a guardian's feedback thread. `role` is
    /// `"reviewer"` (the human) or `"guardian"` (the triage agent). `image` is
    /// an optional base64 data-URI attached to the message (RAL-59).
    /// `branch_id` scopes the message to one review branch (RAL-272).
    /// `author` is the registered user this feedback is attributed to (shown
    /// in the UI); `submitted_by` is the resolved authenticated/default
    /// requester (audit-only, never shown) (RAL-379). Returns the new
    /// message's `seq`. RAL-380: a `"reviewer"`-role message starts life with
    /// `action_status = Received` -- every message that role can ever get is
    /// posted through `guardian_merge::start_feedback`, which always kicks
    /// off a resolver-agent pass for it, so there is no "non-actionable
    /// reviewer message" to special-case here.
    #[allow(clippy::too_many_arguments)]
    pub fn add_guardian_message(
        &self,
        guardian_id: &str,
        role: &str,
        text: &str,
        image: Option<&str>,
        branch_id: Option<&str>,
        author: Option<&str>,
        submitted_by: Option<&str>,
    ) -> Result<i64> {
        let action_status = (role == "reviewer").then_some(FeedbackActionStatus::Received.as_str());
        self.conn.execute(
            "INSERT INTO guardian_messages(guardian_id, role, text, at_ms, image, branch_id, author, submitted_by, action_status) VALUES(?,?,?,?,?,?,?,?,?)",
            params![
                guardian_id,
                role,
                text,
                crate::store::now_ms(),
                image,
                branch_id,
                author,
                submitted_by,
                action_status,
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// One review branch's feedback thread, oldest first (RAL-272). Only
    /// messages explicitly scoped to `branch_id` are returned -- an
    /// unscoped message (`branch_id IS NULL`) never appears here.
    pub fn guardian_branch_messages(
        &self,
        guardian_id: &str,
        branch_id: &str,
    ) -> Result<Vec<MessageView>> {
        let mut stmt = self.conn.prepare(
            "SELECT seq, role, text, at_ms, image, author, submitted_by, action_status \
             FROM guardian_messages WHERE guardian_id=? AND branch_id=? ORDER BY seq",
        )?;
        let rows = stmt
            .query_map(params![guardian_id, branch_id], |r| {
                Ok(MessageView {
                    seq: r.get(0)?,
                    role: r.get(1)?,
                    text: r.get(2)?,
                    at_ms: r.get(3)?,
                    image: r.get(4)?,
                    author: r.get(5)?,
                    submitted_by: r.get(6)?,
                    action_status: r.get(7)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// RAL-380: mark every still-`received` "reviewer" message on this branch
    /// as `superseded` -- called right before a new feedback message is
    /// recorded, so an older bubble's checkmark can never be mistaken for
    /// progress on the newer request that is about to overtake it.
    pub fn supersede_pending_branch_feedback(
        &self,
        guardian_id: &str,
        branch_id: &str,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE guardian_messages SET action_status=? \
             WHERE guardian_id=? AND branch_id=? AND role='reviewer' AND action_status=?",
            params![
                FeedbackActionStatus::Superseded.as_str(),
                guardian_id,
                branch_id,
                FeedbackActionStatus::Received.as_str()
            ],
        )?;
        Ok(())
    }

    /// RAL-380: resolve one feedback message (by `seq`) to a terminal
    /// [`FeedbackActionStatus`] once `guardian_merge::run_feedback` finishes
    /// acting on it. Guarded on the row still being `Received` so a stale
    /// completion (e.g. a crash-recovered re-run finishing after a newer
    /// feedback message already superseded this one) can never clobber a
    /// `Superseded` row back to `Done`/`Failed`.
    pub fn set_message_action_status(&self, seq: i64, status: FeedbackActionStatus) -> Result<()> {
        self.conn.execute(
            "UPDATE guardian_messages SET action_status=? WHERE seq=? AND action_status=?",
            params![
                status.as_str(),
                seq,
                FeedbackActionStatus::Received.as_str()
            ],
        )?;
        Ok(())
    }

    /// RAL-380: the most recent still-`received` "reviewer" message's `seq`
    /// on this branch, if any -- used only by startup recovery
    /// (`scheduler::recover_interrupted_reviews`) to reattach a re-run
    /// `run_feedback` call to the message it's resuming, since at that point
    /// no other feedback round can be concurrently in flight for the branch.
    pub fn latest_received_feedback_message_seq(
        &self,
        guardian_id: &str,
        branch_id: &str,
    ) -> Result<Option<i64>> {
        Ok(self
            .conn
            .query_row(
                "SELECT seq FROM guardian_messages \
                 WHERE guardian_id=? AND branch_id=? AND role='reviewer' AND action_status=? \
                 ORDER BY seq DESC LIMIT 1",
                params![
                    guardian_id,
                    branch_id,
                    FeedbackActionStatus::Received.as_str()
                ],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Set a guardian's status (and optional detail).
    pub fn set_guardian_status(
        &self,
        id: &str,
        status: GuardianStatus,
        detail: Option<&str>,
    ) -> Result<()> {
        let old = self
            .conn
            .query_row(
                "SELECT status FROM guardians WHERE id=?",
                params![id],
                |r| r.get::<_, String>(0),
            )
            .optional()
            .ok()
            .flatten()
            .unwrap_or_else(|| "unknown".to_string());
        let n = self.conn.execute(
            "UPDATE guardians SET status=?, detail=?, updated_at_ms=? WHERE id=?",
            params![status.as_str(), detail, crate::store::now_ms(), id],
        )?;
        if n == 0 {
            Err(StoreError::NotFound)
        } else {
            crate::rlog!(
                INFO,
                "ralphus [state] guardian {id} {old} → {}",
                status.as_str()
            );
            let msg = match detail {
                Some(d) => format!("review → {} ({d})", status.as_str()),
                None => format!("review → {}", status.as_str()),
            };
            let _ = self.log_event(None, Some(id), "guardian", None, &msg);
            let failed = matches!(status, GuardianStatus::MergeFailed);
            let _ = self.notify_watchers(
                if failed {
                    crate::monitor::NotifiableEventKind::ReviewFailed
                } else {
                    crate::monitor::NotifiableEventKind::ReviewStatusChanged
                },
                &format!("guardian:{id}"),
                if failed {
                    crate::mailbox::MailboxPriority::Urgent
                } else {
                    crate::mailbox::MailboxPriority::Normal
                },
                &msg,
                None,
            );
            Ok(())
        }
    }

    /// Record a one-shot, GUI-facing notice for this guardian (RAL-273) --
    /// see [`GuardianView::notice_kind`]. Overwrites any previous notice;
    /// there is no queue, since the board only ever needs to show whichever
    /// one is newest.
    pub fn set_guardian_notice(&self, id: &str, kind: &str, message: &str) -> Result<()> {
        let n = self.conn.execute(
            "UPDATE guardians SET notice_kind=?, notice_message=?, notice_at_ms=? WHERE id=?",
            params![kind, message, crate::store::now_ms(), id],
        )?;
        if n == 0 {
            Err(StoreError::NotFound)
        } else {
            Ok(())
        }
    }

    /// Set the shell check commands run against the review worktree.
    pub fn set_guardian_checks(&self, id: &str, checks: &[String]) -> Result<()> {
        self.conn.execute(
            "UPDATE guardians SET checks=?, updated_at_ms=? WHERE id=?",
            params![crate::store::to_json(checks), crate::store::now_ms(), id],
        )?;
        let _ = self.notify_watchers(
            crate::monitor::NotifiableEventKind::ReviewSettingsChanged,
            &format!("guardian:{id}"),
            crate::mailbox::MailboxPriority::Normal,
            "review checks changed",
            None,
        );
        Ok(())
    }

    /// Set whether the merge skips per-branch worktrees (CCTL-156).
    pub fn set_guardian_skip_worktrees(&self, id: &str, skip: bool) -> Result<()> {
        let n = self.conn.execute(
            "UPDATE guardians SET skip_worktrees=?, updated_at_ms=? WHERE id=?",
            params![i64::from(skip), crate::store::now_ms(), id],
        )?;
        if n == 0 {
            Err(StoreError::NotFound)
        } else {
            Ok(())
        }
    }

    /// Set the conflict-resolver backend/model for this review (from the
    /// top-level `[[review]]` block's `agent`/`model`). Either may be `None` to leave
    /// that side falling back to the env override / built-in default.
    /// Set the machine this review's worktrees, rebase and conflict resolution
    /// run on (RAL-185). `None` means the daemon's own host.
    ///
    /// # Errors
    /// [`StoreError::NotFound`] when no such guardian exists.
    pub fn set_guardian_machine(&self, id: &str, machine: Option<&str>) -> Result<()> {
        let n = self.conn.execute(
            "UPDATE guardians SET machine=?, updated_at_ms=? WHERE id=?",
            params![machine, crate::store::now_ms(), id],
        )?;
        if n == 0 {
            return Err(crate::store::StoreError::NotFound);
        }
        Ok(())
    }

    pub fn set_guardian_resolver(
        &self,
        id: &str,
        agent: Option<&str>,
        model: Option<&str>,
    ) -> Result<()> {
        let n = self.conn.execute(
            "UPDATE guardians SET resolver_agent=?, resolver_model=?, updated_at_ms=? WHERE id=?",
            params![agent, model, crate::store::now_ms(), id],
        )?;
        if n == 0 {
            Err(StoreError::NotFound)
        } else {
            Ok(())
        }
    }

    /// Stamp a guardian's origin (RAL-318) -- [`GUARDIAN_ORIGIN_EXPLICIT`] or
    /// [`GUARDIAN_ORIGIN_ARBITER`]. Called once, right after
    /// `create_guardian_for_squad`, by `crate::reviews::derive_triage_pools`
    /// for an Arbiter-created review; every other creation path leaves the
    /// column at its `explicit` default.
    ///
    /// # Errors
    /// [`StoreError::NotFound`] when no such guardian exists.
    pub fn set_guardian_origin(&self, id: &str, origin: &str) -> Result<()> {
        let n = self.conn.execute(
            "UPDATE guardians SET origin=? WHERE id=?",
            params![origin, id],
        )?;
        if n == 0 {
            Err(StoreError::NotFound)
        } else {
            Ok(())
        }
    }

    /// Set this review's own USD spend cap (RAL-193), from the top-level
    /// `[[review]]` block's `maximum_budget_usd`. Enforced by the guardian
    /// merge machinery against the cumulative sum of [`Self::guardian_cost_total`]
    /// the same way a task/cell cap is enforced against a live `cost_usd`
    /// (RAL-161). `None` means no cap.
    ///
    /// # Errors
    /// [`StoreError::NotFound`] when no such guardian exists.
    pub fn set_guardian_maximum_budget_usd(&self, id: &str, cap: Option<f64>) -> Result<()> {
        let n = self.conn.execute(
            "UPDATE guardians SET maximum_budget_usd=?, updated_at_ms=? WHERE id=?",
            params![cap, crate::store::now_ms(), id],
        )?;
        if n == 0 {
            Err(StoreError::NotFound)
        } else {
            Ok(())
        }
    }

    /// This review's own USD spend cap (RAL-193), a lightweight single-column
    /// read for the merge engine's per-call budget check -- avoids paying for
    /// a full [`Self::get_guardian`] hydration on every resolver/prover call.
    ///
    /// # Errors
    /// [`StoreError::NotFound`] when no such guardian exists.
    pub fn guardian_maximum_budget_usd(&self, id: &str) -> Result<Option<f64>> {
        self.conn
            .query_row(
                "SELECT maximum_budget_usd FROM guardians WHERE id=?",
                params![id],
                |r| r.get(0),
            )
            .optional()?
            .ok_or(StoreError::NotFound)
    }

    /// Bump this review's merge-attempt counter and return the new value
    /// (RAL-193). Called once at the top of a merge/rebase attempt
    /// (`guardian_merge::run_merge`) so every cost line item recorded
    /// during that attempt can be attributed to it, and a per-attempt cost
    /// total can be told apart from the cumulative total.
    ///
    /// # Errors
    /// [`StoreError::NotFound`] when no such guardian exists.
    pub fn bump_guardian_merge_attempt(&self, id: &str) -> Result<i64> {
        let n = self.conn.execute(
            "UPDATE guardians SET merge_attempt = merge_attempt + 1, updated_at_ms=? WHERE id=?",
            params![crate::store::now_ms(), id],
        )?;
        if n == 0 {
            return Err(StoreError::NotFound);
        }
        Ok(self.conn.query_row(
            "SELECT merge_attempt FROM guardians WHERE id=?",
            params![id],
            |r| r.get(0),
        )?)
    }

    /// This guardian's current merge-attempt counter (RAL-193), for
    /// attributing a cost line item recorded outside `run_merge`'s own call
    /// (e.g. a standalone chat/feedback call) to whichever attempt is/was
    /// current.
    ///
    /// # Errors
    /// [`StoreError::NotFound`] when no such guardian exists.
    pub fn guardian_current_attempt(&self, id: &str) -> Result<i64> {
        self.conn
            .query_row(
                "SELECT merge_attempt FROM guardians WHERE id=?",
                params![id],
                |r| r.get(0),
            )
            .optional()?
            .ok_or(StoreError::NotFound)
    }

    /// Record one guardian LLM call's cost as a line item (RAL-193) --
    /// conflict resolution, proving, chat, feedback, summary
    /// generation, etc. `branch_id` is the stable per-branch id
    /// (`guardian_branches.id`) when the call is scoped to one stacked
    /// branch, `None` for a review-wide call (chat, combined final proof).
    #[allow(clippy::too_many_arguments)]
    pub fn record_guardian_cost(
        &self,
        guardian_id: &str,
        branch_id: Option<&str>,
        attempt: i64,
        kind: &str,
        tokens_in: i64,
        tokens_out: i64,
        cost_usd: f64,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO guardian_costs (guardian_id, branch_id, attempt, kind, tokens_in, tokens_out, cost_usd, created_at_ms)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                guardian_id,
                branch_id,
                attempt,
                kind,
                tokens_in,
                tokens_out,
                cost_usd,
                crate::store::now_ms(),
            ],
        )?;
        Ok(())
    }

    /// Sum every recorded cost line item for one guardian (RAL-193) --
    /// `(tokens_in, tokens_out, cost_usd)` cumulative across every
    /// rebase/re-merge attempt.
    pub fn guardian_cost_total(&self, guardian_id: &str) -> Result<(i64, i64, f64)> {
        Ok(self.conn.query_row(
            "SELECT COALESCE(SUM(tokens_in),0), COALESCE(SUM(tokens_out),0), COALESCE(SUM(cost_usd),0)
             FROM guardian_costs WHERE guardian_id=?",
            params![guardian_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?)
    }

    /// Sum cost line items for one guardian scoped to a single merge
    /// attempt (RAL-193) -- `(tokens_in, tokens_out, cost_usd)`.
    pub fn guardian_cost_total_for_attempt(
        &self,
        guardian_id: &str,
        attempt: i64,
    ) -> Result<(i64, i64, f64)> {
        Ok(self.conn.query_row(
            "SELECT COALESCE(SUM(tokens_in),0), COALESCE(SUM(tokens_out),0), COALESCE(SUM(cost_usd),0)
             FROM guardian_costs WHERE guardian_id=? AND attempt=?",
            params![guardian_id, attempt],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?)
    }

    /// Set the review source type (`git` or a future placeholder type).
    pub fn set_guardian_type(&self, id: &str, review_type: &str) -> Result<()> {
        let n = self.conn.execute(
            "UPDATE guardians SET review_type=?, updated_at_ms=? WHERE id=?",
            params![review_type, crate::store::now_ms(), id],
        )?;
        if n == 0 {
            Err(StoreError::NotFound)
        } else {
            Ok(())
        }
    }

    /// Set whether the finalize-time build/check step (explicit checks, config
    /// `auto_build`, and AI-inferred build) is skipped entirely (RAL-110).
    pub fn set_guardian_skip_auto_build(&self, id: &str, skip: bool) -> Result<()> {
        let n = self.conn.execute(
            "UPDATE guardians SET skip_auto_build=?, updated_at_ms=? WHERE id=?",
            params![i64::from(skip), crate::store::now_ms(), id],
        )?;
        if n == 0 {
            Err(StoreError::NotFound)
        } else {
            Ok(())
        }
    }

    /// Whether the guardian's finalize-time build/check step is opted out.
    pub fn guardian_skip_auto_build(&self, id: &str) -> Result<bool> {
        self.conn
            .query_row(
                "SELECT skip_auto_build FROM guardians WHERE id=?",
                params![id],
                |r| r.get(0),
            )
            .optional()?
            .ok_or(StoreError::NotFound)
    }

    /// Set this review's declared build step (RAL-342), from
    /// `[[review.auto_build]]`. Set once at review-derivation time
    /// (`reviews::derive_reviews`); blind-overwrite, not a read-then-merge
    /// like [`Self::set_guardian_build_env_overrides`], since the whole
    /// declaration is authored together in one TOML block. `None` records
    /// that the review declared `skip_auto_build = true` instead.
    pub fn set_guardian_auto_build(
        &self,
        id: &str,
        auto_build: Option<&GuardianAutoBuild>,
    ) -> Result<()> {
        let json =
            auto_build.map(|b| serde_json::to_string(b).unwrap_or_else(|_| "{}".to_string()));
        let n = self.conn.execute(
            "UPDATE guardians SET auto_build_json=?, updated_at_ms=? WHERE id=?",
            params![json, crate::store::now_ms(), id],
        )?;
        if n == 0 {
            Err(StoreError::NotFound)
        } else {
            Ok(())
        }
    }

    /// This review's declared build step -- see [`GuardianView::auto_build`].
    pub fn guardian_auto_build(&self, id: &str) -> Result<Option<GuardianAutoBuild>> {
        let json: Option<String> = self
            .conn
            .query_row(
                "SELECT auto_build_json FROM guardians WHERE id=?",
                params![id],
                |r| r.get(0),
            )
            .optional()?
            .ok_or(StoreError::NotFound)?;
        Ok(json.as_deref().and_then(|s| serde_json::from_str(s).ok()))
    }

    /// RAL-378: set this review's own override for whether its pull request
    /// is pushed to a branch separate from its review branch. `None` resets it
    /// to "inherit the project/global default".
    pub fn set_guardian_separate_pr_branch(&self, id: &str, enabled: Option<bool>) -> Result<()> {
        let n = self.conn.execute(
            "UPDATE guardians SET separate_pr_branch=?, updated_at_ms=? WHERE id=?",
            params![enabled.map(i64::from), crate::store::now_ms(), id],
        )?;
        if n == 0 {
            Err(StoreError::NotFound)
        } else {
            Ok(())
        }
    }

    /// RAL-378: record the readable name claimed for this review's *combined*
    /// worktree branch. Written once, at the first combined build; see
    /// [`GuardianView::review_branch_name`] for why it is never recomputed.
    pub fn set_guardian_review_branch_name(&self, id: &str, name: &str) -> Result<()> {
        let n = self.conn.execute(
            "UPDATE guardians SET review_branch_name=?, updated_at_ms=? WHERE id=?",
            params![name, crate::store::now_ms(), id],
        )?;
        if n == 0 {
            Err(StoreError::NotFound)
        } else {
            Ok(())
        }
    }

    /// RAL-378: record the readable name claimed for one stacked branch's
    /// review branch. Written once, at that branch's first build; deliberately
    /// not cleared by any of the reset paths (see
    /// [`BranchView::review_branch_name`]).
    pub fn set_branch_review_branch_name(
        &self,
        guardian_id: &str,
        branch_id: &str,
        name: &str,
    ) -> Result<()> {
        let n = self.conn.execute(
            "UPDATE guardian_branches SET review_branch_name=? WHERE guardian_id=? AND id=?",
            params![name, guardian_id, branch_id],
        )?;
        if n == 0 {
            Err(StoreError::NotFound)
        } else {
            Ok(())
        }
    }

    /// RAL-378: whether `name` is already spoken for as a branch name within
    /// `project_root`, ignoring the branch identified by `except`.
    ///
    /// Checks all three places a readable review branch can collide: another
    /// stacked branch's claimed name, another review's combined-branch name,
    /// and the remote alias of any pull request opened from this project --
    /// the last because with `separate_pr_branch` off the review branch *is*
    /// the PR branch, so a name already pushed under a different review's PR
    /// would be force-pushed over. Local refs are checked separately by the
    /// caller, which is the side that can reach git.
    ///
    /// Scoped to one project root rather than globally: two unrelated repos
    /// are free to have identically-named review branches, and a global check
    /// would push every name in the second repo to `-2` for no reason. Both
    /// sides of that comparison are trailing-separator-normalized; a residual
    /// mismatch (e.g. differing drive-letter case) can only *under*-report,
    /// and the caller's local-ref check is what actually stops two branches in
    /// one repo from claiming the same name.
    pub fn review_branch_name_taken(
        &self,
        project_root: &str,
        name: &str,
        except: Option<(&str, &str)>,
    ) -> Result<bool> {
        let (except_guardian, except_branch) = match except {
            Some((g, b)) => (g, b),
            None => ("", ""),
        };
        let project_root = project_root.trim_end_matches(['/', '\\']);
        let found: Option<i64> = self
            .conn
            .query_row(
                r"SELECT 1 FROM guardian_branches gb
                    JOIN guardians g ON g.id = gb.guardian_id
                   WHERE gb.review_branch_name = ?1
                     AND rtrim(COALESCE(gb.project, g.git_root), '/\') = ?2
                     AND NOT (gb.guardian_id = ?3 AND gb.id = ?4)
                  UNION ALL
                  SELECT 1 FROM guardians
                   WHERE review_branch_name = ?1 AND rtrim(git_root, '/\') = ?2
                  UNION ALL
                  SELECT 1 FROM guardian_pull_requests p
                    JOIN guardians g2 ON g2.id = p.guardian_id
                   WHERE p.branch_alias = ?1 AND rtrim(g2.git_root, '/\') = ?2
                  LIMIT 1",
                params![name, project_root, except_guardian, except_branch],
                |r| r.get(0),
            )
            .optional()?;
        Ok(found.is_some())
    }

    /// Set this review's own Proof-scope override (RAL-168): one of
    /// `"each_branch"`/`"final_branch"`/`"nothing"`. `None` resets it to
    /// "inherit the project-level default".
    pub fn set_guardian_proof_scope(&self, id: &str, scope: Option<&str>) -> Result<()> {
        let n = self.conn.execute(
            "UPDATE guardians SET proof_scope=?, updated_at_ms=? WHERE id=?",
            params![scope, crate::store::now_ms(), id],
        )?;
        if n == 0 {
            Err(StoreError::NotFound)
        } else {
            Ok(())
        }
    }

    /// Set this review's own override for whether `"each_branch"` scope skips
    /// auto-clean branches (RAL-168). `None` resets it to "inherit the
    /// project-level default".
    pub fn set_guardian_proof_skip_auto_clean(&self, id: &str, skip: Option<bool>) -> Result<()> {
        let n = self.conn.execute(
            "UPDATE guardians SET proof_skip_auto_clean=?, updated_at_ms=? WHERE id=?",
            params![skip.map(i64::from), crate::store::now_ms(), id],
        )?;
        if n == 0 {
            Err(StoreError::NotFound)
        } else {
            Ok(())
        }
    }

    /// Set this review's own override for whether the automatic base-branch
    /// auto-update rebuild is skipped (RAL-250). `None` resets it to "inherit
    /// the project/global default".
    pub fn set_guardian_skip_base_updates(&self, id: &str, skip: Option<bool>) -> Result<()> {
        let n = self.conn.execute(
            "UPDATE guardians SET skip_base_updates=?, updated_at_ms=? WHERE id=?",
            params![skip.map(i64::from), crate::store::now_ms(), id],
        )?;
        if n == 0 {
            Err(StoreError::NotFound)
        } else {
            Ok(())
        }
    }

    /// Set this review's own override for whether a newly submitted PR's
    /// branch defaults to the exact worktree/feature branch name instead of
    /// the convention-derived alias (RAL-307). `None` resets it to "inherit
    /// the project/global default".
    pub fn set_guardian_match_pr_branch_name(&self, id: &str, enabled: Option<bool>) -> Result<()> {
        let n = self.conn.execute(
            "UPDATE guardians SET match_pr_branch_name=?, updated_at_ms=? WHERE id=?",
            params![enabled.map(i64::from), crate::store::now_ms(), id],
        )?;
        if n == 0 {
            Err(StoreError::NotFound)
        } else {
            let _ = self.notify_watchers(
                crate::monitor::NotifiableEventKind::ReviewSettingsChanged,
                &format!("guardian:{id}"),
                crate::mailbox::MailboxPriority::Normal,
                "review match-PR-branch-name setting changed",
                None,
            );
            Ok(())
        }
    }

    /// Set this review's own override for whether the PR stack is
    /// auto-submitted/grown as each branch reaches a terminal merge state
    /// (RAL-317). `None` resets it to "inherit the project/global default".
    pub fn set_guardian_auto_submit_pr_stack(&self, id: &str, enabled: Option<bool>) -> Result<()> {
        let n = self.conn.execute(
            "UPDATE guardians SET auto_submit_pr_stack=?, updated_at_ms=? WHERE id=?",
            params![enabled.map(i64::from), crate::store::now_ms(), id],
        )?;
        if n == 0 {
            Err(StoreError::NotFound)
        } else {
            let _ = self.notify_watchers(
                crate::monitor::NotifiableEventKind::ReviewSettingsChanged,
                &format!("guardian:{id}"),
                crate::mailbox::MailboxPriority::Normal,
                "review auto-submit-PR-stack setting changed",
                None,
            );
            Ok(())
        }
    }

    /// Set whether this review auto-incorporates PR feedback comments instead of
    /// requiring the manual "Pull in PR feedback" action (RAL-117). Data-model
    /// only for v1 -- no background poller reads this flag yet.
    pub fn set_guardian_auto_pr_feedback(&self, id: &str, enabled: bool) -> Result<()> {
        let n = self.conn.execute(
            "UPDATE guardians SET auto_pr_feedback=?, updated_at_ms=? WHERE id=?",
            params![i64::from(enabled), crate::store::now_ms(), id],
        )?;
        if n == 0 {
            Err(StoreError::NotFound)
        } else {
            Ok(())
        }
    }

    /// The shell check commands for a guardian.
    pub fn guardian_checks(&self, id: &str) -> Result<Vec<String>> {
        let s: Option<String> = self
            .conn
            .query_row(
                "SELECT checks FROM guardians WHERE id=?",
                params![id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(crate::store::from_json(&s.ok_or(StoreError::NotFound)?))
    }

    /// Returns `(cell_proofs, task_proofs, cell_system_prompt)` for the
    /// cell whose `review_branch` matches `branch` in this guardian's linked squad.
    /// Returns `None` when the guardian has no `squad_id` or no cell with a matching
    /// `review_branch` exists (e.g. a manually-created review with no task linkage).
    pub fn proof_steps_for_review_branch(
        &self,
        guardian_id: &str,
        branch: &str,
    ) -> Result<Option<BranchProofInfo>> {
        let squad_id: Option<String> = self
            .conn
            .query_row(
                "SELECT squad_id FROM guardians WHERE id=?",
                params![guardian_id],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        let Some(squad_id) = squad_id else {
            return Ok(None);
        };
        let row: Option<(i64, i64, Option<String>)> = self
            .conn
            .query_row(
                "SELECT task_idx, idx, system_prompt \
                 FROM cells WHERE squad_id=? AND review_branch=?",
                params![squad_id, branch],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let Some((task_idx, cell_idx, system_prompt)) = row else {
            return Ok(None);
        };
        let cell_proofs = self.proofs_for(&squad_id, task_idx, "cell", cell_idx)?;
        let task_proofs = self.proofs_for(&squad_id, task_idx, "task", -1)?;
        Ok(Some((cell_proofs, task_proofs, system_prompt)))
    }

    /// Update the base branch for a review and clear the recorded base commit so the
    /// next merge re-baselines against the new branch. Clears `base_commit` so the
    /// next merge detects a fresh base rather than comparing against the old branch's
    /// tip. Returns [`StoreError::NotFound`] if the guardian does not exist.
    pub fn set_guardian_base_branch(&self, id: &str, base_branch: &str) -> Result<()> {
        self.set_guardian_base_branch_at(id, base_branch, crate::store::now_ms())
    }

    /// Set the review base using the source edit's timestamp. Forge polling
    /// uses the PR/MR's `updated_at`; local API calls use the daemon clock.
    pub fn set_guardian_base_branch_at(
        &self,
        id: &str,
        base_branch: &str,
        changed_at_ms: i64,
    ) -> Result<()> {
        let n = self.conn.execute(
            "UPDATE guardians SET base_branch=?, base_commit=NULL, updated_at_ms=?, base_changed_at_ms=? WHERE id=?",
            params![base_branch, crate::store::now_ms(), changed_at_ms, id],
        )?;
        if n == 0 {
            Err(StoreError::NotFound)
        } else {
            Ok(())
        }
    }

    /// Timestamp used by RAL-277's last-write-wins base-ref reconciliation.
    pub fn guardian_base_changed_at_ms(&self, id: &str) -> Result<i64> {
        self.conn
            .query_row(
                "SELECT base_changed_at_ms FROM guardians WHERE id=?",
                params![id],
                |r| r.get(0),
            )
            .optional()?
            .ok_or(StoreError::NotFound)
    }

    /// Apply a forge-authored base only if it is strictly newer. An exact tie
    /// deliberately loses to ralphus. The comparison and write share one SQL
    /// statement so a local edit arriving after the poll's GET cannot be
    /// overwritten by stale forge state.
    pub fn set_guardian_base_branch_if_newer(
        &self,
        id: &str,
        base_branch: &str,
        changed_at_ms: i64,
    ) -> Result<bool> {
        let n = self.conn.execute(
            "UPDATE guardians SET base_branch=?, base_commit=NULL, updated_at_ms=?, base_changed_at_ms=? \
             WHERE id=? AND base_changed_at_ms < ?",
            params![
                base_branch,
                crate::store::now_ms(),
                changed_at_ms,
                id,
                changed_at_ms
            ],
        )?;
        if n > 0 {
            return Ok(true);
        }
        let exists: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM guardians WHERE id=?)",
            params![id],
            |r| r.get(0),
        )?;
        if exists {
            Ok(false)
        } else {
            Err(StoreError::NotFound)
        }
    }

    /// Record the base-branch commit the review was last built against, so a
    /// later shift in the base branch can be detected and auto-rebuilt.
    pub fn set_guardian_base_commit(&self, id: &str, commit: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE guardians SET base_commit=?, updated_at_ms=? WHERE id=?",
            params![commit, crate::store::now_ms(), id],
        )?;
        Ok(())
    }

    /// Set the built review branch name.
    pub fn set_guardian_review_branch(&self, id: &str, branch: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE guardians SET review_branch=?, updated_at_ms=? WHERE id=?",
            params![branch, crate::store::now_ms(), id],
        )?;
        Ok(())
    }

    /// Record a branch's review-owned branch name and worktree path.
    pub fn set_branch_review(
        &self,
        guardian_id: &str,
        branch_id: &str,
        review_branch: &str,
        worktree: &str,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE guardian_branches SET review_branch=?, worktree=? WHERE guardian_id=? AND id=?",
            params![review_branch, worktree, guardian_id, branch_id],
        )?;
        Ok(())
    }

    /// Set a branch's detail note without changing its merge status.
    pub fn set_branch_detail(
        &self,
        guardian_id: &str,
        branch_id: &str,
        detail: &str,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE guardian_branches SET detail=? WHERE guardian_id=? AND id=?",
            params![detail, guardian_id, branch_id],
        )?;
        Ok(())
    }

    /// Record live conflict-resolution progress for a guardian (RAL-72).
    /// Pass `None` for all three to clear at the start of a merge.
    pub fn set_guardian_conflicts(
        &self,
        id: &str,
        found: Option<i64>,
        fixed: Option<i64>,
        committed: Option<i64>,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE guardians SET conflicts_found=?, conflicts_fixed=?, conflicts_committed=?, updated_at_ms=? WHERE id=?",
            params![found, fixed, committed, crate::store::now_ms(), id],
        )?;
        Ok(())
    }

    /// Flag whether a branch introduced no diff over the stack tip beneath it
    /// (RAL-190). See [`BranchView::is_empty`].
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn set_branch_empty(
        &self,
        guardian_id: &str,
        branch_id: &str,
        is_empty: bool,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE guardian_branches SET is_empty=? WHERE guardian_id=? AND id=?",
            params![i64::from(is_empty), guardian_id, branch_id],
        )?;
        Ok(())
    }

    /// Record per-branch conflict-resolution progress (RAL-72). Called alongside
    /// [`set_guardian_conflicts`] so the board can show a per-row progress bar.
    pub fn set_branch_conflicts(
        &self,
        guardian_id: &str,
        branch_id: &str,
        found: Option<i64>,
        fixed: Option<i64>,
        committed: Option<i64>,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE guardian_branches SET conflicts_found=?, conflicts_fixed=?, conflicts_committed=? WHERE guardian_id=? AND id=?",
            params![found, fixed, committed, guardian_id, branch_id],
        )?;
        Ok(())
    }

    /// Store the agent_session_id from the most recent conflict-resolver run on
    /// a branch. Only populated when the resolver backend is claude-code.
    pub fn set_branch_resolver_session_id(
        &self,
        guardian_id: &str,
        branch_id: &str,
        session_id: &str,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE guardian_branches SET resolver_agent_session_id=? WHERE guardian_id=? AND id=?",
            params![session_id, guardian_id, branch_id],
        )?;
        Ok(())
    }

    /// Record the commit the daemon last built a branch's review branch at
    /// (RAL-92). Used as the baseline against which a reviewer's manual
    /// push/amend to the review worktree is detected on the next maintenance sweep.
    pub fn set_branch_review_head(
        &self,
        guardian_id: &str,
        branch_id: &str,
        head: &str,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE guardian_branches SET review_head=? WHERE guardian_id=? AND id=?",
            params![head, guardian_id, branch_id],
        )?;
        Ok(())
    }

    /// Retrieve the recorded review-branch baseline commit for a branch (RAL-92).
    /// `None` when the branch has never been built (or the column is unset).
    pub fn get_branch_review_head(
        &self,
        guardian_id: &str,
        branch_id: &str,
    ) -> Result<Option<String>> {
        let r: Option<Option<String>> = self
            .conn
            .query_row(
                "SELECT review_head FROM guardian_branches WHERE guardian_id=? AND id=?",
                params![guardian_id, branch_id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(r.flatten())
    }

    /// Retrieve the review worktree path for a branch (for opening a plain terminal
    /// when no cell ID is available yet).
    pub fn get_branch_worktree(
        &self,
        guardian_id: &str,
        branch_id: &str,
    ) -> Result<Option<String>> {
        let r: Option<Option<String>> = self
            .conn
            .query_row(
                "SELECT worktree FROM guardian_branches WHERE guardian_id=? AND id=?",
                params![guardian_id, branch_id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(r.flatten())
    }

    /// Fetch a branch's recorded conflict-resolver Claude Code session id,
    /// for its "Open Agent" terminal action (resuming the real `claude` CLI
    /// rather than re-attaching to the runner's tmux wrapper) — see
    /// `Store::get_cell_agent_resume`'s doc comment for the same idea
    /// applied to a plain cell.
    pub fn get_branch_resolver_agent_session_id(
        &self,
        guardian_id: &str,
        branch_id: &str,
    ) -> Result<Option<String>> {
        let r: Option<Option<String>> = self
            .conn
            .query_row(
                "SELECT resolver_agent_session_id FROM guardian_branches WHERE guardian_id=? AND id=?",
                params![guardian_id, branch_id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(r.flatten())
    }

    /// Clear all per-branch conflict counts for a guardian — called at the start
    /// of a fresh merge so stale data from a previous run is not displayed.
    pub fn clear_all_branch_conflicts(&self, guardian_id: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE guardian_branches SET conflicts_found=NULL, conflicts_fixed=NULL, conflicts_committed=NULL WHERE guardian_id=?",
            params![guardian_id],
        )?;
        Ok(())
    }

    /// Set the agent-generated cross-branch change summary (RAL-39), recording
    /// which resolved `agent`/`model` produced it for later inspection (RAL-88).
    pub fn set_guardian_summary(
        &self,
        id: &str,
        summary: &str,
        agent: Option<&str>,
        model: Option<&str>,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE guardians SET change_summary=?, summary_agent=?, summary_model=?, updated_at_ms=? WHERE id=?",
            params![summary, agent, model, crate::store::now_ms(), id],
        )?;
        Ok(())
    }

    /// Persist LLM-generated manual review command strings (RAL-27), recording
    /// which resolved `agent`/`model` produced them for later inspection (RAL-88).
    pub fn set_guardian_manual_commands(
        &self,
        id: &str,
        commands: &[GuardianCheck],
        agent: Option<&str>,
        model: Option<&str>,
    ) -> Result<()> {
        let json = serde_json::to_string(commands).unwrap_or_else(|_| "[]".to_string());
        self.conn.execute(
            "UPDATE guardians SET manual_commands=?, manual_commands_agent=?, manual_commands_model=?, updated_at_ms=? WHERE id=?",
            params![json, agent, model, crate::store::now_ms(), id],
        )?;
        let _ = self.notify_watchers(
            crate::monitor::NotifiableEventKind::ReviewSettingsChanged,
            &format!("guardian:{id}"),
            crate::mailbox::MailboxPriority::Normal,
            "review manual checks changed",
            None,
        );
        Ok(())
    }

    /// Store the agent_session_id from the most recent manual-commands
    /// generation pass. Only populated when the resolved agent is claude-code.
    /// Mirrors [`Self::set_branch_resolver_session_id`] at the guardian level.
    pub fn set_guardian_manual_commands_session_id(
        &self,
        id: &str,
        session_id: &str,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE guardians SET manual_commands_agent_session_id=? WHERE id=?",
            params![session_id, id],
        )?;
        Ok(())
    }

    /// Fetch a guardian's recorded manual-commands agent_session_id, for its
    /// "Open Agent" terminal action, plus the cwd generation ran/runs in
    /// (the combined review worktree, falling back to `git_root` before one
    /// exists). `None` in either position means the corresponding action
    /// isn't available yet.
    pub fn get_guardian_manual_commands_agent_resume(
        &self,
        id: &str,
    ) -> Result<(String, Option<String>, Option<String>)> {
        Ok(self.conn.query_row(
            "SELECT COALESCE(combined_worktree, git_root), manual_commands_agent, manual_commands_agent_session_id FROM guardians WHERE id=?",
            params![id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?)
    }

    /// Persist user-declared action hints from `[[review.action]]` (RAL-77).
    /// Stored as a JSON array; set once at submit time and not touched by the merge engine.
    pub fn set_guardian_action_hints(&self, id: &str, hints: &[GuardianCheck]) -> Result<()> {
        let json = serde_json::to_string(hints).unwrap_or_else(|_| "[]".to_string());
        self.conn.execute(
            "UPDATE guardians SET action_hints=?, updated_at_ms=? WHERE id=?",
            params![json, crate::store::now_ms(), id],
        )?;
        let _ = self.notify_watchers(
            crate::monitor::NotifiableEventKind::ReviewSettingsChanged,
            &format!("guardian:{id}"),
            crate::mailbox::MailboxPriority::Normal,
            "review action checks changed",
            None,
        );
        Ok(())
    }

    /// Persist resolved/submitted [`CheckInput`] values for this guardian
    /// (RAL-164), merging into whatever is already stored so a value
    /// submitted for one input never clobbers another's. This is what makes
    /// a submitted or AI-resolved value "the new default" on subsequent
    /// views of any check referencing it.
    pub fn merge_guardian_input_values(
        &self,
        id: &str,
        values: &std::collections::HashMap<String, String>,
    ) -> Result<()> {
        if values.is_empty() {
            return Ok(());
        }
        let existing_json: Option<String> = self.conn.query_row(
            "SELECT input_values FROM guardians WHERE id=?",
            params![id],
            |r| r.get(0),
        )?;
        let mut merged: std::collections::HashMap<String, String> =
            serde_json::from_str(existing_json.as_deref().unwrap_or("{}")).unwrap_or_default();
        merged.extend(values.iter().map(|(k, v)| (k.clone(), v.clone())));
        let json = serde_json::to_string(&merged).unwrap_or_else(|_| "{}".to_string());
        self.conn.execute(
            "UPDATE guardians SET input_values=?, updated_at_ms=? WHERE id=?",
            params![json, crate::store::now_ms(), id],
        )?;
        let _ = self.notify_watchers(
            crate::monitor::NotifiableEventKind::ReviewSettingsChanged,
            &format!("guardian:{id}"),
            crate::mailbox::MailboxPriority::Normal,
            "review check input values changed",
            None,
        );
        Ok(())
    }

    /// Clear manual review commands so the UI shows them as pending while a
    /// fresh merge recomputes them (RAL-27).
    pub fn clear_guardian_manual_commands(&self, id: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE guardians SET manual_commands='[]', updated_at_ms=? WHERE id=?",
            params![crate::store::now_ms(), id],
        )?;
        Ok(())
    }

    /// Set the guardian's stable combined review worktree path.
    pub fn set_guardian_combined_worktree(&self, id: &str, worktree: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE guardians SET combined_worktree=?, updated_at_ms=? WHERE id=?",
            params![worktree, crate::store::now_ms(), id],
        )?;
        Ok(())
    }

    /// Set a branch's merge status (and optional detail).
    pub fn set_branch_status(
        &self,
        guardian_id: &str,
        branch_id: &str,
        status: MergeStatus,
        detail: Option<&str>,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE guardian_branches SET merge_status=?, detail=? WHERE guardian_id=? AND id=?",
            params![status.as_str(), detail, guardian_id, branch_id],
        )?;
        let msg = match detail {
            Some(d) => format!("branch → {} ({d})", status.as_str()),
            None => format!("branch → {}", status.as_str()),
        };
        let _ = self.log_event(None, Some(guardian_id), "branch", Some(branch_id), &msg);
        Ok(())
    }

    /// RAL-375: persist feedback text as durably pending on a branch, before
    /// `guardian_merge::run_feedback` does any work -- so an unclean daemon
    /// shutdown mid-run leaves a record startup recovery can find and
    /// reapply, instead of the feedback existing only as that function's own
    /// argument (gone the instant the process dies).
    pub fn set_branch_pending_feedback(
        &self,
        guardian_id: &str,
        branch_id: &str,
        feedback: &str,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE guardian_branches SET pending_feedback=? WHERE guardian_id=? AND id=?",
            params![feedback, guardian_id, branch_id],
        )?;
        Ok(())
    }

    /// RAL-375: clear a branch's pending-feedback record. Called from every
    /// real exit path of `guardian_merge::run_feedback` (success or a
    /// legitimate failure) -- only a literal crash mid-run leaves this set,
    /// which is exactly the signal startup recovery looks for.
    pub fn clear_branch_pending_feedback(&self, guardian_id: &str, branch_id: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE guardian_branches SET pending_feedback=NULL WHERE guardian_id=? AND id=?",
            params![guardian_id, branch_id],
        )?;
        Ok(())
    }

    /// RAL-375: every branch with feedback still awaiting application, as
    /// `(guardian_id, branch_id, feedback_text)`. Non-empty only after an
    /// unclean shutdown interrupted `guardian_merge::run_feedback` mid-run;
    /// startup recovery reapplies each one directly, since an ordinary
    /// rebuild would otherwise silently discard it.
    pub fn branches_with_pending_feedback(&self) -> Result<Vec<(String, String, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT guardian_id, id, pending_feedback FROM guardian_branches \
             WHERE pending_feedback IS NOT NULL ORDER BY guardian_id, position",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// RAL-317: set or clear this branch's one-shot auto-submit-PR-stack
    /// failure marker (`None` clears it, e.g. on the next successful
    /// attempt). Best-effort side channel -- see [`BranchView::auto_submit_error`]
    /// -- so it deliberately doesn't call [`Self::log_event`] the way
    /// [`Self::set_branch_status`] does; the caller logs the failure itself.
    pub fn set_branch_auto_submit_error(
        &self,
        guardian_id: &str,
        branch_id: &str,
        error: Option<&str>,
    ) -> Result<()> {
        let n = self.conn.execute(
            "UPDATE guardian_branches SET auto_submit_error=? WHERE guardian_id=? AND id=?",
            params![error, guardian_id, branch_id],
        )?;
        if n == 0 {
            Err(StoreError::NotFound)
        } else {
            Ok(())
        }
    }

    /// RAL-259: reset a branch's `started_at_ms`, marking the start of a fresh
    /// merge attempt. Called at the entry of each per-branch resolution pass
    /// (`guardian_merge::drive_rebase`) so a re-merge/re-restart re-stamps from
    /// scratch via [`Self::stamp_branch_started_at`]'s COALESCE — mirrors the
    /// cell `started_at_ms` clear-on-restart pattern (RAL-210). Leaves the value
    /// NULL (not stamped) for a branch that never invokes a resolver agent.
    pub fn clear_branch_started_at(&self, guardian_id: &str, branch_id: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE guardian_branches SET started_at_ms=NULL WHERE guardian_id=? AND id=?",
            params![guardian_id, branch_id],
        )?;
        Ok(())
    }

    /// RAL-259: stamp a branch's `started_at_ms` once, when its resolver agent
    /// actually begins running. `COALESCE` keeps the first stamp within the
    /// current attempt (the conflict-resolver fix pass) if a later call in the
    /// same attempt (the final-proof call) also fires — see
    /// [`Self::clear_branch_started_at`] for the matching reset.
    pub fn stamp_branch_started_at(&self, guardian_id: &str, branch_id: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE guardian_branches SET started_at_ms=COALESCE(started_at_ms, ?)
             WHERE guardian_id=? AND id=?",
            params![crate::store::now_ms(), guardian_id, branch_id],
        )?;
        Ok(())
    }

    /// RAL-259: stamp when this review's manual-checks generation agent began
    /// work. Unlike the per-branch resolver stamp this is a plain overwrite
    /// (the *most recent* generation's start), matching the manual-checks Live
    /// View's need to show the current/latest generation rather than the first.
    pub fn stamp_guardian_manual_checks_started_at(&self, id: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE guardians SET manual_checks_started_at_ms=? WHERE id=?",
            params![crate::store::now_ms(), id],
        )?;
        Ok(())
    }

    /// Reorder a guardian's branches to match `order` (a permutation of the
    /// existing branch names). Positions are first shifted out of range to avoid
    /// colliding with the `(guardian_id, position)` primary key, then rewritten.
    /// Branch names in `order` that no longer exist are ignored; any left over
    /// keep their relative order after the reordered ones.
    pub fn reorder_guardian_branches(&mut self, guardian_id: &str, order: &[String]) -> Result<()> {
        self.guardian_exists(guardian_id)?;
        let tx = self.conn.transaction()?;
        const SHIFT: i64 = 1_000_000;
        tx.execute(
            "UPDATE guardian_branches SET position = position + ?1 WHERE guardian_id=?2",
            params![SHIFT, guardian_id],
        )?;
        let mut next: i64 = 0;
        for branch in order {
            let updated = tx.execute(
                "UPDATE guardian_branches SET position=?1
                 WHERE guardian_id=?2 AND branch=?3 AND position >= ?4",
                params![next, guardian_id, branch, SHIFT],
            )?;
            if updated > 0 {
                next += 1;
            }
        }
        // Compact any branches not named in `order`, preserving their order.
        let mut stmt = tx.prepare(
            "SELECT position FROM guardian_branches
             WHERE guardian_id=?1 AND position >= ?2 ORDER BY position",
        )?;
        let leftover: Vec<i64> = stmt
            .query_map(params![guardian_id, SHIFT], |r| r.get(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        drop(stmt);
        for old_pos in leftover {
            tx.execute(
                "UPDATE guardian_branches SET position=?1 WHERE guardian_id=?2 AND position=?3",
                params![next, guardian_id, old_pos],
            )?;
            next += 1;
        }
        tx.commit()?;
        Ok(())
    }

    /// The ordered branches of a guardian (all, including disabled).
    pub fn guardian_branches(&self, guardian_id: &str) -> Result<Vec<OrderedBranch>> {
        let mut stmt = self.conn.prepare(
            "SELECT position, branch, enabled, id, readable_review_branch, review_branch_name
             FROM guardian_branches WHERE guardian_id=? ORDER BY position",
        )?;
        let rows = stmt
            .query_map(params![guardian_id], |r| {
                Ok(OrderedBranch {
                    position: r.get(0)?,
                    branch: r.get(1)?,
                    enabled: r.get::<_, i64>(2).map(|v| v != 0).unwrap_or(true),
                    id: r.get(3)?,
                    readable_review_branch: r.get::<_, i64>(4)? != 0,
                    review_branch_name: r.get(5)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Disable all enabled branches whose source cell is not yet `done`
    /// (or have no linked cell at all). Returns the list of disabled branch
    /// names with their source cell state (None = never submitted). Called by
    /// the force-start endpoint (RAL-69).
    pub fn force_start_disable_branches(
        &self,
        guardian_id: &str,
    ) -> Result<Vec<(String, Option<String>)>> {
        let mut stmt = self.conn.prepare(
            "SELECT gb.branch,
                    (SELECT s.state FROM cells s
                     WHERE s.review_branch = gb.branch
                     ORDER BY s.rowid DESC LIMIT 1) AS source_cell_state
             FROM guardian_branches gb
             WHERE gb.guardian_id=? AND gb.enabled=1
             ORDER BY gb.position",
        )?;
        let rows: Vec<(String, Option<String>)> = stmt
            .query_map(params![guardian_id], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let not_ready: Vec<(String, Option<String>)> = rows
            .into_iter()
            .filter(|(_, state)| state.as_deref() != Some("done"))
            .collect();
        for (branch, _) in &not_ready {
            self.set_branch_enabled_by_name(guardian_id, branch, false)?;
        }
        Ok(not_ready)
    }

    /// A review branch's own environment-variable overrides (RAL-191), not
    /// merged with the source cell's. `Some(v)` is an override, `None` is a
    /// tombstone — see [`BranchView::env_overrides`].
    ///
    /// # Errors
    /// [`StoreError::NotFound`] when no such branch exists in that guardian.
    pub fn get_guardian_branch_env_overrides(
        &self,
        guardian_id: &str,
        branch_id: &str,
    ) -> Result<BTreeMap<String, Option<String>>> {
        let raw: Option<String> = self
            .conn
            .query_row(
                "SELECT env_overrides FROM guardian_branches WHERE guardian_id=? AND id=?",
                params![guardian_id, branch_id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(branch_env_from_json(&raw.ok_or(StoreError::NotFound)?))
    }

    /// Mutate a review branch's own environment-variable overrides (RAL-191),
    /// returning the resulting map.
    ///
    /// Three operations, applied in order so the last one named for a given key
    /// wins deterministically:
    /// - `clear` drops the branch's entry entirely, so the key reverts to
    ///   whatever the source cell resolves to.
    /// - `unset` writes a tombstone, removing the inherited key from the
    ///   review worktree's environment.
    /// - `set` writes an override value.
    ///
    /// # Errors
    /// [`StoreError::NotFound`] when no such branch exists in that guardian.
    pub fn set_guardian_branch_env_overrides(
        &self,
        guardian_id: &str,
        branch_id: &str,
        set: &BTreeMap<String, String>,
        unset: &[String],
        clear: &[String],
    ) -> Result<BTreeMap<String, Option<String>>> {
        let mut current = self.get_guardian_branch_env_overrides(guardian_id, branch_id)?;
        for key in clear {
            current.remove(key);
        }
        for key in unset {
            current.insert(key.clone(), None);
        }
        for (k, v) in set {
            current.insert(k.clone(), Some(v.clone()));
        }
        self.conn.execute(
            "UPDATE guardian_branches SET env_overrides=? WHERE guardian_id=? AND id=?",
            params![branch_env_to_json(&current), guardian_id, branch_id],
        )?;
        Ok(current)
    }

    /// The effective environment a review branch's worktree runs under
    /// (RAL-191): the source cell's resolved `squad < task < cell`
    /// overrides with this branch's own layer applied on top.
    ///
    /// A branch with no source cell (added manually, or whose cell was
    /// deleted) inherits nothing — its own overrides are the whole map, and a
    /// tombstone for a key that was never inherited is simply a no-op.
    ///
    /// # Errors
    /// [`StoreError::NotFound`] when no such branch exists in that guardian.
    pub fn resolve_guardian_branch_env(
        &self,
        guardian_id: &str,
        branch_id: &str,
    ) -> Result<BTreeMap<String, String>> {
        let overrides = self.get_guardian_branch_env_overrides(guardian_id, branch_id)?;
        let source: Option<(String, i64, i64)> = self
            .conn
            .query_row(
                "SELECT s.squad_id, s.task_idx, s.idx
                 FROM guardian_branches gb
                 JOIN cells s ON s.rowid = (
                     SELECT s2.rowid FROM cells s2
                     WHERE s2.review_branch = gb.branch
                     ORDER BY s2.rowid DESC LIMIT 1
                 )
                 WHERE gb.guardian_id=? AND gb.id=?",
                params![guardian_id, branch_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let inherited = match source {
            Some((squad_id, ti, si)) => self
                .resolve_cell_env_overrides(&squad_id, ti, si)
                .unwrap_or_default(),
            None => BTreeMap::new(),
        };
        Ok(apply_branch_env(&inherited, &overrides))
    }

    /// This review's own environment-variable overrides for the
    /// finalize-time build/check-gate step (RAL-203), not yet merged with
    /// [`GuardianView::combined_env`]. `Some(v)` is an override, `None` is a
    /// tombstone -- see [`GuardianView::build_env_overrides`].
    ///
    /// # Errors
    /// [`StoreError::NotFound`] when no such guardian exists.
    pub fn get_guardian_build_env_overrides(
        &self,
        guardian_id: &str,
    ) -> Result<BTreeMap<String, Option<String>>> {
        let raw: Option<String> = self
            .conn
            .query_row(
                "SELECT build_env_overrides FROM guardians WHERE id=?",
                params![guardian_id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(branch_env_from_json(&raw.ok_or(StoreError::NotFound)?))
    }

    /// Mutate this review's build-step environment overrides (RAL-203),
    /// returning the resulting map. Same three-operation `set`/`unset`/`clear`
    /// semantics as [`Self::set_guardian_branch_env_overrides`], applied in
    /// order so the last one named for a given key wins deterministically.
    /// Independent of [`Self::set_guardian_manual_checks_env_overrides`] --
    /// mutating one never touches the other.
    ///
    /// # Errors
    /// [`StoreError::NotFound`] when no such guardian exists.
    pub fn set_guardian_build_env_overrides(
        &self,
        guardian_id: &str,
        set: &BTreeMap<String, String>,
        unset: &[String],
        clear: &[String],
    ) -> Result<BTreeMap<String, Option<String>>> {
        let mut current = self.get_guardian_build_env_overrides(guardian_id)?;
        for key in clear {
            current.remove(key);
        }
        for key in unset {
            current.insert(key.clone(), None);
        }
        for (k, v) in set {
            current.insert(k.clone(), Some(v.clone()));
        }
        self.conn.execute(
            "UPDATE guardians SET build_env_overrides=? WHERE id=?",
            params![branch_env_to_json(&current), guardian_id],
        )?;
        Ok(current)
    }

    /// This review's own environment-variable overrides for the
    /// manual-checks step (RAL-203) -- see
    /// [`GuardianView::manual_checks_env_overrides`].
    ///
    /// # Errors
    /// [`StoreError::NotFound`] when no such guardian exists.
    pub fn get_guardian_manual_checks_env_overrides(
        &self,
        guardian_id: &str,
    ) -> Result<BTreeMap<String, Option<String>>> {
        let raw: Option<String> = self
            .conn
            .query_row(
                "SELECT manual_checks_env_overrides FROM guardians WHERE id=?",
                params![guardian_id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(branch_env_from_json(&raw.ok_or(StoreError::NotFound)?))
    }

    /// Mutate this review's manual-checks-step environment overrides
    /// (RAL-203). See [`Self::set_guardian_build_env_overrides`] for the
    /// operation semantics; independent of that build-step layer.
    ///
    /// # Errors
    /// [`StoreError::NotFound`] when no such guardian exists.
    pub fn set_guardian_manual_checks_env_overrides(
        &self,
        guardian_id: &str,
        set: &BTreeMap<String, String>,
        unset: &[String],
        clear: &[String],
    ) -> Result<BTreeMap<String, Option<String>>> {
        let mut current = self.get_guardian_manual_checks_env_overrides(guardian_id)?;
        for key in clear {
            current.remove(key);
        }
        for key in unset {
            current.insert(key.clone(), None);
        }
        for (k, v) in set {
            current.insert(k.clone(), Some(v.clone()));
        }
        self.conn.execute(
            "UPDATE guardians SET manual_checks_env_overrides=? WHERE id=?",
            params![branch_env_to_json(&current), guardian_id],
        )?;
        Ok(current)
    }

    /// Permanently dismiss the "can re-enable" notification for a branch (RAL-69).
    /// Idempotent — unknown branch ids are silently ignored.
    pub fn dismiss_branch_reenable(&self, guardian_id: &str, branch_id: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE guardian_branches SET dismissed_reenable=1 WHERE guardian_id=? AND id=?",
            params![guardian_id, branch_id],
        )?;
        Ok(())
    }

    /// Set a branch's enabled/disabled state by branch name (RAL-43). Unknown
    /// branch names are silently ignored (0 rows updated is not an error).
    pub fn set_branch_enabled_by_name(
        &self,
        guardian_id: &str,
        branch: &str,
        enabled: bool,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE guardian_branches SET enabled=? WHERE guardian_id=? AND branch=?",
            params![i64::from(enabled), guardian_id, branch],
        )?;
        Ok(())
    }

    /// Reset a branch to Pending and clear its review-branch/worktree columns.
    /// Used when a disabled branch is skipped during a merge rebuild (RAL-43).
    pub fn reset_branch_to_pending(&self, guardian_id: &str, branch_id: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE guardian_branches SET merge_status='pending', detail=NULL, review_branch=NULL,
             worktree=NULL, resolver_agent_session_id=NULL, review_head=NULL
             WHERE guardian_id=? AND id=?",
            params![guardian_id, branch_id],
        )?;
        Ok(())
    }

    /// Reset all enabled branches of a guardian before a fresh merge, clearing
    /// their review-branch/worktree columns. Branches already in a
    /// pre-merge state (`pending` or `ready`) keep their status so the
    /// dependency-satisfaction signal survives the reset; only in-flight or
    /// terminal merge states (`in_progress`, `done`, `conflict_resolved`,
    /// `failed`) are rolled back to `pending` (RAL-54, RAL-73).
    pub fn reset_all_enabled_branches_to_pending(&self, guardian_id: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE guardian_branches
             SET merge_status = CASE WHEN merge_status IN ('pending','ready') THEN merge_status ELSE 'pending' END,
                 detail=NULL, review_branch=NULL, worktree=NULL, resolver_agent_session_id=NULL, review_head=NULL
             WHERE guardian_id=? AND enabled=1",
            params![guardian_id],
        )?;
        Ok(())
    }

    /// Transition all enabled `pending` branches of a guardian to `ready`,
    /// signalling that every linked task cell is done and the branch is
    /// waiting for the rebase to start (RAL-73). Branches already past
    /// `pending` (e.g. `in_progress`, `done`, `failed`) are left unchanged.
    pub fn mark_guardian_branches_ready(&self, guardian_id: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE guardian_branches SET merge_status='ready'
             WHERE guardian_id=? AND enabled=1 AND merge_status='pending'",
            params![guardian_id],
        )?;
        Ok(())
    }

    /// Fetch a single guardian view.
    pub fn get_guardian(&self, id: &str) -> Result<GuardianView> {
        let row = self
            .conn
            .query_row(
                "SELECT id, name, base_branch, git_root, review_branch, status, detail, checks, squad_id, combined_worktree, conflicts_found, conflicts_fixed, conflicts_committed, skip_auto_build, skip_worktree_checks, review_type, skip_worktrees, created_at_ms, resolver_agent, resolver_model, base_commit, change_summary, base_commits, manual_commands, action_hints, summary_agent, summary_model, manual_commands_agent, manual_commands_model, manual_commands_agent_session_id, squash_projects, auto_pr_feedback, input_values, proof_scope, proof_skip_auto_clean, machine, build_env_overrides, manual_checks_env_overrides, maximum_budget_usd, merge_attempt, skip_base_updates, manual_checks_started_at_ms, notice_kind, notice_message, notice_at_ms, match_pr_branch_name, auto_submit_pr_stack, origin, auto_build_json, separate_pr_branch, readable_review_branch, review_branch_name
                 FROM guardians WHERE id=?", // `skip_worktree_checks` (col 14) is read-only legacy data (RAL-285) -- see `GuardianRow::legacy_skip_worktree_checks`.
                params![id],
                Self::map_guardian_row,
            )
            .optional()?
            .ok_or(StoreError::NotFound)?;
        self.hydrate_guardian(row)
    }

    /// List all guardians, newest first.
    pub fn list_guardians(&self) -> Result<Vec<GuardianView>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, name, base_branch, git_root, review_branch, status, detail, checks, squad_id, combined_worktree, conflicts_found, conflicts_fixed, conflicts_committed, skip_auto_build, skip_worktree_checks, review_type, skip_worktrees, created_at_ms, resolver_agent, resolver_model, base_commit, change_summary, base_commits, manual_commands, action_hints, summary_agent, summary_model, manual_commands_agent, manual_commands_model, manual_commands_agent_session_id, squash_projects, auto_pr_feedback, input_values, proof_scope, proof_skip_auto_clean, machine, build_env_overrides, manual_checks_env_overrides, maximum_budget_usd, merge_attempt, skip_base_updates, manual_checks_started_at_ms, notice_kind, notice_message, notice_at_ms, match_pr_branch_name, auto_submit_pr_stack, origin, auto_build_json, separate_pr_branch, readable_review_branch, review_branch_name
             FROM guardians ORDER BY created_at_ms DESC", // `skip_worktree_checks` (col 14) is read-only legacy data (RAL-285) -- see `GuardianRow::legacy_skip_worktree_checks`.
        )?;
        let rows = stmt
            .query_map([], Self::map_guardian_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        rows.into_iter().map(|r| self.hydrate_guardian(r)).collect()
    }

    fn map_guardian_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<GuardianRow> {
        Ok(GuardianRow {
            id: r.get(0)?,
            name: r.get(1)?,
            base_branch: r.get(2)?,
            git_root: r.get(3)?,
            review_branch: r.get(4)?,
            status: r.get(5)?,
            detail: r.get(6)?,
            checks: r.get(7)?,
            squad_id: r.get(8)?,
            combined_worktree: r.get(9)?,
            conflicts_found: r.get(10)?,
            conflicts_fixed: r.get(11)?,
            conflicts_committed: r.get(12)?,
            skip_auto_build: r.get(13)?,
            legacy_skip_worktree_checks: r.get(14)?,
            review_type: r.get(15)?,
            skip_worktrees: r.get(16)?,
            created_at_ms: r.get(17)?,
            resolver_agent: r.get(18)?,
            resolver_model: r.get(19)?,
            base_commit: r.get(20)?,
            change_summary: r.get(21)?,
            base_commits: r.get(22)?,
            manual_commands: r.get(23)?,
            action_hints: r.get(24)?,
            summary_agent: r.get(25)?,
            summary_model: r.get(26)?,
            manual_commands_agent: r.get(27)?,
            manual_commands_model: r.get(28)?,
            manual_commands_agent_session_id: r.get(29)?,
            squash_projects: r.get(30)?,
            auto_pr_feedback: r.get(31)?,
            input_values: r.get(32)?,
            proof_scope: r.get(33)?,
            proof_skip_auto_clean: r.get::<_, Option<i64>>(34)?.map(|v| v != 0),
            machine: r.get(35)?,
            build_env_overrides: r.get(36)?,
            manual_checks_env_overrides: r.get(37)?,
            maximum_budget_usd: r.get(38)?,
            merge_attempt: r.get(39)?,
            skip_base_updates: r.get::<_, Option<i64>>(40)?.map(|v| v != 0),
            manual_checks_started_at_ms: r.get(41)?,
            notice_kind: r.get(42)?,
            notice_message: r.get(43)?,
            notice_at_ms: r.get(44)?,
            match_pr_branch_name: r.get::<_, Option<i64>>(45)?.map(|v| v != 0),
            auto_submit_pr_stack: r.get::<_, Option<i64>>(46)?.map(|v| v != 0),
            origin: r.get(47)?,
            auto_build_json: r.get(48)?,
            separate_pr_branch: r.get::<_, Option<i64>>(49)?.map(|v| v != 0),
            readable_review_branch: r.get::<_, i64>(50)? != 0,
            review_branch_name: r.get(51)?,
        })
    }

    fn hydrate_guardian(&self, row: GuardianRow) -> Result<GuardianView> {
        // RAL-121: one correlated subquery per branch (finding that branch's
        // most-recent cell by rowid) instead of the previous four -- each of
        // state/squad_id/task_idx/idx was a separate subquery re-scanning
        // `cells` for the same row. Paired with `idx_cells_review_branch`
        // (see `store.rs`'s migration list) this is now an index seek, not a
        // table scan, per branch.
        let mut stmt = self.conn.prepare(
            "SELECT gb.position, gb.branch, gb.merge_status, gb.detail, gb.review_branch,
                    gb.worktree, gb.conflicts_found, gb.conflicts_fixed, gb.conflicts_committed,
                    gb.enabled, gb.project, gb.dismissed_reenable,
                    s.state AS source_cell_state,
                    s.squad_id AS source_squad_id,
                    s.task_idx AS source_task_idx,
                    s.idx AS source_cell_idx,
                    gb.resolver_agent_session_id, gb.moved_from_guardian_id, gb.id,
                    gb.is_empty, s.machine AS source_cell_machine,
                    gb.env_overrides, gb.started_at_ms, gb.auto_submit_error,
                    gb.readable_review_branch, gb.review_branch_name
             FROM guardian_branches gb
             LEFT JOIN cells s ON s.rowid = (
                 SELECT s2.rowid FROM cells s2
                 WHERE s2.review_branch = gb.branch
                 ORDER BY s2.rowid DESC LIMIT 1
             )
             WHERE gb.guardian_id=? ORDER BY gb.position",
        )?;
        let mut branches = stmt
            .query_map(params![row.id], |r| {
                let is_empty = r.get::<_, i64>(19).map(|v| v != 0).unwrap_or(false);
                let source_cell_machine: Option<String> = r.get(20)?;
                let enabled = r.get::<_, i64>(9).map(|v| v != 0).unwrap_or(true);
                let dismissed = r.get::<_, i64>(11).map(|v| v != 0).unwrap_or(false);
                let source_cell_state: Option<String> = r.get(12)?;
                let can_reenable =
                    !enabled && source_cell_state.as_deref() == Some("done") && !dismissed;
                let merge_status: String = r.get(2)?;
                let ready = merge_status == "ready";
                let worktree: Option<String> = r.get(5)?;
                let resolver_agent_session_id: Option<String> = r.get(16)?;
                let terminal_modes = terminal_modes_for(
                    row.resolver_agent.as_deref(),
                    resolver_agent_session_id.is_some(),
                    worktree.is_some(),
                    Path::new(&row.git_root),
                );
                Ok(BranchView {
                    id: r.get(18)?,
                    position: r.get(0)?,
                    branch: r.get(1)?,
                    merge_status,
                    detail: r.get(3)?,
                    review_branch: r.get(4)?,
                    readable_review_branch: r.get::<_, i64>(24)? != 0,
                    review_branch_name: r.get(25)?,
                    worktree,
                    conflicts_found: r.get(6)?,
                    conflicts_fixed: r.get(7)?,
                    conflicts_committed: r.get(8)?,
                    is_empty,
                    source_cell_machine,
                    enabled,
                    project: r.get(10)?,
                    source_cell_state,
                    can_reenable,
                    source_squad_id: r.get(13)?,
                    source_task_idx: r.get(14)?,
                    source_cell_idx: r.get(15)?,
                    resolver_agent_session_id,
                    ready,
                    terminal_modes,
                    moved_from_guardian_id: r.get(17)?,
                    rebase_commands_done: None,
                    rebase_commands_total: None,
                    // RAL-191: the raw per-branch layer; `inherited_env` and
                    // `resolved_env` are filled in below, where the source
                    // cell's own resolution is reachable.
                    env_overrides: branch_env_from_json(&r.get::<_, String>(21)?),
                    resolved_env: BTreeMap::new(),
                    inherited_env: BTreeMap::new(),
                    started_at_ms: r.get(22)?,
                    auto_submit_error: r.get(23)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        // RAL-191: resolve each branch's effective environment. The inherited
        // half needs the source cell's full `squad < task < cell` chain, so
        // it is a second pass rather than more columns on the query above.
        // RAL-121 follow-up: batched into one query per table instead of
        // three per branch -- hydrate_guardian runs on every guardian on
        // every board poll, so this was the same N+1 shape the squads/tasks
        // board view was already fixed for.
        let env_refs: Vec<crate::store::CellRef> = branches
            .iter()
            .filter_map(|b| {
                match (
                    b.source_squad_id.as_deref(),
                    b.source_task_idx,
                    b.source_cell_idx,
                ) {
                    (Some(squad_id), Some(ti), Some(si)) => Some((squad_id.to_string(), ti, si)),
                    _ => None,
                }
            })
            .collect();
        let resolved_envs = self
            .resolve_cell_env_overrides_batch(&env_refs)
            .unwrap_or_default();
        for b in &mut branches {
            b.inherited_env = match (
                b.source_squad_id.as_deref(),
                b.source_task_idx,
                b.source_cell_idx,
            ) {
                (Some(squad_id), Some(ti), Some(si)) => resolved_envs
                    .get(&(squad_id.to_string(), ti, si))
                    .cloned()
                    .unwrap_or_default(),
                // A branch added manually (or whose source cell has since
                // been deleted) has nothing to inherit -- its own overrides are
                // the whole environment.
                _ => BTreeMap::new(),
            };
            b.resolved_env = apply_branch_env(&b.inherited_env, &b.env_overrides);
        }
        // RAL-145: git's own rebase-todo progress is a live filesystem read,
        // so it's gated to the (normally singular) branch actually mid-rebase
        // rather than scanned across every branch on every board poll.
        for b in &mut branches {
            if b.merge_status != "in_progress" {
                continue;
            }
            let Some(wt) = b.worktree.as_deref() else {
                continue;
            };
            if let Some((done, total)) = crate::guardian_merge::rebase_command_progress(
                &crate::workspace::Workspace::local(wt),
            ) {
                b.rebase_commands_done = Some(done);
                b.rebase_commands_total = Some(total);
            }
        }

        // Compute the ordered distinct project roots from the branch list. A branch
        // with project=None uses the guardian's own git_root.
        let mut seen = std::collections::HashSet::new();
        let mut projects: Vec<String> = Vec::new();
        for b in &branches {
            let proj = b.project.clone().unwrap_or_else(|| row.git_root.clone());
            if seen.insert(proj.clone()) {
                projects.push(proj);
            }
        }
        // A guardian with no branches still has its git_root as its sole project.
        if projects.is_empty() {
            projects.push(row.git_root.clone());
        }

        let base_commits: std::collections::HashMap<String, String> =
            serde_json::from_str(row.base_commits.as_deref().unwrap_or("{}")).unwrap_or_default();

        let ready = row.status == "in_review";
        let merge_progress = MergeProgress::compute(&branches);
        // RAL-103: two states only -- summary computation is synchronous (a
        // git-log-only preliminary summary, upgraded to the LLM-authored final
        // one once a branch's stacked review commit exists) and is never
        // intentionally cleared mid-rebuild, so there is no "generating" gap.
        let summary_state: &'static str = if row.change_summary.is_some() {
            "ready"
        } else {
            "waiting"
        };
        let manual_commands: Vec<GuardianCheck> =
            serde_json::from_str(row.manual_commands.as_deref().unwrap_or("[]"))
                .unwrap_or_default();
        // RAL-103: manual-checks generation only ever runs once every ENABLED
        // branch has finished rebasing cleanly (see `generate_manual_commands`'s
        // call sites in guardian_merge.rs, always after the stack fully
        // rebuilds) -- so "generating" is scoped to that narrow window, not the
        // whole `merging` status. Disabled branches are reset to `pending` and
        // deliberately excluded here (unlike `merge_progress`, which reports
        // over every branch for the UI's overall progress display) -- they
        // never reach "done", and generation doesn't wait on them either.
        let enabled_branches: Vec<&BranchView> = branches.iter().filter(|b| b.enabled).collect();
        let enabled_done = enabled_branches
            .iter()
            .filter(|b| matches!(b.merge_status.as_str(), "done" | "conflict_resolved"))
            .count();
        let enabled_failed = enabled_branches
            .iter()
            .filter(|b| b.merge_status == "failed")
            .count();
        let checks_state: &'static str = if !manual_commands.is_empty() {
            "ready"
        } else if row.status == "merging"
            && !enabled_branches.is_empty()
            && enabled_done == enabled_branches.len()
            && enabled_failed == 0
        {
            "generating"
        } else {
            "waiting"
        };
        let input_resolutions = self.guardian_input_resolutions(&row.id)?;

        // RAL-203: the combined worktree has no upstream task cell of its
        // own to inherit an environment from (unlike a per-branch worktree,
        // which borrows its source cell's), so the build/check-gate and
        // manual-checks steps against it instead borrow the union of every
        // enabled branch's own resolved environment -- computed now that
        // `branches` above has each one's `resolved_env` filled in.
        let combined_env = combined_env_from_branches(&branches);
        let build_env_overrides =
            branch_env_from_json(row.build_env_overrides.as_deref().unwrap_or("{}"));
        let manual_checks_env_overrides =
            branch_env_from_json(row.manual_checks_env_overrides.as_deref().unwrap_or("{}"));
        let build_env = apply_branch_env(&combined_env, &build_env_overrides);
        let manual_checks_env = apply_branch_env(&combined_env, &manual_checks_env_overrides);
        // RAL-342: `None` here is a valid, distinct state (the review declared
        // `skip_auto_build = true`, or predates this migration) -- unlike the
        // `{}`-default env-override maps above, there is no fallback to apply.
        let auto_build = row
            .auto_build_json
            .as_deref()
            .and_then(|json| serde_json::from_str::<GuardianAutoBuild>(json).ok());

        // RAL-168: resolve this review's own Proof-scope override (if any)
        // against the project-level `.ralphus.toml [review] default_proof_scope`
        // default -- so the UI can show the effective value as the dropdown's
        // initial selection (interview Q7) without a second round-trip, and
        // the merge engine (`guardian_merge.rs`) has a single, always-populated
        // field to gate on.
        let project_review_config = crate::config::resolve(Path::new(&row.git_root));
        // RAL-285: `skip_worktree_checks` was retired in favor of `proof_scope`
        // alone, but a row persisted before this change may still have the old
        // flag set with no explicit `proof_scope` override -- read-time
        // migration (not a bulk data migration) resolves that combination to
        // "nothing" so existing reviews keep their prior behavior.
        let effective_proof_scope = if row.proof_scope.is_none() && row.legacy_skip_worktree_checks
        {
            "nothing".to_string()
        } else {
            row.proof_scope
                .as_deref()
                .filter(|s| matches!(*s, "each_branch" | "final_branch" | "nothing"))
                .unwrap_or_else(|| project_review_config.default_proof_scope())
                .to_string()
        };
        let effective_proof_skip_auto_clean = row
            .proof_skip_auto_clean
            .unwrap_or_else(|| project_review_config.verify_skip_auto_clean());

        // RAL-250: effective base-branch auto-update opt-out, layered
        // per-review override > explicit `.ralphus.toml [review]` value > this
        // project's creation-time stamp (the frozen global value) > the live
        // global config > `false` (auto-update on). The raw per-review column
        // is read separately from `project_review_config` (which already
        // merges global in) so an explicit project override can be told apart
        // from the stamp and the live global.
        let explicit_project = crate::config::project_review_config(Path::new(&row.git_root));
        let stamp = self.project_skip_base_updates_stamp(&row.git_root);
        let live_global = crate::config::global_review_config();
        let effective_skip_base_updates = row
            .skip_base_updates
            .or(explicit_project.skip_base_updates)
            .or(stamp)
            .or(live_global.skip_base_updates)
            .unwrap_or(false);

        // RAL-307: same layering as `effective_skip_base_updates` above, for
        // whether a newly submitted PR's branch defaults to the exact
        // worktree/feature branch name instead of the convention-derived
        // alias.
        let match_pr_branch_name_stamp = self.project_match_pr_branch_name_stamp(&row.git_root);
        let effective_match_pr_branch_name = row
            .match_pr_branch_name
            .or(explicit_project.match_pr_branch_name)
            .or(match_pr_branch_name_stamp)
            .or(live_global.match_pr_branch_name)
            .unwrap_or(false);

        // RAL-317: same layering as `effective_match_pr_branch_name` above,
        // for whether the PR stack is auto-submitted/grown as each branch
        // reaches a terminal merge state.
        let auto_submit_pr_stack_stamp = self.project_auto_submit_pr_stack_stamp(&row.git_root);
        let effective_auto_submit_pr_stack = row
            .auto_submit_pr_stack
            .or(explicit_project.auto_submit_pr_stack)
            .or(auto_submit_pr_stack_stamp)
            .or(live_global.auto_submit_pr_stack)
            .unwrap_or(false);

        // RAL-378: same layering as `effective_match_pr_branch_name` above,
        // for whether the PR gets a branch of its own or is opened straight
        // from the review branch.
        let separate_pr_branch_stamp = self.project_separate_pr_branch_stamp(&row.git_root);
        let effective_separate_pr_branch = row
            .separate_pr_branch
            .or(explicit_project.separate_pr_branch)
            .or(separate_pr_branch_stamp)
            .or(live_global.separate_pr_branch)
            .unwrap_or(false);

        // RAL-193: this review's own agent cost -- conflict resolution and
        // prover calls made by the guardian merge machinery -- scoped to
        // the current merge attempt and cumulatively across every
        // rebase/re-merge attempt. Deliberately excludes the cost of the
        // tasks/cells that fed into the review (per the RAL-193 user
        // decision), which is why this sums `guardian_costs` rather than
        // joining `cells`.
        let (attempt_tokens_in, attempt_tokens_out, attempt_cost_usd) =
            self.guardian_cost_total_for_attempt(&row.id, row.merge_attempt)?;
        let (cumulative_tokens_in, cumulative_tokens_out, cumulative_cost_usd) =
            self.guardian_cost_total(&row.id)?;

        Ok(GuardianView {
            id: row.id,
            name: row.name,
            base_branch: row.base_branch,
            base_commit: row.base_commit,
            git_root: row.git_root,
            review_branch: row.review_branch,
            status: row.status,
            detail: row.detail,
            squad_id: row.squad_id,
            combined_worktree: row.combined_worktree,
            conflicts_found: row.conflicts_found,
            conflicts_fixed: row.conflicts_fixed,
            conflicts_committed: row.conflicts_committed,
            skip_auto_build: row.skip_auto_build,
            review_type: row.review_type,
            skip_worktrees: row.skip_worktrees,
            resolver_agent: row.resolver_agent,
            resolver_model: row.resolver_model,
            created_at_ms: row.created_at_ms,
            checks: crate::store::from_json(&row.checks),
            branches,
            change_summary: row.change_summary,
            projects,
            base_commits,
            manual_commands,
            action_hints: serde_json::from_str(row.action_hints.as_deref().unwrap_or("[]"))
                .unwrap_or_default(),
            input_values: serde_json::from_str(row.input_values.as_deref().unwrap_or("{}"))
                .unwrap_or_default(),
            input_resolutions,
            summary_agent: row.summary_agent,
            summary_model: row.summary_model,
            manual_commands_agent: row.manual_commands_agent,
            manual_commands_model: row.manual_commands_model,
            manual_commands_agent_session_id: row.manual_commands_agent_session_id,
            squash_projects: crate::store::from_json(
                row.squash_projects.as_deref().unwrap_or("[]"),
            ),
            auto_pr_feedback: row.auto_pr_feedback,
            proof_scope: row.proof_scope,
            proof_skip_auto_clean: row.proof_skip_auto_clean,
            machine: row.machine,
            effective_proof_scope,
            effective_proof_skip_auto_clean,
            skip_base_updates: row.skip_base_updates,
            effective_skip_base_updates,
            match_pr_branch_name: row.match_pr_branch_name,
            effective_match_pr_branch_name,
            separate_pr_branch: row.separate_pr_branch,
            effective_separate_pr_branch,
            readable_review_branch: row.readable_review_branch,
            review_branch_name: row.review_branch_name,
            auto_submit_pr_stack: row.auto_submit_pr_stack,
            effective_auto_submit_pr_stack,
            origin: row.origin,
            ready,
            merge_progress,
            summary_state,
            checks_state,
            build_env_overrides,
            manual_checks_env_overrides,
            combined_env,
            build_env,
            manual_checks_env,
            maximum_budget_usd: row.maximum_budget_usd,
            merge_attempt: row.merge_attempt,
            manual_checks_started_at_ms: row.manual_checks_started_at_ms,
            notice_kind: row.notice_kind,
            notice_message: row.notice_message,
            notice_at_ms: row.notice_at_ms,
            auto_build,
            attempt_tokens_in,
            attempt_tokens_out,
            attempt_cost_usd,
            cumulative_tokens_in,
            cumulative_tokens_out,
            cumulative_cost_usd,
        })
    }

    /// Record the per-project base commit for a multi-project guardian (RAL-29).
    /// Updates the JSON `base_commits` map entry for `project` and, when `project`
    /// matches the guardian's primary `git_root`, also sets the legacy `base_commit`
    /// column for backward compatibility.
    pub fn set_guardian_project_base_commit(
        &self,
        id: &str,
        project: &str,
        sha: &str,
    ) -> Result<()> {
        // Read–modify–write the JSON map.
        let stored: Option<String> = self
            .conn
            .query_row(
                "SELECT base_commits FROM guardians WHERE id=?",
                params![id],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        let mut map: std::collections::HashMap<String, String> =
            serde_json::from_str(stored.as_deref().unwrap_or("{}")).unwrap_or_default();
        map.insert(project.to_string(), sha.to_string());
        let json = serde_json::to_string(&map).unwrap_or_else(|_| "{}".to_string());
        // Also update the legacy base_commit when it matches the primary git_root.
        let git_root: Option<String> = self
            .conn
            .query_row(
                "SELECT git_root FROM guardians WHERE id=?",
                params![id],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        if git_root.as_deref() == Some(project) {
            self.conn.execute(
                "UPDATE guardians SET base_commits=?, base_commit=?, updated_at_ms=? WHERE id=?",
                params![json, sha, crate::store::now_ms(), id],
            )?;
        } else {
            self.conn.execute(
                "UPDATE guardians SET base_commits=?, updated_at_ms=? WHERE id=?",
                params![json, crate::store::now_ms(), id],
            )?;
        }
        Ok(())
    }

    /// The stored per-project base commits for base-shift detection (RAL-29).
    /// Returns an empty map when not yet set.
    pub fn guardian_project_base_commits(
        &self,
        id: &str,
    ) -> Result<std::collections::HashMap<String, String>> {
        let stored: Option<String> = self
            .conn
            .query_row(
                "SELECT base_commits FROM guardians WHERE id=?",
                params![id],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        Ok(serde_json::from_str(stored.as_deref().unwrap_or("{}")).unwrap_or_default())
    }

    /// Enable or disable per-commit squashing for one git project within a review
    /// (RAL-91). Read–modify–write the JSON `squash_projects` array: adding the
    /// project when `enabled`, removing it otherwise. Idempotent — enabling an
    /// already-enabled project (or disabling an absent one) is a no-op on the set.
    pub fn set_guardian_project_squash(
        &self,
        id: &str,
        project: &str,
        enabled: bool,
    ) -> Result<()> {
        let stored: Option<String> = self
            .conn
            .query_row(
                "SELECT squash_projects FROM guardians WHERE id=?",
                params![id],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        let mut projects: Vec<String> =
            serde_json::from_str(stored.as_deref().unwrap_or("[]")).unwrap_or_default();
        projects.retain(|p| p != project);
        if enabled {
            projects.push(project.to_string());
        }
        let json = serde_json::to_string(&projects).unwrap_or_else(|_| "[]".to_string());
        let n = self.conn.execute(
            "UPDATE guardians SET squash_projects=?, updated_at_ms=? WHERE id=?",
            params![json, crate::store::now_ms(), id],
        )?;
        if n == 0 {
            Err(StoreError::NotFound)
        } else {
            Ok(())
        }
    }

    /// Approve a guardian that is in review.
    pub fn approve_guardian(&self, id: &str) -> Result<GuardianStatus> {
        match GuardianStatus::parse(&self.guardian_status_str(id)?) {
            Some(GuardianStatus::InReview) => {
                self.set_guardian_status(id, GuardianStatus::Approved, None)?;
                Ok(GuardianStatus::Approved)
            }
            Some(other) => Err(StoreError::InvalidTransition(format!(
                "can only approve a guardian in review, it is {}",
                other.as_str()
            ))),
            None => Err(StoreError::NotFound),
        }
    }

    /// Cancel a guardian that is in a cancellable state (collecting, merging, in_review, merge_failed, merge_stopped, or approved).
    /// Background threads that are still running should check the status on completion
    /// and discard their result if the guardian is already cancelled.
    pub fn cancel_guardian(&self, id: &str) -> Result<GuardianStatus> {
        match GuardianStatus::parse(&self.guardian_status_str(id)?) {
            Some(
                GuardianStatus::Collecting
                | GuardianStatus::Merging
                | GuardianStatus::MergeFailed
                | GuardianStatus::MergeStopped
                | GuardianStatus::InReview
                | GuardianStatus::Approved,
            ) => {
                self.set_guardian_status(id, GuardianStatus::Cancelled, None)?;
                Ok(GuardianStatus::Cancelled)
            }
            Some(other) => Err(StoreError::InvalidTransition(format!(
                "cannot cancel a guardian with status {}",
                other.as_str()
            ))),
            None => Err(StoreError::NotFound),
        }
    }

    /// Reset a guardian from `merging` or `in_review` back to `collecting` so
    /// a fresh merge can be started immediately. Used by the cancel-and-restart
    /// flow: the in-flight background thread is superseded by the new one.
    pub fn reset_guardian_to_collecting(&self, id: &str) -> Result<()> {
        let n = self.conn.execute(
            "UPDATE guardians SET status='collecting', detail=NULL, updated_at_ms=? \
             WHERE id=? AND status IN ('merging','in_review')",
            params![crate::store::now_ms(), id],
        )?;
        if n == 0 {
            let _ = self.guardian_status_str(id)?; // propagate NotFound if missing
            Err(StoreError::InvalidTransition(
                "can only reset a guardian that is merging or in_review".into(),
            ))
        } else {
            let _ = self.log_event(
                None,
                Some(id),
                "guardian",
                None,
                "review → collecting (reset for restart)",
            );
            Ok(())
        }
    }

    /// Reopen a `cancelled` guardian back to `collecting` so a fresh merge can
    /// be attempted. Distinct from [`Self::reset_guardian_to_collecting`]
    /// (which resumes an in-flight `merging`/`in_review` guardian whose
    /// worker must be stopped first): a cancelled review's merge worker was
    /// already stopped before the `cancelled` write landed (see
    /// `stop_merge_worker_for_cancel`), so there is nothing to interrupt here
    /// -- only the terminal status itself blocks a fresh start.
    pub fn reopen_cancelled_guardian(&self, id: &str) -> Result<()> {
        let n = self.conn.execute(
            "UPDATE guardians SET status='collecting', detail=NULL, updated_at_ms=? \
             WHERE id=? AND status='cancelled'",
            params![crate::store::now_ms(), id],
        )?;
        if n == 0 {
            let status = self.guardian_status_str(id)?; // propagate NotFound if missing
            return Err(StoreError::InvalidTransition(format!(
                "can only reopen a guardian that is cancelled, it is {status}"
            )));
        }
        let _ = self.log_event(
            None,
            Some(id),
            "guardian",
            None,
            "review → collecting (reopened)",
        );
        Ok(())
    }

    /// Halt a guardian's in-flight merge (RAL-249), leaving it in the
    /// recoverable `merge_stopped` state rather than cancelled. Only valid from
    /// `merging`; the calling worker must already have been told to stop (via
    /// its cancel token) before this is called. The atomic `WHERE status =
    /// 'merging'` guard means a merge that actually completed (now `in_review`)
    /// in the meantime is left untouched rather than mis-labelled.
    pub fn stop_guardian_merge(&self, id: &str) -> Result<GuardianStatus> {
        let n = self.conn.execute(
            "UPDATE guardians SET status='merge_stopped', detail=NULL, updated_at_ms=? \
             WHERE id=? AND status='merging'",
            params![crate::store::now_ms(), id],
        )?;
        if n == 0 {
            let _ = self.guardian_status_str(id)?; // propagate NotFound if missing
            return Err(StoreError::InvalidTransition(
                "can only stop a guardian that is currently merging".into(),
            ));
        }
        let _ = self.log_event(
            None,
            Some(id),
            "guardian",
            None,
            "review → merge_stopped (stopped mid-rebase)",
        );
        let active_branch_ids: Vec<String> = self
            .get_guardian(id)?
            .branches
            .into_iter()
            .filter(|branch| {
                matches!(
                    branch.merge_status.as_str(),
                    "in_progress" | "proof_pending" | "actioning"
                )
            })
            .map(|branch| branch.id)
            .collect();
        for branch_id in active_branch_ids {
            self.set_branch_status(id, &branch_id, MergeStatus::Stopped, Some("merge stopped"))?;
        }
        Ok(GuardianStatus::MergeStopped)
    }

    fn guardian_status_str(&self, id: &str) -> Result<String> {
        self.conn
            .query_row(
                "SELECT status FROM guardians WHERE id=?",
                params![id],
                |r| r.get(0),
            )
            .optional()?
            .ok_or(StoreError::NotFound)
    }
}

struct GuardianRow {
    id: String,
    name: String,
    base_branch: String,
    git_root: String,
    review_branch: Option<String>,
    base_commit: Option<String>,
    /// JSON map of {project_root: sha} for multi-project base-shift detection.
    base_commits: Option<String>,
    status: String,
    detail: Option<String>,
    checks: String,
    squad_id: Option<String>,
    combined_worktree: Option<String>,
    conflicts_found: Option<i64>,
    conflicts_fixed: Option<i64>,
    conflicts_committed: Option<i64>,
    skip_auto_build: bool,
    /// RAL-110's retired `skip_worktree_checks` column (RAL-285 removed the
    /// setting itself). No runtime path writes it -- only `Store::migrate`'s
    /// pre-RAL-110 `skip_checks` backfill does. Kept solely so
    /// `hydrate_guardian` can resolve a row carrying
    /// `skip_worktree_checks=1` with no explicit `proof_scope` to
    /// `effective_proof_scope="nothing"`, preserving its prior behavior.
    legacy_skip_worktree_checks: bool,
    review_type: String,
    skip_worktrees: bool,
    resolver_agent: Option<String>,
    resolver_model: Option<String>,
    created_at_ms: i64,
    change_summary: Option<String>,
    manual_commands: Option<String>,
    action_hints: Option<String>,
    summary_agent: Option<String>,
    summary_model: Option<String>,
    manual_commands_agent: Option<String>,
    manual_commands_model: Option<String>,
    manual_commands_agent_session_id: Option<String>,
    /// JSON array of project roots with squash enabled (RAL-91).
    squash_projects: Option<String>,
    /// RAL-117: opts this review into auto-incorporating PR feedback.
    auto_pr_feedback: bool,
    /// JSON map of {input_name: value} -- resolved/submitted [`CheckInput`]
    /// values, scoped to this guardian (RAL-164).
    input_values: Option<String>,
    /// RAL-168: per-review Proof-scope override. `None` inherits the
    /// project-level default.
    proof_scope: Option<String>,
    /// RAL-168: per-review auto-clean-skip override. `None` inherits the
    /// project-level default.
    proof_skip_auto_clean: Option<bool>,
    /// RAL-250: per-review base-branch auto-update opt-out. `None` inherits
    /// the project/global default.
    skip_base_updates: Option<bool>,
    /// RAL-378: per-review separate-PR-branch override. `None` inherits the
    /// project/global default.
    separate_pr_branch: Option<bool>,
    /// RAL-378: whether this review's combined branch is named readably.
    readable_review_branch: bool,
    /// RAL-378: the sticky readable name claimed for the combined branch.
    review_branch_name: Option<String>,
    /// RAL-185: the machine this review runs on. NULL means the daemon's host.
    machine: Option<String>,
    /// RAL-203: this review's own env overrides for the finalize-time
    /// build/check-gate step. Same `{key: value|null}` shape as
    /// `guardian_branches.env_overrides`.
    build_env_overrides: Option<String>,
    /// RAL-203: this review's own env overrides for the manual-checks step.
    manual_checks_env_overrides: Option<String>,
    /// RAL-193: this review's own USD spend cap. `None` means no cap.
    maximum_budget_usd: Option<f64>,
    /// RAL-193: current merge-attempt counter, bumped once per rebase/re-merge.
    merge_attempt: i64,
    /// RAL-259: when the manual-checks generation agent most recently began work.
    manual_checks_started_at_ms: Option<i64>,
    /// RAL-273: see [`GuardianView::notice_kind`].
    notice_kind: Option<String>,
    notice_message: Option<String>,
    notice_at_ms: Option<i64>,
    /// RAL-307: per-review override for whether a newly submitted PR's
    /// branch defaults to the worktree/feature branch name. `None` inherits
    /// the project/global default.
    match_pr_branch_name: Option<bool>,
    /// RAL-317: per-review override for whether the PR stack is
    /// auto-submitted/grown as each branch reaches a terminal merge state.
    /// `None` inherits the project/global default.
    auto_submit_pr_stack: Option<bool>,
    /// RAL-318: `"explicit"` (an authored `[[review]]` block) or `"arbiter"`
    /// (drained from a Triage pool) -- see [`GUARDIAN_ORIGIN_EXPLICIT`] /
    /// [`GUARDIAN_ORIGIN_ARBITER`].
    origin: String,
    /// RAL-342: JSON-serialized [`GuardianAutoBuild`], this review's own
    /// declared build step. `None` means the review declared
    /// `skip_auto_build = true` instead, or predates the RAL-342 migration --
    /// unlike the `Option`-typed overrides above, `None` here never means
    /// "inherit the project config default".
    auto_build_json: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::NodeState;

    // ── CLI_PARITY_PLAN.local.md Phase 5: ported board.html domain logic ────────

    #[test]
    fn worst_cell_state_picks_first_rank_match() {
        // "running" outranks "done" regardless of array order.
        assert_eq!(worst_cell_state(&["done", "running"]), Some("running"));
        assert_eq!(worst_cell_state(&["done", "failed"]), Some("failed"));
        assert_eq!(worst_cell_state(&["queued", "cancelled"]), Some("queued"));
    }

    #[test]
    fn worst_cell_state_falls_back_to_first_element() {
        // An unranked state (not in CELL_STATE_RANK) falls back to states[0],
        // mirroring the JS `CELL_STATE_RANK.find(...) || cells[0].state`.
        assert_eq!(worst_cell_state(&["weird_state"]), Some("weird_state"));
        assert_eq!(worst_cell_state(&[]), None);
    }

    #[test]
    fn terminal_modes_with_session_id_are_always_readonly_and_open() {
        // A resolver cell id makes both modes available regardless of agent
        // or worktree presence.
        assert_eq!(
            terminal_modes_for(None, true, false, Path::new(".")),
            vec!["readonly", "open"]
        );
        assert_eq!(
            terminal_modes_for(Some("ollama"), true, true, Path::new(".")),
            vec!["readonly", "open"]
        );
    }

    #[test]
    fn terminal_modes_cli_agent_with_worktree_offers_worktree_only() {
        for agent in ["claude-code", "codex", "codex-cli", "pi"] {
            assert_eq!(
                terminal_modes_for(Some(agent), false, true, Path::new(".")),
                vec!["worktree"]
            );
        }
    }

    #[test]
    fn terminal_modes_none_available_without_cell_or_worktree() {
        assert_eq!(
            terminal_modes_for(Some("claude-code"), false, false, Path::new(".")),
            Vec::<&str>::new()
        );
        assert_eq!(
            terminal_modes_for(Some("ollama"), false, true, Path::new(".")),
            Vec::<&str>::new()
        );
        assert_eq!(
            terminal_modes_for(None, false, false, Path::new(".")),
            Vec::<&str>::new()
        );
    }

    #[test]
    fn merge_progress_counts_done_and_conflict_resolved_as_merged() {
        let branches = vec![
            branch_view_with_status("a", "done"),
            branch_view_with_status("b", "conflict_resolved"),
            branch_view_with_status("c", "in_progress"),
            branch_view_with_status("d", "failed"),
        ];
        let mp = MergeProgress::compute(&branches);
        assert_eq!(mp.done, 2);
        assert_eq!(mp.total, 4);
        assert_eq!(mp.failed, 1);
        assert!((mp.pct - 50.0).abs() < f64::EPSILON);
    }

    #[test]
    fn merge_progress_of_no_branches_is_zero_not_nan() {
        let mp = MergeProgress::compute(&[]);
        assert_eq!(mp.total, 0);
        assert_eq!(mp.done, 0);
        assert_eq!(mp.pct, 0.0);
    }

    fn branch_view_with_status(branch: &str, merge_status: &str) -> BranchView {
        BranchView {
            id: format!("branch-test-{branch}"),
            position: 0,
            branch: branch.to_string(),
            merge_status: merge_status.to_string(),
            detail: None,
            review_branch: None,
            readable_review_branch: true,
            review_branch_name: None,
            worktree: None,
            conflicts_found: None,
            conflicts_fixed: None,
            conflicts_committed: None,
            is_empty: false,
            source_cell_machine: None,
            enabled: true,
            project: None,
            source_cell_state: None,
            can_reenable: false,
            source_squad_id: None,
            source_task_idx: None,
            source_cell_idx: None,
            resolver_agent_session_id: None,
            ready: merge_status == "ready",
            terminal_modes: Vec::new(),
            moved_from_guardian_id: None,
            rebase_commands_done: None,
            rebase_commands_total: None,
            env_overrides: BTreeMap::new(),
            resolved_env: BTreeMap::new(),
            inherited_env: BTreeMap::new(),
            started_at_ms: None,
            auto_submit_error: None,
        }
    }

    #[test]
    fn guardian_view_exposes_ready_and_summary_state() {
        let store = Store::open_in_memory().unwrap();
        let id = store.create_guardian("r", "main", "/repo").unwrap();

        // collecting, no summary -> not ready, summary_state "waiting" (RAL-103:
        // no "generating" state -- summary computation is synchronous).
        let g = store.get_guardian(&id).unwrap();
        assert!(!g.ready);
        assert_eq!(g.summary_state, "waiting");

        // merging, still no summary -> stays "waiting" (nothing clears it back
        // to empty mid-rebuild, so this is only reachable when nothing was ever
        // computed).
        store
            .set_guardian_status(&id, GuardianStatus::Merging, None)
            .unwrap();
        let g = store.get_guardian(&id).unwrap();
        assert_eq!(g.summary_state, "waiting");

        // Any content (preliminary git-log or final LLM summary) -> "ready".
        store
            .set_guardian_summary(&id, "did a thing", None, None)
            .unwrap();
        let g = store.get_guardian(&id).unwrap();
        assert_eq!(g.summary_state, "ready");

        // in_review -> ready.
        store
            .set_guardian_status(&id, GuardianStatus::InReview, None)
            .unwrap();
        let g = store.get_guardian(&id).unwrap();
        assert!(g.ready);
    }

    // ── RAL-191: review-worktree environment inheritance + per-branch overrides ──

    /// Build a guardian whose single branch `feat` is linked to a real cell
    /// carrying task- and cell-level env, so inheritance has something to
    /// resolve. Returns `(guardian_id, branch_id)`.
    fn guardian_with_env_source_cell(store: &mut Store) -> (String, String) {
        let src = "[[task]]\nname=\"t0\"\nenvironment={SHARED=\"from-task\", TASK_ONLY=\"1\"}\n\
                   [[task.cell]]\nid=\"s0\"\ncwd=\"/r\"\nprompt=\"p\"\n\
                   environment={SHARED=\"from-cell\", CELL_ONLY=\"2\"}\n";
        let tf: ralphus_core::schema::TaskFile = toml::from_str(src).expect("valid fixture");
        let run = store.insert_squad(&tf, Some("r"), false).unwrap();
        store.set_cell_review_branch(&run, 0, 0, "feat").unwrap();
        let id = store.create_guardian("r", "main", "/repo").unwrap();
        store.add_guardian_branch(&id, "feat").unwrap();
        let bid = store.get_guardian(&id).unwrap().branches[0].id.clone();
        (id, bid)
    }

    #[test]
    fn a_review_branch_inherits_its_source_cells_resolved_environment() {
        // The core of RAL-191: a review worktree is built from a cell's
        // work, so by default it runs under that cell's environment --
        // including the task-level values the cell itself inherited.
        let mut store = Store::open_in_memory().unwrap();
        let (id, bid) = guardian_with_env_source_cell(&mut store);

        let env = store.resolve_guardian_branch_env(&id, &bid).unwrap();
        assert_eq!(env.get("SHARED").map(String::as_str), Some("from-cell"));
        assert_eq!(env.get("TASK_ONLY").map(String::as_str), Some("1"));
        assert_eq!(env.get("CELL_ONLY").map(String::as_str), Some("2"));

        // ...and the same values are on the view the board renders.
        let b = &store.get_guardian(&id).unwrap().branches[0];
        assert_eq!(
            b.inherited_env.get("TASK_ONLY").map(String::as_str),
            Some("1")
        );
        assert_eq!(
            b.resolved_env.get("TASK_ONLY").map(String::as_str),
            Some("1")
        );
        assert!(b.env_overrides.is_empty(), "no per-branch layer set yet");
    }

    #[test]
    fn a_branch_override_shadows_the_inherited_value_without_touching_the_cell() {
        let mut store = Store::open_in_memory().unwrap();
        let (id, bid) = guardian_with_env_source_cell(&mut store);

        let mut set = BTreeMap::new();
        set.insert("SHARED".to_string(), "from-review".to_string());
        store
            .set_guardian_branch_env_overrides(&id, &bid, &set, &[], &[])
            .unwrap();

        let env = store.resolve_guardian_branch_env(&id, &bid).unwrap();
        assert_eq!(env.get("SHARED").map(String::as_str), Some("from-review"));
        // Untouched keys still come through from the cell.
        assert_eq!(env.get("CELL_ONLY").map(String::as_str), Some("2"));
        // The source cell itself is unchanged -- the override is review-only.
        let run = store.list_squads().unwrap()[0].id.clone();
        assert_eq!(
            store
                .resolve_cell_env_overrides(&run, 0, 0)
                .unwrap()
                .get("SHARED")
                .map(String::as_str),
            Some("from-cell")
        );
    }

    #[test]
    fn a_branch_tombstone_removes_an_inherited_variable_entirely() {
        // The distinguishing case for the tombstone design: `unset` here must
        // mean "this worktree does not get the variable at all", not "drop my
        // override and fall back to the cell's value" (which is `clear`).
        let mut store = Store::open_in_memory().unwrap();
        let (id, bid) = guardian_with_env_source_cell(&mut store);

        store
            .set_guardian_branch_env_overrides(
                &id,
                &bid,
                &BTreeMap::new(),
                &["CELL_ONLY".to_string()],
                &[],
            )
            .unwrap();

        let env = store.resolve_guardian_branch_env(&id, &bid).unwrap();
        assert!(
            !env.contains_key("CELL_ONLY"),
            "tombstoned key must not reach the review worktree, got {env:?}"
        );
        assert_eq!(env.get("SHARED").map(String::as_str), Some("from-cell"));

        // The stored layer records the tombstone explicitly as `None`.
        let own = store.get_guardian_branch_env_overrides(&id, &bid).unwrap();
        assert_eq!(own.get("CELL_ONLY"), Some(&None));
    }

    #[test]
    fn clearing_a_branch_entry_restores_the_inherited_value() {
        // `clear` is the third operation -- it drops the branch's own entry
        // (override *or* tombstone) so the key inherits again.
        let mut store = Store::open_in_memory().unwrap();
        let (id, bid) = guardian_with_env_source_cell(&mut store);

        store
            .set_guardian_branch_env_overrides(
                &id,
                &bid,
                &BTreeMap::new(),
                &["SHARED".to_string()],
                &[],
            )
            .unwrap();
        assert!(
            !store
                .resolve_guardian_branch_env(&id, &bid)
                .unwrap()
                .contains_key("SHARED")
        );

        store
            .set_guardian_branch_env_overrides(
                &id,
                &bid,
                &BTreeMap::new(),
                &[],
                &["SHARED".to_string()],
            )
            .unwrap();
        assert_eq!(
            store
                .resolve_guardian_branch_env(&id, &bid)
                .unwrap()
                .get("SHARED")
                .map(String::as_str),
            Some("from-cell"),
            "clearing the tombstone must restore inheritance, not leave it removed"
        );
    }

    #[test]
    fn a_branch_with_no_source_cell_has_only_its_own_overrides() {
        // A manually-added branch (or one whose cell was deleted) inherits
        // nothing; a tombstone for a never-inherited key is a harmless no-op.
        let store = Store::open_in_memory().unwrap();
        let id = store.create_guardian("r", "main", "/repo").unwrap();
        store.add_guardian_branch(&id, "orphan").unwrap();
        let bid = store.get_guardian(&id).unwrap().branches[0].id.clone();

        let mut set = BTreeMap::new();
        set.insert("ONLY_HERE".to_string(), "x".to_string());
        store
            .set_guardian_branch_env_overrides(&id, &bid, &set, &["NEVER_SET".to_string()], &[])
            .unwrap();

        let env = store.resolve_guardian_branch_env(&id, &bid).unwrap();
        assert_eq!(env.get("ONLY_HERE").map(String::as_str), Some("x"));
        assert!(!env.contains_key("NEVER_SET"));
        assert_eq!(env.len(), 1);
    }

    #[test]
    fn branch_env_for_an_unknown_branch_is_not_found() {
        let store = Store::open_in_memory().unwrap();
        let id = store.create_guardian("r", "main", "/repo").unwrap();
        assert!(matches!(
            store.get_guardian_branch_env_overrides(&id, "branch-nope"),
            Err(StoreError::NotFound)
        ));
    }

    // ── RAL-203: combined-worktree env inheritance for the build/check-gate
    // and manual-checks steps, with independent per-section overrides ──────

    #[test]
    fn combined_worktree_steps_inherit_the_last_branch_env_by_default() {
        // The core of RAL-203: the combined worktree has no source cell of
        // its own, so both the build/check-gate step and the manual-checks
        // step borrow `combined_env` (the last enabled branch's resolved
        // env) by default, with no overrides of their own yet.
        let mut store = Store::open_in_memory().unwrap();
        let (id, _bid) = guardian_with_env_source_cell(&mut store);

        let g = store.get_guardian(&id).unwrap();
        for env in [&g.combined_env, &g.build_env, &g.manual_checks_env] {
            assert_eq!(env.get("SHARED").map(String::as_str), Some("from-cell"));
            assert_eq!(env.get("TASK_ONLY").map(String::as_str), Some("1"));
            assert_eq!(env.get("CELL_ONLY").map(String::as_str), Some("2"));
        }
        assert!(g.build_env_overrides.is_empty());
        assert!(g.manual_checks_env_overrides.is_empty());
    }

    #[test]
    fn a_build_only_override_does_not_leak_into_manual_checks_or_combined_env() {
        let mut store = Store::open_in_memory().unwrap();
        let (id, _bid) = guardian_with_env_source_cell(&mut store);

        let mut set = BTreeMap::new();
        set.insert("SHARED".to_string(), "from-build-override".to_string());
        store
            .set_guardian_build_env_overrides(&id, &set, &[], &[])
            .unwrap();

        let g = store.get_guardian(&id).unwrap();
        assert_eq!(
            g.build_env.get("SHARED").map(String::as_str),
            Some("from-build-override")
        );
        // Independent of the build layer: manual-checks and the shared
        // combined baseline are both untouched.
        assert_eq!(
            g.manual_checks_env.get("SHARED").map(String::as_str),
            Some("from-cell")
        );
        assert_eq!(
            g.combined_env.get("SHARED").map(String::as_str),
            Some("from-cell")
        );
    }

    #[test]
    fn a_manual_checks_only_override_does_not_leak_into_build_or_combined_env() {
        let mut store = Store::open_in_memory().unwrap();
        let (id, _bid) = guardian_with_env_source_cell(&mut store);

        let mut set = BTreeMap::new();
        set.insert("SHARED".to_string(), "from-manual-override".to_string());
        store
            .set_guardian_manual_checks_env_overrides(&id, &set, &[], &[])
            .unwrap();

        let g = store.get_guardian(&id).unwrap();
        assert_eq!(
            g.manual_checks_env.get("SHARED").map(String::as_str),
            Some("from-manual-override")
        );
        assert_eq!(
            g.build_env.get("SHARED").map(String::as_str),
            Some("from-cell")
        );
        assert_eq!(
            g.combined_env.get("SHARED").map(String::as_str),
            Some("from-cell")
        );
    }

    #[test]
    fn build_and_manual_checks_overrides_apply_independently_and_differently() {
        let mut store = Store::open_in_memory().unwrap();
        let (id, _bid) = guardian_with_env_source_cell(&mut store);

        let mut build_set = BTreeMap::new();
        build_set.insert("SHARED".to_string(), "build-value".to_string());
        store
            .set_guardian_build_env_overrides(&id, &build_set, &[], &[])
            .unwrap();

        let mut manual_set = BTreeMap::new();
        manual_set.insert("SHARED".to_string(), "manual-value".to_string());
        // Also tombstone a key only for manual-checks -- proves the tombstone
        // is scoped to this layer, not the shared combined_env.
        store
            .set_guardian_manual_checks_env_overrides(
                &id,
                &manual_set,
                &["TASK_ONLY".to_string()],
                &[],
            )
            .unwrap();

        let g = store.get_guardian(&id).unwrap();
        assert_eq!(
            g.build_env.get("SHARED").map(String::as_str),
            Some("build-value")
        );
        assert_eq!(
            g.manual_checks_env.get("SHARED").map(String::as_str),
            Some("manual-value")
        );
        assert_eq!(
            g.build_env.get("TASK_ONLY").map(String::as_str),
            Some("1"),
            "the manual-checks tombstone must not affect the build layer"
        );
        assert!(
            !g.manual_checks_env.contains_key("TASK_ONLY"),
            "manual-checks own tombstone should remove the inherited key"
        );
        assert_eq!(
            g.combined_env.get("SHARED").map(String::as_str),
            Some("from-cell"),
            "the shared branch-union baseline is never mutated by either section's overrides"
        );

        // `clear` on one section restores inheritance for that section only.
        store
            .set_guardian_build_env_overrides(&id, &BTreeMap::new(), &[], &["SHARED".to_string()])
            .unwrap();
        let g = store.get_guardian(&id).unwrap();
        assert_eq!(
            g.build_env.get("SHARED").map(String::as_str),
            Some("from-cell")
        );
        assert_eq!(
            g.manual_checks_env.get("SHARED").map(String::as_str),
            Some("manual-value"),
            "clearing the build override must not touch manual-checks' own override"
        );
    }

    #[test]
    fn build_and_manual_checks_env_overrides_for_an_unknown_guardian_are_not_found() {
        let store = Store::open_in_memory().unwrap();
        assert!(matches!(
            store.get_guardian_build_env_overrides("guardian-nope"),
            Err(StoreError::NotFound)
        ));
        assert!(matches!(
            store.get_guardian_manual_checks_env_overrides("guardian-nope"),
            Err(StoreError::NotFound)
        ));
    }

    #[test]
    fn guardian_view_exposes_checks_state() {
        let store = Store::open_in_memory().unwrap();
        let id = store.create_guardian("r", "main", "/repo").unwrap();
        store.add_guardian_branch(&id, "feat").unwrap();
        let bid = store.get_guardian(&id).unwrap().branches[0].id.clone();

        // collecting, branch still pending -> "waiting".
        let g = store.get_guardian(&id).unwrap();
        assert_eq!(g.checks_state, "waiting");

        // merging, branch not yet done rebasing -> still "waiting", not
        // "generating" (RAL-103: the old bug showed "generating" for the whole
        // merging phase, even while branches were still being rebased).
        store
            .set_guardian_status(&id, GuardianStatus::Merging, None)
            .unwrap();
        store
            .set_branch_status(&id, &bid, MergeStatus::InProgress, None)
            .unwrap();
        let g = store.get_guardian(&id).unwrap();
        assert_eq!(g.checks_state, "waiting");

        // merging, every enabled branch done rebasing, no commands yet ->
        // "generating" (the real, narrow window while the LLM call is in flight).
        store
            .set_branch_status(&id, &bid, MergeStatus::Done, None)
            .unwrap();
        let g = store.get_guardian(&id).unwrap();
        assert_eq!(g.checks_state, "generating");

        // commands persisted -> "ready", regardless of status.
        store
            .set_guardian_manual_commands(
                &id,
                &[GuardianCheck {
                    label: None,
                    command: Some("echo hi".to_string()),
                    prompt: None,
                    cleanup_command: None,
                    inputs: vec![],
                }],
                None,
                None,
            )
            .unwrap();
        let g = store.get_guardian(&id).unwrap();
        assert_eq!(g.checks_state, "ready");
    }

    // ── RAL-164: structured GuardianCheck + input_values persistence ────────

    #[test]
    fn manual_commands_round_trip_structured_fields() {
        let store = Store::open_in_memory().unwrap();
        let id = store.create_guardian("r", "main", "/repo").unwrap();
        let checks = vec![
            GuardianCheck {
                label: None,
                command: Some("cargo test".to_string()),
                prompt: None,
                cleanup_command: None,
                inputs: vec![],
            },
            GuardianCheck {
                label: None,
                command: Some("ralphus-daemon serve --port {port}".to_string()),
                prompt: None,
                cleanup_command: Some("ralphus-daemon stop --port {port}".to_string()),
                inputs: vec![CheckInput {
                    name: "port".to_string(),
                    message: "Port for the daemon".to_string(),
                    default: "7890".to_string(),
                    r#type: CheckInputType::Int,
                }],
            },
        ];
        store
            .set_guardian_manual_commands(&id, &checks, Some("claude"), Some("opus"))
            .unwrap();
        let g = store.get_guardian(&id).unwrap();
        assert_eq!(g.manual_commands.len(), 2);
        assert!(g.manual_commands[0].cleanup_command.is_none());
        assert_eq!(
            g.manual_commands[1].cleanup_command.as_deref(),
            Some("ralphus-daemon stop --port {port}")
        );
        assert_eq!(g.manual_commands[1].inputs.len(), 1);
        assert_eq!(g.manual_commands[1].inputs[0].name, "port");
        assert_eq!(g.manual_commands[1].inputs[0].default, "7890");
    }

    #[test]
    fn action_hints_round_trip_structured_fields() {
        let store = Store::open_in_memory().unwrap();
        let id = store.create_guardian("r", "main", "/repo").unwrap();
        let hints = vec![GuardianCheck {
            label: Some("Serve locally".to_string()),
            command: Some("ralphus-daemon serve --port {port}".to_string()),
            prompt: None,
            cleanup_command: Some("ralphus-daemon stop --port {port}".to_string()),
            inputs: vec![CheckInput {
                name: "port".to_string(),
                message: "Port for the daemon".to_string(),
                default: "7890".to_string(),
                r#type: CheckInputType::Int,
            }],
        }];
        store.set_guardian_action_hints(&id, &hints).unwrap();
        let g = store.get_guardian(&id).unwrap();
        assert_eq!(g.action_hints.len(), 1);
        assert_eq!(g.action_hints[0].label.as_deref(), Some("Serve locally"));
        assert_eq!(g.action_hints[0].inputs[0].name, "port");
    }

    #[test]
    fn merge_guardian_input_values_merges_without_clobbering() {
        let store = Store::open_in_memory().unwrap();
        let id = store.create_guardian("r", "main", "/repo").unwrap();

        let g = store.get_guardian(&id).unwrap();
        assert!(g.input_values.is_empty());

        store
            .merge_guardian_input_values(
                &id,
                &std::collections::HashMap::from([("port".to_string(), "9001".to_string())]),
            )
            .unwrap();
        let g = store.get_guardian(&id).unwrap();
        assert_eq!(g.input_values.get("port"), Some(&"9001".to_string()));

        // A second merge for a different key must not clobber the first.
        store
            .merge_guardian_input_values(
                &id,
                &std::collections::HashMap::from([("branch".to_string(), "main".to_string())]),
            )
            .unwrap();
        let g = store.get_guardian(&id).unwrap();
        assert_eq!(g.input_values.get("port"), Some(&"9001".to_string()));
        assert_eq!(g.input_values.get("branch"), Some(&"main".to_string()));

        // Re-submitting the same key overwrites just that value.
        store
            .merge_guardian_input_values(
                &id,
                &std::collections::HashMap::from([("port".to_string(), "9002".to_string())]),
            )
            .unwrap();
        let g = store.get_guardian(&id).unwrap();
        assert_eq!(g.input_values.get("port"), Some(&"9002".to_string()));
    }

    #[test]
    fn merge_guardian_input_values_empty_map_is_a_no_op() {
        let store = Store::open_in_memory().unwrap();
        let id = store.create_guardian("r", "main", "/repo").unwrap();
        store
            .merge_guardian_input_values(&id, &std::collections::HashMap::new())
            .unwrap();
        let g = store.get_guardian(&id).unwrap();
        assert!(g.input_values.is_empty());
    }

    #[test]
    fn claim_guardian_input_resolution_is_atomic_and_rejects_concurrent_duplicate() {
        let store = Store::open_in_memory().unwrap();
        let id = store.create_guardian("r", "main", "/repo").unwrap();

        // First claim wins.
        assert!(store.claim_guardian_input_resolution(&id, "port").unwrap());
        // A concurrent duplicate claim for the SAME input loses -- this is
        // the actual spam-proofing, not a UI-side disabled button.
        assert!(!store.claim_guardian_input_resolution(&id, "port").unwrap());
        // A DIFFERENT input on the same guardian is unaffected.
        assert!(
            store
                .claim_guardian_input_resolution(&id, "branch")
                .unwrap()
        );

        let g = store.get_guardian(&id).unwrap();
        assert_eq!(g.input_resolutions["port"].status, "resolving");
        assert!(g.input_resolutions["port"].value.is_none());

        // Once resolved, a fresh claim for the same input is allowed again
        // (e.g. clicking "set it for me" a second time later).
        store
            .set_guardian_input_resolution_ready(&id, "port", "9001")
            .unwrap();
        let g = store.get_guardian(&id).unwrap();
        assert_eq!(g.input_resolutions["port"].status, "ready");
        assert_eq!(g.input_resolutions["port"].value.as_deref(), Some("9001"));
        assert!(store.claim_guardian_input_resolution(&id, "port").unwrap());
    }

    #[test]
    fn set_guardian_input_resolution_failed_marks_status() {
        let store = Store::open_in_memory().unwrap();
        let id = store.create_guardian("r", "main", "/repo").unwrap();
        store.claim_guardian_input_resolution(&id, "port").unwrap();
        store
            .set_guardian_input_resolution_failed(&id, "port")
            .unwrap();
        let g = store.get_guardian(&id).unwrap();
        assert_eq!(g.input_resolutions["port"].status, "failed");
        assert!(g.input_resolutions["port"].value.is_none());
        // A failed resolution can be retried.
        assert!(store.claim_guardian_input_resolution(&id, "port").unwrap());
    }

    #[test]
    fn recover_orphaned_input_resolutions_resets_stuck_resolving_rows() {
        let store = Store::open_in_memory().unwrap();
        let id = store.create_guardian("r", "main", "/repo").unwrap();
        store.claim_guardian_input_resolution(&id, "port").unwrap();

        assert!(
            !store
                .recover_orphaned_input_resolutions()
                .unwrap()
                .is_empty()
        );
        let g = store.get_guardian(&id).unwrap();
        assert_eq!(g.input_resolutions["port"].status, "failed");

        // A second sweep finds nothing left to recover.
        assert!(
            store
                .recover_orphaned_input_resolutions()
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn guardian_view_checks_state_ignores_disabled_branches() {
        // RAL-103: a disabled branch is reset to `pending` and never rebased, so
        // it must not count against the "every branch is done rebasing" gate --
        // otherwise a review with any disabled branch could never show
        // "generating" and would understate real progress to the user.
        let store = Store::open_in_memory().unwrap();
        let id = store.create_guardian("r", "main", "/repo").unwrap();
        store.add_guardian_branch(&id, "feat").unwrap();
        store.add_guardian_branch(&id, "skip-me").unwrap();
        let bid = store.get_guardian(&id).unwrap().branches[0].id.clone();
        store
            .set_branch_enabled_by_name(&id, "skip-me", false)
            .unwrap();

        store
            .set_guardian_status(&id, GuardianStatus::Merging, None)
            .unwrap();
        // The disabled branch stays `pending`; only the enabled one finishes.
        store
            .set_branch_status(&id, &bid, MergeStatus::Done, None)
            .unwrap();
        let g = store.get_guardian(&id).unwrap();
        assert_eq!(g.checks_state, "generating");
    }

    #[test]
    fn create_add_and_fetch() {
        let store = Store::open_in_memory().unwrap();
        let id = store.create_guardian("my review", "main", "/repo").unwrap();
        assert_eq!(id, "guardian-000000000001");
        assert_eq!(store.add_guardian_branch(&id, "feature/a").unwrap(), 0);
        assert_eq!(store.add_guardian_branch(&id, "feature/b").unwrap(), 1);

        let g = store.get_guardian(&id).unwrap();
        assert_eq!(g.name, "my review");
        assert_eq!(g.status, "collecting");
        assert_eq!(g.branches.len(), 2);
        assert_eq!(g.branches[0].branch, "feature/a");
        assert_eq!(g.branches[1].position, 1);
    }

    #[test]
    fn status_transitions_and_approve() {
        let store = Store::open_in_memory().unwrap();
        let id = store.create_guardian("r", "main", "/repo").unwrap();
        store
            .create_watch(
                "watcher",
                &format!("guardian:{id}"),
                &[crate::mailbox::MailboxPriority::Normal],
            )
            .unwrap();
        assert!(store.approve_guardian(&id).is_err()); // not in review yet
        store
            .set_guardian_status(&id, GuardianStatus::InReview, None)
            .unwrap();
        assert_eq!(
            store.approve_guardian(&id).unwrap(),
            GuardianStatus::Approved
        );
        assert_eq!(store.get_guardian(&id).unwrap().status, "approved");
        let expected_uri = format!("guardian:{id}");
        let messages = store
            .personal_mailbox_messages_for_user("watcher", false, None)
            .unwrap();
        assert_eq!(messages.len(), 2);
        assert!(messages.iter().all(|message| {
            message.event_kind.as_deref() == Some("review_status_changed")
                && message.entity_uri.as_deref() == Some(expected_uri.as_str())
        }));
    }

    #[test]
    fn branch_status_and_review_branch() {
        let store = Store::open_in_memory().unwrap();
        let id = store.create_guardian("r", "main", "/repo").unwrap();
        store.add_guardian_branch(&id, "feat").unwrap();
        let bid = store.get_guardian(&id).unwrap().branches[0].id.clone();
        store
            .set_branch_status(&id, &bid, MergeStatus::ConflictResolved, Some("2 files"))
            .unwrap();
        store
            .set_guardian_review_branch(&id, "guardian/abc")
            .unwrap();
        let g = store.get_guardian(&id).unwrap();
        assert_eq!(g.branches[0].merge_status, "conflict_resolved");
        assert_eq!(g.branches[0].detail.as_deref(), Some("2 files"));
        assert_eq!(g.review_branch.as_deref(), Some("guardian/abc"));
    }

    #[test]
    fn skip_worktrees_defaults_off_and_toggles() {
        let store = Store::open_in_memory().unwrap();
        let id = store.create_guardian("r", "main", "/repo").unwrap();
        assert!(!store.get_guardian(&id).unwrap().skip_worktrees);
        store.set_guardian_skip_worktrees(&id, true).unwrap();
        assert!(store.get_guardian(&id).unwrap().skip_worktrees);
        assert!(store.set_guardian_skip_worktrees("nope", true).is_err());
    }

    #[test]
    fn review_type_defaults_git_and_sets() {
        let store = Store::open_in_memory().unwrap();
        let id = store.create_guardian("r", "main", "/repo").unwrap();
        assert_eq!(store.get_guardian(&id).unwrap().review_type, "git");
        store.set_guardian_type(&id, "document").unwrap();
        assert_eq!(store.get_guardian(&id).unwrap().review_type, "document");
        assert!(store.set_guardian_type("nope", "git").is_err());
    }

    #[test]
    fn resolver_defaults_none_and_sets() {
        let store = Store::open_in_memory().unwrap();
        let id = store.create_guardian("r", "main", "/repo").unwrap();
        let g = store.get_guardian(&id).unwrap();
        assert_eq!(g.resolver_agent, None);
        assert_eq!(g.resolver_model, None);
        store
            .set_guardian_resolver(&id, Some("claude"), Some("claude-opus-4-8"))
            .unwrap();
        let g = store.get_guardian(&id).unwrap();
        assert_eq!(g.resolver_agent.as_deref(), Some("claude"));
        assert_eq!(g.resolver_model.as_deref(), Some("claude-opus-4-8"));
        assert!(
            store
                .set_guardian_resolver("nope", Some("x"), None)
                .is_err()
        );
    }

    #[test]
    fn origin_defaults_explicit_and_sets_to_arbiter() {
        let store = Store::open_in_memory().unwrap();
        let id = store.create_guardian("r", "main", "/repo").unwrap();
        let g = store.get_guardian(&id).unwrap();
        assert_eq!(g.origin, GUARDIAN_ORIGIN_EXPLICIT);
        store
            .set_guardian_origin(&id, GUARDIAN_ORIGIN_ARBITER)
            .unwrap();
        let g = store.get_guardian(&id).unwrap();
        assert_eq!(g.origin, GUARDIAN_ORIGIN_ARBITER);
        assert!(
            store
                .set_guardian_origin("nope", GUARDIAN_ORIGIN_ARBITER)
                .is_err()
        );
    }

    #[test]
    fn deleting_a_guardian_clears_its_messages() {
        let store = Store::open_in_memory().unwrap();
        let id = store.create_guardian("r", "main", "/repo").unwrap();
        store
            .add_guardian_message(&id, "reviewer", "hi", None, Some("branch-a"), None, None)
            .unwrap();
        store.delete_guardian(&id).unwrap();
        assert!(
            store
                .guardian_branch_messages(&id, "branch-a")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn guardian_branch_messages_are_isolated_per_branch() {
        // RAL-272: a branch-scoped message only shows up under its own
        // branch_id, and an unscoped message (branch_id=None) never leaks
        // into a per-branch query.
        let store = Store::open_in_memory().unwrap();
        let id = store.create_guardian("r", "main", "/repo").unwrap();
        store
            .add_guardian_message(&id, "reviewer", "unscoped", None, None, None, None)
            .unwrap();
        store
            .add_guardian_message(
                &id,
                "reviewer",
                "feedback on a",
                None,
                Some("branch-a"),
                None,
                None,
            )
            .unwrap();
        store
            .add_guardian_message(
                &id,
                "guardian",
                "reply on a",
                None,
                Some("branch-a"),
                None,
                None,
            )
            .unwrap();
        store
            .add_guardian_message(
                &id,
                "reviewer",
                "feedback on b",
                None,
                Some("branch-b"),
                None,
                None,
            )
            .unwrap();

        let a = store.guardian_branch_messages(&id, "branch-a").unwrap();
        assert_eq!(a.len(), 2);
        assert_eq!(a[0].text, "feedback on a");
        assert_eq!(a[1].text, "reply on a");

        let b = store.guardian_branch_messages(&id, "branch-b").unwrap();
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].text, "feedback on b");

        assert!(
            store
                .guardian_branch_messages(&id, "branch-c")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn guardian_message_records_author_and_submitted_by_separately() {
        // RAL-379: the attributed author and the authenticated submitter are
        // independent identities -- a message can carry both, even when they
        // differ (e.g. Bob submitting feedback attributed to Alice).
        let store = Store::open_in_memory().unwrap();
        let id = store.create_guardian("r", "main", "/repo").unwrap();
        store
            .add_guardian_message(
                &id,
                "reviewer",
                "please fix",
                None,
                Some("branch-a"),
                Some("alice"),
                Some("bob"),
            )
            .unwrap();
        let msgs = store.guardian_branch_messages(&id, "branch-a").unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].author.as_deref(), Some("alice"));
        assert_eq!(msgs[0].submitted_by.as_deref(), Some("bob"));
    }

    #[test]
    fn guardian_role_message_has_no_action_status() {
        // RAL-380: only a "reviewer" message is actionable -- a "guardian"
        // acknowledgment never gets a completion checkmark on the board.
        let store = Store::open_in_memory().unwrap();
        let id = store.create_guardian("r", "main", "/repo").unwrap();
        store
            .add_guardian_message(&id, "guardian", "ack", None, Some("branch-a"), None, None)
            .unwrap();
        let msgs = store.guardian_branch_messages(&id, "branch-a").unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].action_status, None);
    }

    #[test]
    fn reviewer_message_action_status_lifecycle() {
        // RAL-380: a reviewer message starts `received`; a newer feedback
        // message on the same branch supersedes it; and a stale completion
        // for the superseded message must never clobber that terminal state
        // back to `done`/`failed`.
        let store = Store::open_in_memory().unwrap();
        let id = store.create_guardian("r", "main", "/repo").unwrap();
        let seq_a = store
            .add_guardian_message(&id, "reviewer", "first", None, Some("branch-a"), None, None)
            .unwrap();
        assert_eq!(
            store.guardian_branch_messages(&id, "branch-a").unwrap()[0]
                .action_status
                .as_deref(),
            Some("received")
        );

        store
            .supersede_pending_branch_feedback(&id, "branch-a")
            .unwrap();
        let seq_b = store
            .add_guardian_message(
                &id,
                "reviewer",
                "second",
                None,
                Some("branch-a"),
                None,
                None,
            )
            .unwrap();
        let msgs = store.guardian_branch_messages(&id, "branch-a").unwrap();
        assert_eq!(msgs[0].action_status.as_deref(), Some("superseded"));
        assert_eq!(msgs[1].action_status.as_deref(), Some("received"));

        // The first (now-stale) run finishing late must not clobber `superseded`.
        store
            .set_message_action_status(seq_a, FeedbackActionStatus::Done)
            .unwrap();
        assert_eq!(
            store.guardian_branch_messages(&id, "branch-a").unwrap()[0]
                .action_status
                .as_deref(),
            Some("superseded")
        );

        store
            .set_message_action_status(seq_b, FeedbackActionStatus::Failed)
            .unwrap();
        assert_eq!(
            store.guardian_branch_messages(&id, "branch-a").unwrap()[1]
                .action_status
                .as_deref(),
            Some("failed")
        );
    }

    #[test]
    fn latest_received_feedback_message_seq_finds_the_newest_pending_one() {
        // RAL-380: used only by startup recovery to reattach a re-run
        // `run_feedback` call to the message it's resuming.
        let store = Store::open_in_memory().unwrap();
        let id = store.create_guardian("r", "main", "/repo").unwrap();
        assert_eq!(
            store
                .latest_received_feedback_message_seq(&id, "branch-a")
                .unwrap(),
            None
        );
        let seq = store
            .add_guardian_message(
                &id,
                "reviewer",
                "fix it",
                None,
                Some("branch-a"),
                None,
                None,
            )
            .unwrap();
        assert_eq!(
            store
                .latest_received_feedback_message_seq(&id, "branch-a")
                .unwrap(),
            Some(seq)
        );
        store
            .set_message_action_status(seq, FeedbackActionStatus::Done)
            .unwrap();
        assert_eq!(
            store
                .latest_received_feedback_message_seq(&id, "branch-a")
                .unwrap(),
            None
        );
    }

    #[test]
    fn claim_guardian_merge_is_atomic_and_covers_all_claimable_states() {
        let store = Store::open_in_memory().unwrap();
        let id = store.create_guardian("r", "main", "/repo").unwrap();
        store.add_guardian_branch(&id, "feat").unwrap();

        assert!(store.claim_guardian_merge(&id).unwrap());
        assert_eq!(store.get_guardian(&id).unwrap().status, "merging");

        // Already `merging`: a second concurrent claim must lose.
        assert!(!store.claim_guardian_merge(&id).unwrap());

        store
            .set_guardian_status(&id, GuardianStatus::MergeFailed, Some("conflict"))
            .unwrap();
        assert!(store.claim_guardian_merge(&id).unwrap());
        assert_eq!(store.get_guardian(&id).unwrap().status, "merging");

        // RAL-108: pressing "Merge / rebase" on an already-`in_review` review
        // (branches all merged, review marked done) must force a fresh rebase
        // rather than 409ing as a no-op.
        store
            .set_guardian_status(&id, GuardianStatus::InReview, None)
            .unwrap();
        assert!(store.claim_guardian_merge(&id).unwrap());
        assert_eq!(store.get_guardian(&id).unwrap().status, "merging");

        // RAL-249: a `merge_stopped` review can be resumed with a fresh merge.
        store
            .set_guardian_status(&id, GuardianStatus::MergeStopped, None)
            .unwrap();
        assert!(store.claim_guardian_merge(&id).unwrap());
        assert_eq!(store.get_guardian(&id).unwrap().status, "merging");
    }

    #[test]
    fn stop_guardian_merge_stops_each_active_branch() {
        let store = Store::open_in_memory().unwrap();
        let id = store.create_guardian("r", "main", "/repo").unwrap();
        store.add_guardian_branch(&id, "feat").unwrap();
        store.add_guardian_branch(&id, "proof").unwrap();
        store.add_guardian_branch(&id, "feedback").unwrap();

        // Not merging: stop must be rejected.
        assert!(store.stop_guardian_merge(&id).is_err());
        assert_eq!(store.get_guardian(&id).unwrap().status, "collecting");

        // Merging: stop flips to merge_stopped.
        store.claim_guardian_merge(&id).unwrap();
        let branch_ids: Vec<String> = store
            .get_guardian(&id)
            .unwrap()
            .branches
            .iter()
            .map(|branch| branch.id.clone())
            .collect();
        store
            .set_branch_status(&id, &branch_ids[0], MergeStatus::InProgress, None)
            .unwrap();
        store
            .set_branch_status(&id, &branch_ids[1], MergeStatus::ProofPending, None)
            .unwrap();
        store
            .set_branch_status(&id, &branch_ids[2], MergeStatus::Actioning, None)
            .unwrap();
        assert_eq!(
            store.stop_guardian_merge(&id).unwrap(),
            GuardianStatus::MergeStopped
        );
        assert_eq!(store.get_guardian(&id).unwrap().status, "merge_stopped");
        assert!(
            store
                .get_guardian(&id)
                .unwrap()
                .branches
                .iter()
                .all(|branch| { branch.merge_status == MergeStatus::Stopped.as_str() })
        );

        // merge_stopped is distinct from cancelled and is cancellable.
        assert_eq!(
            store.cancel_guardian(&id).unwrap(),
            GuardianStatus::Cancelled
        );
        assert_eq!(store.get_guardian(&id).unwrap().status, "cancelled");

        // A terminal/cancelled review can't be stopped.
        assert!(store.stop_guardian_merge(&id).is_err());
    }

    #[test]
    fn reopen_cancelled_guardian_only_accepts_cancelled() {
        let store = Store::open_in_memory().unwrap();
        let id = store.create_guardian("r", "main", "/repo").unwrap();
        store.add_guardian_branch(&id, "feat").unwrap();

        // Not cancelled yet: reopen must be rejected.
        assert!(store.reopen_cancelled_guardian(&id).is_err());
        assert_eq!(store.get_guardian(&id).unwrap().status, "collecting");

        store.claim_guardian_merge(&id).unwrap();
        assert_eq!(
            store.cancel_guardian(&id).unwrap(),
            GuardianStatus::Cancelled
        );
        assert_eq!(store.get_guardian(&id).unwrap().status, "cancelled");

        store.reopen_cancelled_guardian(&id).unwrap();
        assert_eq!(store.get_guardian(&id).unwrap().status, "collecting");

        // Already reopened: a second reopen call must be rejected.
        assert!(store.reopen_cancelled_guardian(&id).is_err());
    }

    /// RAL-375: a guardian left `merging` by an unclean shutdown must land
    /// back in `collecting` -- reclaimable by `claim_guardian_merge` on the
    /// very next scheduler tick -- not `merge_failed`, which used to sit idle
    /// until a human ran `ralphus review merge` by hand. This is the
    /// store-level half of the fix (`interrupted_merges` +
    /// `reset_guardian_to_collecting`); `scheduler::recover_interrupted_reviews`
    /// is what actually calls this pair at startup, ahead of the scheduler's
    /// main loop, replacing the old `Store::recover_orphaned_merges`
    /// (RAL-48) which reset to `merge_failed` instead and, worse, ran too
    /// early in `server::serve` for this auto-resume path to ever see a
    /// guardian still in `merging`.
    #[test]
    fn interrupted_merge_lands_back_in_collecting_not_merge_failed() {
        let store = Store::open_in_memory().unwrap();
        let id = store.create_guardian("r", "main", "/repo").unwrap();
        store.add_guardian_branch(&id, "feat").unwrap();

        assert!(store.interrupted_merges().unwrap().is_empty());

        store.claim_guardian_merge(&id).unwrap();
        assert_eq!(store.get_guardian(&id).unwrap().status, "merging");

        let interrupted = store.interrupted_merges().unwrap();
        assert_eq!(interrupted, vec![id.clone()]);
        for gid in &interrupted {
            store.reset_guardian_to_collecting(gid).unwrap();
        }

        let g = store.get_guardian(&id).unwrap();
        assert_eq!(
            g.status, "collecting",
            "an interrupted merge must auto-resume via `collecting`, not sit \
             in `merge_failed` waiting on a human"
        );

        // Reclaimable immediately, exactly as a fresh `collecting` guardian
        // the scheduler picks up on its own would be -- this puts it back in
        // `merging` for real, which is why a fresh `interrupted_merges` query
        // right after this would (correctly) find it again.
        assert!(store.claim_guardian_merge(&id).unwrap());
    }

    /// RAL-375: feedback text is persisted durably the moment it's set, and
    /// is found by `branches_with_pending_feedback` -- the mechanism startup
    /// recovery uses to reapply feedback an unclean shutdown interrupted
    /// mid-`guardian_merge::run_feedback`, instead of it being silently lost
    /// (previously this text existed only as that function's own in-memory
    /// argument). Clearing it (as every real completion path of
    /// `run_feedback` does) removes it from that recovery set again.
    #[test]
    fn pending_feedback_persists_until_explicitly_cleared() {
        let store = Store::open_in_memory().unwrap();
        let id = store.create_guardian("r", "main", "/repo").unwrap();
        let pos = store.add_guardian_branch(&id, "feat").unwrap();
        let branch_id = store.get_guardian(&id).unwrap().branches[pos as usize]
            .id
            .clone();

        assert!(store.branches_with_pending_feedback().unwrap().is_empty());

        store
            .set_branch_pending_feedback(&id, &branch_id, "fix the compile error")
            .unwrap();
        assert_eq!(
            store.branches_with_pending_feedback().unwrap(),
            vec![(
                id.clone(),
                branch_id.clone(),
                "fix the compile error".to_string()
            )]
        );

        // Simulating a daemon restart (re-reading the same durable state)
        // still finds it -- this is exactly what an in-memory-only argument
        // would NOT survive.
        assert_eq!(store.branches_with_pending_feedback().unwrap().len(), 1);

        store
            .clear_branch_pending_feedback(&id, &branch_id)
            .unwrap();
        assert!(store.branches_with_pending_feedback().unwrap().is_empty());
    }

    #[test]
    fn skip_auto_build_defaults_off_and_toggles() {
        let store = Store::open_in_memory().unwrap();
        let id = store.create_guardian("r", "main", "/repo").unwrap();
        assert!(!store.get_guardian(&id).unwrap().skip_auto_build);
        assert!(!store.guardian_skip_auto_build(&id).unwrap());
        store.set_guardian_skip_auto_build(&id, true).unwrap();
        assert!(store.get_guardian(&id).unwrap().skip_auto_build);
        assert!(store.guardian_skip_auto_build(&id).unwrap());
        assert!(store.set_guardian_skip_auto_build("nope", true).is_err());
    }

    #[test]
    fn auto_build_defaults_none_and_round_trips_both_shapes() {
        let store = Store::open_in_memory().unwrap();
        let id = store.create_guardian("r", "main", "/repo").unwrap();
        assert!(store.get_guardian(&id).unwrap().auto_build.is_none());
        assert!(store.guardian_auto_build(&id).unwrap().is_none());

        let command_build = GuardianAutoBuild {
            command: Some("make build".to_string()),
            prompt: None,
            system_prompt: None,
            system_prompt_position: None,
            agent: None,
            model: None,
        };
        store
            .set_guardian_auto_build(&id, Some(&command_build))
            .unwrap();
        assert_eq!(
            store.get_guardian(&id).unwrap().auto_build,
            Some(command_build.clone())
        );
        assert_eq!(store.guardian_auto_build(&id).unwrap(), Some(command_build));

        let agent_build = GuardianAutoBuild {
            command: None,
            prompt: Some("build the project".to_string()),
            system_prompt: Some("you are a build agent".to_string()),
            system_prompt_position: Some("append".to_string()),
            agent: Some("claude".to_string()),
            model: Some("sonnet".to_string()),
        };
        store
            .set_guardian_auto_build(&id, Some(&agent_build))
            .unwrap();
        assert_eq!(
            store.get_guardian(&id).unwrap().auto_build,
            Some(agent_build)
        );

        store.set_guardian_auto_build(&id, None).unwrap();
        assert!(store.get_guardian(&id).unwrap().auto_build.is_none());

        assert!(store.set_guardian_auto_build("nope", None).is_err());
    }

    #[test]
    fn legacy_skip_worktree_checks_column_migrates_to_nothing_scope_at_read_time() {
        // RAL-285: `skip_worktree_checks` is retired -- no code can set this
        // column anymore -- but a row persisted before this change may still
        // have it set with no explicit `proof_scope`. Simulate that with a raw
        // write (the only way to reach this state now) and confirm the
        // read-time migration resolves `effective_proof_scope` to "nothing".
        let store = Store::open_in_memory().unwrap();
        let id = store.create_guardian("r", "main", "/repo").unwrap();
        store
            .conn
            .execute(
                "UPDATE guardians SET skip_worktree_checks=1 WHERE id=?",
                params![id],
            )
            .unwrap();
        let g = store.get_guardian(&id).unwrap();
        assert_eq!(g.proof_scope, None);
        assert_eq!(g.effective_proof_scope, "nothing");
        // Independent axis: the legacy flag must not affect skip_auto_build
        // (RAL-110 split the old single skip_checks flag).
        assert!(!g.skip_auto_build);

        // An explicit `proof_scope` override takes precedence over the legacy flag.
        store
            .set_guardian_proof_scope(&id, Some("each_branch"))
            .unwrap();
        let g = store.get_guardian(&id).unwrap();
        assert_eq!(g.effective_proof_scope, "each_branch");
    }

    #[test]
    fn proof_scope_defaults_to_inherited_each_branch_and_toggles_independently() {
        let store = Store::open_in_memory().unwrap();
        let id = store.create_guardian("r", "main", "/repo").unwrap();
        let g = store.get_guardian(&id).unwrap();
        assert_eq!(g.proof_scope, None);
        assert_eq!(g.effective_proof_scope, "each_branch");
        assert!(!g.effective_proof_skip_auto_clean);

        store
            .set_guardian_proof_scope(&id, Some("final_branch"))
            .unwrap();
        let g = store.get_guardian(&id).unwrap();
        assert_eq!(g.proof_scope.as_deref(), Some("final_branch"));
        assert_eq!(g.effective_proof_scope, "final_branch");

        store
            .set_guardian_proof_skip_auto_clean(&id, Some(true))
            .unwrap();
        let g = store.get_guardian(&id).unwrap();
        assert_eq!(g.proof_skip_auto_clean, Some(true));
        assert!(g.effective_proof_skip_auto_clean);
        // Independent axis: toggling skip_auto_clean must not affect the scope.
        assert_eq!(g.effective_proof_scope, "final_branch");

        // Resetting back to None restores "inherit the project default".
        store.set_guardian_proof_scope(&id, None).unwrap();
        let g = store.get_guardian(&id).unwrap();
        assert_eq!(g.proof_scope, None);
        assert_eq!(g.effective_proof_scope, "each_branch");

        assert!(
            store
                .set_guardian_proof_scope("nope", Some("nothing"))
                .is_err()
        );
        assert!(
            store
                .set_guardian_proof_skip_auto_clean("nope", Some(true))
                .is_err()
        );
    }

    #[test]
    fn skip_base_updates_defaults_off_and_toggles_independently() {
        let store = Store::open_in_memory().unwrap();
        let id = store.create_guardian("r", "main", "/repo").unwrap();
        let g = store.get_guardian(&id).unwrap();
        assert_eq!(g.skip_base_updates, None);
        assert!(
            !g.effective_skip_base_updates,
            "defaults to auto-update on (no skip)"
        );

        store
            .set_guardian_skip_base_updates(&id, Some(true))
            .unwrap();
        let g = store.get_guardian(&id).unwrap();
        assert_eq!(g.skip_base_updates, Some(true));
        assert!(g.effective_skip_base_updates);

        // Resetting back to None restores "inherit the project/global default".
        store.set_guardian_skip_base_updates(&id, None).unwrap();
        let g = store.get_guardian(&id).unwrap();
        assert_eq!(g.skip_base_updates, None);
        assert!(!g.effective_skip_base_updates);

        assert!(
            store
                .set_guardian_skip_base_updates("nope", Some(true))
                .is_err()
        );
    }

    #[test]
    fn match_pr_branch_name_is_stamped_false_at_creation_when_no_project_default() {
        // Unlike `skip_base_updates` (left `NULL`/"inherit" forever), a new
        // review stamps a concrete value from the owning project's effective
        // default at creation time (RAL-307) -- with no registered project,
        // that resolves to the live global default, `false`.
        let store = Store::open_in_memory().unwrap();
        let id = store.create_guardian("r", "main", "/repo").unwrap();
        let g = store.get_guardian(&id).unwrap();
        assert_eq!(g.match_pr_branch_name, Some(false));
        assert!(!g.effective_match_pr_branch_name);
    }

    #[test]
    fn match_pr_branch_name_stamps_the_owning_projects_effective_default() {
        let store = Store::open_in_memory().unwrap();
        store
            .register_project_ex("proj", "", "/repo", "git", Some(true))
            .unwrap();
        let id = store.create_guardian("r", "main", "/repo").unwrap();
        let g = store.get_guardian(&id).unwrap();
        assert_eq!(
            g.match_pr_branch_name,
            Some(true),
            "the project's stamped default is frozen onto the new review"
        );
        assert!(g.effective_match_pr_branch_name);
    }

    #[test]
    fn auto_submit_pr_stack_is_stamped_false_at_creation_when_no_project_default() {
        // Same creation-time stamping shape as `match_pr_branch_name` (RAL-307)
        // above -- a concrete value from the owning project's effective
        // default, not a perpetual "inherit" `NULL` -- with no registered
        // project, that resolves to the live global default, `false`.
        let store = Store::open_in_memory().unwrap();
        let id = store.create_guardian("r", "main", "/repo").unwrap();
        let g = store.get_guardian(&id).unwrap();
        assert_eq!(g.auto_submit_pr_stack, Some(false));
        assert!(!g.effective_auto_submit_pr_stack);
    }

    #[test]
    fn auto_submit_pr_stack_toggles_independently_per_review() {
        let store = Store::open_in_memory().unwrap();
        let id = store.create_guardian("r", "main", "/repo").unwrap();
        store
            .set_guardian_auto_submit_pr_stack(&id, Some(true))
            .unwrap();
        let g = store.get_guardian(&id).unwrap();
        assert_eq!(g.auto_submit_pr_stack, Some(true));
        assert!(g.effective_auto_submit_pr_stack);

        // Resetting back to None restores "inherit the project/global default".
        store.set_guardian_auto_submit_pr_stack(&id, None).unwrap();
        let g = store.get_guardian(&id).unwrap();
        assert_eq!(g.auto_submit_pr_stack, None);
        assert!(!g.effective_auto_submit_pr_stack);

        assert!(
            store
                .set_guardian_auto_submit_pr_stack("nope", Some(true))
                .is_err()
        );
    }

    #[test]
    fn match_pr_branch_name_toggles_independently_per_review() {
        let store = Store::open_in_memory().unwrap();
        let id = store.create_guardian("r", "main", "/repo").unwrap();
        store
            .set_guardian_match_pr_branch_name(&id, Some(true))
            .unwrap();
        let g = store.get_guardian(&id).unwrap();
        assert_eq!(g.match_pr_branch_name, Some(true));
        assert!(g.effective_match_pr_branch_name);

        // Resetting back to None restores "inherit the project/global default".
        store.set_guardian_match_pr_branch_name(&id, None).unwrap();
        let g = store.get_guardian(&id).unwrap();
        assert_eq!(g.match_pr_branch_name, None);
        assert!(!g.effective_match_pr_branch_name);

        assert!(
            store
                .set_guardian_match_pr_branch_name("nope", Some(true))
                .is_err()
        );
    }

    // ── RAL-378: separate_pr_branch + sticky review-branch names ───────────

    #[test]
    fn separate_pr_branch_is_stamped_false_at_creation() {
        // Same concrete-value-from-the-start stamping shape as
        // `match_pr_branch_name`: with no registered project it resolves to
        // the live global default, `false` -- the PR branch and the review
        // branch are one and the same.
        let store = Store::open_in_memory().unwrap();
        let id = store.create_guardian("r", "main", "/repo").unwrap();
        let g = store.get_guardian(&id).unwrap();
        assert_eq!(g.separate_pr_branch, Some(false));
        assert!(!g.effective_separate_pr_branch);
    }

    #[test]
    fn separate_pr_branch_toggles_independently_per_review() {
        let store = Store::open_in_memory().unwrap();
        let id = store.create_guardian("r", "main", "/repo").unwrap();
        store
            .set_guardian_separate_pr_branch(&id, Some(true))
            .unwrap();
        let g = store.get_guardian(&id).unwrap();
        assert_eq!(g.separate_pr_branch, Some(true));
        assert!(g.effective_separate_pr_branch);

        // Resetting back to None restores "inherit the project/global default".
        store.set_guardian_separate_pr_branch(&id, None).unwrap();
        let g = store.get_guardian(&id).unwrap();
        assert_eq!(g.separate_pr_branch, None);
        assert!(!g.effective_separate_pr_branch);

        assert!(
            store
                .set_guardian_separate_pr_branch("nope", Some(true))
                .is_err()
        );
    }

    #[test]
    fn a_newly_registered_branch_opts_into_readable_naming_with_no_name_yet() {
        let store = Store::open_in_memory().unwrap();
        let id = store.create_guardian("r", "main", "/repo").unwrap();
        store.add_guardian_branch(&id, "feature-a").unwrap();
        let g = store.get_guardian(&id).unwrap();
        assert!(g.branches[0].readable_review_branch);
        // Unresolved until the branch's first build claims a name.
        assert_eq!(g.branches[0].review_branch_name, None);
        assert!(g.readable_review_branch);
        assert_eq!(g.review_branch_name, None);
    }

    #[test]
    fn a_claimed_review_branch_name_survives_a_branch_reset() {
        // The whole point of the separate column: the reset paths clear
        // `review_branch`, but re-resolving the *name* on every rebuild would
        // walk the collision suffix forward and orphan an open PR.
        let store = Store::open_in_memory().unwrap();
        let id = store.create_guardian("r", "main", "/repo").unwrap();
        store.add_guardian_branch(&id, "feature-a").unwrap();
        let bid = store.get_guardian(&id).unwrap().branches[0].id.clone();
        store
            .set_branch_review_branch_name(&id, &bid, "feature-a-review")
            .unwrap();
        store
            .set_branch_review(&id, &bid, "feature-a-review", "/wt")
            .unwrap();

        store.reset_all_enabled_branches_to_pending(&id).unwrap();

        let g = store.get_guardian(&id).unwrap();
        assert_eq!(g.branches[0].review_branch, None);
        assert_eq!(
            g.branches[0].review_branch_name.as_deref(),
            Some("feature-a-review")
        );
    }

    #[test]
    fn review_branch_name_taken_sees_names_claimed_by_other_branches() {
        let store = Store::open_in_memory().unwrap();
        let id = store.create_guardian("r", "main", "/repo").unwrap();
        store.add_guardian_branch(&id, "feature-a").unwrap();
        store.add_guardian_branch(&id, "feature-b").unwrap();
        let branches = store.get_guardian(&id).unwrap().branches;
        let (a, b) = (branches[0].id.clone(), branches[1].id.clone());
        store
            .set_branch_review_branch_name(&id, &a, "shared-review")
            .unwrap();

        assert!(
            store
                .review_branch_name_taken("/repo", "shared-review", Some((&id, &b)))
                .unwrap()
        );
        // A branch never collides with its own prior claim.
        assert!(
            !store
                .review_branch_name_taken("/repo", "shared-review", Some((&id, &a)))
                .unwrap()
        );
        // A different repo is a different namespace.
        assert!(
            !store
                .review_branch_name_taken("/other", "shared-review", Some((&id, &b)))
                .unwrap()
        );
        // Trailing separators on either side don't split the namespace.
        assert!(
            store
                .review_branch_name_taken("/repo/", "shared-review", Some((&id, &b)))
                .unwrap()
        );
    }

    #[test]
    fn review_branch_name_taken_sees_a_combined_reviews_name() {
        let store = Store::open_in_memory().unwrap();
        let owner = store.create_guardian("owner", "main", "/repo").unwrap();
        store
            .set_guardian_review_branch_name(&owner, "owner-review")
            .unwrap();
        assert!(
            store
                .review_branch_name_taken("/repo", "owner-review", None)
                .unwrap()
        );
    }

    #[test]
    fn review_branch_name_taken_sees_an_open_prs_remote_alias() {
        // With `separate_pr_branch` off the review branch is pushed under its
        // own name, so a name another review already has a PR on would be
        // force-pushed over.
        let store = Store::open_in_memory().unwrap();
        let other = store.create_guardian("other", "main", "/repo").unwrap();
        store
            .create_pull_request(
                &other,
                None,
                "github",
                "acme/widget",
                "feature-a-review",
                "main",
                "t",
                "d",
                Some(7),
                Some("https://example.invalid/pr/7"),
            )
            .unwrap();
        let mine = store.create_guardian("mine", "main", "/repo").unwrap();
        store.add_guardian_branch(&mine, "feature-a").unwrap();
        let bid = store.get_guardian(&mine).unwrap().branches[0].id.clone();
        assert!(
            store
                .review_branch_name_taken("/repo", "feature-a-review", Some((&mine, &bid)))
                .unwrap()
        );
    }

    #[test]
    fn proof_scope_unrecognized_stored_value_falls_back_to_each_branch() {
        // Defense in depth: a value that somehow got into the DB outside the
        // three recognized scopes (manual SQL edit, future rollback) must not
        // silently disable proving.
        let store = Store::open_in_memory().unwrap();
        let id = store.create_guardian("r", "main", "/repo").unwrap();
        store.set_guardian_proof_scope(&id, Some("bogus")).unwrap();
        let g = store.get_guardian(&id).unwrap();
        assert_eq!(g.effective_proof_scope, "each_branch");
    }

    #[test]
    fn missing_guardian_is_not_found() {
        let store = Store::open_in_memory().unwrap();
        assert!(matches!(
            store.get_guardian("nope"),
            Err(StoreError::NotFound)
        ));
        assert!(store.add_guardian_branch("nope", "x").is_err());
    }

    #[test]
    fn reorder_branches_permutes_positions() {
        let mut store = Store::open_in_memory().unwrap();
        let id = store.create_guardian("r", "main", "/repo").unwrap();
        store.add_guardian_branch(&id, "a").unwrap();
        store.add_guardian_branch(&id, "b").unwrap();
        store.add_guardian_branch(&id, "c").unwrap();

        store
            .reorder_guardian_branches(&id, &["c".into(), "a".into(), "b".into()])
            .unwrap();
        let g = store.get_guardian(&id).unwrap();
        assert_eq!(
            g.branches
                .iter()
                .map(|b| b.branch.as_str())
                .collect::<Vec<_>>(),
            vec!["c", "a", "b"]
        );
        assert_eq!(g.branches[0].position, 0);
        assert_eq!(g.branches[2].position, 2);
    }

    #[test]
    fn branch_id_survives_reorder_unlike_position() {
        // RAL-122: position is a mutable display-order attribute; id is the
        // stable addressing key. Record each branch's id before a reorder
        // shuffles positions, then prove every id still resolves (via a
        // position-keyed Store call would silently now hit a DIFFERENT
        // branch) to the SAME branch it identified before the reorder.
        let mut store = Store::open_in_memory().unwrap();
        let id = store.create_guardian("r", "main", "/repo").unwrap();
        store.add_guardian_branch(&id, "a").unwrap();
        store.add_guardian_branch(&id, "b").unwrap();
        store.add_guardian_branch(&id, "c").unwrap();

        let before = store.get_guardian(&id).unwrap();
        let bid_a = before.branches[0].id.clone();
        let bid_b = before.branches[1].id.clone();
        let bid_c = before.branches[2].id.clone();
        assert_eq!(before.branches[0].branch, "a");
        assert_eq!(before.branches[1].branch, "b");
        assert_eq!(before.branches[2].branch, "c");

        // Tag each branch's worktree with its own name before the reorder, so a
        // later `get_branch_worktree` lookup can prove which branch an id
        // actually resolves to.
        store
            .set_branch_review(&id, &bid_a, "review/a", "/wt/a")
            .unwrap();
        store
            .set_branch_review(&id, &bid_b, "review/b", "/wt/b")
            .unwrap();
        store
            .set_branch_review(&id, &bid_c, "review/c", "/wt/c")
            .unwrap();

        // Shuffle: c, a, b -- every branch's position changes.
        store
            .reorder_guardian_branches(&id, &["c".into(), "a".into(), "b".into()])
            .unwrap();
        let after = store.get_guardian(&id).unwrap();
        assert_eq!(after.branches[0].branch, "c");
        assert_eq!(after.branches[1].branch, "a");
        assert_eq!(after.branches[2].branch, "b");
        // Every branch's position moved except "a", which stayed in the middle
        // by coincidence of the chosen permutation -- assert the two that
        // definitely moved, to make the regression meaningful.
        assert_eq!(
            after
                .branches
                .iter()
                .find(|b| b.id == bid_c)
                .unwrap()
                .position,
            0
        );
        assert_eq!(
            after
                .branches
                .iter()
                .find(|b| b.id == bid_b)
                .unwrap()
                .position,
            2
        );

        // Each original id still resolves to the SAME branch's worktree as
        // before the reorder, regardless of its new position.
        assert_eq!(
            store.get_branch_worktree(&id, &bid_a).unwrap().as_deref(),
            Some("/wt/a")
        );
        assert_eq!(
            store.get_branch_worktree(&id, &bid_b).unwrap().as_deref(),
            Some("/wt/b")
        );
        assert_eq!(
            store.get_branch_worktree(&id, &bid_c).unwrap().as_deref(),
            Some("/wt/c")
        );
    }

    #[test]
    fn reorder_ignores_unknown_and_keeps_leftovers() {
        let mut store = Store::open_in_memory().unwrap();
        let id = store.create_guardian("r", "main", "/repo").unwrap();
        store.add_guardian_branch(&id, "a").unwrap();
        store.add_guardian_branch(&id, "b").unwrap();
        // Only name "b"; "a" is a leftover and "zzz" does not exist.
        store
            .reorder_guardian_branches(&id, &["b".into(), "zzz".into()])
            .unwrap();
        let g = store.get_guardian(&id).unwrap();
        assert_eq!(
            g.branches
                .iter()
                .map(|b| b.branch.as_str())
                .collect::<Vec<_>>(),
            vec!["b", "a"]
        );
    }

    #[test]
    fn move_guardian_branch_appends_to_destination_and_compacts_source() {
        let mut store = Store::open_in_memory().unwrap();
        let src = store.create_guardian("r1", "main", "/repo").unwrap();
        let dst = store.create_guardian("r2", "main", "/repo").unwrap();
        store.add_guardian_branch(&src, "a").unwrap();
        store.add_guardian_branch(&src, "b").unwrap();
        store.add_guardian_branch(&src, "c").unwrap();
        store.add_guardian_branch(&dst, "x").unwrap();
        let bid_b = store.get_guardian(&src).unwrap().branches[1].id.clone();

        // Move the middle branch ("b", position 1) out of the source stack.
        let new_pos = store.move_guardian_branch(&src, &bid_b, &dst).unwrap();
        assert_eq!(new_pos, 1); // appended after dst's existing "x" at position 0

        let source_view = store.get_guardian(&src).unwrap();
        assert_eq!(
            source_view
                .branches
                .iter()
                .map(|b| (b.position, b.branch.as_str()))
                .collect::<Vec<_>>(),
            vec![(0, "a"), (1, "c")],
            "source stack must renumber contiguously after the branch leaves"
        );

        let dest_view = store.get_guardian(&dst).unwrap();
        assert_eq!(
            dest_view
                .branches
                .iter()
                .map(|b| (b.position, b.branch.as_str()))
                .collect::<Vec<_>>(),
            vec![(0, "x"), (1, "b")]
        );
        let moved = &dest_view.branches[1];
        assert_eq!(moved.moved_from_guardian_id.as_deref(), Some(src.as_str()));
        assert_eq!(moved.merge_status, "pending");
    }

    #[test]
    fn move_guardian_branch_resets_stale_review_state() {
        let mut store = Store::open_in_memory().unwrap();
        let src = store.create_guardian("r1", "main", "/repo").unwrap();
        let dst = store.create_guardian("r2", "main", "/repo").unwrap();
        store.add_guardian_branch(&src, "a").unwrap();
        let bid = store.get_guardian(&src).unwrap().branches[0].id.clone();
        store
            .set_branch_review(&src, &bid, "refs/ralphus/review/r1/a", "/wt/a")
            .unwrap();
        store
            .set_branch_status(&src, &bid, MergeStatus::Done, None)
            .unwrap();

        store.move_guardian_branch(&src, &bid, &dst).unwrap();

        let moved = &store.get_guardian(&dst).unwrap().branches[0];
        assert_eq!(moved.merge_status, "pending");
        assert!(moved.review_branch.is_none());
        assert!(moved.worktree.is_none());
    }

    #[test]
    fn move_guardian_branch_preserves_original_provenance_across_repeated_moves() {
        let mut store = Store::open_in_memory().unwrap();
        let a = store.create_guardian("a", "main", "/repo").unwrap();
        let b = store.create_guardian("b", "main", "/repo").unwrap();
        let c = store.create_guardian("c", "main", "/repo").unwrap();
        store.add_guardian_branch(&a, "feat").unwrap();
        let bid = store.get_guardian(&a).unwrap().branches[0].id.clone();

        store.move_guardian_branch(&a, &bid, &b).unwrap();
        let after_first = &store.get_guardian(&b).unwrap().branches[0];
        assert_eq!(
            after_first.moved_from_guardian_id.as_deref(),
            Some(a.as_str())
        );
        // The branch id must be carried forward across the move, unlike position
        // (RAL-122) -- otherwise a second move couldn't address it.
        assert_eq!(after_first.id, bid);

        // A second move (b -> c) must still point back to the *original* owner (a),
        // not the intermediate one (b) -- RAL-118 Q2 provenance pointer.
        store.move_guardian_branch(&b, &bid, &c).unwrap();
        let after_second = &store.get_guardian(&c).unwrap().branches[0];
        assert_eq!(
            after_second.moved_from_guardian_id.as_deref(),
            Some(a.as_str())
        );
    }

    #[test]
    fn move_guardian_branch_rejects_same_guardian() {
        let mut store = Store::open_in_memory().unwrap();
        let id = store.create_guardian("r", "main", "/repo").unwrap();
        store.add_guardian_branch(&id, "a").unwrap();
        let bid = store.get_guardian(&id).unwrap().branches[0].id.clone();
        assert!(matches!(
            store.move_guardian_branch(&id, &bid, &id),
            Err(StoreError::InvalidTransition(_))
        ));
    }

    #[test]
    fn move_guardian_branch_rejects_across_different_git_roots() {
        let mut store = Store::open_in_memory().unwrap();
        let src = store.create_guardian("r1", "main", "/repo-a").unwrap();
        let dst = store.create_guardian("r2", "main", "/repo-b").unwrap();
        store.add_guardian_branch(&src, "a").unwrap();
        let bid = store.get_guardian(&src).unwrap().branches[0].id.clone();
        assert!(matches!(
            store.move_guardian_branch(&src, &bid, &dst),
            Err(StoreError::InvalidTransition(_))
        ));
    }

    #[test]
    fn move_guardian_branch_rejects_unknown_guardian_or_position() {
        let mut store = Store::open_in_memory().unwrap();
        let src = store.create_guardian("r1", "main", "/repo").unwrap();
        let dst = store.create_guardian("r2", "main", "/repo").unwrap();
        store.add_guardian_branch(&src, "a").unwrap();
        let bid = store.get_guardian(&src).unwrap().branches[0].id.clone();

        assert!(matches!(
            store.move_guardian_branch("nope", &bid, &dst),
            Err(StoreError::NotFound)
        ));
        assert!(matches!(
            store.move_guardian_branch(&src, &bid, "nope"),
            Err(StoreError::NotFound)
        ));
        assert!(matches!(
            store.move_guardian_branch(&src, "nope", &dst),
            Err(StoreError::NotFound)
        ));
    }

    #[test]
    fn move_guardian_branch_blocked_while_source_or_destination_is_merging() {
        let mut store = Store::open_in_memory().unwrap();
        let src = store.create_guardian("r1", "main", "/repo").unwrap();
        let dst = store.create_guardian("r2", "main", "/repo").unwrap();
        store.add_guardian_branch(&src, "a").unwrap();
        store.add_guardian_branch(&dst, "b").unwrap();
        let bid = store.get_guardian(&src).unwrap().branches[0].id.clone();

        // RAL-118 Q4: blocked while the source has an active merge/rebase.
        assert!(store.claim_guardian_merge(&src).unwrap());
        assert!(matches!(
            store.move_guardian_branch(&src, &bid, &dst),
            Err(StoreError::InvalidTransition(_))
        ));
        store
            .set_guardian_status(&src, GuardianStatus::Collecting, None)
            .unwrap();

        // ...and while the destination has one.
        assert!(store.claim_guardian_merge(&dst).unwrap());
        assert!(matches!(
            store.move_guardian_branch(&src, &bid, &dst),
            Err(StoreError::InvalidTransition(_))
        ));
        store
            .set_guardian_status(&dst, GuardianStatus::Collecting, None)
            .unwrap();

        // Now that neither is merging, the move succeeds.
        assert!(store.move_guardian_branch(&src, &bid, &dst).is_ok());
    }

    #[test]
    fn ordered_branches() {
        let store = Store::open_in_memory().unwrap();
        let id = store.create_guardian("r", "main", "/repo").unwrap();
        store.add_guardian_branch(&id, "a").unwrap();
        store.add_guardian_branch(&id, "b").unwrap();
        let ordered = store.guardian_branches(&id).unwrap();
        assert_eq!(
            ordered
                .iter()
                .map(|b| b.branch.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "b"]
        );
    }

    #[test]
    fn branch_enabled_defaults_true_and_toggles() {
        // RAL-43: branches start enabled; can be disabled and re-enabled.
        let store = Store::open_in_memory().unwrap();
        let id = store.create_guardian("r", "main", "/repo").unwrap();
        store.add_guardian_branch(&id, "a").unwrap();
        store.add_guardian_branch(&id, "b").unwrap();

        let g = store.get_guardian(&id).unwrap();
        assert!(g.branches[0].enabled, "default enabled");
        assert!(g.branches[1].enabled, "default enabled");

        store.set_branch_enabled_by_name(&id, "a", false).unwrap();
        let g = store.get_guardian(&id).unwrap();
        assert!(!g.branches[0].enabled, "a disabled");
        assert!(g.branches[1].enabled, "b still enabled");

        // guardian_branches also reflects the enabled flag.
        let ordered = store.guardian_branches(&id).unwrap();
        assert!(!ordered[0].enabled);
        assert!(ordered[1].enabled);

        // Re-enable a.
        store.set_branch_enabled_by_name(&id, "a", true).unwrap();
        assert!(store.get_guardian(&id).unwrap().branches[0].enabled);
    }

    #[test]
    fn reset_branch_to_pending_clears_review_fields() {
        // RAL-43: reset_branch_to_pending clears review_branch, worktree, and
        // resolver_agent_session_id so Watch Live never points at a stale session.
        let store = Store::open_in_memory().unwrap();
        let id = store.create_guardian("r", "main", "/repo").unwrap();
        store.add_guardian_branch(&id, "feat").unwrap();
        let bid = store.get_guardian(&id).unwrap().branches[0].id.clone();
        store
            .set_branch_review(&id, &bid, "guardian/1/b000", "/some/worktree")
            .unwrap();
        store
            .set_branch_status(&id, &bid, MergeStatus::Done, None)
            .unwrap();
        store
            .set_branch_resolver_session_id(&id, &bid, "old-session-123")
            .unwrap();

        store.reset_branch_to_pending(&id, &bid).unwrap();
        let g = store.get_guardian(&id).unwrap();
        assert_eq!(g.branches[0].merge_status, "pending");
        assert!(g.branches[0].review_branch.is_none());
        assert!(g.branches[0].worktree.is_none());
        assert!(
            g.branches[0].resolver_agent_session_id.is_none(),
            "session ID must be cleared so Watch Live doesn't point at a stale session"
        );
    }

    #[test]
    fn reset_all_branches_clears_resolver_session_id() {
        // Regression: bulk reset must clear resolver_agent_session_id so that
        // pressing Merge/Rebase (or a base-branch auto-rebuild) doesn't leave
        // Watch Live pointing at the previous conflict-resolution session.
        let store = Store::open_in_memory().unwrap();
        let id = store.create_guardian("r", "main", "/repo").unwrap();
        store.add_guardian_branch(&id, "a").unwrap();
        let bid = store.get_guardian(&id).unwrap().branches[0].id.clone();
        store
            .set_branch_resolver_session_id(&id, &bid, "session-from-last-run")
            .unwrap();
        store
            .set_branch_status(&id, &bid, MergeStatus::Done, None)
            .unwrap();

        store.reset_all_enabled_branches_to_pending(&id).unwrap();
        let g = store.get_guardian(&id).unwrap();
        assert!(
            g.branches[0].resolver_agent_session_id.is_none(),
            "session ID must be cleared on bulk reset"
        );
    }

    #[test]
    fn set_branch_enabled_by_name_ignores_unknown_branch() {
        // RAL-43: unknown branch names in the enabled map are silently ignored.
        let store = Store::open_in_memory().unwrap();
        let id = store.create_guardian("r", "main", "/repo").unwrap();
        store.add_guardian_branch(&id, "a").unwrap();
        // Should not error on an unknown branch.
        store
            .set_branch_enabled_by_name(&id, "does-not-exist", false)
            .unwrap();
        // The existing branch is unaffected.
        assert!(store.get_guardian(&id).unwrap().branches[0].enabled);
    }

    #[test]
    fn reset_all_enabled_branches_to_pending_skips_disabled() {
        // RAL-54: bulk reset clears enabled branches; disabled branches are
        // untouched (they are handled separately in run_merge via reset_branch_to_pending).
        let store = Store::open_in_memory().unwrap();
        let id = store.create_guardian("r", "main", "/repo").unwrap();
        store.add_guardian_branch(&id, "a").unwrap();
        store.add_guardian_branch(&id, "b").unwrap();
        store.add_guardian_branch(&id, "c").unwrap();

        // Move all to Done and set review fields to simulate a prior build.
        let branches = store.get_guardian(&id).unwrap().branches;
        for (pos, branch) in branches.iter().enumerate() {
            store
                .set_branch_review(&id, &branch.id, &format!("review/b{pos:03}"), "/wt")
                .unwrap();
            store
                .set_branch_status(&id, &branch.id, MergeStatus::Done, Some("clean"))
                .unwrap();
        }

        // Disable branch 1.
        store.set_branch_enabled_by_name(&id, "b", false).unwrap();

        store.reset_all_enabled_branches_to_pending(&id).unwrap();

        let g = store.get_guardian(&id).unwrap();
        // Enabled branches (a and c) are reset.
        assert_eq!(g.branches[0].merge_status, "pending");
        assert!(g.branches[0].review_branch.is_none());
        assert!(g.branches[0].worktree.is_none());
        assert_eq!(g.branches[2].merge_status, "pending");
        // Disabled branch (b) is untouched.
        assert_eq!(g.branches[1].merge_status, "done");
        assert!(g.branches[1].review_branch.is_some());
    }

    #[test]
    fn mark_branches_ready_transitions_pending_to_ready() {
        // RAL-73: mark_guardian_branches_ready flips pending → ready; other
        // states (in_progress, done, failed) are not touched.
        let store = Store::open_in_memory().unwrap();
        let id = store.create_guardian("r", "main", "/repo").unwrap();
        store.add_guardian_branch(&id, "a").unwrap();
        store.add_guardian_branch(&id, "b").unwrap();
        store.add_guardian_branch(&id, "c").unwrap();
        let bid_b = store.get_guardian(&id).unwrap().branches[1].id.clone();

        // Advance branch b to done to simulate a branch already processed.
        store
            .set_branch_status(&id, &bid_b, MergeStatus::Done, None)
            .unwrap();

        store.mark_guardian_branches_ready(&id).unwrap();

        let g = store.get_guardian(&id).unwrap();
        // pending → ready.
        assert_eq!(g.branches[0].merge_status, "ready");
        // done stays done (only pending is flipped).
        assert_eq!(g.branches[1].merge_status, "done");
        // pending → ready.
        assert_eq!(g.branches[2].merge_status, "ready");
    }

    #[test]
    fn reset_preserves_ready_but_clears_merge_states() {
        // RAL-73: reset_all_enabled_branches_to_pending keeps `ready` branches
        // in place (they are pre-merge, dependency-satisfied) while rolling back
        // in-flight or terminal merge states to `pending`.
        let store = Store::open_in_memory().unwrap();
        let id = store.create_guardian("r", "main", "/repo").unwrap();
        store.add_guardian_branch(&id, "a").unwrap(); // will be ready
        store.add_guardian_branch(&id, "b").unwrap(); // will be done (stale)
        store.add_guardian_branch(&id, "c").unwrap(); // will be pending
        let branches = store.get_guardian(&id).unwrap().branches;
        let bid_a = branches[0].id.clone();
        let bid_b = branches[1].id.clone();

        store
            .set_branch_status(&id, &bid_a, MergeStatus::Ready, None)
            .unwrap();
        store
            .set_branch_review(&id, &bid_b, "review/b001", "/wt")
            .unwrap();
        store
            .set_branch_status(&id, &bid_b, MergeStatus::Done, Some("clean"))
            .unwrap();
        // branch c stays pending (default).

        store.reset_all_enabled_branches_to_pending(&id).unwrap();

        let g = store.get_guardian(&id).unwrap();
        // ready is preserved (RAL-73 dependency signal must survive the reset).
        assert_eq!(g.branches[0].merge_status, "ready");
        // done → pending (stale merge result cleared).
        assert_eq!(g.branches[1].merge_status, "pending");
        assert!(g.branches[1].review_branch.is_none());
        assert!(g.branches[1].worktree.is_none());
        // pending stays pending.
        assert_eq!(g.branches[2].merge_status, "pending");
    }

    // ── RAL-193: guardian cost tracking ──────────────────────────────────

    #[test]
    fn guardian_cost_defaults_to_none_and_zero() {
        let store = Store::open_in_memory().unwrap();
        let id = store.create_guardian("r", "main", "/repo").unwrap();
        let g = store.get_guardian(&id).unwrap();
        assert_eq!(g.maximum_budget_usd, None);
        assert_eq!(g.merge_attempt, 0);
        assert_eq!(g.attempt_tokens_in, 0);
        assert_eq!(g.attempt_tokens_out, 0);
        assert_eq!(g.attempt_cost_usd, 0.0);
        assert_eq!(g.cumulative_tokens_in, 0);
        assert_eq!(g.cumulative_tokens_out, 0);
        assert_eq!(g.cumulative_cost_usd, 0.0);
    }

    // ── RAL-273: guardian notice (GitHub reorder toast) ──────────────────

    #[test]
    fn guardian_notice_defaults_to_none() {
        let store = Store::open_in_memory().unwrap();
        let id = store.create_guardian("r", "main", "/repo").unwrap();
        let g = store.get_guardian(&id).unwrap();
        assert_eq!(g.notice_kind, None);
        assert_eq!(g.notice_message, None);
        assert_eq!(g.notice_at_ms, None);
    }

    #[test]
    fn set_guardian_notice_round_trips_and_overwrites() {
        let store = Store::open_in_memory().unwrap();
        let id = store.create_guardian("r", "main", "/repo").unwrap();
        store
            .set_guardian_notice(
                &id,
                "forge_reorder_interrupted_local",
                "GitHub reorder interrupted your local reorder",
            )
            .unwrap();
        let g = store.get_guardian(&id).unwrap();
        assert_eq!(
            g.notice_kind.as_deref(),
            Some("forge_reorder_interrupted_local")
        );
        assert_eq!(
            g.notice_message.as_deref(),
            Some("GitHub reorder interrupted your local reorder")
        );
        assert!(g.notice_at_ms.is_some());

        // A later notice overwrites the earlier one -- no queue.
        store
            .set_guardian_notice(&id, "other_kind", "a different message")
            .unwrap();
        let g2 = store.get_guardian(&id).unwrap();
        assert_eq!(g2.notice_kind.as_deref(), Some("other_kind"));
        assert_eq!(g2.notice_message.as_deref(), Some("a different message"));
    }

    #[test]
    fn set_guardian_notice_on_missing_guardian_is_not_found() {
        let store = Store::open_in_memory().unwrap();
        let err = store
            .set_guardian_notice("guardian-does-not-exist", "kind", "message")
            .unwrap_err();
        assert!(matches!(err, StoreError::NotFound));
    }

    // ── RAL-277: review/forge base last-write-wins ───────────────────────

    #[test]
    fn forge_base_change_only_wins_when_strictly_newer() {
        let store = Store::open_in_memory().unwrap();
        let id = store.create_guardian("r", "main", "/repo").unwrap();
        store
            .set_guardian_base_branch_at(&id, "local-base", 2_000)
            .unwrap();

        assert!(
            !store
                .set_guardian_base_branch_if_newer(&id, "older-forge", 1_999)
                .unwrap()
        );
        assert!(
            !store
                .set_guardian_base_branch_if_newer(&id, "tied-forge", 2_000)
                .unwrap()
        );
        assert_eq!(store.get_guardian(&id).unwrap().base_branch, "local-base");

        assert!(
            store
                .set_guardian_base_branch_if_newer(&id, "newer-forge", 2_001)
                .unwrap()
        );
        assert_eq!(store.get_guardian(&id).unwrap().base_branch, "newer-forge");
        assert_eq!(store.guardian_base_changed_at_ms(&id).unwrap(), 2_001);
    }

    #[test]
    fn set_guardian_maximum_budget_usd_round_trips() {
        let store = Store::open_in_memory().unwrap();
        let id = store.create_guardian("r", "main", "/repo").unwrap();
        store
            .set_guardian_maximum_budget_usd(&id, Some(2.5))
            .unwrap();
        assert_eq!(store.guardian_maximum_budget_usd(&id).unwrap(), Some(2.5));
        assert_eq!(
            store.get_guardian(&id).unwrap().maximum_budget_usd,
            Some(2.5)
        );
        store.set_guardian_maximum_budget_usd(&id, None).unwrap();
        assert_eq!(store.guardian_maximum_budget_usd(&id).unwrap(), None);
    }

    #[test]
    fn bump_guardian_merge_attempt_increments_and_current_attempt_reads_it() {
        let store = Store::open_in_memory().unwrap();
        let id = store.create_guardian("r", "main", "/repo").unwrap();
        assert_eq!(store.guardian_current_attempt(&id).unwrap(), 0);
        assert_eq!(store.bump_guardian_merge_attempt(&id).unwrap(), 1);
        assert_eq!(store.guardian_current_attempt(&id).unwrap(), 1);
        assert_eq!(store.bump_guardian_merge_attempt(&id).unwrap(), 2);
        assert_eq!(store.guardian_current_attempt(&id).unwrap(), 2);
    }

    #[test]
    fn record_guardian_cost_sums_per_attempt_and_cumulative() {
        // Two calls in attempt 1, one call in attempt 2 -- per-attempt totals
        // must only reflect their own attempt, cumulative must sum all three.
        let store = Store::open_in_memory().unwrap();
        let id = store.create_guardian("r", "main", "/repo").unwrap();
        store.add_guardian_branch(&id, "feat").unwrap();
        let bid = store.get_guardian(&id).unwrap().branches[0].id.clone();

        let attempt1 = store.bump_guardian_merge_attempt(&id).unwrap();
        store
            .record_guardian_cost(&id, Some(&bid), attempt1, "resolve_conflict", 100, 50, 0.01)
            .unwrap();
        store
            .record_guardian_cost(&id, Some(&bid), attempt1, "proof", 30, 10, 0.002)
            .unwrap();
        let attempt2 = store.bump_guardian_merge_attempt(&id).unwrap();
        store
            .record_guardian_cost(&id, Some(&bid), attempt2, "resolve_conflict", 200, 80, 0.02)
            .unwrap();

        let (in1, out1, cost1) = store
            .guardian_cost_total_for_attempt(&id, attempt1)
            .unwrap();
        assert_eq!((in1, out1), (130, 60));
        assert!((cost1 - 0.012).abs() < 1e-9);

        let (in2, out2, cost2) = store
            .guardian_cost_total_for_attempt(&id, attempt2)
            .unwrap();
        assert_eq!((in2, out2), (200, 80));
        assert!((cost2 - 0.02).abs() < 1e-9);

        let (in_all, out_all, cost_all) = store.guardian_cost_total(&id).unwrap();
        assert_eq!((in_all, out_all), (330, 140));
        assert!((cost_all - 0.032).abs() < 1e-9);

        // GuardianView surfaces the current attempt's total plus cumulative.
        let g = store.get_guardian(&id).unwrap();
        assert_eq!(g.merge_attempt, attempt2);
        assert_eq!(g.attempt_tokens_in, 200);
        assert_eq!(g.attempt_tokens_out, 80);
        assert_eq!(g.cumulative_tokens_in, 330);
        assert_eq!(g.cumulative_tokens_out, 140);
    }

    #[test]
    fn guardian_cost_total_for_unknown_guardian_is_zero() {
        let store = Store::open_in_memory().unwrap();
        assert_eq!(
            store.guardian_cost_total("guardian-nope").unwrap(),
            (0, 0, 0.0)
        );
    }

    // ── stale-cell-row regression: a superseded attempt must not block a
    // branch whose current attempt has finished ─────────────────────────

    fn insert_cell_for_branch(store: &mut Store, branch: &str, state: NodeState) -> String {
        let src = "[[task]]\nname=\"t0\"\n[[task.cell]]\nid=\"s0\"\ncwd=\"/r\"\nprompt=\"p\"\n";
        let tf: ralphus_core::schema::TaskFile = toml::from_str(src).expect("valid fixture");
        let squad_id = store.insert_squad(&tf, None, false).unwrap();
        store
            .set_cell_review_branch(&squad_id, 0, 0, branch)
            .unwrap();
        store.set_cell_state(&squad_id, 0, 0, state).unwrap();
        squad_id
    }

    #[test]
    fn guardian_unfinished_linked_branches_ignores_a_stale_superseded_cell_row() {
        // Same scenario the merge-deferral bug hit live (RAL-295): a branch
        // gets a failed cell from an earlier attempt, then a fresh attempt
        // (a new squad reusing the same branch name) finishes `done`. Only
        // the *latest* cell should decide readiness -- the old failed row
        // must not haunt the branch forever.
        let mut store = Store::open_in_memory().unwrap();
        insert_cell_for_branch(&mut store, "feat", NodeState::Failed);
        insert_cell_for_branch(&mut store, "feat", NodeState::Done);

        let id = store.create_guardian("r", "main", "/repo").unwrap();
        store.add_guardian_branch(&id, "feat").unwrap();

        assert_eq!(
            store.guardian_unfinished_linked_branches(&id).unwrap(),
            Vec::<String>::new(),
            "the branch's latest cell is done -- the earlier attempt's failed \
             row must not count against it"
        );
    }

    #[test]
    fn guardian_unfinished_linked_branches_still_blocks_on_a_genuinely_unfinished_latest_cell() {
        // Guards the fix above from over-correcting: if the *latest* cell for
        // a branch is not done, it must still be reported, even though an
        // earlier attempt at the same branch name happened to succeed.
        let mut store = Store::open_in_memory().unwrap();
        insert_cell_for_branch(&mut store, "feat", NodeState::Done);
        insert_cell_for_branch(&mut store, "feat", NodeState::Running);

        let id = store.create_guardian("r", "main", "/repo").unwrap();
        store.add_guardian_branch(&id, "feat").unwrap();

        assert_eq!(
            store.guardian_unfinished_linked_branches(&id).unwrap(),
            vec!["feat".to_string()]
        );
    }

    #[test]
    fn collecting_guardians_ready_ignores_a_stale_superseded_cell_row() {
        let mut store = Store::open_in_memory().unwrap();
        insert_cell_for_branch(&mut store, "feat", NodeState::Failed);
        insert_cell_for_branch(&mut store, "feat", NodeState::Done);

        let id = store.create_guardian("r", "main", "/repo").unwrap();
        store.add_guardian_branch(&id, "feat").unwrap();

        assert_eq!(store.collecting_guardians_ready().unwrap(), vec![id]);
    }

    // ── RAL-280: dispatch-priority signal for the scheduler ──────────────────

    const TWO_TASKS: &str = "[[task]]\nname=\"a\"\n[[task.cell]]\ncwd=\".\"\ncommand=\"x\"\n\
                              [[task]]\nname=\"b\"\n[[task.cell]]\ncwd=\".\"\ncommand=\"y\"\n";

    #[test]
    fn cell_review_dispatch_priority_is_none_without_a_review_branch() {
        let mut store = Store::open_in_memory().unwrap();
        let tf: ralphus_core::schema::TaskFile = toml::from_str(TWO_TASKS).unwrap();
        let squad = store.insert_squad(&tf, Some("r"), false).unwrap();
        assert_eq!(
            store.cell_review_dispatch_priority(&squad, 0, 0).unwrap(),
            None
        );
    }

    #[test]
    fn cell_review_dispatch_priority_favors_the_first_unfinished_branch_in_position_order() {
        // Two branches, stacked in position order "a" then "b", each fed by a
        // different task's single cell.
        let mut store = Store::open_in_memory().unwrap();
        let tf: ralphus_core::schema::TaskFile = toml::from_str(TWO_TASKS).unwrap();
        let squad = store.insert_squad(&tf, Some("r"), false).unwrap();
        let gid = store
            .create_guardian_for_squad("Stack", "main", "/repo", Some(&squad))
            .unwrap();
        store.add_guardian_branch(&gid, "a").unwrap();
        store.add_guardian_branch(&gid, "b").unwrap();
        store.set_cell_review_branch(&squad, 0, 0, "a").unwrap();
        store.set_cell_review_branch(&squad, 1, 0, "b").unwrap();

        // "a" has no earlier branch, so it's already first-in-line: priority
        // boost equal to the stack's 2 enabled branches.
        assert_eq!(
            store.cell_review_dispatch_priority(&squad, 0, 0).unwrap(),
            Some(2)
        );
        // "b" is blocked behind "a", whose cell hasn't finished yet -- no boost.
        assert_eq!(
            store.cell_review_dispatch_priority(&squad, 1, 0).unwrap(),
            None
        );

        // Once "a"'s cell finishes, "b" becomes the first not-yet-contributed
        // branch and picks up the same boost.
        store
            .set_cell_state(&squad, 0, 0, crate::store::NodeState::Done)
            .unwrap();
        assert_eq!(
            store.cell_review_dispatch_priority(&squad, 1, 0).unwrap(),
            Some(2)
        );
    }

    #[test]
    fn cell_review_dispatch_priority_skips_disabled_earlier_branches() {
        // A disabled earlier branch never blocks -- "b" is first-in-line
        // despite "a" (position 0) never having finished, because "a" is
        // disabled. The enabled-branch count (1) excludes the disabled branch.
        let mut store = Store::open_in_memory().unwrap();
        let tf: ralphus_core::schema::TaskFile = toml::from_str(TWO_TASKS).unwrap();
        let squad = store.insert_squad(&tf, Some("r"), false).unwrap();
        let gid = store
            .create_guardian_for_squad("Stack", "main", "/repo", Some(&squad))
            .unwrap();
        store.add_guardian_branch(&gid, "a").unwrap();
        store.add_guardian_branch(&gid, "b").unwrap();
        store.set_cell_review_branch(&squad, 0, 0, "a").unwrap();
        store.set_cell_review_branch(&squad, 1, 0, "b").unwrap();
        store.set_branch_enabled_by_name(&gid, "a", false).unwrap();

        assert_eq!(
            store.cell_review_dispatch_priority(&squad, 1, 0).unwrap(),
            Some(1)
        );
    }
}
