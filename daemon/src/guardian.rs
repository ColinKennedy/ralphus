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

use crate::store::{Result, Store, StoreError, VerifyView};

/// Return type of [`Store::verify_steps_for_review_branch`]:
/// `(session_verifies, task_verifies, session_system_prompt)`.
pub type BranchVerifyInfo = (Vec<VerifyView>, Vec<VerifyView>, Option<String>);

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
    /// Waiting for linked task sessions to complete.
    Pending,
    /// All linked task sessions are done; branch is waiting for the rebase to start.
    Ready,
    /// Being rebased onto the stack.
    InProgress,
    /// Rebased cleanly.
    Done,
    /// All conflict markers for this branch have been resolved and committed,
    /// but the dedicated final-verification agent call (RAL-149) has not yet
    /// run. Transient: set right before that call and cleared (to
    /// [`Self::ConflictResolved`]) once it completes, pass or fail.
    VerifyPending,
    /// Rebased after resolving conflicts (and, when the fix pass hit
    /// conflicts, after the RAL-149 final-verification call has run).
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
            Self::Done => "done",
            Self::VerifyPending => "verify_pending",
            Self::ConflictResolved => "conflict_resolved",
            Self::Failed => "failed",
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
/// inherited from its source session (RAL-191): `Some(v)` replaces the
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
    /// task's session rather than at a diff.
    pub is_empty: bool,
    /// The machine the session that produced this branch ran on (RAL-185).
    /// `None` means the daemon's own host — every pre-RAL-185 branch, and any
    /// branch whose work was done locally.
    ///
    /// When this is set, the branch's commits live on *that* machine, so the
    /// review must fetch them from the project's shared remote before it can
    /// stack them (see `guardian_merge::fetch_branch_for_remote_session`).
    pub source_session_machine: Option<String>,
    /// Whether this branch is included in the rebase stack (RAL-43). Disabled
    /// branches are skipped during merge but remain visible in the branch list.
    pub enabled: bool,
    /// Which git project root this branch lives in (RAL-29). `None` means the
    /// guardian's primary `git_root` (backward compatible with single-project).
    pub project: Option<String>,
    /// State of the session whose work lives in this branch's worktree (RAL-69).
    /// `None` when no session has `review_branch = branch` (branch was never
    /// submitted or was added manually). Used to determine force-start readiness.
    pub source_session_state: Option<String>,
    /// `true` when this branch is disabled, all its source sessions are `done`,
    /// and the "can re-enable" notification has not been dismissed (RAL-69).
    /// TODO(RAL-73): wire to the dedicated `ready` signal when that lands.
    pub can_reenable: bool,
    /// Run ID of the most recent session submitted for this branch (for board navigation).
    pub source_run_id: Option<String>,
    /// Task index within the run for the source session.
    pub source_task_idx: Option<i64>,
    /// Session index within the task for the source session.
    pub source_session_idx: Option<i64>,
    /// agent_session_id from the conflict-resolver run on this branch.
    /// Only populated when resolver_agent is "claude-code". Enables terminal resume.
    pub resolver_agent_session_id: Option<String>,
    /// `true` when this branch's stacked commit is staged and ready to merge
    /// as-is (`merge_status == "ready"`). Ported from board.html's
    /// `branchBadge` (CLI_PARITY_PLAN.local.md Phase 5).
    pub ready: bool,
    /// Which terminal-resume modes are available for this branch's conflict
    /// resolver: `"readonly"`/`"open"` once a resolver session exists, or
    /// `"worktree"` for a CLI agent with a worktree but no session yet. Empty
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
    /// top of whatever its source session resolves to. A `Some(value)` entry
    /// overrides the inherited value; a `None` entry is a tombstone meaning
    /// "remove this inherited variable entirely". A key absent from this map
    /// is simply inherited. Set via
    /// `POST /api/guardians/{id}/branches/{bid}/env`.
    pub env_overrides: BTreeMap<String, Option<String>>,
    /// RAL-191: the *effective* environment this branch's review worktree runs
    /// under — the source session's resolved overrides with this branch's own
    /// [`Self::env_overrides`] applied (values replaced, tombstones removed).
    /// This is exactly what the conflict resolver, feedback routing, and check
    /// gates are spawned with.
    pub resolved_env: BTreeMap<String, String>,
    /// RAL-191: the environment inherited from the source session *before*
    /// this branch's own overrides are applied. Lets the board show which keys
    /// are inherited, overridden, or tombstoned without recomputing the merge.
    pub inherited_env: BTreeMap<String, String>,
}

/// One message in a guardian's global feedback thread (RAL-22).
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
}

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
    /// The run this review was derived from, if any (manual reviews have none).
    pub run_id: Option<String>,
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
    /// (RAL-110) are all skipped. Independent of [`Self::skip_worktree_checks`].
    pub skip_auto_build: bool,
    /// When true, the quality-bar system prompt normally folded into each
    /// per-branch conflict-resolution agent call is omitted (RAL-110) — the
    /// resolver is not told to run task/session verify steps while fixing
    /// conflicts. Independent of [`Self::skip_auto_build`].
    pub skip_worktree_checks: bool,
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
    /// RAL-88: the resolved agent/model that produced the latest feedback-chat
    /// reply. `None` until the guardian has answered at least one chat message.
    pub chat_agent: Option<String>,
    /// Model behind the latest chat reply (see [`Self::chat_agent`]).
    pub chat_model: Option<String>,
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
    /// RAL-168: this review's own Verify-scope override -- `"each_branch"`,
    /// `"final_branch"`, or `"nothing"`. `None` means "inherit the
    /// project-level default" (`.ralphus.toml [review] verify_scope`,
    /// resolved into [`Self::effective_verify_scope`] at hydration time).
    /// Governs whether/how often the dedicated LLM-based final-verify call
    /// ([`crate::guardian_merge::run_final_verify`]) fires -- replaces the
    /// old `verify_mid_resolution` flag outright, not layered alongside it.
    pub verify_scope: Option<String>,
    /// RAL-168: this review's own override for whether `"each_branch"` scope
    /// additionally skips verification on branches whose rebase applied
    /// cleanly with no conflict (an "auto-clean" branch). `None` means
    /// "inherit the project-level default".
    pub verify_skip_auto_clean: Option<bool>,
    /// RAL-168: [`Self::verify_scope`] resolved against the project-level
    /// `.ralphus.toml [review] verify_scope` default -- always one of
    /// `"each_branch"`/`"final_branch"`/`"nothing"`, never empty. This is
    /// what the merge engine actually gates on; the raw field above is only
    /// for the UI to distinguish "explicit override" from "inherited".
    pub effective_verify_scope: String,
    /// RAL-168: [`Self::verify_skip_auto_clean`] resolved against the
    /// project-level default.
    pub effective_verify_skip_auto_clean: bool,
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
    /// `"waiting"` (no branch has reached `Ready` yet). Summary computation is
    /// synchronous and never intentionally cleared mid-rebuild (see
    /// `recompute_preliminary_summary`/`generate_summary` in
    /// `guardian_merge.rs`), so there is no meaningful in-between "generating"
    /// state to report here.
    pub summary_state: &'static str,
    /// `manual_commands` display state (RAL-103): `"ready"` (commands are
    /// available), `"generating"` (every enabled branch has finished rebasing
    /// cleanly and the manual-checks LLM call is expected to be in flight --
    /// see `generate_manual_commands`'s call sites, always after the stack
    /// fully rebuilds), or `"waiting"` (branches are still being collected or
    /// rebased, so generation has not started).
    pub checks_state: &'static str,
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

/// Session state priority for picking the "worst" state among several linked
/// sessions, worst-first order (NOT a numeric scale -- first match in this
/// list wins). Ported verbatim from board.html's `SESSION_STATE_RANK`
/// (CLI_PARITY_PLAN.local.md Phase 5).
pub const SESSION_STATE_RANK: [&str; 6] = [
    "running",
    "failed",
    "pending",
    "queued",
    "cancelled",
    "done",
];

/// The first state in [`SESSION_STATE_RANK`] present in `states`, or `states[0]`
/// if none match (mirrors the JS `SESSION_STATE_RANK.find(...) || sessions[0].state`
/// fallback). `None` when `states` is empty.
#[must_use]
pub fn worst_session_state<'a>(states: &[&'a str]) -> Option<&'a str> {
    for rank in SESSION_STATE_RANK {
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
) -> Vec<&'static str> {
    if has_session_id {
        return vec!["readonly", "open"];
    }
    let agent = resolver_agent.unwrap_or("ollama");
    let is_cli_agent = matches!(agent, "claude-code" | "codex" | "codex-cli");
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
}

impl Store {
    /// Create a guardian in the `Collecting` state; returns its id.
    pub fn create_guardian(&self, name: &str, base_branch: &str, git_root: &str) -> Result<String> {
        self.create_guardian_for_run(name, base_branch, git_root, None)
    }

    /// Create a guardian, optionally tagged with the run it was derived from.
    pub fn create_guardian_for_run(
        &self,
        name: &str,
        base_branch: &str,
        git_root: &str,
        run_id: Option<&str>,
    ) -> Result<String> {
        self.create_guardian_keyed(name, base_branch, git_root, run_id, None)
    }

    /// Like [`Store::create_guardian_for_run`] but also stores a stable
    /// `review_key` (from a `ralphus:new-review/<key>` link id) so later
    /// submissions can find this guardian and append their branches to it.
    pub fn create_guardian_keyed(
        &self,
        name: &str,
        base_branch: &str,
        git_root: &str,
        run_id: Option<&str>,
        review_key: Option<&str>,
    ) -> Result<String> {
        let id = self.next_id("guardian_seq", "guardian")?;
        let now = crate::store::now_ms();
        self.conn.execute(
            "INSERT INTO guardians(id, name, base_branch, git_root, review_branch, status, detail, run_id, review_key, created_at_ms, updated_at_ms)
             VALUES(?,?,?,?,NULL,?,NULL,?,?,?,?)",
            params![id, name, base_branch, git_root, GuardianStatus::Collecting.as_str(), run_id, review_key, now, now],
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
    /// or an explicit cancel is required.
    pub fn claim_guardian_merge(&self, id: &str) -> Result<bool> {
        let n = self.conn.execute(
            "UPDATE guardians SET status='merging', updated_at_ms=? \
             WHERE id=? AND status IN ('collecting','merge_failed','in_review')",
            params![crate::store::now_ms(), id],
        )?;
        Ok(n > 0)
    }

    /// Crash recovery: guardians left `merging` after an unclean shutdown have
    /// no background thread to complete them. Reset each to `merge_failed` so
    /// the user can see the interruption and re-trigger. Run at daemon startup
    /// before the scheduler begins; returns the recovered guardian ids.
    pub fn recover_orphaned_merges(&self) -> Result<Vec<String>> {
        let ids: Vec<String> = {
            let mut stmt = self
                .conn
                .prepare("SELECT id FROM guardians WHERE status='merging'")?;
            stmt.query_map([], |r| r.get::<_, String>(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?
        };
        for id in &ids {
            self.conn.execute(
                "UPDATE guardians SET status='merge_failed', \
                 detail='merge interrupted by daemon restart', \
                 updated_at_ms=? WHERE id=?",
                params![crate::store::now_ms(), id],
            )?;
            crate::rlog!(
                WARNING,
                "ralphus [recovery] guardian {id} merging → merge_failed (daemon restart)"
            );
            let _ = self.cartographer_log(crate::cartographer::CartographerEntry {
                level: crate::logging::LogLevel::WARNING,
                source: "recovery",
                message: "guardian merge interrupted: merging → merge_failed (daemon restart)",
                scope: Some("guardian"),
                run_id: None,
                guardian_id: Some(id),
                session_id: None,
                task: None,
                payload: serde_json::json!({}),
            });
        }
        Ok(ids)
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

    /// Ids of the guardians derived from a run, oldest first.
    pub fn guardians_for_run(&self, run_id: &str) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id FROM guardians WHERE run_id=? ORDER BY created_at_ms, id")?;
        let ids = stmt
            .query_map(params![run_id], |r| r.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(ids)
    }

    /// Ids of collecting guardians that have at least one branch whose name
    /// matches a session `review_branch` in the given run. This catches linked
    /// reviews whose guardian `run_id` points to an earlier submission (because
    /// the guardian was found — not created — when the newer run was submitted).
    pub fn collecting_guardians_for_sessions(&self, run_id: &str) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT g.id FROM guardians g
             JOIN guardian_branches gb ON g.id = gb.guardian_id
             JOIN sessions s ON s.review_branch = gb.branch
             WHERE s.run_id = ? AND g.status = 'collecting'
             ORDER BY g.created_at_ms, g.id",
        )?;
        let ids = stmt
            .query_map(params![run_id], |r| r.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(ids)
    }

    /// Ids of collecting guardians that are ready to start: every enabled branch
    /// that has a contributing session (matched by `sessions.review_branch =
    /// guardian_branches.branch`) is in the `done` state. Guardians with no
    /// session-linked branches are excluded (they haven't been triggered yet).
    /// Used at daemon startup to recover guardians that were left `collecting`
    /// because the daemon was restarted after the run completed.
    ///
    /// This already requires ALL matching sessions done, not just one — which,
    /// since RAL-159, includes implicit worktree-sharing siblings alongside the
    /// explicitly review-linked session (see `mark_ready_branches_with_done_sessions`).
    pub fn collecting_guardians_ready(&self) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT id FROM guardians WHERE status = 'collecting'
             AND EXISTS (
                 SELECT 1 FROM guardian_branches gb
                 JOIN sessions s ON s.review_branch = gb.branch
                 WHERE gb.guardian_id = guardians.id AND gb.enabled = 1
             )
             AND NOT EXISTS (
                 SELECT 1 FROM guardian_branches gb
                 JOIN sessions s ON s.review_branch = gb.branch
                 WHERE gb.guardian_id = guardians.id AND gb.enabled = 1
                   AND s.state != 'done'
             )
             ORDER BY created_at_ms, id",
        )?;
        let ids = stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(ids)
    }

    /// Ids of guardians that have already left `collecting` (`in_review` or
    /// `merge_failed`) but still have an enabled branch stuck at `pending` whose
    /// contributing session has since finished. This is the straggler case: a
    /// linked review (RAL-97/98) whose branches arrive from separate runs, where
    /// the guardian moved on after its first run's task finished, before the
    /// second run's task — and therefore `collecting_guardians_for_sessions`,
    /// which only matches `status = 'collecting'` — ever saw it. Picked up by
    /// [`crate::guardian_merge::review_maintenance`]'s periodic sweep so the
    /// branch is not stuck `pending` forever.
    pub fn guardians_with_ready_stragglers(&self) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT g.id FROM guardians g
             JOIN guardian_branches gb ON g.id = gb.guardian_id
             JOIN sessions s ON s.review_branch = gb.branch
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
    /// whose contributing session(s) have ALL actually finished — unlike
    /// [`Self::mark_guardian_branches_ready`], which blindly promotes every
    /// pending branch and is only safe to call once the caller has separately
    /// verified every blocking task is done. Used for the straggler sweep, where
    /// a guardian may still have other, genuinely-unfinished pending branches
    /// that must not be promoted early.
    ///
    /// RAL-159: a branch's `sessions.review_branch` set can now contain more
    /// than one row — an explicitly review-linked session plus any sibling
    /// sessions that merely share its git worktree (e.g. a nested cwd
    /// subfolder), attached by `reviews::derive_reviews`. Requiring `NOT
    /// EXISTS` a non-done contributor (rather than the old `EXISTS` a done
    /// one) means the branch is only marked `ready` once every one of them —
    /// explicit or implicit — has finished, not just the first.
    pub fn mark_ready_branches_with_done_sessions(&self, guardian_id: &str) -> Result<usize> {
        let n = self.conn.execute(
            "UPDATE guardian_branches
             SET merge_status='ready'
             WHERE guardian_id=? AND enabled=1 AND merge_status='pending'
               AND EXISTS (
                   SELECT 1 FROM sessions s
                   WHERE s.review_branch = guardian_branches.branch
               )
               AND NOT EXISTS (
                   SELECT 1 FROM sessions s
                   WHERE s.review_branch = guardian_branches.branch AND s.state != 'done'
               )",
            params![guardian_id],
        )?;
        Ok(n)
    }

    /// The `cwd` of the most recent session that contributed to `branch` (matched
    /// via `sessions.review_branch`), if any (RAL-103). Used to compute a
    /// preliminary, git-log-only change summary from the task's own worktree
    /// before any review worktree has been built for that branch.
    pub fn session_cwd_for_branch(&self, branch: &str) -> Result<Option<String>> {
        let cwd: Option<Option<String>> = self
            .conn
            .query_row(
                "SELECT cwd FROM sessions WHERE review_branch=? ORDER BY rowid DESC LIMIT 1",
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
            "INSERT INTO guardian_branches(guardian_id, position, branch, merge_status, detail, project, id)
             VALUES(?,?,?,?,NULL,?,?)",
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
        if from_status == "merging" || to_status == "merging" {
            return Err(StoreError::InvalidTransition(
                "cannot move a branch while the source or destination review has a merge/rebase in progress"
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
            "INSERT INTO guardian_branches
                 (guardian_id, position, branch, merge_status, detail, enabled, moved_from_guardian_id, id)
             VALUES (?,?,?,?,NULL,1,?,?)",
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
        self.conn.execute(
            "DELETE FROM guardian_input_resolutions WHERE guardian_id=?",
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

    /// Append a message to a guardian's global feedback thread (RAL-22). `role`
    /// is `"reviewer"` (the human) or `"guardian"` (the triage agent). `image`
    /// is an optional base64 data-URI attached to the message (RAL-59).
    pub fn add_guardian_message(
        &self,
        guardian_id: &str,
        role: &str,
        text: &str,
        image: Option<&str>,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO guardian_messages(guardian_id, role, text, at_ms, image) VALUES(?,?,?,?,?)",
            params![guardian_id, role, text, crate::store::now_ms(), image],
        )?;
        Ok(())
    }

    /// Delete all messages with `seq >= from_seq` for the given guardian.
    /// Used by the conversation-branching fork operation (RAL-59).
    pub fn delete_guardian_messages_from_seq(
        &self,
        guardian_id: &str,
        from_seq: i64,
    ) -> Result<()> {
        self.conn.execute(
            "DELETE FROM guardian_messages WHERE guardian_id=? AND seq>=?",
            params![guardian_id, from_seq],
        )?;
        Ok(())
    }

    /// A guardian's global feedback thread, oldest first (RAL-22).
    pub fn guardian_messages(&self, guardian_id: &str) -> Result<Vec<MessageView>> {
        let mut stmt = self.conn.prepare(
            "SELECT seq, role, text, at_ms, image FROM guardian_messages WHERE guardian_id=? ORDER BY seq",
        )?;
        let rows = stmt
            .query_map(params![guardian_id], |r| {
                Ok(MessageView {
                    seq: r.get(0)?,
                    role: r.get(1)?,
                    text: r.get(2)?,
                    at_ms: r.get(3)?,
                    image: r.get(4)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
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
            Ok(())
        }
    }

    /// Set the shell check commands run against the review worktree.
    pub fn set_guardian_checks(&self, id: &str, checks: &[String]) -> Result<()> {
        self.conn.execute(
            "UPDATE guardians SET checks=?, updated_at_ms=? WHERE id=?",
            params![crate::store::to_json(checks), crate::store::now_ms(), id],
        )?;
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

    /// Set whether the per-branch conflict-resolution quality-bar system prompt
    /// is omitted (RAL-110). Independent of `skip_auto_build`.
    pub fn set_guardian_skip_worktree_checks(&self, id: &str, skip: bool) -> Result<()> {
        let n = self.conn.execute(
            "UPDATE guardians SET skip_worktree_checks=?, updated_at_ms=? WHERE id=?",
            params![i64::from(skip), crate::store::now_ms(), id],
        )?;
        if n == 0 {
            Err(StoreError::NotFound)
        } else {
            Ok(())
        }
    }

    /// Whether the per-branch conflict-resolution quality-bar prompt is opted out.
    pub fn guardian_skip_worktree_checks(&self, id: &str) -> Result<bool> {
        self.conn
            .query_row(
                "SELECT skip_worktree_checks FROM guardians WHERE id=?",
                params![id],
                |r| r.get(0),
            )
            .optional()?
            .ok_or(StoreError::NotFound)
    }

    /// Set this review's own Verify-scope override (RAL-168): one of
    /// `"each_branch"`/`"final_branch"`/`"nothing"`. `None` resets it to
    /// "inherit the project-level default".
    pub fn set_guardian_verify_scope(&self, id: &str, scope: Option<&str>) -> Result<()> {
        let n = self.conn.execute(
            "UPDATE guardians SET verify_scope=?, updated_at_ms=? WHERE id=?",
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
    pub fn set_guardian_verify_skip_auto_clean(&self, id: &str, skip: Option<bool>) -> Result<()> {
        let n = self.conn.execute(
            "UPDATE guardians SET verify_skip_auto_clean=?, updated_at_ms=? WHERE id=?",
            params![skip.map(i64::from), crate::store::now_ms(), id],
        )?;
        if n == 0 {
            Err(StoreError::NotFound)
        } else {
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

    /// Returns `(session_verifies, task_verifies, session_system_prompt)` for the
    /// session whose `review_branch` matches `branch` in this guardian's linked run.
    /// Returns `None` when the guardian has no `run_id` or no session with a matching
    /// `review_branch` exists (e.g. a manually-created review with no task linkage).
    pub fn verify_steps_for_review_branch(
        &self,
        guardian_id: &str,
        branch: &str,
    ) -> Result<Option<BranchVerifyInfo>> {
        let run_id: Option<String> = self
            .conn
            .query_row(
                "SELECT run_id FROM guardians WHERE id=?",
                params![guardian_id],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        let Some(run_id) = run_id else {
            return Ok(None);
        };
        let row: Option<(i64, i64, Option<String>)> = self
            .conn
            .query_row(
                "SELECT task_idx, idx, system_prompt \
                 FROM sessions WHERE run_id=? AND review_branch=?",
                params![run_id, branch],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let Some((task_idx, session_idx, system_prompt)) = row else {
            return Ok(None);
        };
        let session_verifies = self.verifies_for(&run_id, task_idx, "session", session_idx)?;
        let task_verifies = self.verifies_for(&run_id, task_idx, "task", -1)?;
        Ok(Some((session_verifies, task_verifies, system_prompt)))
    }

    /// Update the base branch for a review and clear the recorded base commit so the
    /// next merge re-baselines against the new branch. Clears `base_commit` so the
    /// next merge detects a fresh base rather than comparing against the old branch's
    /// tip. Returns [`StoreError::NotFound`] if the guardian does not exist.
    pub fn set_guardian_base_branch(&self, id: &str, base_branch: &str) -> Result<()> {
        let n = self.conn.execute(
            "UPDATE guardians SET base_branch=?, base_commit=NULL, updated_at_ms=? WHERE id=?",
            params![base_branch, crate::store::now_ms(), id],
        )?;
        if n == 0 {
            Err(StoreError::NotFound)
        } else {
            Ok(())
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
    /// when no session ID is available yet).
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
    /// `Store::get_session_agent_resume`'s doc comment for the same idea
    /// applied to a plain session.
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
    /// (mirrors the chat fallback's own cwd choice — see
    /// `guardian_merge.rs::resolve_guardian_chat`). `None` in either position
    /// means the corresponding action isn't available yet.
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

    /// Record which resolved `agent`/`model` produced the latest feedback-chat
    /// reply, so the reviewer can inspect it (RAL-88). Overwritten on each reply.
    pub fn set_guardian_chat_agent(
        &self,
        id: &str,
        agent: &str,
        model: Option<&str>,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE guardians SET chat_agent=?, chat_model=?, updated_at_ms=? WHERE id=?",
            params![agent, model, crate::store::now_ms(), id],
        )?;
        Ok(())
    }

    /// Persist user-declared action hints from `[[review.action]]` (RAL-77).
    /// Stored as a JSON array; set once at submit time and not touched by the merge engine.
    pub fn set_guardian_action_hints(&self, id: &str, hints: &[GuardianCheck]) -> Result<()> {
        let json = serde_json::to_string(hints).unwrap_or_else(|_| "[]".to_string());
        self.conn.execute(
            "UPDATE guardians SET action_hints=?, updated_at_ms=? WHERE id=?",
            params![json, crate::store::now_ms(), id],
        )?;
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
            "SELECT position, branch, enabled, id FROM guardian_branches WHERE guardian_id=? ORDER BY position",
        )?;
        let rows = stmt
            .query_map(params![guardian_id], |r| {
                Ok(OrderedBranch {
                    position: r.get(0)?,
                    branch: r.get(1)?,
                    enabled: r.get::<_, i64>(2).map(|v| v != 0).unwrap_or(true),
                    id: r.get(3)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Disable all enabled branches whose source session is not yet `done`
    /// (or have no linked session at all). Returns the list of disabled branch
    /// names with their source session state (None = never submitted). Called by
    /// the force-start endpoint (RAL-69).
    pub fn force_start_disable_branches(
        &self,
        guardian_id: &str,
    ) -> Result<Vec<(String, Option<String>)>> {
        let mut stmt = self.conn.prepare(
            "SELECT gb.branch,
                    (SELECT s.state FROM sessions s
                     WHERE s.review_branch = gb.branch
                     ORDER BY s.rowid DESC LIMIT 1) AS source_session_state
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
    /// merged with the source session's. `Some(v)` is an override, `None` is a
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
    ///   whatever the source session resolves to.
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
    /// (RAL-191): the source session's resolved `run < task < session`
    /// overrides with this branch's own layer applied on top.
    ///
    /// A branch with no source session (added manually, or whose session was
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
                "SELECT s.run_id, s.task_idx, s.idx
                 FROM guardian_branches gb
                 JOIN sessions s ON s.rowid = (
                     SELECT s2.rowid FROM sessions s2
                     WHERE s2.review_branch = gb.branch
                     ORDER BY s2.rowid DESC LIMIT 1
                 )
                 WHERE gb.guardian_id=? AND gb.id=?",
                params![guardian_id, branch_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let inherited = match source {
            Some((run_id, ti, si)) => self
                .resolve_session_env_overrides(&run_id, ti, si)
                .unwrap_or_default(),
            None => BTreeMap::new(),
        };
        Ok(apply_branch_env(&inherited, &overrides))
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
    /// signalling that every linked task session is done and the branch is
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
                "SELECT id, name, base_branch, git_root, review_branch, status, detail, checks, run_id, combined_worktree, conflicts_found, conflicts_fixed, conflicts_committed, skip_auto_build, skip_worktree_checks, review_type, skip_worktrees, created_at_ms, resolver_agent, resolver_model, base_commit, change_summary, base_commits, manual_commands, action_hints, summary_agent, summary_model, manual_commands_agent, manual_commands_model, manual_commands_agent_session_id, chat_agent, chat_model, squash_projects, auto_pr_feedback, input_values, verify_scope, verify_skip_auto_clean, machine
                 FROM guardians WHERE id=?",
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
            "SELECT id, name, base_branch, git_root, review_branch, status, detail, checks, run_id, combined_worktree, conflicts_found, conflicts_fixed, conflicts_committed, skip_auto_build, skip_worktree_checks, review_type, skip_worktrees, created_at_ms, resolver_agent, resolver_model, base_commit, change_summary, base_commits, manual_commands, action_hints, summary_agent, summary_model, manual_commands_agent, manual_commands_model, manual_commands_agent_session_id, chat_agent, chat_model, squash_projects, auto_pr_feedback, input_values, verify_scope, verify_skip_auto_clean, machine
             FROM guardians ORDER BY created_at_ms DESC",
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
            run_id: r.get(8)?,
            combined_worktree: r.get(9)?,
            conflicts_found: r.get(10)?,
            conflicts_fixed: r.get(11)?,
            conflicts_committed: r.get(12)?,
            skip_auto_build: r.get(13)?,
            skip_worktree_checks: r.get(14)?,
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
            chat_agent: r.get(30)?,
            chat_model: r.get(31)?,
            squash_projects: r.get(32)?,
            auto_pr_feedback: r.get(33)?,
            input_values: r.get(34)?,
            verify_scope: r.get(35)?,
            verify_skip_auto_clean: r.get::<_, Option<i64>>(36)?.map(|v| v != 0),
            machine: r.get(37)?,
        })
    }

    fn hydrate_guardian(&self, row: GuardianRow) -> Result<GuardianView> {
        // RAL-121: one correlated subquery per branch (finding that branch's
        // most-recent session by rowid) instead of the previous four -- each of
        // state/run_id/task_idx/idx was a separate subquery re-scanning
        // `sessions` for the same row. Paired with `idx_sessions_review_branch`
        // (see `store.rs`'s migration list) this is now an index seek, not a
        // table scan, per branch.
        let mut stmt = self.conn.prepare(
            "SELECT gb.position, gb.branch, gb.merge_status, gb.detail, gb.review_branch,
                    gb.worktree, gb.conflicts_found, gb.conflicts_fixed, gb.conflicts_committed,
                    gb.enabled, gb.project, gb.dismissed_reenable,
                    s.state AS source_session_state,
                    s.run_id AS source_run_id,
                    s.task_idx AS source_task_idx,
                    s.idx AS source_session_idx,
                    gb.resolver_agent_session_id, gb.moved_from_guardian_id, gb.id,
                    gb.is_empty, s.machine AS source_session_machine,
                    gb.env_overrides
             FROM guardian_branches gb
             LEFT JOIN sessions s ON s.rowid = (
                 SELECT s2.rowid FROM sessions s2
                 WHERE s2.review_branch = gb.branch
                 ORDER BY s2.rowid DESC LIMIT 1
             )
             WHERE gb.guardian_id=? ORDER BY gb.position",
        )?;
        let mut branches = stmt
            .query_map(params![row.id], |r| {
                let is_empty = r.get::<_, i64>(19).map(|v| v != 0).unwrap_or(false);
                let source_session_machine: Option<String> = r.get(20)?;
                let enabled = r.get::<_, i64>(9).map(|v| v != 0).unwrap_or(true);
                let dismissed = r.get::<_, i64>(11).map(|v| v != 0).unwrap_or(false);
                let source_session_state: Option<String> = r.get(12)?;
                let can_reenable =
                    !enabled && source_session_state.as_deref() == Some("done") && !dismissed;
                let merge_status: String = r.get(2)?;
                let ready = merge_status == "ready";
                let worktree: Option<String> = r.get(5)?;
                let resolver_agent_session_id: Option<String> = r.get(16)?;
                let terminal_modes = terminal_modes_for(
                    row.resolver_agent.as_deref(),
                    resolver_agent_session_id.is_some(),
                    worktree.is_some(),
                );
                Ok(BranchView {
                    id: r.get(18)?,
                    position: r.get(0)?,
                    branch: r.get(1)?,
                    merge_status,
                    detail: r.get(3)?,
                    review_branch: r.get(4)?,
                    worktree,
                    conflicts_found: r.get(6)?,
                    conflicts_fixed: r.get(7)?,
                    conflicts_committed: r.get(8)?,
                    is_empty,
                    source_session_machine,
                    enabled,
                    project: r.get(10)?,
                    source_session_state,
                    can_reenable,
                    source_run_id: r.get(13)?,
                    source_task_idx: r.get(14)?,
                    source_session_idx: r.get(15)?,
                    resolver_agent_session_id,
                    ready,
                    terminal_modes,
                    moved_from_guardian_id: r.get(17)?,
                    rebase_commands_done: None,
                    rebase_commands_total: None,
                    // RAL-191: the raw per-branch layer; `inherited_env` and
                    // `resolved_env` are filled in below, where the source
                    // session's own resolution is reachable.
                    env_overrides: branch_env_from_json(&r.get::<_, String>(21)?),
                    resolved_env: BTreeMap::new(),
                    inherited_env: BTreeMap::new(),
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        // RAL-191: resolve each branch's effective environment. The inherited
        // half needs the source session's full `run < task < session` chain, so
        // it is a second pass rather than more columns on the query above.
        for b in &mut branches {
            b.inherited_env = match (
                b.source_run_id.as_deref(),
                b.source_task_idx,
                b.source_session_idx,
            ) {
                (Some(run_id), Some(ti), Some(si)) => self
                    .resolve_session_env_overrides(run_id, ti, si)
                    .unwrap_or_default(),
                // A branch added manually (or whose source session has since
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

        // RAL-168: resolve this review's own Verify-scope override (if any)
        // against the project-level `.ralphus.toml [review] verify_scope`
        // default -- so the UI can show the effective value as the dropdown's
        // initial selection (interview Q7) without a second round-trip, and
        // the merge engine (`guardian_merge.rs`) has a single, always-populated
        // field to gate on.
        let project_review_config = crate::config::resolve(Path::new(&row.git_root));
        let effective_verify_scope = row
            .verify_scope
            .as_deref()
            .filter(|s| matches!(*s, "each_branch" | "final_branch" | "nothing"))
            .unwrap_or_else(|| project_review_config.verify_scope())
            .to_string();
        let effective_verify_skip_auto_clean = row
            .verify_skip_auto_clean
            .unwrap_or_else(|| project_review_config.verify_skip_auto_clean());

        Ok(GuardianView {
            id: row.id,
            name: row.name,
            base_branch: row.base_branch,
            base_commit: row.base_commit,
            git_root: row.git_root,
            review_branch: row.review_branch,
            status: row.status,
            detail: row.detail,
            run_id: row.run_id,
            combined_worktree: row.combined_worktree,
            conflicts_found: row.conflicts_found,
            conflicts_fixed: row.conflicts_fixed,
            conflicts_committed: row.conflicts_committed,
            skip_auto_build: row.skip_auto_build,
            skip_worktree_checks: row.skip_worktree_checks,
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
            chat_agent: row.chat_agent,
            chat_model: row.chat_model,
            squash_projects: crate::store::from_json(
                row.squash_projects.as_deref().unwrap_or("[]"),
            ),
            auto_pr_feedback: row.auto_pr_feedback,
            verify_scope: row.verify_scope,
            verify_skip_auto_clean: row.verify_skip_auto_clean,
            machine: row.machine,
            effective_verify_scope,
            effective_verify_skip_auto_clean,
            ready,
            merge_progress,
            summary_state,
            checks_state,
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

    /// Cancel a guardian that is in a cancellable state (collecting, merging, in_review, merge_failed, or approved).
    /// Background threads that are still running should check the status on completion
    /// and discard their result if the guardian is already cancelled.
    pub fn cancel_guardian(&self, id: &str) -> Result<GuardianStatus> {
        match GuardianStatus::parse(&self.guardian_status_str(id)?) {
            Some(
                GuardianStatus::Collecting
                | GuardianStatus::Merging
                | GuardianStatus::MergeFailed
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
    run_id: Option<String>,
    combined_worktree: Option<String>,
    conflicts_found: Option<i64>,
    conflicts_fixed: Option<i64>,
    conflicts_committed: Option<i64>,
    skip_auto_build: bool,
    skip_worktree_checks: bool,
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
    chat_agent: Option<String>,
    chat_model: Option<String>,
    /// JSON array of project roots with squash enabled (RAL-91).
    squash_projects: Option<String>,
    /// RAL-117: opts this review into auto-incorporating PR feedback.
    auto_pr_feedback: bool,
    /// JSON map of {input_name: value} -- resolved/submitted [`CheckInput`]
    /// values, scoped to this guardian (RAL-164).
    input_values: Option<String>,
    /// RAL-168: per-review Verify-scope override. `None` inherits the
    /// project-level default.
    verify_scope: Option<String>,
    /// RAL-168: per-review auto-clean-skip override. `None` inherits the
    /// project-level default.
    verify_skip_auto_clean: Option<bool>,
    /// RAL-185: the machine this review runs on. NULL means the daemon's host.
    machine: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── CLI_PARITY_PLAN.local.md Phase 5: ported board.html domain logic ────────

    #[test]
    fn worst_session_state_picks_first_rank_match() {
        // "running" outranks "done" regardless of array order.
        assert_eq!(worst_session_state(&["done", "running"]), Some("running"));
        assert_eq!(worst_session_state(&["done", "failed"]), Some("failed"));
        assert_eq!(
            worst_session_state(&["queued", "cancelled"]),
            Some("queued")
        );
    }

    #[test]
    fn worst_session_state_falls_back_to_first_element() {
        // An unranked state (not in SESSION_STATE_RANK) falls back to states[0],
        // mirroring the JS `SESSION_STATE_RANK.find(...) || sessions[0].state`.
        assert_eq!(worst_session_state(&["weird_state"]), Some("weird_state"));
        assert_eq!(worst_session_state(&[]), None);
    }

    #[test]
    fn terminal_modes_with_session_id_are_always_readonly_and_open() {
        // A resolver session id makes both modes available regardless of agent
        // or worktree presence.
        assert_eq!(
            terminal_modes_for(None, true, false),
            vec!["readonly", "open"]
        );
        assert_eq!(
            terminal_modes_for(Some("ollama"), true, true),
            vec!["readonly", "open"]
        );
    }

    #[test]
    fn terminal_modes_cli_agent_with_worktree_offers_worktree_only() {
        for agent in ["claude-code", "codex", "codex-cli"] {
            assert_eq!(
                terminal_modes_for(Some(agent), false, true),
                vec!["worktree"]
            );
        }
    }

    #[test]
    fn terminal_modes_none_available_without_session_or_worktree() {
        assert_eq!(
            terminal_modes_for(Some("claude-code"), false, false),
            Vec::<&str>::new()
        );
        assert_eq!(
            terminal_modes_for(Some("ollama"), false, true),
            Vec::<&str>::new()
        );
        assert_eq!(terminal_modes_for(None, false, false), Vec::<&str>::new());
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
            worktree: None,
            conflicts_found: None,
            conflicts_fixed: None,
            conflicts_committed: None,
            is_empty: false,
            source_session_machine: None,
            enabled: true,
            project: None,
            source_session_state: None,
            can_reenable: false,
            source_run_id: None,
            source_task_idx: None,
            source_session_idx: None,
            resolver_agent_session_id: None,
            ready: merge_status == "ready",
            terminal_modes: Vec::new(),
            moved_from_guardian_id: None,
            rebase_commands_done: None,
            rebase_commands_total: None,
            env_overrides: BTreeMap::new(),
            resolved_env: BTreeMap::new(),
            inherited_env: BTreeMap::new(),
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

    /// Build a guardian whose single branch `feat` is linked to a real session
    /// carrying task- and session-level env, so inheritance has something to
    /// resolve. Returns `(guardian_id, branch_id)`.
    fn guardian_with_env_source_session(store: &mut Store) -> (String, String) {
        let src = "[[task]]\nname=\"t0\"\nenvironment={SHARED=\"from-task\", TASK_ONLY=\"1\"}\n\
                   [[task.session]]\nid=\"s0\"\ncwd=\"/r\"\nprompt=\"p\"\n\
                   environment={SHARED=\"from-session\", SESSION_ONLY=\"2\"}\n";
        let tf: ralphus_core::schema::TaskFile = toml::from_str(src).expect("valid fixture");
        let run = store.insert_run(&tf, Some("r"), false).unwrap();
        store.set_session_review_branch(&run, 0, 0, "feat").unwrap();
        let id = store.create_guardian("r", "main", "/repo").unwrap();
        store.add_guardian_branch(&id, "feat").unwrap();
        let bid = store.get_guardian(&id).unwrap().branches[0].id.clone();
        (id, bid)
    }

    #[test]
    fn a_review_branch_inherits_its_source_sessions_resolved_environment() {
        // The core of RAL-191: a review worktree is built from a session's
        // work, so by default it runs under that session's environment --
        // including the task-level values the session itself inherited.
        let mut store = Store::open_in_memory().unwrap();
        let (id, bid) = guardian_with_env_source_session(&mut store);

        let env = store.resolve_guardian_branch_env(&id, &bid).unwrap();
        assert_eq!(env.get("SHARED").map(String::as_str), Some("from-session"));
        assert_eq!(env.get("TASK_ONLY").map(String::as_str), Some("1"));
        assert_eq!(env.get("SESSION_ONLY").map(String::as_str), Some("2"));

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
    fn a_branch_override_shadows_the_inherited_value_without_touching_the_session() {
        let mut store = Store::open_in_memory().unwrap();
        let (id, bid) = guardian_with_env_source_session(&mut store);

        let mut set = BTreeMap::new();
        set.insert("SHARED".to_string(), "from-review".to_string());
        store
            .set_guardian_branch_env_overrides(&id, &bid, &set, &[], &[])
            .unwrap();

        let env = store.resolve_guardian_branch_env(&id, &bid).unwrap();
        assert_eq!(env.get("SHARED").map(String::as_str), Some("from-review"));
        // Untouched keys still come through from the session.
        assert_eq!(env.get("SESSION_ONLY").map(String::as_str), Some("2"));
        // The source session itself is unchanged -- the override is review-only.
        let run = store.list_runs().unwrap()[0].id.clone();
        assert_eq!(
            store
                .resolve_session_env_overrides(&run, 0, 0)
                .unwrap()
                .get("SHARED")
                .map(String::as_str),
            Some("from-session")
        );
    }

    #[test]
    fn a_branch_tombstone_removes_an_inherited_variable_entirely() {
        // The distinguishing case for the tombstone design: `unset` here must
        // mean "this worktree does not get the variable at all", not "drop my
        // override and fall back to the session's value" (which is `clear`).
        let mut store = Store::open_in_memory().unwrap();
        let (id, bid) = guardian_with_env_source_session(&mut store);

        store
            .set_guardian_branch_env_overrides(
                &id,
                &bid,
                &BTreeMap::new(),
                &["SESSION_ONLY".to_string()],
                &[],
            )
            .unwrap();

        let env = store.resolve_guardian_branch_env(&id, &bid).unwrap();
        assert!(
            !env.contains_key("SESSION_ONLY"),
            "tombstoned key must not reach the review worktree, got {env:?}"
        );
        assert_eq!(env.get("SHARED").map(String::as_str), Some("from-session"));

        // The stored layer records the tombstone explicitly as `None`.
        let own = store.get_guardian_branch_env_overrides(&id, &bid).unwrap();
        assert_eq!(own.get("SESSION_ONLY"), Some(&None));
    }

    #[test]
    fn clearing_a_branch_entry_restores_the_inherited_value() {
        // `clear` is the third operation -- it drops the branch's own entry
        // (override *or* tombstone) so the key inherits again.
        let mut store = Store::open_in_memory().unwrap();
        let (id, bid) = guardian_with_env_source_session(&mut store);

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
            Some("from-session"),
            "clearing the tombstone must restore inheritance, not leave it removed"
        );
    }

    #[test]
    fn a_branch_with_no_source_session_has_only_its_own_overrides() {
        // A manually-added branch (or one whose session was deleted) inherits
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
        assert!(store.approve_guardian(&id).is_err()); // not in review yet
        store
            .set_guardian_status(&id, GuardianStatus::InReview, None)
            .unwrap();
        assert_eq!(
            store.approve_guardian(&id).unwrap(),
            GuardianStatus::Approved
        );
        assert_eq!(store.get_guardian(&id).unwrap().status, "approved");
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
    fn guardian_messages_thread_persists_in_order() {
        // RAL-22: the global feedback thread stores messages oldest-first with
        // their roles, and starts empty.
        let store = Store::open_in_memory().unwrap();
        let id = store.create_guardian("r", "main", "/repo").unwrap();
        assert!(store.guardian_messages(&id).unwrap().is_empty());
        store
            .add_guardian_message(&id, "reviewer", "please fix the naming", None)
            .unwrap();
        store
            .add_guardian_message(&id, "guardian", "that lands on branch feature/a", None)
            .unwrap();
        let thread = store.guardian_messages(&id).unwrap();
        assert_eq!(thread.len(), 2);
        assert_eq!(thread[0].role, "reviewer");
        assert_eq!(thread[0].text, "please fix the naming");
        assert_eq!(thread[1].role, "guardian");
    }

    #[test]
    fn deleting_a_guardian_clears_its_messages() {
        let store = Store::open_in_memory().unwrap();
        let id = store.create_guardian("r", "main", "/repo").unwrap();
        store
            .add_guardian_message(&id, "reviewer", "hi", None)
            .unwrap();
        store.delete_guardian(&id).unwrap();
        assert!(store.guardian_messages(&id).unwrap().is_empty());
    }

    #[test]
    fn delete_guardian_messages_from_seq_truncates_thread() {
        let store = Store::open_in_memory().unwrap();
        let id = store.create_guardian("r", "main", "/repo").unwrap();
        store
            .add_guardian_message(&id, "reviewer", "msg1", None)
            .unwrap();
        store
            .add_guardian_message(&id, "guardian", "msg2", None)
            .unwrap();
        store
            .add_guardian_message(&id, "reviewer", "msg3", None)
            .unwrap();
        let thread = store.guardian_messages(&id).unwrap();
        assert_eq!(thread.len(), 3);
        let fork_seq = thread[1].seq;
        store
            .delete_guardian_messages_from_seq(&id, fork_seq)
            .unwrap();
        let after = store.guardian_messages(&id).unwrap();
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].text, "msg1");
    }

    #[test]
    fn delete_guardian_messages_from_seq_prunes_from_that_point() {
        // RAL-59: conversation branching prunes everything from the fork seq onwards.
        let store = Store::open_in_memory().unwrap();
        let id = store.create_guardian("r", "main", "/repo").unwrap();
        store
            .add_guardian_message(&id, "reviewer", "a", None)
            .unwrap();
        store
            .add_guardian_message(&id, "guardian", "b", None)
            .unwrap();
        store
            .add_guardian_message(&id, "reviewer", "c", None)
            .unwrap();
        let msgs = store.guardian_messages(&id).unwrap();
        assert_eq!(msgs.len(), 3);
        let seq_b = msgs[1].seq;
        store.delete_guardian_messages_from_seq(&id, seq_b).unwrap();
        let after = store.guardian_messages(&id).unwrap();
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].text, "a");
        assert_eq!(after[0].role, "reviewer");
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
    }

    #[test]
    fn recover_orphaned_merges_resets_merging_guardians_to_merge_failed() {
        let store = Store::open_in_memory().unwrap();
        let id = store.create_guardian("r", "main", "/repo").unwrap();
        store.add_guardian_branch(&id, "feat").unwrap();

        assert!(store.recover_orphaned_merges().unwrap().is_empty());

        store.claim_guardian_merge(&id).unwrap();
        assert_eq!(store.get_guardian(&id).unwrap().status, "merging");

        let recovered = store.recover_orphaned_merges().unwrap();
        assert_eq!(recovered, vec![id.clone()]);

        let g = store.get_guardian(&id).unwrap();
        assert_eq!(g.status, "merge_failed");
        assert!(
            g.detail.as_deref().unwrap_or("").contains("restart"),
            "detail should mention restart: {:?}",
            g.detail
        );

        assert!(store.claim_guardian_merge(&id).unwrap());
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
    fn skip_worktree_checks_defaults_off_and_toggles_independently() {
        let store = Store::open_in_memory().unwrap();
        let id = store.create_guardian("r", "main", "/repo").unwrap();
        assert!(!store.get_guardian(&id).unwrap().skip_worktree_checks);
        assert!(!store.guardian_skip_worktree_checks(&id).unwrap());
        store.set_guardian_skip_worktree_checks(&id, true).unwrap();
        assert!(store.get_guardian(&id).unwrap().skip_worktree_checks);
        assert!(store.guardian_skip_worktree_checks(&id).unwrap());
        // Independent axis: toggling skip_worktree_checks must not affect
        // skip_auto_build (RAL-110 split the old single skip_checks flag).
        assert!(!store.get_guardian(&id).unwrap().skip_auto_build);
        assert!(
            store
                .set_guardian_skip_worktree_checks("nope", true)
                .is_err()
        );
    }

    #[test]
    fn verify_scope_defaults_to_inherited_each_branch_and_toggles_independently() {
        let store = Store::open_in_memory().unwrap();
        let id = store.create_guardian("r", "main", "/repo").unwrap();
        let g = store.get_guardian(&id).unwrap();
        assert_eq!(g.verify_scope, None);
        assert_eq!(g.effective_verify_scope, "each_branch");
        assert!(!g.effective_verify_skip_auto_clean);

        store
            .set_guardian_verify_scope(&id, Some("final_branch"))
            .unwrap();
        let g = store.get_guardian(&id).unwrap();
        assert_eq!(g.verify_scope.as_deref(), Some("final_branch"));
        assert_eq!(g.effective_verify_scope, "final_branch");

        store
            .set_guardian_verify_skip_auto_clean(&id, Some(true))
            .unwrap();
        let g = store.get_guardian(&id).unwrap();
        assert_eq!(g.verify_skip_auto_clean, Some(true));
        assert!(g.effective_verify_skip_auto_clean);
        // Independent axis: toggling skip_auto_clean must not affect the scope.
        assert_eq!(g.effective_verify_scope, "final_branch");

        // Resetting back to None restores "inherit the project default".
        store.set_guardian_verify_scope(&id, None).unwrap();
        let g = store.get_guardian(&id).unwrap();
        assert_eq!(g.verify_scope, None);
        assert_eq!(g.effective_verify_scope, "each_branch");

        assert!(
            store
                .set_guardian_verify_scope("nope", Some("nothing"))
                .is_err()
        );
        assert!(
            store
                .set_guardian_verify_skip_auto_clean("nope", Some(true))
                .is_err()
        );
    }

    #[test]
    fn verify_scope_unrecognized_stored_value_falls_back_to_each_branch() {
        // Defense in depth: a value that somehow got into the DB outside the
        // three recognized scopes (manual SQL edit, future rollback) must not
        // silently disable verification.
        let store = Store::open_in_memory().unwrap();
        let id = store.create_guardian("r", "main", "/repo").unwrap();
        store.set_guardian_verify_scope(&id, Some("bogus")).unwrap();
        let g = store.get_guardian(&id).unwrap();
        assert_eq!(g.effective_verify_scope, "each_branch");
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
}
