//! SQLite-backed task store — the daemon's authoritative state.
//!
//! Only the daemon opens this database (WAL mode); the CLI and librarian reach
//! it through the HTTP API. On submission a task file is fully ingested into
//! these tables, so the database — not any on-disk TOML — is the source of truth
//! (the predecessor learned this the hard way; see `FINDINGS.local.md` §2.4 and
//! CCTL-149).

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use ralphus_core::schema::{ResolvedAgent, TaskFile};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;

/// Errors the store can produce.
#[derive(Debug)]
pub enum StoreError {
    /// An underlying rusqlite error.
    Sqlite(rusqlite::Error),
    /// The requested run does not exist.
    NotFound,
    /// The operation is not valid for the run's current state.
    InvalidTransition(String),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Sqlite(e) => write!(f, "database error: {e}"),
            Self::NotFound => write!(f, "run not found"),
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

/// Lifecycle state of a whole run (submission). See `docs/daemon-api.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
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
    /// Reversible: a user can set an ignored run back to `pending`.
    Ignored,
}

impl RunState {
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

/// Execution state of a session or a task node.
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
}

// ── Read views (serialized straight to the API) ──────────────────────────────

/// One row from [`Store::verify_specs`]:
/// `(idx, kind, spec, model, timeout_sec, budget_tokens)`.
pub type VerifySpecRow = (
    i64,
    String,
    String,
    Option<String>,
    Option<i64>,
    Option<i64>,
);

/// A verify step as shown in the board.
#[derive(Debug, Clone, Serialize)]
pub struct VerifyView {
    /// Optional step id.
    pub id: Option<String>,
    /// One of `command` / `brain` / `prompt` / `approval`.
    pub kind: String,
    /// Current state string.
    pub state: String,
    /// Captured command output, once the verifier has run (CCTL-99).
    pub output: Option<String>,
    /// The step definition: command text, prompt text, or empty for brain/approval.
    pub spec: String,
    /// Model override (meaningful for `prompt`-kind steps).
    pub model: Option<String>,
    /// Resolved agent program (inherited from the owning session or task defaults).
    pub agent: String,
    /// Claude Code session UUID captured when the step ran via the claude-code
    /// backend. `None` for non-claude-code steps or steps that have not yet run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub claude_session_id: Option<String>,
}

/// A session as shown in the board.
#[derive(Debug, Clone, Serialize)]
pub struct SessionView {
    /// Session id (or a generated `session-N`).
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
    /// AI prompt, if a prompt session.
    pub prompt: Option<String>,
    /// Shell command, if a command session.
    pub command: Option<String>,
    /// Current state string.
    pub state: String,
    /// Input tokens recorded so far.
    pub tokens_in: i64,
    /// Output tokens recorded so far.
    pub tokens_out: i64,
    /// Cost recorded so far, USD.
    pub cost_usd: f64,
    /// Failure detail, when the session failed.
    pub error: Option<String>,
    /// Dependency references (within-task session ids or `task/session`).
    pub depends_on: Vec<String>,
    /// Session-level verify steps (`[[task.session.verify]]`), in order.
    pub verify: Vec<VerifyView>,
    /// Reviews (guardians) this session participates in — those whose stack
    /// includes the session's review branch (RAL-17). Empty for most sessions.
    pub reviews: Vec<RunReviewRef>,
    /// Claude Code session UUID for `claude --resume`, captured from the
    /// claude-code backend output. `None` for non-claude-code sessions or
    /// sessions that have not yet completed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub claude_session_id: Option<String>,
}

/// A task as shown in the board.
#[derive(Debug, Clone, Serialize)]
pub struct TaskView {
    /// Task name.
    pub name: String,
    /// Project label.
    pub project: Option<String>,
    /// Current state string.
    pub state: String,
    /// Sessions in the task.
    pub sessions: Vec<SessionView>,
    /// Task-level verify steps.
    pub verify: Vec<VerifyView>,
    /// Task-level dependency references (other task names).
    pub depends_on: Vec<String>,
}

/// A lightweight reference to a review (guardian) derived from a run.
#[derive(Debug, Clone, Serialize)]
pub struct RunReviewRef {
    /// Guardian id.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Guardian status string.
    pub status: String,
    /// The specific branch in the review stack this session contributes to.
    /// `None` for run-level review refs (not tied to a branch).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
}

/// One entry in the execution/transition log (CCTL-99).
#[derive(Debug, Clone, Serialize)]
pub struct EventView {
    /// Entity scope: `run` / `task` / `session` / `verify` / `guardian` / `branch`.
    pub scope: String,
    /// A reference within the scope (e.g. `session s1`, `b003`), if any.
    #[serde(rename = "ref")]
    pub reference: Option<String>,
    /// Human-readable transition/note.
    pub message: String,
    /// When it happened (Unix epoch milliseconds).
    pub at_ms: i64,
}

/// A run (submission) as shown in the board.
#[derive(Debug, Clone, Serialize)]
pub struct RunView {
    /// Run id, e.g. `run-000000000001`.
    pub id: String,
    /// Optional human label.
    pub label: Option<String>,
    /// Current run state string.
    pub state: String,
    /// Creation time (Unix epoch milliseconds).
    pub created_at_ms: i64,
    /// The tasks in the run.
    pub tasks: Vec<TaskView>,
    /// Reviews (guardians) derived from this run.
    pub reviews: Vec<RunReviewRef>,
}

/// A node in the cross-run `[[default]] depends_on` gating graph
/// (CLI_PARITY_PLAN.local.md Phase 6, `ralphus graph --global`).
#[derive(Debug, Clone, Serialize)]
pub struct GlobalGraphNode {
    /// Run id.
    pub id: String,
    /// Optional human label.
    pub label: Option<String>,
    /// Current run state string.
    pub state: String,
}

/// The cross-run gating graph: nodes are runs, edges are `[[default]]
/// depends_on` references (see [`Store::global_graph`]).
#[derive(Debug, Clone, Serialize)]
pub struct GlobalGraph {
    /// One entry per included run.
    pub nodes: Vec<GlobalGraphNode>,
    /// `from` (the dependency run) must complete before `to` (the dependent run).
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
    /// VCS kind. Only `"git"` is implemented today.
    pub vcs: String,
    /// Registration time (Unix epoch milliseconds).
    pub created_at_ms: i64,
}

/// Outcome of a bulk [`Store::clear_all`].
pub struct ClearOutcome {
    /// Number of runs deleted (with their sessions/tasks/verifies/events).
    pub runs_deleted: usize,
    /// Number of guardians (reviews) deleted, with their branches.
    pub guardians_deleted: usize,
    /// `(guardian_id, project_root)` pairs for each deleted guardian — one entry
    /// per project root — so the caller can purge all on-disk review worktrees.
    /// Multi-project guardians produce multiple entries with the same guardian_id.
    pub guardian_roots: Vec<(String, String)>,
}

// ── Store ────────────────────────────────────────────────────────────────────

/// The task store.
pub struct Store {
    pub(crate) conn: Connection,
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

impl Store {
    /// Open (creating if needed) a store at `path`, in WAL mode.
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        let store = Self { conn };
        store.init_schema()?;
        Ok(store)
    }

    /// Open an in-memory store (used by tests).
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        let store = Self { conn };
        store.init_schema()?;
        Ok(store)
    }

    fn init_schema(&self) -> Result<()> {
        self.conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS meta (
                key   TEXT PRIMARY KEY,
                value INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS runs (
                id            TEXT PRIMARY KEY,
                label         TEXT,
                state         TEXT NOT NULL,
                depends_on    TEXT NOT NULL DEFAULT '[]',
                created_at_ms INTEGER NOT NULL,
                updated_at_ms INTEGER NOT NULL,
                trace_context TEXT
            );
            CREATE TABLE IF NOT EXISTS tasks (
                run_id     TEXT NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
                idx        INTEGER NOT NULL,
                name       TEXT NOT NULL,
                project    TEXT,
                state      TEXT NOT NULL,
                depends_on TEXT NOT NULL DEFAULT '[]',
                queue_rank REAL,
                PRIMARY KEY (run_id, idx)
            );
            CREATE TABLE IF NOT EXISTS sessions (
                run_id     TEXT NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
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
                state      TEXT NOT NULL,
                depends_on TEXT NOT NULL DEFAULT '[]',
                tokens_in  INTEGER NOT NULL DEFAULT 0,
                tokens_out INTEGER NOT NULL DEFAULT 0,
                cost_usd   REAL NOT NULL DEFAULT 0,
                error      TEXT,
                review_branch TEXT,
                timeout_sec   INTEGER,
                budget_tokens INTEGER,
                claude_session_id TEXT,
                upstream      TEXT,
                queue_rank    REAL,
                PRIMARY KEY (run_id, task_idx, idx)
            );
            CREATE TABLE IF NOT EXISTS verifies (
                run_id      TEXT NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
                task_idx    INTEGER NOT NULL,
                scope       TEXT NOT NULL,
                session_idx INTEGER NOT NULL,
                idx         INTEGER NOT NULL,
                vid         TEXT,
                kind        TEXT NOT NULL,
                spec        TEXT NOT NULL,
                model       TEXT,
                agent       TEXT NOT NULL DEFAULT 'claude',
                state       TEXT NOT NULL,
                output      TEXT,
                claude_session_id TEXT,
                timeout_sec   INTEGER,
                budget_tokens INTEGER,
                queue_rank    REAL,
                PRIMARY KEY (run_id, task_idx, scope, session_idx, idx)
            );
            CREATE TABLE IF NOT EXISTS guardians (
                id                TEXT PRIMARY KEY,
                name              TEXT NOT NULL,
                base_branch       TEXT NOT NULL,
                base_commit       TEXT,
                git_root          TEXT NOT NULL,
                review_branch     TEXT,
                status            TEXT NOT NULL,
                detail            TEXT,
                checks            TEXT NOT NULL DEFAULT '[]',
                run_id            TEXT,
                combined_worktree TEXT,
                conflicts_total     INTEGER,
                conflicts_remaining INTEGER,
                conflicts_found     INTEGER,
                conflicts_fixed     INTEGER,
                conflicts_committed INTEGER,
                skip_auto_build   INTEGER NOT NULL DEFAULT 0,
                skip_worktree_checks INTEGER NOT NULL DEFAULT 0,
                review_type       TEXT NOT NULL DEFAULT 'git',
                skip_worktrees    INTEGER NOT NULL DEFAULT 0,
                squash_projects   TEXT NOT NULL DEFAULT '[]',
                review_key        TEXT,
                resolver_agent    TEXT,
                resolver_model    TEXT,
                created_at_ms     INTEGER NOT NULL,
                updated_at_ms     INTEGER NOT NULL
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
                PRIMARY KEY (guardian_id, position)
            );
            CREATE TABLE IF NOT EXISTS events (
                seq         INTEGER PRIMARY KEY AUTOINCREMENT,
                run_id      TEXT,
                guardian_id TEXT,
                scope       TEXT NOT NULL,
                ref         TEXT,
                message     TEXT NOT NULL,
                at_ms       INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_events_run ON events(run_id, seq);
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
                run_id      TEXT,
                guardian_id TEXT,
                session_id  TEXT,
                task        TEXT,
                payload     TEXT NOT NULL DEFAULT '{}'
            );
            CREATE INDEX IF NOT EXISTS idx_carto_at ON cartographer_events(at_ms);
            CREATE INDEX IF NOT EXISTS idx_carto_run ON cartographer_events(run_id);
            CREATE INDEX IF NOT EXISTS idx_carto_guardian ON cartographer_events(guardian_id);
            CREATE INDEX IF NOT EXISTS idx_carto_session ON cartographer_events(session_id);
            CREATE INDEX IF NOT EXISTS idx_carto_source ON cartographer_events(source);
            CREATE TABLE IF NOT EXISTS projects (
                name          TEXT PRIMARY KEY,
                description   TEXT NOT NULL DEFAULT '',
                path          TEXT NOT NULL,
                vcs           TEXT NOT NULL DEFAULT 'git',
                created_at_ms INTEGER NOT NULL
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
            -- RAL-136: ephemeral, queryable handoff notes ('ghosts') a task
            -- session or review worktree publishes for downstream work.  One
            -- row per owner (`owner_uri`) -- a rewrite merges onto the
            -- existing row rather than inserting a second one (see
            -- `ghost::merge_content`). `run_id`/`guardian_id` are mutually
            -- exclusive depending on `kind` and exist so a run/guardian
            -- deletion can cascade-clean its ghosts (done explicitly in
            -- `delete_run`/`delete_guardian`/`clear_all`, like every other
            -- child table -- see the comment on `delete_run`).
            CREATE TABLE IF NOT EXISTS ghosts (
                owner_uri     TEXT PRIMARY KEY,
                kind          TEXT NOT NULL,
                run_id        TEXT REFERENCES runs(id) ON DELETE CASCADE,
                guardian_id   TEXT REFERENCES guardians(id) ON DELETE CASCADE,
                content       TEXT NOT NULL,
                revision      TEXT,
                created_at_ms INTEGER NOT NULL,
                updated_at_ms INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_ghosts_run ON ghosts(run_id);
            CREATE INDEX IF NOT EXISTS idx_ghosts_guardian ON ghosts(guardian_id);
            ",
        )?;
        // Best-effort migrations for databases created before these columns
        // existed. Each fails harmlessly (duplicate column) once present.
        for stmt in [
            "ALTER TABLE guardians ADD COLUMN run_id TEXT",
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
            "ALTER TABLE verifies ADD COLUMN model TEXT",
            "ALTER TABLE verifies ADD COLUMN agent TEXT NOT NULL DEFAULT 'claude'",
            "ALTER TABLE verifies ADD COLUMN claude_session_id TEXT",
            "ALTER TABLE sessions ADD COLUMN review_branch TEXT",
            "ALTER TABLE sessions ADD COLUMN timeout_sec INTEGER",
            "ALTER TABLE sessions ADD COLUMN budget_tokens INTEGER",
            "ALTER TABLE sessions ADD COLUMN system_prompt TEXT",
            "ALTER TABLE sessions ADD COLUMN system_prompt_position TEXT",
            "ALTER TABLE sessions ADD COLUMN subprojects TEXT",
            "ALTER TABLE sessions ADD COLUMN name TEXT",
            "ALTER TABLE sessions ADD COLUMN claude_session_id TEXT",
            "ALTER TABLE verifies ADD COLUMN timeout_sec INTEGER",
            "ALTER TABLE verifies ADD COLUMN budget_tokens INTEGER",
            // RAL-50: branch-chaining upstream sentinel.
            "ALTER TABLE sessions ADD COLUMN upstream TEXT",
            // RAL-69: tracks whether the user dismissed the "can re-enable" icon
            // on a force-started disabled branch. Once dismissed it never reappears.
            "ALTER TABLE guardian_branches ADD COLUMN dismissed_reenable INTEGER NOT NULL DEFAULT 0",
            // RAL-77: user-declared action hints from [[review.action]] in TOML.
            // JSON array of {label, command?, prompt?} objects, set at submit time.
            "ALTER TABLE guardians ADD COLUMN action_hints TEXT NOT NULL DEFAULT '[]'",
            // RAL-59: optional base64 image attached to a chat message.
            "ALTER TABLE guardian_messages ADD COLUMN image TEXT",
            // claude_session_id from the conflict-resolver run on this branch.
            // Only set when the resolver backend is claude-code; enables terminal resume.
            "ALTER TABLE guardian_branches ADD COLUMN resolver_claude_session_id TEXT",
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
            "ALTER TABLE sessions ADD COLUMN queue_rank REAL",
            "ALTER TABLE verifies ADD COLUMN queue_rank REAL",
            // RAL-96: the W3C `traceparent` of the request that created this run
            // (browser click or CLI submit), persisted so the scheduler's later,
            // asynchronous work (run-claim, session execution, verify execution)
            // continues the same OpenTelemetry trace instead of starting a new one.
            "ALTER TABLE runs ADD COLUMN trace_context TEXT",
            // RAL-110: split the old single `skip_checks` flag into two independent
            // opt-outs -- see the backfill-and-drop block below, which carries
            // forward any existing `skip_checks` value into both.
            "ALTER TABLE guardians ADD COLUMN skip_auto_build INTEGER NOT NULL DEFAULT 0",
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
        ] {
            let _ = self.conn.execute(stmt, []);
        }
        // RAL-121: `hydrate_guardian` looks up each branch's most recent session
        // by `review_branch` (set once, at submit time, by
        // `reviews::derive_reviews` -> `set_session_review_branch`; RAL-118's
        // `move_guardian_branch` reassigns a branch's *guardian*, never its
        // `sessions.review_branch` value, so this stays correct across moves).
        // Without an index every guardian-list load did a full `sessions` table
        // scan per branch; `sessions` only grows over a project's life. Created
        // after the ALTER-TABLE migrations above so it's safe against a database
        // created before the `review_branch` column existed.
        let _ = self.conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_sessions_review_branch ON sessions(review_branch)",
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

    fn next_run_id(&self) -> Result<String> {
        self.next_id("run_seq", "run")
    }

    /// Ingest a validated task file, returning the new run id. `hold` submits to
    /// `Queued` (staged); otherwise the run goes straight to `Pending`.
    pub fn insert_run(
        &mut self,
        file: &TaskFile,
        label: Option<&str>,
        hold: bool,
    ) -> Result<String> {
        let run_id = self.next_run_id()?;
        let now = now_ms();
        let state = if hold {
            RunState::Queued
        } else {
            RunState::Pending
        };

        let run_deps = file
            .defaults
            .first()
            .map(|d| d.depends_on.as_slice())
            .unwrap_or(&[]);
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO runs(id, label, state, depends_on, created_at_ms, updated_at_ms) VALUES(?,?,?,?,?,?)",
            params![run_id, label, state.as_str(), to_json(run_deps), now, now],
        )?;

        for (t_idx, task) in file.task.iter().enumerate() {
            let t_idx_i = i64::try_from(t_idx).unwrap_or(0);
            tx.execute(
                "INSERT INTO tasks(run_id, idx, name, project, state, depends_on, queue_rank) VALUES(?,?,?,?,?,?,?)",
                params![
                    run_id,
                    t_idx_i,
                    task.name,
                    task.project,
                    NodeState::Pending.as_str(),
                    to_json(&task.depends_on),
                    task.priority.map(f64::from),
                ],
            )?;

            for (s_idx, session) in task.session.iter().enumerate() {
                let resolved = ResolvedAgent::resolve(task, session);
                let sid = session
                    .id
                    .clone()
                    .unwrap_or_else(|| format!("session-{s_idx}"));
                // A session inherits the task's timeout/budget unless it sets its
                // own. Timeout is stored in seconds; budget in total tokens.
                let timeout_sec =
                    resolve_timeout_sec(session.timeout_minutes, task.timeout_minutes);
                let budget_tokens = resolve_budget(session.budget_tokens, task.budget_tokens);
                tx.execute(
                    "INSERT INTO sessions(run_id, task_idx, idx, sid, name, cwd, subprojects, prompt, command, agent, model, system_prompt, system_prompt_position, state, depends_on, timeout_sec, budget_tokens, upstream, queue_rank)
                     VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
                    params![
                        run_id,
                        t_idx_i,
                        i64::try_from(s_idx).unwrap_or(0),
                        sid,
                        session.name,
                        session.cwd,
                        to_json(&session.subprojects),
                        session.prompt,
                        session.command,
                        resolved.program,
                        resolved.model,
                        session.system_prompt,
                        session.system_prompt_position,
                        NodeState::Pending.as_str(),
                        to_json(&session.depends_on),
                        timeout_sec,
                        budget_tokens,
                        session.upstream,
                        // Seed the queue rank from the session's own priority, or
                        // the owning task's priority as a fallback, so a task-level
                        // `priority` nudges all its sessions' starting position.
                        session.priority.or(task.priority).map(f64::from),
                    ],
                )?;

                for (v_idx, v) in session.verify.iter().enumerate() {
                    insert_verify(
                        &tx,
                        &run_id,
                        t_idx_i,
                        "session",
                        i64::try_from(s_idx).unwrap_or(0),
                        v_idx,
                        v,
                        task,
                        &resolved.program,
                    )?;
                }
            }

            // Task-level verifies inherit the first session's resolved agent to
            // match the scheduler's runtime behaviour (scheduler takes
            // `task_session.map(|s| s.agent)`). Fall back to the task-level
            // agent field when there are no sessions.
            let task_verify_agent = task
                .session
                .first()
                .map(|s| ResolvedAgent::resolve(task, s).program)
                .unwrap_or_else(|| ResolvedAgent::from_task(task).program);
            for (v_idx, v) in task.verify.iter().enumerate() {
                insert_verify(
                    &tx,
                    &run_id,
                    t_idx_i,
                    "task",
                    -1,
                    v_idx,
                    v,
                    task,
                    &task_verify_agent,
                )?;
            }
        }

        tx.commit()?;
        crate::rlog!(
            INFO,
            "ralphus [submit] run {run_id} inserted state={} tasks={}",
            state.as_str(),
            file.task.len()
        );
        let _ = self.cartographer_log(crate::cartographer::CartographerEntry {
            level: crate::logging::LogLevel::INFO,
            source: "submit",
            message: "run inserted",
            scope: Some("run"),
            run_id: Some(&run_id),
            guardian_id: None,
            session_id: None,
            task: None,
            payload: serde_json::json!({"state": state.as_str(), "tasks": file.task.len()}),
        });
        Ok(run_id)
    }

    /// Append an entry to the execution/transition log (CCTL-99). Failures to
    /// log are swallowed by callers (a missing audit line must never break a
    /// transition), so this returns the raw rusqlite result only for tests.
    pub fn log_event(
        &self,
        run_id: Option<&str>,
        guardian_id: Option<&str>,
        scope: &str,
        reference: Option<&str>,
        message: &str,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO events(run_id, guardian_id, scope, ref, message, at_ms)
             VALUES(?,?,?,?,?,?)",
            params![run_id, guardian_id, scope, reference, message, now_ms()],
        )?;
        // Cartographer subsumes this per-run/per-guardian audit trail (RAL-98):
        // every `log_event` call also lands in the global structured log, so
        // the two never drift apart.
        let _ = self.cartographer_log(crate::cartographer::CartographerEntry {
            level: crate::logging::LogLevel::INFO,
            source: "store",
            message,
            scope: Some(scope),
            run_id,
            guardian_id,
            session_id: None,
            task: None,
            payload: reference.map_or(serde_json::json!({}), |r| serde_json::json!({"ref": r})),
        });
        Ok(())
    }

    /// The audit log for a run, oldest first, capped at `limit` most-recent rows.
    pub fn events_for_run(&self, run_id: &str, limit: i64) -> Result<Vec<EventView>> {
        self.events_where("run_id", run_id, limit)
    }

    /// The audit log for a guardian (review cycle), oldest first, capped.
    pub fn events_for_guardian(&self, guardian_id: &str, limit: i64) -> Result<Vec<EventView>> {
        self.events_where("guardian_id", guardian_id, limit)
    }

    fn events_where(&self, column: &str, value: &str, limit: i64) -> Result<Vec<EventView>> {
        // `column` is a fixed internal literal ("run_id"/"guardian_id"), never
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

    /// The current state of a run.
    pub fn run_state(&self, id: &str) -> Result<RunState> {
        let s: Option<String> = self
            .conn
            .query_row("SELECT state FROM runs WHERE id=?", params![id], |r| {
                r.get(0)
            })
            .optional()?;
        let s = s.ok_or(StoreError::NotFound)?;
        RunState::parse(&s).ok_or(StoreError::NotFound)
    }

    /// Set a run's state.
    pub fn set_run_state(&self, id: &str, state: RunState) -> Result<()> {
        let old = self.run_state(id).map(|s| s.as_str()).unwrap_or("unknown");
        let n = self.conn.execute(
            "UPDATE runs SET state=?, updated_at_ms=? WHERE id=?",
            params![state.as_str(), now_ms(), id],
        )?;
        if n == 0 {
            Err(StoreError::NotFound)
        } else {
            crate::rlog!(INFO, "ralphus [state] run {id} {old} → {}", state.as_str());
            let _ = self.log_event(
                Some(id),
                None,
                "run",
                None,
                &format!("run → {}", state.as_str()),
            );
            Ok(())
        }
    }

    /// The W3C `traceparent` recorded against a run at submit time (RAL-96),
    /// if the submitting request carried one. `Ok(None)` for a run submitted
    /// with no trace context (or one predating this column).
    pub fn run_trace_context(&self, id: &str) -> Result<Option<String>> {
        self.conn
            .query_row(
                "SELECT trace_context FROM runs WHERE id=?",
                params![id],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()?
            .ok_or(StoreError::NotFound)
    }

    /// Record the `traceparent` of the request that created `id` (RAL-96), so
    /// the scheduler's later asynchronous work (run-claim, session execution,
    /// verify execution) can rebuild a [`crate::otel::Context`] that continues
    /// the same trace instead of starting a disconnected one.
    pub fn set_run_trace_context(&self, id: &str, trace_context: &str) -> Result<()> {
        let n = self.conn.execute(
            "UPDATE runs SET trace_context=? WHERE id=?",
            params![trace_context, id],
        )?;
        if n == 0 {
            Err(StoreError::NotFound)
        } else {
            Ok(())
        }
    }

    /// Activate a held (`Queued`) run, moving it to `Pending`.
    pub fn activate(&self, id: &str) -> Result<RunState> {
        match self.run_state(id)? {
            RunState::Queued => {
                self.set_run_state(id, RunState::Pending)?;
                Ok(RunState::Pending)
            }
            other => Err(StoreError::InvalidTransition(format!(
                "can only activate a queued run, run is {}",
                other.as_str()
            ))),
        }
    }

    /// Cancel a run, regardless of its current state (RAL-116). Always
    /// available and idempotent — even a run that already reached a terminal
    /// state (`done`/`failed`/already `cancelled`) is (re-)flipped to
    /// `cancelled`, so it can never be picked up again by another trigger
    /// (a restart, cross-run gating, etc). In-flight nodes are flipped too;
    /// the worker thread stops on its own via the cancel token, so it will
    /// not re-run any node this flips.
    pub fn cancel(&self, id: &str) -> Result<RunState> {
        self.run_state(id)?;
        self.set_run_state(id, RunState::Cancelled)?;
        self.cancel_nonterminal_nodes(id)?;
        Ok(RunState::Cancelled)
    }

    /// Flip every task/session/verify still in a non-terminal state
    /// (`pending`/`running`) to `cancelled`, so the board reflects a cancelled
    /// run immediately. Terminal nodes (`done`/`failed`/already `cancelled`) are
    /// left untouched — a session that already finished keeps its real outcome.
    /// The worker thread stops on its own via the cancel token, so it will not
    /// re-run any node this flips.
    fn cancel_nonterminal_nodes(&self, run_id: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET state='cancelled' WHERE run_id=? AND state IN ('pending','running')",
            params![run_id],
        )?;
        self.conn.execute(
            "UPDATE tasks SET state='cancelled' WHERE run_id=? AND state IN ('pending','running')",
            params![run_id],
        )?;
        self.conn.execute(
            "UPDATE verifies SET state='cancelled' WHERE run_id=? AND state IN ('pending','running')",
            params![run_id],
        )?;
        Ok(())
    }

    /// Run ids that are ready to schedule: Pending, and with every cross-run
    /// dependency (from the run's `[[default]]` `depends_on`) already Done.
    /// A dependency reference `run-id` or `run-id/task/session` is satisfied when
    /// that whole run is Done (path-precise gating is a later refinement).
    pub fn list_ready(&self) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, depends_on FROM runs WHERE state='pending' ORDER BY created_at_ms ASC",
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

    /// Whether every cross-run dependency reference points at a Done run.
    fn deps_satisfied(&self, deps: &[String]) -> Result<bool> {
        for dep in deps {
            let dep_run = dep.split('/').next().unwrap_or(dep);
            let state: Option<String> = self
                .conn
                .query_row("SELECT state FROM runs WHERE id=?", params![dep_run], |r| {
                    r.get(0)
                })
                .optional()?;
            match state.as_deref().and_then(RunState::parse) {
                Some(s) if s.satisfies_dependents() => {}
                _ => return Ok(false),
            }
        }
        Ok(true)
    }

    /// Count of currently running runs.
    pub fn running_count(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("SELECT COUNT(*) FROM runs WHERE state='running'", [], |r| {
                r.get(0)
            })?)
    }

    /// Count of sessions currently executing — the real in-flight work, bounded
    /// by the scheduler's task-level concurrency limit. Unlike `running_count`
    /// (which counts runs), this reflects how many agent sessions run at once.
    pub fn running_session_count(&self) -> Result<i64> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*) FROM sessions WHERE state='running'",
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

    /// Set a session's state.
    pub fn set_session_state(
        &self,
        run_id: &str,
        task_idx: i64,
        idx: i64,
        state: NodeState,
    ) -> Result<()> {
        let old = self
            .conn
            .query_row(
                "SELECT state FROM sessions WHERE run_id=? AND task_idx=? AND idx=?",
                params![run_id, task_idx, idx],
                |r| r.get::<_, String>(0),
            )
            .optional()
            .ok()
            .flatten()
            .unwrap_or_else(|| "unknown".to_string());
        self.conn.execute(
            "UPDATE sessions SET state=? WHERE run_id=? AND task_idx=? AND idx=?",
            params![state.as_str(), run_id, task_idx, idx],
        )?;
        crate::rlog!(
            DEBUG,
            "ralphus [state] session {run_id}/t{task_idx}/s{idx} {old} → {}",
            state.as_str()
        );
        let _ = self.log_event(
            Some(run_id),
            None,
            "session",
            Some(&format!("t{task_idx}/s{idx}")),
            &format!("session → {}", state.as_str()),
        );
        Ok(())
    }

    /// Set a task node's state.
    pub fn set_task_state(&self, run_id: &str, task_idx: i64, state: NodeState) -> Result<()> {
        let old = self
            .conn
            .query_row(
                "SELECT state FROM tasks WHERE run_id=? AND idx=?",
                params![run_id, task_idx],
                |r| r.get::<_, String>(0),
            )
            .optional()
            .ok()
            .flatten()
            .unwrap_or_else(|| "unknown".to_string());
        self.conn.execute(
            "UPDATE tasks SET state=? WHERE run_id=? AND idx=?",
            params![state.as_str(), run_id, task_idx],
        )?;
        crate::rlog!(
            INFO,
            "ralphus [state] task {run_id}/t{task_idx} {old} → {}",
            state.as_str()
        );
        let _ = self.log_event(
            Some(run_id),
            None,
            "task",
            Some(&format!("t{task_idx}")),
            &format!("task → {}", state.as_str()),
        );
        Ok(())
    }

    /// Fetch a single run's full board view.
    pub fn get_run(&self, id: &str) -> Result<RunView> {
        let row = self
            .conn
            .query_row(
                "SELECT id, label, state, created_at_ms FROM runs WHERE id=?",
                params![id],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, Option<String>>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, i64>(3)?,
                    ))
                },
            )
            .optional()?
            .ok_or(StoreError::NotFound)?;
        self.build_run_view(row.0, row.1, row.2, row.3)
    }

    /// Fetch all runs, newest first.
    pub fn list_runs(&self) -> Result<Vec<RunView>> {
        // Tie-break on id so runs created within the same millisecond still order
        // deterministically. Run ids are monotonic, zero-padded, fixed-width, so
        // lexicographic `id DESC` == newest-first.
        let mut stmt = self.conn.prepare(
            "SELECT id, label, state, created_at_ms FROM runs ORDER BY created_at_ms DESC, id DESC",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, i64>(3)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        rows.into_iter()
            .map(|(id, label, state, ts)| self.build_run_view(id, label, state, ts))
            .collect()
    }

    /// The cross-run `[[default]] depends_on` gating graph across every
    /// submitted run (CLI_PARITY_PLAN.local.md Phase 6, `ralphus graph
    /// --global`). `include_terminal` selects between "active runs only"
    /// (queued/pending/running -- the default per plan Q5) and "every run
    /// including done/failed/cancelled" (`--all`).
    ///
    /// A dependency reference to a run outside the included set (filtered out,
    /// or simply unresolvable) produces no edge -- same best-effort philosophy
    /// as [`crate::plan::plan`] for within-run refs.
    pub fn global_graph(&self, include_terminal: bool) -> Result<GlobalGraph> {
        let all_runs = self.list_runs()?;
        let included: Vec<&RunView> = all_runs
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
            for dep in self.run_depends_on(&r.id)? {
                let dep_run = dep.split('/').next().unwrap_or(&dep);
                if included_ids.contains(dep_run) {
                    edges.push(crate::plan::GraphEdge {
                        from: dep_run.to_string(),
                        to: r.id.clone(),
                    });
                }
            }
        }
        Ok(GlobalGraph { nodes, edges })
    }

    fn build_run_view(
        &self,
        id: String,
        label: Option<String>,
        state: String,
        created_at_ms: i64,
    ) -> Result<RunView> {
        let mut tstmt = self.conn.prepare(
            "SELECT idx, name, project, state, depends_on FROM tasks WHERE run_id=? ORDER BY idx",
        )?;
        let task_rows = tstmt
            .query_map(params![id], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        // Map each guardian branch of this run back to its review, so a session
        // whose review branch is in a guardian's stack lists that review (RAL-17).
        let review_by_branch = self.reviews_by_branch(&id)?;
        let mut tasks = Vec::with_capacity(task_rows.len());
        for (t_idx, name, project, tstate, deps) in task_rows {
            tasks.push(TaskView {
                name,
                project,
                state: tstate,
                sessions: self.sessions_for(&id, t_idx, &review_by_branch)?,
                verify: self.verifies_for(&id, t_idx, "task", -1)?,
                depends_on: from_json(&deps),
            });
        }

        let reviews = self.reviews_for_run(&id)?;
        Ok(RunView {
            id,
            label,
            state,
            created_at_ms,
            tasks,
            reviews,
        })
    }

    /// The reviews (guardians) derived from a run, oldest first.
    fn reviews_for_run(&self, run_id: &str) -> Result<Vec<RunReviewRef>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, name, status FROM guardians WHERE run_id=? ORDER BY created_at_ms, id",
        )?;
        let rows = stmt
            .query_map(params![run_id], |r| {
                Ok(RunReviewRef {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    status: r.get(2)?,
                    branch: None,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    fn sessions_for(
        &self,
        run_id: &str,
        task_idx: i64,
        review_by_branch: &HashMap<String, Vec<RunReviewRef>>,
    ) -> Result<Vec<SessionView>> {
        // Fetch the session rows first (dropping the statement), then attach each
        // session's own verify steps — `verifies_for` re-borrows `self.conn`.
        let mut rows: Vec<(i64, SessionView)> = {
            let mut stmt = self.conn.prepare(
                "SELECT idx, sid, name, cwd, agent, model, state, tokens_in, tokens_out, cost_usd, error, prompt, command, depends_on, review_branch, claude_session_id
                 FROM sessions WHERE run_id=? AND task_idx=? ORDER BY idx",
            )?;
            stmt.query_map(params![run_id, task_idx], |r| {
                let review_branch: Option<String> = r.get(14)?;
                let reviews = review_branch
                    .and_then(|b| review_by_branch.get(&b).cloned())
                    .unwrap_or_default();
                Ok((
                    r.get::<_, i64>(0)?,
                    SessionView {
                        id: r.get::<_, String>(1)?,
                        name: r.get::<_, Option<String>>(2)?,
                        cwd: r.get::<_, Option<String>>(3)?,
                        agent: r.get::<_, String>(4)?,
                        model: r.get::<_, Option<String>>(5)?,
                        state: r.get::<_, String>(6)?,
                        tokens_in: r.get::<_, i64>(7)?,
                        tokens_out: r.get::<_, i64>(8)?,
                        cost_usd: r.get::<_, f64>(9)?,
                        error: r.get::<_, Option<String>>(10)?,
                        prompt: r.get::<_, Option<String>>(11)?,
                        command: r.get::<_, Option<String>>(12)?,
                        depends_on: from_json(&r.get::<_, String>(13)?),
                        verify: Vec::new(),
                        reviews,
                        claude_session_id: r.get::<_, Option<String>>(15)?,
                    },
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?
        };
        for (idx, session) in rows.iter_mut() {
            session.verify = self.verifies_for(run_id, task_idx, "session", *idx)?;
            session.state = effective_session_state(&session.state, &session.verify);
        }
        Ok(rows.into_iter().map(|(_, session)| session).collect())
    }

    /// Map each of this run's session branches back to the reviews containing
    /// it, so a session can list the reviews its branch participates in
    /// (RAL-17). Joins on `sessions.review_branch = guardian_branches.branch`
    /// rather than filtering guardians by `guardians.run_id`, because a
    /// guardian can be *found* (not created) by a later submission that shares
    /// a `ralphus:new-review/<key>` link or was attached to manually — its
    /// `run_id` then still points at whichever run created it, even though a
    /// different run's session branch was appended to it (see
    /// `collecting_guardians_for_sessions`, which needs the same join).
    fn reviews_by_branch(&self, run_id: &str) -> Result<HashMap<String, Vec<RunReviewRef>>> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT gb.branch, g.id, g.name, g.status
             FROM guardian_branches gb
             JOIN guardians g ON g.id = gb.guardian_id
             JOIN sessions s ON s.review_branch = gb.branch
             WHERE s.run_id = ?
             ORDER BY g.created_at_ms, g.id",
        )?;
        let rows = stmt
            .query_map(params![run_id], |r| {
                let branch: String = r.get(0)?;
                Ok((
                    branch.clone(),
                    RunReviewRef {
                        id: r.get(1)?,
                        name: r.get(2)?,
                        status: r.get(3)?,
                        branch: Some(branch),
                    },
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let mut map: HashMap<String, Vec<RunReviewRef>> = HashMap::new();
        for (branch, rref) in rows {
            map.entry(branch).or_default().push(rref);
        }
        Ok(map)
    }

    pub(crate) fn verifies_for(
        &self,
        run_id: &str,
        task_idx: i64,
        scope: &str,
        session_idx: i64,
    ) -> Result<Vec<VerifyView>> {
        let mut stmt = self.conn.prepare(
            "SELECT vid, kind, state, output, spec, model, agent, claude_session_id FROM verifies
             WHERE run_id=? AND task_idx=? AND scope=? AND session_idx=? ORDER BY idx",
        )?;
        let rows = stmt
            .query_map(params![run_id, task_idx, scope, session_idx], |r| {
                Ok(VerifyView {
                    id: r.get::<_, Option<String>>(0)?,
                    kind: r.get::<_, String>(1)?,
                    state: r.get::<_, String>(2)?,
                    output: r.get::<_, Option<String>>(3)?,
                    spec: r.get::<_, String>(4)?,
                    model: r.get::<_, Option<String>>(5)?,
                    agent: r.get::<_, String>(6)?,
                    claude_session_id: r.get::<_, Option<String>>(7)?,
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
        self.conn.execute(
            "INSERT INTO projects(name, description, path, vcs, created_at_ms) VALUES(?,?,?,?,?)
             ON CONFLICT(name) DO UPDATE SET description=excluded.description, path=excluded.path, vcs=excluded.vcs",
            params![name, description, path, vcs, now_ms()],
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
            run_id: None,
            guardian_id: None,
            session_id: None,
            task: None,
            payload: serde_json::json!({"name": name, "path": path, "vcs": vcs}),
        });
        Ok(())
    }

    /// A project by its exact registered name, or `None` when absent.
    pub fn get_project(&self, name: &str) -> Result<Option<ProjectView>> {
        self.conn
            .query_row(
                "SELECT name, description, path, vcs, created_at_ms FROM projects WHERE name=?",
                params![name],
                |r| {
                    Ok(ProjectView {
                        name: r.get(0)?,
                        description: r.get(1)?,
                        path: r.get(2)?,
                        vcs: r.get(3)?,
                        created_at_ms: r.get(4)?,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    /// All registered projects, newest first.
    pub fn list_projects(&self) -> Result<Vec<ProjectView>> {
        let mut stmt = self.conn.prepare(
            "SELECT name, description, path, vcs, created_at_ms FROM projects ORDER BY created_at_ms DESC, name",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(ProjectView {
                    name: r.get(0)?,
                    description: r.get(1)?,
                    path: r.get(2)?,
                    vcs: r.get(3)?,
                    created_at_ms: r.get(4)?,
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

    /// Rewrite a session's `cwd` — used to resolve a worktree placeholder
    /// (`ralphus:new-worktree/<branch>`) to its real materialized path
    /// (RAL-100) before the session runs.
    pub fn set_session_cwd(&self, run_id: &str, task_idx: i64, idx: i64, cwd: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET cwd=? WHERE run_id=? AND task_idx=? AND idx=?",
            params![cwd, run_id, task_idx, idx],
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

/// The state shown for a session in the board API/UI.
///
/// The persisted `sessions.state` column is set to `done` as soon as the
/// session's own agent body finishes, *before* its session-level verify
/// steps run — deliberately, so crash recovery can tell "body finished,
/// verify pending" apart from a fresh session and only re-run the verify
/// tail rather than redoing the (expensive) agent work (RAL-64; see
/// `Store::done_sessions`). Left as-is, that reads to a viewer as the
/// session being finished while its verify checklist is still running or
/// has failed. This folds verify progress back in for display only, without
/// touching the persisted column the scheduler relies on.
fn effective_session_state(raw: &str, verify: &[VerifyView]) -> String {
    if raw != "done" {
        return raw.to_string();
    }
    if verify.iter().any(|v| v.state == "failed") {
        return "failed".to_string();
    }
    if verify.iter().any(|v| {
        !matches!(
            v.state.as_str(),
            "done" | "failed" | "cancelled" | "ignored"
        )
    }) {
        return "running".to_string();
    }
    raw.to_string()
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

#[allow(clippy::too_many_arguments)]
fn insert_verify(
    tx: &rusqlite::Transaction<'_>,
    run_id: &str,
    task_idx: i64,
    scope: &str,
    session_idx: i64,
    v_idx: usize,
    v: &ralphus_core::schema::VerifyStep,
    task: &ralphus_core::schema::TaskDef,
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
    tx.execute(
        "INSERT INTO verifies(run_id, task_idx, scope, session_idx, idx, vid, kind, spec, model, agent, state, timeout_sec, budget_tokens)
         VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?)",
        params![
            run_id,
            task_idx,
            scope,
            session_idx,
            i64::try_from(v_idx).unwrap_or(0),
            v.id,
            kind,
            spec,
            v.model,
            agent,
            NodeState::Pending.as_str(),
            timeout_sec,
            budget_tokens,
        ],
    )?;
    Ok(())
}

/// The executable fields of a session, as the scheduler needs them to build a
/// runner spec.
#[derive(Debug, Clone)]
pub struct SessionRow {
    /// Index of the owning task within the run.
    pub task_idx: i64,
    /// Index of the session within the task.
    pub idx: i64,
    /// Owning task name.
    pub task_name: String,
    /// Session id.
    pub session_id: String,
    /// Working directory.
    pub cwd: Option<String>,
    /// Subproject paths within the repository root (e.g. `["packages/foo"]`).
    /// Empty when the session targets the whole repository (RAL-23).
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
    /// This session's dependency references (within-task session ids or
    /// cross-task `task/session`).
    pub depends_on: Vec<String>,
    /// Effective wall-clock timeout in seconds (resolved from session/task), or
    /// `None` for no limit. The daemon kills the runner subprocess past this.
    pub timeout_sec: Option<i64>,
    /// Effective total-token budget (resolved from session/task), or `None` for
    /// no cap. The runner fails the session if usage exceeds it.
    pub budget_tokens: Option<i64>,
    /// Upstream sentinel, e.g. `"<<task:task-name>>"`. When present the
    /// scheduler rebases this session's branch onto the named dependency's
    /// current branch tip before starting the runner (RAL-50).
    pub upstream: Option<String>,
}

/// Editable session definition fields (from the details pane).
#[derive(Debug, Clone)]
pub struct SessionEdit<'a> {
    /// Working directory.
    pub cwd: Option<&'a str>,
    /// Agent program.
    pub agent: &'a str,
    /// Model.
    pub model: Option<&'a str>,
    /// AI prompt (mutually exclusive with command).
    pub prompt: Option<&'a str>,
    /// Shell command.
    pub command: Option<&'a str>,
}

/// A task's identity and dependencies, for scheduling.
#[derive(Debug, Clone)]
pub struct TaskRow {
    /// Task index within the run.
    pub idx: i64,
    /// Task name.
    pub name: String,
    /// Registered project name (RAL-100). Required whenever any of this
    /// task's sessions uses a `ralphus:new-worktree/<branch>` placeholder
    /// `cwd` -- that's the project the daemon materializes the worktree
    /// under.
    pub project: Option<String>,
    /// Task-level dependency references.
    pub depends_on: Vec<String>,
}

/// The full set of entities a restart would dirty (RAL-104): sessions/tasks
/// reset to Pending within the target run, and other runs — transitively
/// dependent on it — that get dirtied too. Computed once by
/// [`Store::compute_run_restart_impact`] / [`Store::compute_session_restart_impact`]
/// and shared by both the non-mutating dry-run preview and the real restart,
/// so the two can never drift out of sync.
#[derive(Debug, Clone, Default, Serialize)]
pub struct RestartImpact {
    /// Sessions within the target run that will be reset to Pending.
    pub sessions: Vec<RestartImpactSession>,
    /// Tasks within the target run that will be reset to Pending.
    pub tasks: Vec<RestartImpactTask>,
    /// Other runs, transitively dependent on the target run, that will be
    /// dirtied (reset to Pending).
    pub dirtied_runs: Vec<RestartImpactRun>,
}

/// One session affected by a restart, for display in the dry-run preview.
#[derive(Debug, Clone, Serialize)]
pub struct RestartImpactSession {
    /// Index of the owning task within the run.
    pub task_idx: i64,
    /// Index of the session within the task.
    pub idx: i64,
    /// Owning task name.
    pub task_name: String,
    /// Session id.
    pub session_id: String,
}

/// One task affected by a restart, for display in the dry-run preview.
#[derive(Debug, Clone, Serialize)]
pub struct RestartImpactTask {
    /// Task index within the run.
    pub idx: i64,
    /// Task name.
    pub name: String,
}

/// One dependent run that would be dirtied by a restart, for display in the
/// dry-run preview.
#[derive(Debug, Clone, Serialize)]
pub struct RestartImpactRun {
    /// Run id.
    pub id: String,
    /// Optional human label.
    pub label: Option<String>,
}

/// Everything cancelling a run affects, computed by [`Store::cancel_run`] and
/// shared by the non-mutating dry-run preview and the real cascading cancel
/// (RAL-116), so the two can never drift out of sync.
#[derive(Debug, Clone, Default, Serialize)]
pub struct CancelImpact {
    /// The target run plus every run transitively dependent on it — all of
    /// which will be (or were) cancelled. The target run is always first.
    pub runs: Vec<RestartImpactRun>,
}

impl Store {
    /// All sessions of a run, in insertion order.
    pub fn sessions_of(&self, run_id: &str) -> Result<Vec<SessionRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT s.task_idx, s.idx, t.name, s.sid, s.cwd, s.subprojects, s.prompt, s.command, s.agent, s.model, s.system_prompt, s.system_prompt_position, s.depends_on, s.timeout_sec, s.budget_tokens, s.upstream
             FROM sessions s JOIN tasks t ON t.run_id = s.run_id AND t.idx = s.task_idx
             WHERE s.run_id = ? ORDER BY s.task_idx, s.idx",
        )?;
        let rows = stmt
            .query_map(params![run_id], |r| {
                Ok(SessionRow {
                    task_idx: r.get(0)?,
                    idx: r.get(1)?,
                    task_name: r.get(2)?,
                    session_id: r.get(3)?,
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
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// All tasks of a run with their dependencies, in order.
    pub fn tasks_of(&self, run_id: &str) -> Result<Vec<TaskRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT idx, name, project, depends_on FROM tasks WHERE run_id=? ORDER BY idx",
        )?;
        let rows = stmt
            .query_map(params![run_id], |r| {
                Ok(TaskRow {
                    idx: r.get(0)?,
                    name: r.get(1)?,
                    project: r.get(2)?,
                    depends_on: from_json(&r.get::<_, String>(3)?),
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// The cross-run dependency references declared in the run's `[[default]]`.
    pub fn run_depends_on(&self, run_id: &str) -> Result<Vec<String>> {
        let s: Option<String> = self
            .conn
            .query_row(
                "SELECT depends_on FROM runs WHERE id=?",
                params![run_id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(from_json(&s.ok_or(StoreError::NotFound)?))
    }

    /// Add a cross-run dependency (RAL-105): appends `target_id` to `run_id`'s
    /// `[[default]] depends_on`, reusing the existing whole-run gating in
    /// [`Store::list_ready`]/[`Store::deps_satisfied`] — no new scheduling
    /// path. A `run_id`/`target_id` that doesn't exist is `NotFound`; a
    /// self-reference or a reference that would create a cycle in the
    /// cross-run dependency graph is rejected as `InvalidTransition`. Adding
    /// a dependency that is already present is a no-op. Returns the run's
    /// updated `depends_on` list.
    pub fn add_run_dependency(&self, run_id: &str, target_id: &str) -> Result<Vec<String>> {
        if run_id == target_id {
            return Err(StoreError::InvalidTransition(
                "a run cannot depend on itself".to_string(),
            ));
        }
        let mut deps = self.run_depends_on(run_id)?;
        self.run_depends_on(target_id)?; // existence check
        if deps
            .iter()
            .any(|d| d.split('/').next().unwrap_or(d) == target_id)
        {
            return Ok(deps);
        }
        if self.run_transitively_depends_on(target_id, run_id)? {
            return Err(StoreError::InvalidTransition(format!(
                "adding a dependency on {target_id} would create a cycle"
            )));
        }
        deps.push(target_id.to_string());
        self.conn.execute(
            "UPDATE runs SET depends_on=?, updated_at_ms=? WHERE id=?",
            params![to_json(&deps), now_ms(), run_id],
        )?;
        let _ = self.log_event(
            Some(run_id),
            None,
            "run",
            None,
            &format!("dependency added: now depends on {target_id}"),
        );
        Ok(deps)
    }

    /// Whether `from` transitively depends on `to` via cross-run `depends_on`
    /// edges (BFS). Dangling references (a dep id that no longer resolves to
    /// a run) are skipped — same best-effort philosophy as
    /// [`Store::global_graph`].
    fn run_transitively_depends_on(&self, from: &str, to: &str) -> Result<bool> {
        let mut seen: HashSet<String> = HashSet::new();
        let mut frontier = vec![from.to_string()];
        while let Some(cur) = frontier.pop() {
            if cur == to {
                return Ok(true);
            }
            if !seen.insert(cur.clone()) {
                continue;
            }
            for dep in self.run_depends_on(&cur).unwrap_or_default() {
                let dep_run = dep.split('/').next().unwrap_or(&dep).to_string();
                frontier.push(dep_run);
            }
        }
        Ok(false)
    }

    /// The distinct task indices of a run, ascending.
    pub fn task_indices(&self, run_id: &str) -> Result<Vec<i64>> {
        let mut stmt = self
            .conn
            .prepare("SELECT idx FROM tasks WHERE run_id=? ORDER BY idx")?;
        let ids = stmt
            .query_map(params![run_id], |r| r.get::<_, i64>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(ids)
    }

    /// Returns `true` when every task in `task_indices` has state `done` for the
    /// given run. An empty set is vacuously true. Used by the per-task review
    /// gate to decide whether a guardian's blocking tasks have all finished.
    pub fn all_tasks_done(&self, run_id: &str, task_indices: &HashSet<i64>) -> Result<bool> {
        if task_indices.is_empty() {
            return Ok(true);
        }
        let mut stmt = self
            .conn
            .prepare("SELECT idx FROM tasks WHERE run_id=? AND state != 'done'")?;
        let non_done: Vec<i64> = stmt
            .query_map(params![run_id], |r| r.get::<_, i64>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(non_done.iter().all(|idx| !task_indices.contains(idx)))
    }

    /// The verify steps of a scope (`"task"` with `session_idx = -1`, or
    /// `"session"` with the session's index), in order:
    /// `(idx, kind, spec, model)` — `model` is only meaningful for `agent`.
    pub fn verify_specs(
        &self,
        run_id: &str,
        task_idx: i64,
        scope: &str,
        session_idx: i64,
    ) -> Result<Vec<VerifySpecRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT idx, kind, spec, model, timeout_sec, budget_tokens FROM verifies
             WHERE run_id=? AND task_idx=? AND scope=? AND session_idx=? ORDER BY idx",
        )?;
        let rows = stmt
            .query_map(params![run_id, task_idx, scope, session_idx], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, Option<i64>>(4)?,
                    r.get::<_, Option<i64>>(5)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Fresh current state of one verify step (not a snapshot from
    /// [`Store::verify_specs`]) — lets the scheduler notice a user manually
    /// setting a not-yet-executed step to `ignored` mid-run and honor it
    /// instead of racing ahead with a stale snapshot.
    pub fn verify_state(
        &self,
        run_id: &str,
        task_idx: i64,
        scope: &str,
        session_idx: i64,
        idx: i64,
    ) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT state FROM verifies WHERE run_id=? AND task_idx=? AND scope=? AND session_idx=? AND idx=?",
                params![run_id, task_idx, scope, session_idx, idx],
                |r| r.get::<_, String>(0),
            )
            .optional()?)
    }

    /// Set a verify step's state.
    pub fn set_verify_state(
        &self,
        run_id: &str,
        task_idx: i64,
        scope: &str,
        session_idx: i64,
        idx: i64,
        state: NodeState,
    ) -> Result<()> {
        let old = self
            .conn
            .query_row(
                "SELECT state FROM verifies WHERE run_id=? AND task_idx=? AND scope=? AND session_idx=? AND idx=?",
                params![run_id, task_idx, scope, session_idx, idx],
                |r| r.get::<_, String>(0),
            )
            .optional()
            .ok()
            .flatten()
            .unwrap_or_else(|| "unknown".to_string());
        self.conn.execute(
            "UPDATE verifies SET state=?
             WHERE run_id=? AND task_idx=? AND scope=? AND session_idx=? AND idx=?",
            params![state.as_str(), run_id, task_idx, scope, session_idx, idx],
        )?;
        crate::rlog!(
            DEBUG,
            "ralphus [state] verify {run_id}/t{task_idx}/{scope}/#{idx} {old} → {}",
            state.as_str()
        );
        let _ = self.cartographer_log(crate::cartographer::CartographerEntry {
            level: crate::logging::LogLevel::DEBUG,
            source: "store",
            message: "verify state transition",
            scope: Some("verify"),
            run_id: Some(run_id),
            guardian_id: None,
            session_id: None,
            task: None,
            payload: serde_json::json!({
                "task_idx": task_idx,
                "verify_scope": scope,
                "session_idx": session_idx,
                "idx": idx,
                "old": old,
                "new": state.as_str(),
            }),
        });
        Ok(())
    }

    /// Record a verifier's terminal state and captured output, logging it (CCTL-99).
    #[allow(clippy::too_many_arguments)]
    pub fn set_verify_result(
        &self,
        run_id: &str,
        task_idx: i64,
        scope: &str,
        session_idx: i64,
        idx: i64,
        state: NodeState,
        output: &str,
        claude_session_id: Option<&str>,
    ) -> Result<()> {
        // Query old state and vid together before the UPDATE so we have both for
        // logging (vid doesn't change, but reading it before avoids a second round trip).
        let (old, vid): (String, Option<String>) = self
            .conn
            .query_row(
                "SELECT state, vid FROM verifies WHERE run_id=? AND task_idx=? AND scope=? AND session_idx=? AND idx=?",
                params![run_id, task_idx, scope, session_idx, idx],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .ok()
            .flatten()
            .unwrap_or_else(|| ("unknown".to_string(), None));
        self.conn.execute(
            "UPDATE verifies SET state=?, output=?, claude_session_id=?
             WHERE run_id=? AND task_idx=? AND scope=? AND session_idx=? AND idx=?",
            params![
                state.as_str(),
                output,
                claude_session_id,
                run_id,
                task_idx,
                scope,
                session_idx,
                idx
            ],
        )?;
        let reference = match &vid {
            Some(v) => format!("{scope} t{task_idx}/{v}"),
            None => format!("{scope} t{task_idx} #{idx}"),
        };
        crate::rlog!(
            INFO,
            "ralphus [state] verify {run_id}/t{task_idx}/{scope}/#{idx} {old} → {} output_len={}",
            state.as_str(),
            output.len()
        );
        let _ = self.log_event(
            Some(run_id),
            None,
            "verify",
            Some(&reference),
            &format!("verify → {}", state.as_str()),
        );
        Ok(())
    }

    /// Edit a run's label.
    pub fn edit_run_label(&self, run_id: &str, label: Option<&str>) -> Result<()> {
        let n = self.conn.execute(
            "UPDATE runs SET label=?, updated_at_ms=? WHERE id=?",
            params![label, now_ms(), run_id],
        )?;
        if n == 0 {
            Err(StoreError::NotFound)
        } else {
            Ok(())
        }
    }

    /// Edit a task's name and project.
    pub fn edit_task_fields(
        &self,
        run_id: &str,
        task_idx: i64,
        name: &str,
        project: Option<&str>,
    ) -> Result<()> {
        let n = self.conn.execute(
            "UPDATE tasks SET name=?, project=? WHERE run_id=? AND idx=?",
            params![name, project, run_id, task_idx],
        )?;
        if n == 0 {
            Err(StoreError::NotFound)
        } else {
            Ok(())
        }
    }

    /// Edit a session's editable definition fields. Exactly one of `prompt` /
    /// `command` should be non-empty (the other is cleared).
    pub fn edit_session_fields(
        &self,
        run_id: &str,
        task_idx: i64,
        idx: i64,
        edit: &SessionEdit<'_>,
    ) -> Result<()> {
        let n = self.conn.execute(
            "UPDATE sessions SET cwd=?, agent=?, model=?, prompt=?, command=?
             WHERE run_id=? AND task_idx=? AND idx=?",
            params![
                edit.cwd,
                edit.agent,
                edit.model,
                edit.prompt,
                edit.command,
                run_id,
                task_idx,
                idx
            ],
        )?;
        if n == 0 {
            Err(StoreError::NotFound)
        } else {
            Ok(())
        }
    }

    /// Reset a run and all its nodes back to `Pending` — the dirty→pending gate
    /// applied after an edit, so the run re-executes with the new values. A
    /// currently-running worker's final state write is skipped (see the
    /// scheduler), so this effectively stops in-flight work.
    pub fn reset_run_to_pending(&self, run_id: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE runs SET state='pending', updated_at_ms=? WHERE id=?",
            params![now_ms(), run_id],
        )?;
        self.conn.execute(
            "UPDATE tasks SET state='pending' WHERE run_id=?",
            params![run_id],
        )?;
        self.conn.execute(
            "UPDATE sessions SET state='pending', error=NULL WHERE run_id=?",
            params![run_id],
        )?;
        self.conn.execute(
            "UPDATE verifies SET state='pending' WHERE run_id=?",
            params![run_id],
        )?;
        Ok(())
    }

    /// Crash recovery: runs left `Running` after an unclean shutdown have no
    /// worker to finish them. Reset each such run — and only its still-in-flight
    /// (`running`) tasks/sessions/verifies — back to `Pending`, so the scheduler
    /// re-claims and resumes it. `Done` sessions are deliberately left `Done` so
    /// re-execution skips finished work and only the unfinished tail re-runs
    /// (RAL-19). Runs a single pass at startup, before the scheduler begins;
    /// returns the recovered run ids. Safe because nothing is executing yet, so
    /// any `running` row is by definition orphaned.
    pub fn recover_orphaned_runs(&self) -> Result<Vec<String>> {
        let ids: Vec<String> = {
            let mut stmt = self
                .conn
                .prepare("SELECT id FROM runs WHERE state='running'")?;
            stmt.query_map([], |r| r.get::<_, String>(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?
        };
        for id in &ids {
            crate::rlog!(
                WARNING,
                "ralphus [recovery] run {id}: running → pending (orphaned on startup)"
            );
            let _ = self.cartographer_log(crate::cartographer::CartographerEntry {
                level: crate::logging::LogLevel::WARNING,
                source: "recovery",
                message: "run recovered: running → pending (orphaned on startup)",
                scope: Some("run"),
                run_id: Some(id),
                guardian_id: None,
                session_id: None,
                task: None,
                payload: serde_json::json!({}),
            });
            self.conn.execute(
                "UPDATE sessions SET state='pending', error=NULL WHERE run_id=? AND state='running'",
                params![id],
            )?;
            self.conn.execute(
                "UPDATE verifies SET state='pending' WHERE run_id=? AND state='running'",
                params![id],
            )?;
            self.conn.execute(
                "UPDATE tasks SET state='pending' WHERE run_id=? AND state='running'",
                params![id],
            )?;
            self.conn.execute(
                "UPDATE runs SET state='pending', updated_at_ms=? WHERE id=?",
                params![now_ms(), id],
            )?;
        }
        Ok(ids)
    }

    /// Task indices that have at least one session in [`done_sessions`] whose
    /// session-level verify previously **failed**. Used by the scheduler to seed
    /// `progress.failed` on a partial restart: those sessions are skipped (they
    /// are already Done), but their prior failure must still condemn the task so
    /// that the task finalizer does not incorrectly set the task to Done.
    pub fn done_sessions_with_failed_verify(&self, run_id: &str) -> Result<HashSet<i64>> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT s.task_idx
             FROM sessions s
             WHERE s.run_id=? AND s.state='done'
             AND NOT EXISTS (
                 SELECT 1 FROM verifies v
                 WHERE v.run_id=s.run_id AND v.task_idx=s.task_idx
                 AND v.scope='session' AND v.session_idx=s.idx
                 AND v.state NOT IN ('done','failed','cancelled')
             )
             AND EXISTS (
                 SELECT 1 FROM verifies v2
                 WHERE v2.run_id=s.run_id AND v2.task_idx=s.task_idx
                 AND v2.scope='session' AND v2.session_idx=s.idx
                 AND v2.state='failed'
             )",
        )?;
        let rows = stmt
            .query_map(params![run_id], |r| r.get::<_, i64>(0))?
            .collect::<std::result::Result<HashSet<_>, _>>()?;
        Ok(rows)
    }

    /// The `(task_idx, idx)` of every session in a run that is already `Done`
    /// *and* whose session-level verify steps are all in a terminal state.
    /// The scheduler skips these so a restarted run only re-runs its dirty
    /// (reset-to-pending) subset instead of redoing finished work (RAL-19).
    ///
    /// A session whose verifies are still pending (e.g. the daemon was stopped
    /// between `record_session_result` and `run_verifies`) is excluded so the
    /// session worker re-runs and the verifies are executed (RAL-64).
    pub fn done_sessions(&self, run_id: &str) -> Result<HashSet<(i64, i64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT s.task_idx, s.idx
             FROM sessions s
             WHERE s.run_id=? AND s.state='done'
             AND NOT EXISTS (
                 SELECT 1 FROM verifies v
                 WHERE v.run_id=s.run_id
                 AND v.task_idx=s.task_idx
                 AND v.scope='session'
                 AND v.session_idx=s.idx
                 AND v.state NOT IN ('done','failed','cancelled')
             )",
        )?;
        let rows = stmt
            .query_map(params![run_id], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
            })?
            .collect::<std::result::Result<HashSet<_>, _>>()?;
        Ok(rows)
    }

    /// The `(task_idx, idx)` of every session already `Failed`. The scheduler
    /// seeds these as terminally Failed (never re-dispatched) rather than
    /// falling through to Pending -- otherwise a *scoped* session/verify
    /// restart, which only resets its own target + downstream to Pending but
    /// still flips the whole run back to Pending so the scheduler reactivates
    /// it, would silently redispatch every other still-`failed` session in
    /// the run too (a session/task genuinely reset by the restart is already
    /// `pending` in the DB by the time this runs, so it's excluded here).
    pub fn failed_sessions(&self, run_id: &str) -> Result<HashSet<(i64, i64)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT task_idx, idx FROM sessions WHERE run_id=? AND state='failed'")?;
        let rows = stmt
            .query_map(params![run_id], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
            })?
            .collect::<std::result::Result<HashSet<_>, _>>()?;
        Ok(rows)
    }

    /// The `(task_idx, idx)` of every session manually set to `ignored`. The
    /// scheduler seeds these as satisfied so their downstream sessions run,
    /// exactly as a `done` upstream would (they are themselves never executed).
    pub fn ignored_sessions(&self, run_id: &str) -> Result<HashSet<(i64, i64)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT task_idx, idx FROM sessions WHERE run_id=? AND state='ignored'")?;
        let rows = stmt
            .query_map(params![run_id], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
            })?
            .collect::<std::result::Result<HashSet<_>, _>>()?;
        Ok(rows)
    }

    /// Compute everything [`Store::restart_run`] would dirty, without mutating
    /// anything: every session/task in the run (a whole-run restart resets all
    /// of them) plus every run transitively dependent on it. Shared by the
    /// non-mutating dry-run preview and the real restart (RAL-104) so the two
    /// can never drift out of sync.
    pub fn compute_run_restart_impact(&self, run_id: &str) -> Result<RestartImpact> {
        let exists: Option<String> = self
            .conn
            .query_row("SELECT id FROM runs WHERE id=?", params![run_id], |r| {
                r.get(0)
            })
            .optional()?;
        if exists.is_none() {
            return Err(StoreError::NotFound);
        }
        let sessions = self
            .sessions_of(run_id)?
            .into_iter()
            .map(|s| RestartImpactSession {
                task_idx: s.task_idx,
                idx: s.idx,
                task_name: s.task_name,
                session_id: s.session_id,
            })
            .collect();
        let tasks = self
            .tasks_of(run_id)?
            .into_iter()
            .map(|t| RestartImpactTask {
                idx: t.idx,
                name: t.name,
            })
            .collect();
        let dirtied_runs = self.compute_dirty_dependents(run_id)?;
        Ok(RestartImpact {
            sessions,
            tasks,
            dirtied_runs,
        })
    }

    /// Compute (and, unless `dry_run`, perform) a cascading cancel of
    /// `run_id`: the run itself plus every run transitively dependent on it
    /// (RAL-116). One function drives both the non-mutating dry-run preview
    /// and the real cancel, so they can never drift apart — mirrors
    /// [`Store::compute_run_restart_impact`] (RAL-104). Every listed run is
    /// cancelled regardless of its current state, including already-terminal
    /// ones, matching [`Store::cancel`]'s always-available semantics.
    pub fn cancel_run(&self, run_id: &str, dry_run: bool) -> Result<CancelImpact> {
        let row: Option<(String, Option<String>)> = self
            .conn
            .query_row(
                "SELECT id, label FROM runs WHERE id=?",
                params![run_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((id, label)) = row else {
            return Err(StoreError::NotFound);
        };
        let mut runs = vec![RestartImpactRun { id, label }];
        runs.extend(self.compute_dirty_dependents(run_id)?);

        if !dry_run {
            for r in &runs {
                self.set_run_state(&r.id, RunState::Cancelled)?;
                self.cancel_nonterminal_nodes(&r.id)?;
                let _ = self.log_event(Some(&r.id), None, "run", None, "cancelled");
            }
        }

        Ok(CancelImpact { runs })
    }

    /// Restart a whole run: reset it (and all its nodes) to Pending and dirty
    /// every run that transitively depends on it, so the dependents re-run once
    /// this run finishes again (RAL-19). Returns the dirtied dependent run ids.
    pub fn restart_run(&self, run_id: &str) -> Result<Vec<String>> {
        let impact = self.compute_run_restart_impact(run_id)?;
        self.reset_run_to_pending(run_id)?;
        let _ = self.log_event(Some(run_id), None, "run", None, "restarted");
        self.apply_dirty_dependents(&impact.dirtied_runs)?;
        Ok(impact.dirtied_runs.into_iter().map(|r| r.id).collect())
    }

    /// Compute everything [`Store::restart_session`] would dirty, without
    /// mutating anything: the target session plus every session downstream of
    /// it within the run (forward reachability over the plan graph), the tasks
    /// that own any of those sessions, and every run transitively dependent on
    /// this one. Shared by the non-mutating dry-run preview and the real
    /// restart (RAL-104) so the two can never drift out of sync.
    pub fn compute_session_restart_impact(
        &self,
        run_id: &str,
        task_idx: i64,
        idx: i64,
    ) -> Result<RestartImpact> {
        let sessions = self.sessions_of(run_id)?;
        let tasks = self.tasks_of(run_id)?;
        let target = sessions
            .iter()
            .position(|s| s.task_idx == task_idx && s.idx == idx)
            .ok_or(StoreError::NotFound)?;
        let plan = crate::plan::plan(&sessions, &tasks).map_err(StoreError::InvalidTransition)?;

        // Forward reachability: session `j` is downstream of `target` when
        // `target` is one of its transitive prerequisites. Invert deps into
        // child edges, then BFS out from `target`.
        let mut children: Vec<Vec<usize>> = vec![Vec::new(); sessions.len()];
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
        let mut affected_sessions: Vec<RestartImpactSession> = affected
            .iter()
            .map(|&pos| {
                let s = &sessions[pos];
                affected_task_idxs.insert(s.task_idx);
                RestartImpactSession {
                    task_idx: s.task_idx,
                    idx: s.idx,
                    task_name: s.task_name.clone(),
                    session_id: s.session_id.clone(),
                }
            })
            .collect();
        affected_sessions.sort_by_key(|s| (s.task_idx, s.idx));

        let mut affected_tasks: Vec<RestartImpactTask> = tasks
            .iter()
            .filter(|t| affected_task_idxs.contains(&t.idx))
            .map(|t| RestartImpactTask {
                idx: t.idx,
                name: t.name.clone(),
            })
            .collect();
        affected_tasks.sort_by_key(|t| t.idx);

        let dirtied_runs = self.compute_dirty_dependents(run_id)?;

        Ok(RestartImpact {
            sessions: affected_sessions,
            tasks: affected_tasks,
            dirtied_runs,
        })
    }

    /// Restart a single session: reset it and every session downstream of it
    /// within the run to Pending, put the run (and each affected task) back to
    /// Pending, and dirty every run that depends on this one (RAL-19). Upstream
    /// sessions stay Done and are skipped on re-run. Returns the dirtied
    /// dependent run ids.
    pub fn restart_session(&self, run_id: &str, task_idx: i64, idx: i64) -> Result<Vec<String>> {
        let impact = self.compute_session_restart_impact(run_id, task_idx, idx)?;

        for s in &impact.sessions {
            self.conn.execute(
                "UPDATE sessions SET state='pending', error=NULL WHERE run_id=? AND task_idx=? AND idx=?",
                params![run_id, s.task_idx, s.idx],
            )?;
            self.conn.execute(
                "UPDATE verifies SET state='pending' WHERE run_id=? AND task_idx=? AND scope='session' AND session_idx=?",
                params![run_id, s.task_idx, s.idx],
            )?;
        }
        for t in &impact.tasks {
            self.conn.execute(
                "UPDATE tasks SET state='pending' WHERE run_id=? AND idx=?",
                params![run_id, t.idx],
            )?;
            self.conn.execute(
                "UPDATE verifies SET state='pending' WHERE run_id=? AND task_idx=? AND scope='task'",
                params![run_id, t.idx],
            )?;
        }
        self.conn.execute(
            "UPDATE runs SET state='pending', updated_at_ms=? WHERE id=?",
            params![now_ms(), run_id],
        )?;
        let _ = self.log_event(
            Some(run_id),
            None,
            "session",
            Some(&format!("{task_idx}/{idx}")),
            "restarted (with downstream)",
        );
        self.apply_dirty_dependents(&impact.dirtied_runs)?;
        Ok(impact.dirtied_runs.into_iter().map(|r| r.id).collect())
    }

    /// Read-only BFS over cross-run dependencies: every run transitively
    /// dependent on `run_id`, in discovery order. Does not mutate anything —
    /// shared by the dry-run preview and [`Store::dirty_dependents`] (RAL-104).
    pub fn compute_dirty_dependents(&self, run_id: &str) -> Result<Vec<RestartImpactRun>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, label, depends_on FROM runs")?;
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
        seen.insert(run_id.to_string());
        let mut frontier = vec![run_id.to_string()];
        let mut dirtied = Vec::new();
        while let Some(cur) = frontier.pop() {
            for (id, label, deps) in &all {
                if seen.contains(id) {
                    continue;
                }
                // A dep reference is `run-id` or `run-id/task/session`; the run
                // is the first path segment.
                let depends = deps.iter().any(|d| d.split('/').next().unwrap_or(d) == cur);
                if depends {
                    seen.insert(id.clone());
                    dirtied.push(RestartImpactRun {
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
    /// reset each listed run to Pending and log the dirtying event.
    pub fn apply_dirty_dependents(&self, runs: &[RestartImpactRun]) -> Result<()> {
        for r in runs {
            self.reset_run_to_pending(&r.id)?;
            let _ = self.log_event(
                Some(&r.id),
                None,
                "run",
                None,
                "dirtied by upstream restart",
            );
        }
        Ok(())
    }

    /// Reset every run that transitively depends on `run_id` back to Pending, so
    /// it re-runs once the upstream completes again (RAL-19). Cross-run gating
    /// (`list_ready`) then holds each dependent until its upstreams are Done.
    /// Returns the dirtied run ids.
    pub fn dirty_dependents(&self, run_id: &str) -> Result<Vec<String>> {
        let impacted = self.compute_dirty_dependents(run_id)?;
        self.apply_dirty_dependents(&impacted)?;
        Ok(impacted.into_iter().map(|r| r.id).collect())
    }

    /// Delete a run and all of its child rows. Children are removed explicitly
    /// (rather than relying on `ON DELETE CASCADE`, which is off for in-memory
    /// test databases) inside one transaction.
    pub fn delete_run(&mut self, run_id: &str) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM events WHERE run_id=?", params![run_id])?;
        tx.execute("DELETE FROM verifies WHERE run_id=?", params![run_id])?;
        tx.execute("DELETE FROM sessions WHERE run_id=?", params![run_id])?;
        tx.execute("DELETE FROM tasks WHERE run_id=?", params![run_id])?;
        tx.execute("DELETE FROM ghosts WHERE run_id=?", params![run_id])?;
        let n = tx.execute("DELETE FROM runs WHERE id=?", params![run_id])?;
        tx.commit()?;
        if n == 0 {
            Err(StoreError::NotFound)
        } else {
            Ok(())
        }
    }

    /// Bulk-clear state (RAL-13). With an empty `states` filter this wipes
    /// everything — all runs (and their sessions/tasks/verifies/events), all
    /// guardians (and their branches), and the id sequences reset so the next
    /// run/guardian id restarts at 1. When `states` is non-empty, only runs
    /// whose state is in the set are deleted (with their children); guardians
    /// and the id sequences are left untouched, since the filter is expressed
    /// in run states. The returned git roots let the caller purge on-disk
    /// review worktrees for any deleted guardian.
    pub fn clear_all(&mut self, states: &[RunState]) -> Result<ClearOutcome> {
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
            let tx = self.conn.transaction()?;
            tx.execute("DELETE FROM events", [])?;
            tx.execute("DELETE FROM verifies", [])?;
            tx.execute("DELETE FROM sessions", [])?;
            tx.execute("DELETE FROM tasks", [])?;
            tx.execute("DELETE FROM ghosts", [])?;
            let runs_deleted = tx.execute("DELETE FROM runs", [])?;
            tx.execute("DELETE FROM guardian_branches", [])?;
            tx.execute("DELETE FROM guardian_messages", [])?;
            let guardians_deleted = tx.execute("DELETE FROM guardians", [])?;
            // Reset id sequences so the next run/guardian id restarts at 1.
            tx.execute(
                "DELETE FROM meta WHERE key IN ('run_seq', 'guardian_seq')",
                [],
            )?;
            tx.commit()?;
            return Ok(ClearOutcome {
                runs_deleted,
                guardians_deleted,
                guardian_roots,
            });
        }
        // Filtered: delete only runs whose state matches, plus their children.
        let wanted: Vec<&str> = states.iter().map(|s| s.as_str()).collect();
        let ids: Vec<String> = {
            let mut stmt = self.conn.prepare("SELECT id, state FROM runs")?;
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
            tx.execute("DELETE FROM events WHERE run_id=?", params![id])?;
            tx.execute("DELETE FROM verifies WHERE run_id=?", params![id])?;
            tx.execute("DELETE FROM sessions WHERE run_id=?", params![id])?;
            tx.execute("DELETE FROM tasks WHERE run_id=?", params![id])?;
            tx.execute("DELETE FROM ghosts WHERE run_id=?", params![id])?;
            tx.execute("DELETE FROM runs WHERE id=?", params![id])?;
        }
        tx.commit()?;
        Ok(ClearOutcome {
            runs_deleted: ids.len(),
            guardians_deleted: 0,
            guardian_roots: Vec::new(),
        })
    }

    /// Record the git branch a review session contributes to a guardian stack,
    /// so the board can link the session to its review(s) (RAL-17).
    pub fn set_session_review_branch(
        &self,
        run_id: &str,
        task_idx: i64,
        idx: i64,
        branch: &str,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET review_branch=? WHERE run_id=? AND task_idx=? AND idx=?",
            params![branch, run_id, task_idx, idx],
        )?;
        Ok(())
    }

    /// Record a session's final outcome (state, usage, error, and session UUID).
    ///
    /// `claude_session_id` uses `COALESCE(?, claude_session_id)` rather than a
    /// plain overwrite: a live mid-run scrape
    /// ([`Self::set_session_claude_session_id_live`]) may already have
    /// recorded a real Claude Code UUID, but a failed outcome always carries
    /// `claude_session_id: None` (`RunnerResult::failure`) — a plain
    /// overwrite would clobber that good value back to `NULL` on every
    /// failure, permanently disabling "Open Agent" for a session that really
    /// did start one.
    pub fn record_session_result(
        &self,
        run_id: &str,
        task_idx: i64,
        idx: i64,
        outcome: &SessionOutcome,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET state=?, tokens_in=?, tokens_out=?, cost_usd=?, error=?, claude_session_id=COALESCE(?, claude_session_id)
             WHERE run_id=? AND task_idx=? AND idx=?",
            params![
                outcome.state.as_str(),
                outcome.tokens_in,
                outcome.tokens_out,
                outcome.cost_usd,
                outcome.error.as_deref(),
                outcome.claude_session_id.as_deref(),
                run_id,
                task_idx,
                idx,
            ],
        )?;
        Ok(())
    }

    /// Fetch a session's declared id (`sid`, e.g. `"s0"`) for the tmux
    /// capture-pane/attach endpoints (RAL-102). The daemon derives the tmux
    /// session name from `(run_id, task, session_id)` the same way the
    /// scheduler does when building a `RunnerSpec` (see
    /// `crate::tmux::session_name`), so this — paired with
    /// [`Store::get_task_name`] — lets those HTTP handlers recompute the same
    /// name without any new bookkeeping.
    ///
    /// Returns `Err(StoreError::NotFound)` when the run or session row does
    /// not exist.
    pub fn get_session_id(&self, run_id: &str, task_idx: i64, session_idx: i64) -> Result<String> {
        self.conn
            .query_row(
                "SELECT sid FROM sessions WHERE run_id=? AND task_idx=? AND idx=?",
                params![run_id, task_idx, session_idx],
                |r| r.get::<_, String>(0),
            )
            .optional()?
            .ok_or(StoreError::NotFound)
    }

    /// Fetch a task's declared name for the tmux capture-pane/attach
    /// endpoints (RAL-102) — see [`Store::get_session_id`]'s doc comment for
    /// why the caller needs this alongside the session id.
    ///
    /// Returns `Err(StoreError::NotFound)` when the run or task row does not
    /// exist.
    pub fn get_task_name(&self, run_id: &str, task_idx: i64) -> Result<String> {
        self.conn
            .query_row(
                "SELECT name FROM tasks WHERE run_id=? AND idx=?",
                params![run_id, task_idx],
                |r| r.get::<_, String>(0),
            )
            .optional()?
            .ok_or(StoreError::NotFound)
    }

    /// Fetch a session's cwd and (if any) recorded Claude Code session id, for
    /// the "Open Agent" terminal action — resuming the real `claude` CLI
    /// (`claude --resume <id>`) rather than re-attaching to the runner's tmux
    /// wrapper, which only shows its log/event stream (see
    /// `crate::server::open_agent_terminal`).
    ///
    /// Returns `Err(StoreError::NotFound)` when the run or session row does
    /// not exist; `claude_session_id` is `None` when the session hasn't
    /// started (or run under a non-claude-code agent) rather than an error.
    pub fn get_session_agent_resume(
        &self,
        run_id: &str,
        task_idx: i64,
        session_idx: i64,
    ) -> Result<(String, Option<String>)> {
        self.conn
            .query_row(
                "SELECT cwd, claude_session_id FROM sessions WHERE run_id=? AND task_idx=? AND idx=?",
                params![run_id, task_idx, session_idx],
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

    /// Persist a session's Claude Code session id as soon as it's known —
    /// before the session finishes — so "Open Agent" activates immediately
    /// rather than only once the whole session completes. Called from
    /// `runner::forward_runner_event` when the runner subprocess emits an
    /// `llm-invoke` event carrying `claude_session_id` in its payload (RAL-102
    /// follow-up; mirrors `guardian::set_branch_resolver_session_id`'s "Watch
    /// Live" idea, applied to plain task sessions instead of a side-channel
    /// file + watcher thread).
    ///
    /// Best-effort and silently a no-op when `(run_id, task_name, session_sid)`
    /// doesn't match a session row — e.g. a verify step or a Guardian
    /// resolver invocation, which route through the very same event-forwarding
    /// code path but aren't rows in this table at all.
    pub fn set_session_claude_session_id_live(
        &self,
        run_id: &str,
        task_name: &str,
        session_sid: &str,
        claude_session_id: &str,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET claude_session_id=?
             WHERE run_id=? AND sid=? AND task_idx=(SELECT idx FROM tasks WHERE run_id=? AND name=?)",
            params![claude_session_id, run_id, session_sid, run_id, task_name],
        )?;
        Ok(())
    }

    /// Fetch a task's first (lowest-`idx`) session's cwd — the cwd a
    /// task-scope verify step's "Open Agent" action resumes into, mirroring
    /// how `scheduler.rs::run_verifies` itself picks a cwd for a task-scope
    /// step (`sessions.iter().find(|s| s.task_idx == task_idx)`).
    ///
    /// Returns `Err(StoreError::NotFound)` when the run/task has no sessions.
    pub fn get_task_first_session_cwd(&self, run_id: &str, task_idx: i64) -> Result<String> {
        self.conn
            .query_row(
                "SELECT cwd FROM sessions WHERE run_id=? AND task_idx=? ORDER BY idx LIMIT 1",
                params![run_id, task_idx],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()?
            .map(|c| c.unwrap_or_default())
            .ok_or(StoreError::NotFound)
    }

    /// Fetch a `prompt`-kind verify step's recorded Claude Code session id,
    /// for its "Open Agent" terminal action — see
    /// [`Store::get_session_agent_resume`]'s doc comment for the same idea
    /// applied to a plain session.
    ///
    /// Returns `Err(StoreError::NotFound)` when the verify row does not
    /// exist; the id itself is `None` when the step hasn't run yet (or ran
    /// under a non-claude-code agent) rather than an error.
    pub fn get_verify_claude_session_id(
        &self,
        run_id: &str,
        task_idx: i64,
        scope: &str,
        session_idx: i64,
        idx: i64,
    ) -> Result<Option<String>> {
        self.conn
            .query_row(
                "SELECT claude_session_id FROM verifies
                 WHERE run_id=? AND task_idx=? AND scope=? AND session_idx=? AND idx=?",
                params![run_id, task_idx, scope, session_idx, idx],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()?
            .ok_or(StoreError::NotFound)
    }

    /// Restart a single session's verify steps from `verify_from` onwards:
    /// reset only the session-level verifies at index >= `verify_from` to
    /// Pending while leaving the session itself Done. The owning task and run
    /// are put back to Pending so the scheduler re-enters them. The scheduler
    /// detects that the session is Done with pending verifies via
    /// [`Store::sessions_needing_verify_only`] and skips re-running the
    /// session body, executing only the verify steps. Returns dirtied
    /// dependent run ids.
    pub fn restart_session_verify(
        &self,
        run_id: &str,
        task_idx: i64,
        session_idx: i64,
        verify_from: i64,
    ) -> Result<Vec<String>> {
        let exists: Option<i64> = self
            .conn
            .query_row(
                "SELECT idx FROM sessions WHERE run_id=? AND task_idx=? AND idx=?",
                params![run_id, task_idx, session_idx],
                |r| r.get(0),
            )
            .optional()?;
        if exists.is_none() {
            return Err(StoreError::NotFound);
        }
        // Reset only verifies at idx >= verify_from — the session body stays
        // Done so the scheduler's verify-only path re-runs verifies without
        // re-running the session.
        self.conn.execute(
            "UPDATE verifies SET state='pending' WHERE run_id=? AND task_idx=? AND scope='session' AND session_idx=? AND idx>=?",
            params![run_id, task_idx, session_idx, verify_from],
        )?;
        self.conn.execute(
            "UPDATE tasks SET state='pending' WHERE run_id=? AND idx=?",
            params![run_id, task_idx],
        )?;
        self.conn.execute(
            "UPDATE runs SET state='pending', updated_at_ms=? WHERE id=?",
            params![now_ms(), run_id],
        )?;
        let _ = self.log_event(
            Some(run_id),
            None,
            "verify",
            Some(&format!("session t{task_idx}/s{session_idx}")),
            "restarted",
        );
        self.dirty_dependents(run_id)
    }

    /// Restart a task's task-level verify steps from `verify_from` onwards:
    /// reset only the task-scope verifies at index >= `verify_from` to Pending
    /// while leaving all sessions and their session-level verifies intact. The
    /// task and run are put back to Pending so the scheduler's task finalizer
    /// fires and re-runs the task-level verifies. Returns dirtied dependent
    /// run ids.
    pub fn restart_task_verify(
        &self,
        run_id: &str,
        task_idx: i64,
        verify_from: i64,
    ) -> Result<Vec<String>> {
        let exists: Option<i64> = self
            .conn
            .query_row(
                "SELECT idx FROM tasks WHERE run_id=? AND idx=?",
                params![run_id, task_idx],
                |r| r.get(0),
            )
            .optional()?;
        if exists.is_none() {
            return Err(StoreError::NotFound);
        }
        // Reset only task-scope verifies at idx >= verify_from. Session states
        // and session-level verifies are intentionally left untouched: all
        // sessions remain Done so the scheduler's task finalizer fires
        // immediately and re-runs only the affected task-level verifies,
        // without re-running any session body.
        self.conn.execute(
            "UPDATE verifies SET state='pending' WHERE run_id=? AND task_idx=? AND scope='task' AND idx>=?",
            params![run_id, task_idx, verify_from],
        )?;
        self.conn.execute(
            "UPDATE tasks SET state='pending' WHERE run_id=? AND idx=?",
            params![run_id, task_idx],
        )?;
        self.conn.execute(
            "UPDATE runs SET state='pending', updated_at_ms=? WHERE id=?",
            params![now_ms(), run_id],
        )?;
        let _ = self.log_event(
            Some(run_id),
            None,
            "verify",
            Some(&format!("task t{task_idx}")),
            "restarted",
        );
        self.dirty_dependents(run_id)
    }

    /// Sessions that are `done` in the DB but have at least one session-level
    /// verify in a non-terminal state. The scheduler uses this to identify
    /// "verify-only restart" cases: these sessions skip the runner and execute
    /// only their verify steps.
    pub fn sessions_needing_verify_only(&self, run_id: &str) -> Result<HashSet<(i64, i64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT s.task_idx, s.idx FROM sessions s
             WHERE s.run_id=? AND s.state='done'
             AND EXISTS (
                 SELECT 1 FROM verifies v
                 WHERE v.run_id=s.run_id AND v.task_idx=s.task_idx
                 AND v.scope='session' AND v.session_idx=s.idx
                 AND v.state NOT IN ('done','failed','cancelled')
             )",
        )?;
        let rows = stmt
            .query_map(params![run_id], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
            })?
            .collect::<std::result::Result<HashSet<_>, _>>()?;
        Ok(rows)
    }

    // ── Queue view + reorder (RAL Queue) ─────────────────────────────────────

    /// The flat list of runnable work items across every schedulable run
    /// (`pending`/`running`/`queued`), classified for the Queue view. Each item
    /// is a session, a session-level verify, or a task-level verify still in a
    /// `pending`/`running` state. Ordered canonically by
    /// `(queue_rank NULLS LAST, run created_at, task_idx, sessions-before-verifies, idx)`.
    pub fn queue(&self) -> Result<Vec<QueueItem>> {
        let runs: Vec<(String, Option<String>, String, i64)> = {
            let mut stmt = self.conn.prepare(
                "SELECT id, label, state, created_at_ms FROM runs
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
        for (run_id, run_label, run_state, run_created) in runs {
            self.queue_items_for_run(
                &run_id,
                run_label.as_deref(),
                &run_state,
                run_created,
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

    fn queue_items_for_run(
        &self,
        run_id: &str,
        run_label: Option<&str>,
        run_state: &str,
        run_created: i64,
        out: &mut Vec<QueueItem>,
    ) -> Result<()> {
        let sessions = self.sessions_of(run_id)?;
        let tasks = self.tasks_of(run_id)?;
        let plan = match crate::plan::plan(&sessions, &tasks) {
            Ok(p) => p,
            Err(_) => return Ok(()), // a cyclic run cannot be queued
        };
        let run_deps_ok = self.deps_satisfied(&self.run_depends_on(run_id)?)?;

        // Each task's declared `depends_on` (task names), for the header display.
        let task_deps: HashMap<i64, Vec<String>> = tasks
            .iter()
            .map(|t| (t.idx, t.depends_on.clone()))
            .collect();

        // Position of each (task_idx, idx) within `sessions` (== plan indexing).
        let mut pos_of: HashMap<(i64, i64), usize> = HashMap::new();
        for (i, s) in sessions.iter().enumerate() {
            pos_of.insert((s.task_idx, s.idx), i);
        }

        // Session metadata: state, display name, queue_rank.
        struct SMeta {
            state: String,
            name: String,
            rank: Option<f64>,
        }
        let smeta: HashMap<(i64, i64), SMeta> = {
            let mut stmt = self.conn.prepare(
                "SELECT task_idx, idx, sid, name, state, queue_rank FROM sessions WHERE run_id=?",
            )?;
            stmt.query_map(params![run_id], |r| {
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

        // ── sessions ──
        for s in &sessions {
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
            if !run_deps_ok {
                blocked_by.push("upstream run".to_string());
            }
            let mut deps_paths: Vec<String> = Vec::new();
            for &d in &plan.deps[pos] {
                let (dti, dsi) = (sessions[d].task_idx, sessions[d].idx);
                deps_paths.push(session_path(run_id, dti, dsi));
                let dep_state = state_of(dti, dsi);
                let dep_label = smeta
                    .get(&(dti, dsi))
                    .map(|m| format!("session {}", m.name))
                    .unwrap_or_else(|| format!("session t{dti}/s{dsi}"));
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
                blocked_by.is_empty() && run_deps_ok,
            );
            out.push(QueueItem {
                run_id: run_id.to_string(),
                run_label: run_label.map(str::to_string),
                run_state: run_state.to_string(),
                run_created_at_ms: run_created,
                kind: "session".to_string(),
                path: session_path(run_id, s.task_idx, s.idx),
                indent: 2,
                task_idx: s.task_idx,
                task_name: s.task_name.clone(),
                session_idx: s.idx,
                verify_idx: -1,
                verify_scope: String::new(),
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

        // ── verifies (session-scope and task-scope) ──
        let verify_rows: Vec<VerifyQueueRow> = {
            let mut stmt = self.conn.prepare(
                "SELECT task_idx, scope, session_idx, idx, vid, kind, state, queue_rank
                 FROM verifies WHERE run_id=? ORDER BY task_idx, scope, session_idx, idx",
            )?;
            stmt.query_map(params![run_id], |r| {
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
        for (ti, scope, sidx, vi, vid, kind, vstate, rank) in verify_rows {
            if !matches!(vstate.as_str(), "pending" | "running") {
                continue;
            }
            let mut blocked_by: Vec<String> = Vec::new();
            let mut excluded = false;
            let mut deps_paths: Vec<String> = Vec::new();
            if !run_deps_ok {
                blocked_by.push("upstream run".to_string());
            }
            let (name, path, indent, kind_str);
            if scope == "session" {
                name = vid.unwrap_or_else(|| kind.clone());
                path = sverify_path(run_id, ti, sidx, vi);
                indent = 3;
                kind_str = "session_verify".to_string();
                // Depends on the owning session, then prior session-verify.
                deps_paths.push(session_path(run_id, ti, sidx));
                let ss = state_of(ti, sidx);
                match ss.as_str() {
                    "done" | "ignored" => {}
                    "failed" | "cancelled" => {
                        excluded = true;
                        blocked_by.push("owning session".to_string());
                    }
                    _ => blocked_by.push("owning session".to_string()),
                }
                if vi > 0 {
                    deps_paths.push(sverify_path(run_id, ti, sidx, vi - 1));
                    blocked_by.push("prior verify step".to_string());
                }
            } else {
                name = vid.unwrap_or_else(|| kind.clone());
                path = tverify_path(run_id, ti, vi);
                indent = 2;
                kind_str = "task_verify".to_string();
                // Depends on every session in the task, then prior task-verify.
                for s in sessions.iter().filter(|s| s.task_idx == ti) {
                    deps_paths.push(session_path(run_id, ti, s.idx));
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
                if sessions.iter().filter(|s| s.task_idx == ti).any(|s| {
                    !NodeState::parse(&state_of(ti, s.idx))
                        .is_some_and(|n| n.satisfies_dependents())
                }) {
                    blocked_by.push(format!("task {tname} sessions"));
                }
                if vi > 0 {
                    deps_paths.push(tverify_path(run_id, ti, vi - 1));
                    blocked_by.push("prior verify step".to_string());
                }
            }
            let readiness = classify(
                vstate.as_str(),
                excluded,
                blocked_by.is_empty() && run_deps_ok,
            );
            let tname = task_names.get(&ti).cloned().unwrap_or_default();
            out.push(QueueItem {
                run_id: run_id.to_string(),
                run_label: run_label.map(str::to_string),
                run_state: run_state.to_string(),
                run_created_at_ms: run_created,
                kind: kind_str,
                path,
                indent,
                task_idx: ti,
                task_name: tname,
                session_idx: if scope == "session" { sidx } else { -1 },
                verify_idx: vi,
                verify_scope: scope,
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
            QueuePathKind::Session => self.conn.execute(
                "UPDATE sessions SET queue_rank=? WHERE run_id=? AND task_idx=? AND idx=?",
                params![rank, p.run_id, p.task_idx, p.session_idx],
            )?,
            QueuePathKind::SessionVerify => self.conn.execute(
                "UPDATE verifies SET queue_rank=? WHERE run_id=? AND task_idx=? AND scope='session' AND session_idx=? AND idx=?",
                params![rank, p.run_id, p.task_idx, p.session_idx, p.verify_idx],
            )?,
            QueuePathKind::TaskVerify => self.conn.execute(
                "UPDATE verifies SET queue_rank=? WHERE run_id=? AND task_idx=? AND scope='task' AND session_idx=-1 AND idx=?",
                params![rank, p.run_id, p.task_idx, p.verify_idx],
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
            run_id: None,
            guardian_id: None,
            session_id: None,
            task: None,
            payload: serde_json::json!({"items": ordered.len()}),
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
            run_id: None,
            guardian_id: None,
            session_id: None,
            task: None,
            payload: serde_json::json!({
                "n": selected.len(),
                "position": position,
                "mode": if absolute { "absolute" } else { "relative" },
            }),
        });
        self.reorder_queue(&desired)
    }
}

/// The recorded outcome of running a session.
#[derive(Debug, Clone)]
pub struct SessionOutcome {
    /// Final session state.
    pub state: NodeState,
    /// Input tokens used.
    pub tokens_in: i64,
    /// Output tokens used.
    pub tokens_out: i64,
    /// Cost in USD.
    pub cost_usd: f64,
    /// Error detail, if failed.
    pub error: Option<String>,
    /// Claude Code session UUID for `claude --resume`, if captured.
    pub claude_session_id: Option<String>,
}

// ── Queue view types + helpers (RAL Queue) ───────────────────────────────────

/// A single reorderable unit of runnable work in the Queue view.
#[derive(Debug, Clone, Serialize)]
pub struct QueueItem {
    /// Owning run id.
    pub run_id: String,
    /// Owning run label, if any.
    pub run_label: Option<String>,
    /// Owning run state.
    pub run_state: String,
    /// Owning run creation time (stable grouping / tie-break).
    pub run_created_at_ms: i64,
    /// `session` | `session_verify` | `task_verify`.
    pub kind: String,
    /// Selector path addressing this item (see [`parse_queue_path`]).
    pub path: String,
    /// Tree depth: run=0, task=1, session/task_verify=2, session_verify=3.
    pub indent: u8,
    /// Owning task index.
    pub task_idx: i64,
    /// Owning task display name.
    pub task_name: String,
    /// Session index (-1 for task-scope verifies).
    pub session_idx: i64,
    /// Verify index (-1 for sessions).
    pub verify_idx: i64,
    /// `""` | `session` | `task`.
    pub verify_scope: String,
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
    /// This item's own declared dependency references (session `depends_on`;
    /// empty for verifies). Shown on the row so the link is visible.
    pub depends_on: Vec<String>,
    /// Paths of the queue items this one depends on (for drag-along + reorder).
    pub deps_paths: Vec<String>,
    /// Global priority rank (lower = sooner). `None` = unranked (sorts last).
    pub queue_rank: Option<f64>,
}

/// One row of `queue_items_for_run`'s verify query:
/// `(task_idx, scope, session_idx, idx, vid, kind, state, queue_rank)`.
type VerifyQueueRow = (
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
    Session,
    SessionVerify,
    TaskVerify,
}

struct ParsedQueuePath {
    run_id: String,
    kind: QueuePathKind,
    task_idx: i64,
    session_idx: i64,
    verify_idx: i64,
}

fn session_path(run_id: &str, ti: i64, si: i64) -> String {
    format!("{run_id}/t{ti}/s{si}")
}
fn sverify_path(run_id: &str, ti: i64, si: i64, vi: i64) -> String {
    format!("{run_id}/t{ti}/s{si}/v{vi}")
}
fn tverify_path(run_id: &str, ti: i64, vi: i64) -> String {
    format!("{run_id}/t{ti}/tv{vi}")
}

/// Parse a queue item path. Grammar (the run id itself never contains `/`):
///  - `<run>/t<ti>/s<si>`       → a session
///  - `<run>/t<ti>/s<si>/v<vi>` → a session-scope verify
///  - `<run>/t<ti>/tv<vi>`      → a task-scope verify
fn parse_queue_path(path: &str) -> Option<ParsedQueuePath> {
    let segs: Vec<&str> = path.split('/').collect();
    match segs.as_slice() {
        [run, t, s] => {
            let ti = t.strip_prefix('t')?.parse().ok()?;
            if let Some(si) = s.strip_prefix('s').and_then(|v| v.parse().ok()) {
                Some(ParsedQueuePath {
                    run_id: (*run).to_string(),
                    kind: QueuePathKind::Session,
                    task_idx: ti,
                    session_idx: si,
                    verify_idx: -1,
                })
            } else {
                s.strip_prefix("tv")
                    .and_then(|v| v.parse().ok())
                    .map(|vi| ParsedQueuePath {
                        run_id: (*run).to_string(),
                        kind: QueuePathKind::TaskVerify,
                        task_idx: ti,
                        session_idx: -1,
                        verify_idx: vi,
                    })
            }
        }
        [run, t, s, v] => {
            let ti = t.strip_prefix('t')?.parse().ok()?;
            let si = s.strip_prefix('s')?.parse().ok()?;
            let vi = v.strip_prefix('v')?.parse().ok()?;
            Some(ParsedQueuePath {
                run_id: (*run).to_string(),
                kind: QueuePathKind::SessionVerify,
                task_idx: ti,
                session_idx: si,
                verify_idx: vi,
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

/// Canonical sort key: ranked items first (ascending), then unranked by run
/// creation, task, sessions-before-verifies, and index.
fn queue_sort_key(i: &QueueItem) -> (f64, i64, i64, i64, i64, i64) {
    let (a, b, c) = match i.kind.as_str() {
        "session" => (i.session_idx, 0, 0),
        "session_verify" => (i.session_idx, 1, i.verify_idx),
        _ => (i64::MAX, 0, i.verify_idx), // task_verify sorts after its sessions
    };
    (
        i.queue_rank.unwrap_or(f64::INFINITY),
        i.run_created_at_ms,
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
[[task.session]]
id = "worker"
cwd = "/repo"
prompt = "make it build"
[[task.session.verify]]
id = "fmt"
command = "cargo fmt --check"
[[task.verify]]
command = "cargo test"
"#;

    fn parse(src: &str) -> TaskFile {
        toml::from_str(src).expect("valid toml")
    }

    #[test]
    fn insert_and_fetch_run() {
        let mut store = Store::open_in_memory().unwrap();
        let id = store
            .insert_run(&parse(SAMPLE), Some("my run"), false)
            .unwrap();
        assert_eq!(id, "run-000000000001");

        let run = store.get_run(&id).unwrap();
        assert_eq!(run.label.as_deref(), Some("my run"));
        assert_eq!(run.state, "pending");
        assert_eq!(run.tasks.len(), 1);
        let task = &run.tasks[0];
        assert_eq!(task.name, "build");
        assert_eq!(task.sessions.len(), 1);
        assert_eq!(task.sessions[0].id, "worker");
        assert_eq!(task.sessions[0].agent, "claude");
        assert_eq!(task.sessions[0].state, "pending");
        // session-level verify is exposed per session in the board view
        assert_eq!(task.sessions[0].verify.len(), 1);
        assert_eq!(task.sessions[0].verify[0].id.as_deref(), Some("fmt"));
        assert_eq!(task.sessions[0].verify[0].kind, "command");
        assert_eq!(task.sessions[0].verify[0].spec, "cargo fmt --check");
        assert!(task.sessions[0].verify[0].model.is_none());
        assert_eq!(task.verify.len(), 1); // task-level verify
        assert_eq!(task.verify[0].kind, "command");
        assert_eq!(task.verify[0].spec, "cargo test");
    }

    #[test]
    fn run_ids_increment() {
        let mut store = Store::open_in_memory().unwrap();
        let a = store.insert_run(&parse(SAMPLE), None, false).unwrap();
        let b = store.insert_run(&parse(SAMPLE), None, false).unwrap();
        assert_eq!(a, "run-000000000001");
        assert_eq!(b, "run-000000000002");
    }

    #[test]
    fn hold_submits_as_queued_then_activates() {
        let mut store = Store::open_in_memory().unwrap();
        let id = store.insert_run(&parse(SAMPLE), None, true).unwrap();
        assert_eq!(store.run_state(&id).unwrap(), RunState::Queued);
        assert!(store.list_ready().unwrap().is_empty());

        assert_eq!(store.activate(&id).unwrap(), RunState::Pending);
        assert_eq!(store.list_ready().unwrap(), vec![id]);
    }

    #[test]
    fn default_submit_is_pending_and_ready() {
        // The old-project fix: submitting makes a run schedulable, not stuck.
        let mut store = Store::open_in_memory().unwrap();
        let id = store.insert_run(&parse(SAMPLE), None, false).unwrap();
        assert_eq!(store.run_state(&id).unwrap(), RunState::Pending);
        assert_eq!(store.list_ready().unwrap(), vec![id]);
    }

    #[test]
    fn cannot_activate_a_pending_run() {
        let mut store = Store::open_in_memory().unwrap();
        let id = store.insert_run(&parse(SAMPLE), None, false).unwrap();
        assert!(store.activate(&id).is_err());
    }

    #[test]
    fn cancel_sets_cancelled() {
        let mut store = Store::open_in_memory().unwrap();
        let id = store.insert_run(&parse(SAMPLE), None, false).unwrap();
        assert_eq!(store.cancel(&id).unwrap(), RunState::Cancelled);
        // Idempotent — cancelling an already-terminal run still succeeds
        // (RAL-116), so it stays locked out of ever being picked up again.
        assert_eq!(store.cancel(&id).unwrap(), RunState::Cancelled);
    }

    #[test]
    fn cancel_is_available_from_a_terminal_done_state() {
        let mut store = Store::open_in_memory().unwrap();
        let id = store.insert_run(&parse(SAMPLE), None, false).unwrap();
        store.set_run_state(&id, RunState::Done).unwrap();
        assert_eq!(store.cancel(&id).unwrap(), RunState::Cancelled);
        assert_eq!(store.run_state(&id).unwrap(), RunState::Cancelled);
    }

    #[test]
    fn cancel_flips_nonterminal_nodes_but_preserves_finished_ones() {
        let mut store = Store::open_in_memory().unwrap();
        let id = store.insert_run(&parse(SAMPLE), None, false).unwrap();
        store.set_run_state(&id, RunState::Running).unwrap();
        // One session already finished; the task is still running.
        store.set_session_state(&id, 0, 0, NodeState::Done).unwrap();
        store.set_task_state(&id, 0, NodeState::Running).unwrap();

        store.cancel(&id).unwrap();
        let run = store.get_run(&id).unwrap();
        assert_eq!(run.state, "cancelled");
        // The still-running task flips to cancelled…
        assert_eq!(run.tasks[0].state, "cancelled");
        // …but the session that already completed keeps its real outcome.
        assert_eq!(run.tasks[0].sessions[0].state, "done");
    }

    #[test]
    fn cancel_run_dry_run_reports_impact_without_mutating() {
        let mut store = Store::open_in_memory().unwrap();
        let (a, b, c) = dependent_chain(&mut store); // a <- b <- c

        let impact = store.cancel_run(&a, true).unwrap();
        let ids: Vec<&str> = impact.runs.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, vec![a.as_str(), b.as_str(), c.as_str()]);

        // Nothing was actually mutated.
        assert_eq!(store.run_state(&a).unwrap(), RunState::Pending);
        assert_eq!(store.run_state(&b).unwrap(), RunState::Pending);
        assert_eq!(store.run_state(&c).unwrap(), RunState::Pending);
    }

    #[test]
    fn cancel_run_cascades_to_downstream_dependents() {
        let mut store = Store::open_in_memory().unwrap();
        let (a, b, c) = dependent_chain(&mut store); // a <- b <- c

        let impact = store.cancel_run(&a, false).unwrap();
        let ids: Vec<&str> = impact.runs.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, vec![a.as_str(), b.as_str(), c.as_str()]);

        assert_eq!(store.run_state(&a).unwrap(), RunState::Cancelled);
        assert_eq!(store.run_state(&b).unwrap(), RunState::Cancelled);
        assert_eq!(store.run_state(&c).unwrap(), RunState::Cancelled);
    }

    #[test]
    fn cancel_run_cascades_even_to_already_terminal_dependents() {
        let mut store = Store::open_in_memory().unwrap();
        let (a, b, _c) = dependent_chain(&mut store); // a <- b <- c
        store.set_run_state(&b, RunState::Done).unwrap();

        store.cancel_run(&a, false).unwrap();
        // Terminal or not, a run that depends (even transitively) on a
        // cancelled run is locked out of ever being picked up again.
        assert_eq!(store.run_state(&b).unwrap(), RunState::Cancelled);
    }

    #[test]
    fn cancel_run_missing_is_not_found() {
        let store = Store::open_in_memory().unwrap();
        assert!(matches!(
            store.cancel_run("nope", true),
            Err(StoreError::NotFound)
        ));
        assert!(matches!(
            store.cancel_run("nope", false),
            Err(StoreError::NotFound)
        ));
    }

    #[test]
    fn session_shows_running_while_its_own_verify_is_still_in_flight() {
        // Regression: the board must not show a session as "done" while one of
        // its own session-level verify steps is still pending/running — even
        // though the persisted `sessions.state` column is (by design, RAL-64)
        // already "done" at that point.
        let mut store = Store::open_in_memory().unwrap();
        let id = store
            .insert_run(&parse(THREE_SESSION_VERIFIES), None, false)
            .unwrap();
        store.set_session_state(&id, 0, 0, NodeState::Done).unwrap();
        store
            .set_verify_state(&id, 0, "session", 0, 0, NodeState::Done)
            .unwrap();
        store
            .set_verify_state(&id, 0, "session", 0, 1, NodeState::Running)
            .unwrap();
        // Index 2 stays "pending".

        let run = store.get_run(&id).unwrap();
        assert_eq!(
            run.tasks[0].sessions[0].state, "running",
            "session must read as running while a verify step is still in flight"
        );

        // Once the failing/last verify fails, the session should read as failed.
        store
            .set_verify_state(&id, 0, "session", 0, 1, NodeState::Failed)
            .unwrap();
        store
            .set_verify_state(&id, 0, "session", 0, 2, NodeState::Cancelled)
            .unwrap();
        let run = store.get_run(&id).unwrap();
        assert_eq!(
            run.tasks[0].sessions[0].state, "failed",
            "session must read as failed when one of its verifies failed"
        );
    }

    #[test]
    fn session_reads_done_when_a_verify_step_is_ignored_not_stuck_running() {
        // Regression: an `ignored` verify step (a user-set skip/pass-through,
        // per the scheduler's `continue`-on-ignored handling) must count as a
        // terminal state for the session rollup, same as done/failed/cancelled
        // — otherwise a session with an ignored verify step reads "running"
        // forever even after the run has actually finished.
        let mut store = Store::open_in_memory().unwrap();
        let id = store
            .insert_run(&parse(THREE_SESSION_VERIFIES), None, false)
            .unwrap();
        store.set_session_state(&id, 0, 0, NodeState::Done).unwrap();
        store
            .set_verify_state(&id, 0, "session", 0, 0, NodeState::Done)
            .unwrap();
        store
            .set_verify_state(&id, 0, "session", 0, 1, NodeState::Ignored)
            .unwrap();
        store
            .set_verify_state(&id, 0, "session", 0, 2, NodeState::Done)
            .unwrap();

        let run = store.get_run(&id).unwrap();
        assert_eq!(
            run.tasks[0].sessions[0].state, "done",
            "an ignored verify step must not keep the session stuck at 'running'"
        );
    }

    #[test]
    fn session_and_task_state_transitions() {
        let mut store = Store::open_in_memory().unwrap();
        let id = store.insert_run(&parse(SAMPLE), None, false).unwrap();
        store.set_run_state(&id, RunState::Running).unwrap();
        store.set_session_state(&id, 0, 0, NodeState::Done).unwrap();
        // SAMPLE's session has its own "fmt" verify — mark it done too so the
        // displayed session state (which folds verify progress back in) reads
        // as fully done, not "running" on a still-pending verify.
        store
            .set_verify_state(&id, 0, "session", 0, 0, NodeState::Done)
            .unwrap();
        store.set_task_state(&id, 0, NodeState::Done).unwrap();
        assert_eq!(store.running_count().unwrap(), 1);

        let run = store.get_run(&id).unwrap();
        assert_eq!(run.state, "running");
        assert_eq!(run.tasks[0].state, "done");
        assert_eq!(run.tasks[0].sessions[0].state, "done");
    }

    #[test]
    fn missing_run_is_not_found() {
        let store = Store::open_in_memory().unwrap();
        assert!(matches!(store.get_run("nope"), Err(StoreError::NotFound)));
    }

    #[test]
    fn recover_orphaned_runs_resets_running_but_keeps_done() {
        let two = r#"
[[task]]
name = "t"
[[task.session]]
id = "a"
cwd = "/repo"
command = "x"
[[task.session]]
id = "b"
cwd = "/repo"
command = "y"
"#;
        let mut store = Store::open_in_memory().unwrap();
        let id = store.insert_run(&parse(two), None, false).unwrap();
        // Simulate an unclean shutdown mid-run: one session finished, one was
        // still executing when the process died.
        store.set_run_state(&id, RunState::Running).unwrap();
        store.set_task_state(&id, 0, NodeState::Running).unwrap();
        store.set_session_state(&id, 0, 0, NodeState::Done).unwrap();
        store
            .set_session_state(&id, 0, 1, NodeState::Running)
            .unwrap();
        assert_eq!(store.running_session_count().unwrap(), 1);

        let recovered = store.recover_orphaned_runs().unwrap();
        assert_eq!(recovered, vec![id.clone()]);

        let run = store.get_run(&id).unwrap();
        assert_eq!(run.state, "pending"); // re-claimable by the scheduler
        assert_eq!(run.tasks[0].state, "pending");
        // Finished work is preserved (skipped on resume); orphaned work resets.
        assert_eq!(run.tasks[0].sessions[0].state, "done");
        assert_eq!(run.tasks[0].sessions[1].state, "pending");
        assert_eq!(store.running_session_count().unwrap(), 0);
    }

    #[test]
    fn cross_run_dependency_gates_readiness() {
        let mut store = Store::open_in_memory().unwrap();
        let a = store.insert_run(&parse(SAMPLE), Some("a"), false).unwrap();
        // Run b depends on run a via a [[default]] depends_on.
        let dep_toml = format!(
            "[[default]]\ndepends_on = [\"{a}\"]\n[[task]]\nname=\"b\"\n[[task.session]]\ncwd=\"/r\"\nprompt=\"p\"\n"
        );
        let b = store
            .insert_run(&parse(&dep_toml), Some("b"), false)
            .unwrap();

        // Only `a` is ready while `a` is still pending.
        let ready = store.list_ready().unwrap();
        assert!(ready.contains(&a));
        assert!(!ready.contains(&b));

        // Once `a` is Done, `b` becomes ready.
        store.set_run_state(&a, RunState::Done).unwrap();
        let ready = store.list_ready().unwrap();
        assert!(ready.contains(&b));
    }

    #[test]
    fn add_run_dependency_appends_and_gates_readiness() {
        let mut store = Store::open_in_memory().unwrap();
        let a = store.insert_run(&parse(SAMPLE), Some("a"), false).unwrap();
        let b = store.insert_run(&parse(SAMPLE), Some("b"), false).unwrap();

        assert!(store.list_ready().unwrap().contains(&b));
        let deps = store.add_run_dependency(&b, &a).unwrap();
        assert_eq!(deps, vec![a.clone()]);
        assert!(!store.list_ready().unwrap().contains(&b));

        store.set_run_state(&a, RunState::Done).unwrap();
        assert!(store.list_ready().unwrap().contains(&b));
    }

    #[test]
    fn add_run_dependency_is_idempotent() {
        let mut store = Store::open_in_memory().unwrap();
        let a = store.insert_run(&parse(SAMPLE), Some("a"), false).unwrap();
        let b = store.insert_run(&parse(SAMPLE), Some("b"), false).unwrap();
        store.add_run_dependency(&b, &a).unwrap();
        let deps = store.add_run_dependency(&b, &a).unwrap();
        assert_eq!(deps, vec![a]);
    }

    #[test]
    fn add_run_dependency_rejects_self_reference() {
        let mut store = Store::open_in_memory().unwrap();
        let a = store.insert_run(&parse(SAMPLE), Some("a"), false).unwrap();
        assert!(matches!(
            store.add_run_dependency(&a, &a),
            Err(StoreError::InvalidTransition(_))
        ));
    }

    #[test]
    fn add_run_dependency_rejects_direct_cycle() {
        let mut store = Store::open_in_memory().unwrap();
        let a = store.insert_run(&parse(SAMPLE), Some("a"), false).unwrap();
        let b = store.insert_run(&parse(SAMPLE), Some("b"), false).unwrap();
        store.add_run_dependency(&b, &a).unwrap(); // b depends on a
        assert!(matches!(
            store.add_run_dependency(&a, &b), // a depends on b -> cycle
            Err(StoreError::InvalidTransition(_))
        ));
    }

    #[test]
    fn add_run_dependency_rejects_transitive_cycle() {
        let mut store = Store::open_in_memory().unwrap();
        let (a, _b, c) = dependent_chain(&mut store); // a <- b <- c
        // c already (transitively) depends on a; making a depend on c is a cycle.
        assert!(matches!(
            store.add_run_dependency(&a, &c),
            Err(StoreError::InvalidTransition(_))
        ));
    }

    #[test]
    fn add_run_dependency_missing_run_or_target_is_not_found() {
        let mut store = Store::open_in_memory().unwrap();
        let a = store.insert_run(&parse(SAMPLE), Some("a"), false).unwrap();
        assert!(matches!(
            store.add_run_dependency("nope", &a),
            Err(StoreError::NotFound)
        ));
        assert!(matches!(
            store.add_run_dependency(&a, "nope"),
            Err(StoreError::NotFound)
        ));
    }

    // RAL-19: helper to build a chain of runs a <- b <- c (each depends on the
    // prior via a [[default]] depends_on) and return their ids.
    fn dependent_chain(store: &mut Store) -> (String, String, String) {
        let a = store.insert_run(&parse(SAMPLE), Some("a"), false).unwrap();
        let dep_b = format!(
            "[[default]]\ndepends_on = [\"{a}\"]\n[[task]]\nname=\"b\"\n[[task.session]]\ncwd=\"/r\"\nprompt=\"p\"\n"
        );
        let b = store.insert_run(&parse(&dep_b), Some("b"), false).unwrap();
        let dep_c = format!(
            "[[default]]\ndepends_on = [\"{b}\"]\n[[task]]\nname=\"c\"\n[[task.session]]\ncwd=\"/r\"\nprompt=\"p\"\n"
        );
        let c = store.insert_run(&parse(&dep_c), Some("c"), false).unwrap();
        (a, b, c)
    }

    #[test]
    fn restarting_a_run_dirties_all_downstream_runs() {
        let mut store = Store::open_in_memory().unwrap();
        let (a, b, c) = dependent_chain(&mut store);
        // Drive the whole chain to Done.
        for id in [&a, &b, &c] {
            store.set_run_state(id, RunState::Done).unwrap();
        }
        // Restart A: it and both downstream runs must be dirty (Pending) again.
        let dirtied = store.restart_run(&a).unwrap();
        assert!(
            dirtied.contains(&b) && dirtied.contains(&c),
            "cascade to b and c"
        );
        assert_eq!(store.run_state(&a).unwrap(), RunState::Pending);
        assert_eq!(store.run_state(&b).unwrap(), RunState::Pending);
        assert_eq!(store.run_state(&c).unwrap(), RunState::Pending);
    }

    #[test]
    fn restarting_a_session_dirties_downstream_runs_too() {
        let mut store = Store::open_in_memory().unwrap();
        let (a, b, _c) = dependent_chain(&mut store);
        for id in [&a, &b] {
            store.set_run_state(id, RunState::Done).unwrap();
        }
        // Restart A's single session (task 0, session 0): A goes Pending and the
        // downstream run B is dirtied.
        let dirtied = store.restart_session(&a, 0, 0).unwrap();
        assert!(dirtied.contains(&b));
        assert_eq!(store.run_state(&a).unwrap(), RunState::Pending);
        assert_eq!(store.run_state(&b).unwrap(), RunState::Pending);
    }

    #[test]
    fn run_restart_preview_matches_the_real_restart_without_mutating() {
        // RAL-104: the dry-run preview must report exactly what the real
        // restart would dirty, and must not touch any state itself.
        let mut store = Store::open_in_memory().unwrap();
        let (a, b, c) = dependent_chain(&mut store);
        for id in [&a, &b, &c] {
            store.set_run_state(id, RunState::Done).unwrap();
        }

        let preview = store.compute_run_restart_impact(&a).unwrap();
        let dirtied_ids: Vec<&str> = preview.dirtied_runs.iter().map(|r| r.id.as_str()).collect();
        assert!(dirtied_ids.contains(&b.as_str()) && dirtied_ids.contains(&c.as_str()));
        assert_eq!(preview.sessions.len(), 1, "a has one session");
        assert_eq!(preview.tasks.len(), 1, "a has one task");

        // Nothing was mutated by computing the preview.
        assert_eq!(store.run_state(&a).unwrap(), RunState::Done);
        assert_eq!(store.run_state(&b).unwrap(), RunState::Done);
        assert_eq!(store.run_state(&c).unwrap(), RunState::Done);

        // The real restart dirties exactly the same set the preview reported.
        let dirtied = store.restart_run(&a).unwrap();
        assert_eq!(dirtied.len(), preview.dirtied_runs.len());
        for id in &dirtied {
            assert!(dirtied_ids.contains(&id.as_str()));
        }
        assert_eq!(store.run_state(&a).unwrap(), RunState::Pending);
        assert_eq!(store.run_state(&b).unwrap(), RunState::Pending);
        assert_eq!(store.run_state(&c).unwrap(), RunState::Pending);
    }

    #[test]
    fn session_restart_preview_matches_the_real_restart_without_mutating() {
        // RAL-104: same guarantee as the run-level preview, for a single
        // session restart with an in-run downstream chain plus a dependent run.
        let mut store = Store::open_in_memory().unwrap();
        let toml = "[[task]]\nname=\"t\"\n\
            [[task.session]]\nid=\"s1\"\ncwd=\"/r\"\nprompt=\"p\"\n\
            [[task.session]]\nid=\"s2\"\ncwd=\"/r\"\nprompt=\"p\"\ndepends_on=[\"s1\"]\n\
            [[task.session]]\nid=\"s3\"\ncwd=\"/r\"\nprompt=\"p\"\ndepends_on=[\"s2\"]\n";
        let run = store.insert_run(&parse(toml), Some("r"), false).unwrap();
        let dep_toml = format!(
            "[[default]]\ndepends_on = [\"{run}\"]\n[[task]]\nname=\"b\"\n[[task.session]]\ncwd=\"/r\"\nprompt=\"p\"\n"
        );
        let dependent = store
            .insert_run(&parse(&dep_toml), Some("dependent"), false)
            .unwrap();
        store.set_run_state(&dependent, RunState::Done).unwrap();
        for idx in 0..3 {
            store
                .set_session_state(&run, 0, idx, NodeState::Done)
                .unwrap();
        }

        let preview = store.compute_session_restart_impact(&run, 0, 1).unwrap();
        let session_ids: Vec<(i64, i64)> = preview
            .sessions
            .iter()
            .map(|s| (s.task_idx, s.idx))
            .collect();
        assert_eq!(
            session_ids,
            vec![(0, 1), (0, 2)],
            "s2 and downstream s3 only"
        );
        assert_eq!(preview.tasks.len(), 1);
        assert_eq!(
            preview
                .dirtied_runs
                .iter()
                .map(|r| r.id.clone())
                .collect::<Vec<_>>(),
            vec![dependent.clone()]
        );

        // Nothing was mutated by computing the preview.
        let done = store.done_sessions(&run).unwrap();
        assert!(done.contains(&(0, 0)) && done.contains(&(0, 1)) && done.contains(&(0, 2)));
        assert_eq!(store.run_state(&dependent).unwrap(), RunState::Done);

        // The real restart applies exactly what the preview reported.
        store.restart_session(&run, 0, 1).unwrap();
        let done = store.done_sessions(&run).unwrap();
        assert!(done.contains(&(0, 0)), "s1 stays done");
        assert!(!done.contains(&(0, 1)) && !done.contains(&(0, 2)));
        assert_eq!(store.run_state(&dependent).unwrap(), RunState::Pending);
    }

    #[test]
    fn restart_session_resets_downstream_sessions_in_run_but_not_upstream() {
        // s1 -> s2 -> s3 within one task; restarting s2 dirties s2 and s3 while
        // s1 stays Done (and is later skipped on re-run).
        let mut store = Store::open_in_memory().unwrap();
        let toml = "[[task]]\nname=\"t\"\n\
            [[task.session]]\nid=\"s1\"\ncwd=\"/r\"\nprompt=\"p\"\n\
            [[task.session]]\nid=\"s2\"\ncwd=\"/r\"\nprompt=\"p\"\ndepends_on=[\"s1\"]\n\
            [[task.session]]\nid=\"s3\"\ncwd=\"/r\"\nprompt=\"p\"\ndepends_on=[\"s2\"]\n";
        let run = store.insert_run(&parse(toml), Some("r"), false).unwrap();
        for idx in 0..3 {
            store
                .set_session_state(&run, 0, idx, NodeState::Done)
                .unwrap();
        }
        store.restart_session(&run, 0, 1).unwrap(); // restart s2
        let done = store.done_sessions(&run).unwrap();
        assert!(done.contains(&(0, 0)), "s1 stays done");
        assert!(!done.contains(&(0, 1)), "s2 dirtied");
        assert!(!done.contains(&(0, 2)), "s3 dirtied (downstream of s2)");
    }

    #[test]
    fn done_sessions_excludes_sessions_with_pending_session_verifies() {
        // RAL-64: if the daemon stopped between record_session_result and
        // run_verifies, the session is 'done' in the DB but its verifies are
        // still 'pending'. done_sessions must NOT return such a session, so that
        // the scheduler re-runs the worker and the verifies actually execute.
        let mut store = Store::open_in_memory().unwrap();
        let toml = "[[task]]\nname=\"t\"\n\
            [[task.session]]\nid=\"s1\"\ncwd=\"/r\"\nprompt=\"p\"\n\
            [[task.session.verify]]\ncommand=\"exit 0\"\n";
        let run = store.insert_run(&parse(toml), Some("r"), false).unwrap();

        // Simulate the crash window: session is Done, verify is still Pending.
        store
            .set_session_state(&run, 0, 0, NodeState::Done)
            .unwrap();
        // Verify is inserted as 'pending' by insert_run — leave it as-is.

        let done = store.done_sessions(&run).unwrap();
        assert!(
            !done.contains(&(0, 0)),
            "session with pending verify must not be in done_sessions"
        );

        // After completing the verify, the session is returned.
        store
            .set_verify_state(&run, 0, "session", 0, 0, NodeState::Done)
            .unwrap();
        let done = store.done_sessions(&run).unwrap();
        assert!(
            done.contains(&(0, 0)),
            "session with completed verify must be in done_sessions"
        );
    }

    #[test]
    fn failed_sessions_returns_only_sessions_in_failed_state() {
        let mut store = Store::open_in_memory().unwrap();
        let toml = "[[task]]\nname=\"a\"\n[[task.session]]\ncwd=\"/r\"\nprompt=\"p\"\n\
            [[task]]\nname=\"b\"\n[[task.session]]\ncwd=\"/r\"\nprompt=\"p\"\n\
            [[task]]\nname=\"c\"\n[[task.session]]\ncwd=\"/r\"\nprompt=\"p\"\n";
        let run = store.insert_run(&parse(toml), Some("r"), false).unwrap();
        store
            .set_session_state(&run, 0, 0, NodeState::Failed)
            .unwrap();
        store
            .set_session_state(&run, 1, 0, NodeState::Done)
            .unwrap();
        // task_idx 2 stays Pending (insert_run's default).

        let failed = store.failed_sessions(&run).unwrap();
        assert_eq!(failed, HashSet::from([(0, 0)]));
    }

    #[test]
    fn restart_run_on_missing_run_is_not_found() {
        let store = Store::open_in_memory().unwrap();
        assert!(matches!(
            store.restart_run("nope"),
            Err(StoreError::NotFound)
        ));
    }

    #[test]
    fn delete_run_removes_it_and_children() {
        let mut store = Store::open_in_memory().unwrap();
        let id = store
            .insert_run(&parse(SAMPLE), Some("gone"), false)
            .unwrap();
        store.delete_run(&id).unwrap();
        assert!(matches!(store.get_run(&id), Err(StoreError::NotFound)));
        // The child rows are gone too, so a re-fetch of sessions is empty.
        assert!(store.sessions_of(&id).unwrap().is_empty());
        // Deleting again is a NotFound.
        assert!(matches!(store.delete_run(&id), Err(StoreError::NotFound)));
    }

    #[test]
    fn session_lists_the_reviews_its_branch_participates_in() {
        // RAL-17: a session whose review branch is in a guardian's stack (both
        // tied to the same run) lists that review in its board view.
        let mut store = Store::open_in_memory().unwrap();
        let run = store.insert_run(&parse(SAMPLE), Some("r"), false).unwrap();
        let gid = store
            .create_guardian_for_run("Backend review", "main", "/repo", Some(&run))
            .unwrap();
        store.add_guardian_branch(&gid, "feature/a").unwrap();
        // The single SAMPLE session is task 0, session 0.
        store
            .set_session_review_branch(&run, 0, 0, "feature/a")
            .unwrap();

        let view = store.get_run(&run).unwrap();
        let session = &view.tasks[0].sessions[0];
        assert_eq!(session.reviews.len(), 1);
        assert_eq!(session.reviews[0].id, gid);
        assert_eq!(session.reviews[0].name, "Backend review");
        // A session with no review branch lists nothing.
        let run2 = store.insert_run(&parse(SAMPLE), Some("r2"), false).unwrap();
        let view2 = store.get_run(&run2).unwrap();
        assert!(view2.tasks[0].sessions[0].reviews.is_empty());
    }

    #[test]
    fn session_lists_a_review_whose_guardian_was_created_by_a_different_run() {
        // A guardian created by one run's submission (its `run_id` column) can
        // later be *found* rather than created for a second run that shares a
        // `ralphus:new-review/<key>` link (or was attached to manually) — its
        // `run_id` still points at the first run, but the second run's session
        // branch is appended to it. The "in reviews" lookup must match by
        // `sessions.review_branch = guardian_branches.branch`, not by
        // `guardians.run_id`, or the second run's session sees no review at all.
        let mut store = Store::open_in_memory().unwrap();
        let run_a = store.insert_run(&parse(SAMPLE), Some("a"), false).unwrap();
        let gid = store
            .create_guardian_for_run("Shared review", "main", "/repo", Some(&run_a))
            .unwrap();
        store.add_guardian_branch(&gid, "feature/b").unwrap();

        let run_b = store.insert_run(&parse(SAMPLE), Some("b"), false).unwrap();
        store
            .set_session_review_branch(&run_b, 0, 0, "feature/b")
            .unwrap();

        let view_b = store.get_run(&run_b).unwrap();
        let session = &view_b.tasks[0].sessions[0];
        assert_eq!(session.reviews.len(), 1);
        assert_eq!(session.reviews[0].id, gid);
        assert_eq!(session.reviews[0].name, "Shared review");
    }

    #[test]
    fn list_runs_newest_first() {
        let mut store = Store::open_in_memory().unwrap();
        let _a = store.insert_run(&parse(SAMPLE), Some("a"), false).unwrap();
        let b = store.insert_run(&parse(SAMPLE), Some("b"), false).unwrap();
        let runs = store.list_runs().unwrap();
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].id, b); // newest first
    }

    #[test]
    fn state_transitions_are_logged_and_cleaned_up() {
        let mut store = Store::open_in_memory().unwrap();
        let id = store.insert_run(&parse(SAMPLE), None, false).unwrap();
        store.set_run_state(&id, RunState::Running).unwrap();
        store.set_run_state(&id, RunState::Done).unwrap();
        let events = store.events_for_run(&id, 100).unwrap();
        // Oldest-first, and the last transition is "done".
        assert!(
            events
                .iter()
                .any(|e| e.scope == "run" && e.message.contains("running"))
        );
        assert_eq!(events.last().unwrap().message, "run → done");
        // Deleting the run removes its events.
        store.delete_run(&id).unwrap();
        assert!(store.events_for_run(&id, 100).unwrap().is_empty());
    }

    #[test]
    fn restart_session_verify_keeps_session_done_but_resets_its_verifies() {
        let mut store = Store::open_in_memory().unwrap();
        let id = store.insert_run(&parse(SAMPLE), None, false).unwrap();
        // Simulate a completed session + verify.
        store.set_session_state(&id, 0, 0, NodeState::Done).unwrap();
        store
            .set_verify_state(&id, 0, "session", 0, 0, NodeState::Done)
            .unwrap();
        store.set_run_state(&id, RunState::Done).unwrap();

        store.restart_session_verify(&id, 0, 0, 0).unwrap();

        let run = store.get_run(&id).unwrap();
        assert_eq!(run.state, "pending", "run must be pending after restart");
        assert_eq!(run.tasks[0].state, "pending", "task must be pending");
        // The persisted session body stays Done (only the verify row is reset —
        // see `done_sessions`, which relies on this for crash-recovery), but the
        // displayed state folds verify progress back in so the board doesn't
        // show the session as finished while its verify re-runs.
        assert_eq!(
            run.tasks[0].sessions[0].state, "running",
            "displayed session state must reflect its pending verify"
        );
        assert_eq!(
            run.tasks[0].sessions[0].verify[0].state, "pending",
            "session verify must be pending"
        );
    }

    #[test]
    fn restart_session_verify_on_missing_session_is_not_found() {
        let mut store = Store::open_in_memory().unwrap();
        let id = store.insert_run(&parse(SAMPLE), None, false).unwrap();
        assert!(matches!(
            store.restart_session_verify(&id, 0, 99, 0),
            Err(StoreError::NotFound)
        ));
    }

    #[test]
    fn restart_task_verify_resets_only_task_verifies_sessions_remain_done() {
        let mut store = Store::open_in_memory().unwrap();
        let id = store.insert_run(&parse(SAMPLE), None, false).unwrap();
        // Simulate a task where sessions passed but the task-level verify failed.
        store.set_session_state(&id, 0, 0, NodeState::Done).unwrap();
        store
            .set_verify_state(&id, 0, "session", 0, 0, NodeState::Done)
            .unwrap();
        store
            .set_verify_state(&id, 0, "task", -1, 0, NodeState::Failed)
            .unwrap();
        store.set_task_state(&id, 0, NodeState::Failed).unwrap();
        store.set_run_state(&id, RunState::Failed).unwrap();

        store.restart_task_verify(&id, 0, 0).unwrap();

        let run = store.get_run(&id).unwrap();
        assert_eq!(run.state, "pending");
        assert_eq!(run.tasks[0].state, "pending");
        // Sessions and session-level verifies are NOT reset — only task-level verifies are.
        assert_eq!(
            run.tasks[0].sessions[0].state, "done",
            "session must remain done"
        );
        assert_eq!(
            run.tasks[0].sessions[0].verify[0].state, "done",
            "session-level verify must remain done"
        );
        assert_eq!(run.tasks[0].verify[0].state, "pending");
    }

    #[test]
    fn restart_task_verify_on_missing_task_is_not_found() {
        let mut store = Store::open_in_memory().unwrap();
        let id = store.insert_run(&parse(SAMPLE), None, false).unwrap();
        assert!(matches!(
            store.restart_task_verify(&id, 99, 0),
            Err(StoreError::NotFound)
        ));
    }

    // Three session-level verify steps: restart from vi=1 leaves vi=0 Done,
    // resets vi=1 and vi=2 to Pending.
    const THREE_SESSION_VERIFIES: &str = r#"
[[task]]
name = "t"
[[task.session]]
cwd = "."
command = "build"
[[task.session.verify]]
command = "check-a"
[[task.session.verify]]
command = "check-b"
[[task.session.verify]]
command = "check-c"
"#;

    // Three task-level verify steps (no session-level verifies).
    const THREE_TASK_VERIFIES: &str = r#"
[[task]]
name = "t"
[[task.session]]
cwd = "."
command = "build"
[[task.verify]]
command = "check-a"
[[task.verify]]
command = "check-b"
[[task.verify]]
command = "check-c"
"#;

    #[test]
    fn restart_session_verify_from_middle_leaves_earlier_step_intact() {
        let mut store = Store::open_in_memory().unwrap();
        let id = store
            .insert_run(&parse(THREE_SESSION_VERIFIES), None, false)
            .unwrap();
        // Simulate: session done, all three verifies done.
        store.set_session_state(&id, 0, 0, NodeState::Done).unwrap();
        for vi in 0..3i64 {
            store
                .set_verify_state(&id, 0, "session", 0, vi, NodeState::Done)
                .unwrap();
        }
        store.set_run_state(&id, RunState::Done).unwrap();

        // Restart from vi=1 — only steps 1 and 2 should reset.
        store.restart_session_verify(&id, 0, 0, 1).unwrap();

        let run = store.get_run(&id).unwrap();
        let vs = &run.tasks[0].sessions[0].verify;
        assert_eq!(vs[0].state, "done", "vi=0 must stay done");
        assert_eq!(vs[1].state, "pending", "vi=1 must be reset to pending");
        assert_eq!(vs[2].state, "pending", "vi=2 must be reset to pending");
        assert_eq!(run.tasks[0].state, "pending");
        assert_eq!(run.state, "pending");
    }

    #[test]
    fn restart_session_verify_from_last_only_resets_that_step() {
        let mut store = Store::open_in_memory().unwrap();
        let id = store
            .insert_run(&parse(THREE_SESSION_VERIFIES), None, false)
            .unwrap();
        store.set_session_state(&id, 0, 0, NodeState::Done).unwrap();
        for vi in 0..3i64 {
            store
                .set_verify_state(&id, 0, "session", 0, vi, NodeState::Done)
                .unwrap();
        }
        store.set_run_state(&id, RunState::Done).unwrap();

        // Restart from vi=2 — only the last step resets.
        store.restart_session_verify(&id, 0, 0, 2).unwrap();

        let run = store.get_run(&id).unwrap();
        let vs = &run.tasks[0].sessions[0].verify;
        assert_eq!(vs[0].state, "done", "vi=0 must stay done");
        assert_eq!(vs[1].state, "done", "vi=1 must stay done");
        assert_eq!(vs[2].state, "pending", "vi=2 must be reset to pending");
    }

    #[test]
    fn restart_task_verify_from_middle_leaves_earlier_step_intact() {
        let mut store = Store::open_in_memory().unwrap();
        let id = store
            .insert_run(&parse(THREE_TASK_VERIFIES), None, false)
            .unwrap();
        // Simulate: session done, all three task-level verifies done.
        store.set_session_state(&id, 0, 0, NodeState::Done).unwrap();
        for vi in 0..3i64 {
            store
                .set_verify_state(&id, 0, "task", -1, vi, NodeState::Done)
                .unwrap();
        }
        store.set_task_state(&id, 0, NodeState::Done).unwrap();
        store.set_run_state(&id, RunState::Done).unwrap();

        // Restart from vi=1 — only steps 1 and 2 should reset.
        store.restart_task_verify(&id, 0, 1).unwrap();

        let run = store.get_run(&id).unwrap();
        let vs = &run.tasks[0].verify;
        assert_eq!(vs[0].state, "done", "vi=0 must stay done");
        assert_eq!(vs[1].state, "pending", "vi=1 must be reset to pending");
        assert_eq!(vs[2].state, "pending", "vi=2 must be reset to pending");
        assert_eq!(run.tasks[0].state, "pending");
        assert_eq!(run.state, "pending");
        // Session must be untouched.
        assert_eq!(run.tasks[0].sessions[0].state, "done");
    }

    // ── RAL Queue ────────────────────────────────────────────────────────────

    const TWO_SESSION_CHAIN: &str = "[[task]]\nname=\"t\"\n\
        [[task.session]]\nid=\"a\"\ncwd=\"/r\"\nprompt=\"p\"\n\
        [[task.session]]\nid=\"b\"\ncwd=\"/r\"\nprompt=\"p\"\ndepends_on=[\"a\"]\n";

    #[test]
    fn ignored_state_round_trips() {
        assert_eq!(NodeState::parse("ignored"), Some(NodeState::Ignored));
        assert_eq!(NodeState::Ignored.as_str(), "ignored");
        assert_eq!(RunState::parse("ignored"), Some(RunState::Ignored));
        assert_eq!(RunState::Ignored.as_str(), "ignored");
        assert!(NodeState::Ignored.satisfies_dependents());
        assert!(RunState::Ignored.satisfies_dependents());
        assert!(!RunState::Ignored.is_terminal(), "ignored is reversible");
    }

    #[test]
    fn ignored_upstream_run_satisfies_dependents() {
        let mut store = Store::open_in_memory().unwrap();
        let a = store.insert_run(&parse(SAMPLE), Some("a"), false).unwrap();
        let dep = format!(
            "[[default]]\ndepends_on = [\"{a}\"]\n[[task]]\nname=\"b\"\n[[task.session]]\ncwd=\"/r\"\nprompt=\"p\"\n"
        );
        let b = store.insert_run(&parse(&dep), Some("b"), false).unwrap();
        assert!(!store.list_ready().unwrap().contains(&b));
        // Ignoring the upstream run unblocks the dependent, exactly like done.
        store.set_run_state(&a, RunState::Ignored).unwrap();
        assert!(store.list_ready().unwrap().contains(&b));
    }

    #[test]
    fn queue_lists_ready_and_blocked_sessions() {
        let mut store = Store::open_in_memory().unwrap();
        let run = store
            .insert_run(&parse(TWO_SESSION_CHAIN), Some("r"), false)
            .unwrap();
        let q = store.queue().unwrap();
        let a = q.iter().find(|i| i.path.ends_with("/s0")).unwrap();
        let b = q.iter().find(|i| i.path.ends_with("/s1")).unwrap();
        assert_eq!(a.readiness, "ready", "session a has no deps");
        assert_eq!(b.readiness, "blocked", "session b waits on a");
        assert_eq!(b.deps_paths, vec![session_path(&run, 0, 0)]);
    }

    #[test]
    fn queue_ignored_upstream_makes_downstream_ready() {
        let mut store = Store::open_in_memory().unwrap();
        let run = store
            .insert_run(&parse(TWO_SESSION_CHAIN), None, false)
            .unwrap();
        // Ignore session a → session b becomes ready.
        store
            .set_session_state(&run, 0, 0, NodeState::Ignored)
            .unwrap();
        let q = store.queue().unwrap();
        // a is ignored (terminal-like) so it drops out of the queue; b is ready.
        assert!(q.iter().all(|i| !i.path.ends_with("/s0")));
        let b = q.iter().find(|i| i.path.ends_with("/s1")).unwrap();
        assert_eq!(b.readiness, "ready");
    }

    #[test]
    fn queue_exposes_task_depends_on() {
        let toml = "[[task]]\nname=\"a\"\n[[task.session]]\ncwd=\"/r\"\ncommand=\"x\"\n\
                    [[task]]\nname=\"b\"\ndepends_on=[\"a\"]\n[[task.session]]\ncwd=\"/r\"\ncommand=\"y\"\n";
        let mut store = Store::open_in_memory().unwrap();
        let _run = store.insert_run(&parse(toml), None, false).unwrap();
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
        let run = store
            .insert_run(&parse(TWO_SESSION_CHAIN), None, false)
            .unwrap();
        let pa = session_path(&run, 0, 0);
        let pb = session_path(&run, 0, 1);
        let order = store.reorder_queue(&[pb.clone(), pa.clone()]).unwrap();
        assert_eq!(order, vec![pa, pb], "dependency dragged along");
    }

    #[test]
    fn set_position_absolute_clamps_to_bottom() {
        let mut store = Store::open_in_memory().unwrap();
        let run = store
            .insert_run(&parse(TWO_SESSION_CHAIN), None, false)
            .unwrap();
        let pa = session_path(&run, 0, 0);
        let pb = session_path(&run, 0, 1);
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
    fn restart_task_verify_from_last_only_resets_that_step() {
        let mut store = Store::open_in_memory().unwrap();
        let id = store
            .insert_run(&parse(THREE_TASK_VERIFIES), None, false)
            .unwrap();
        store.set_session_state(&id, 0, 0, NodeState::Done).unwrap();
        for vi in 0..3i64 {
            store
                .set_verify_state(&id, 0, "task", -1, vi, NodeState::Done)
                .unwrap();
        }
        store.set_task_state(&id, 0, NodeState::Done).unwrap();
        store.set_run_state(&id, RunState::Done).unwrap();

        // Restart from vi=2 — only the last step resets.
        store.restart_task_verify(&id, 0, 2).unwrap();

        let run = store.get_run(&id).unwrap();
        let vs = &run.tasks[0].verify;
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
        assert_eq!(p.vcs, "git");
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
    fn set_session_cwd_rewrites_placeholder_to_real_path() {
        let mut store = Store::open_in_memory().unwrap();
        let id = store.insert_run(&parse(SAMPLE), None, false).unwrap();
        store
            .set_session_cwd(&id, 0, 0, "C:/repos/ralphus/.git/.ralphus_worktrees/feat")
            .unwrap();
        let run = store.get_run(&id).unwrap();
        assert_eq!(
            run.tasks[0].sessions[0].cwd.as_deref(),
            Some("C:/repos/ralphus/.git/.ralphus_worktrees/feat")
        );
    }
}
