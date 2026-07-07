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
            _ => return None,
        })
    }

    /// Whether this is a terminal state (no further transitions).
    #[must_use]
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Done | Self::Failed | Self::Cancelled)
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
}

impl NodeState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
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
                updated_at_ms INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS tasks (
                run_id     TEXT NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
                idx        INTEGER NOT NULL,
                name       TEXT NOT NULL,
                project    TEXT,
                state      TEXT NOT NULL,
                depends_on TEXT NOT NULL DEFAULT '[]',
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
                state       TEXT NOT NULL,
                output      TEXT,
                timeout_sec   INTEGER,
                budget_tokens INTEGER,
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
                skip_checks       INTEGER NOT NULL DEFAULT 0,
                review_type       TEXT NOT NULL DEFAULT 'git',
                skip_worktrees    INTEGER NOT NULL DEFAULT 0,
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
            ",
        )?;
        // Best-effort migrations for databases created before these columns
        // existed. Each fails harmlessly (duplicate column) once present.
        for stmt in [
            "ALTER TABLE guardians ADD COLUMN run_id TEXT",
            "ALTER TABLE guardians ADD COLUMN combined_worktree TEXT",
            "ALTER TABLE guardians ADD COLUMN conflicts_total INTEGER",
            "ALTER TABLE guardians ADD COLUMN conflicts_remaining INTEGER",
            "ALTER TABLE guardians ADD COLUMN skip_checks INTEGER NOT NULL DEFAULT 0",
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
            // RAL-43: branch can be disabled (dropped from the stack) without deletion.
            "ALTER TABLE guardian_branches ADD COLUMN enabled INTEGER NOT NULL DEFAULT 1",
            // Which git project (repository root) this branch lives in (RAL-29).
            // NULL means the guardian's own git_root (backward compatible).
            "ALTER TABLE guardian_branches ADD COLUMN project TEXT",
            "ALTER TABLE verifies ADD COLUMN model TEXT",
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
        ] {
            let _ = self.conn.execute(stmt, []);
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
                "INSERT INTO tasks(run_id, idx, name, project, state, depends_on) VALUES(?,?,?,?,?,?)",
                params![
                    run_id,
                    t_idx_i,
                    task.name,
                    task.project,
                    NodeState::Pending.as_str(),
                    to_json(&task.depends_on),
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
                    "INSERT INTO sessions(run_id, task_idx, idx, sid, name, cwd, subprojects, prompt, command, agent, model, system_prompt, system_prompt_position, state, depends_on, timeout_sec, budget_tokens, upstream)
                     VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
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
                    )?;
                }
            }

            for (v_idx, v) in task.verify.iter().enumerate() {
                insert_verify(&tx, &run_id, t_idx_i, "task", -1, v_idx, v, task)?;
            }
        }

        tx.commit()?;
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
        let n = self.conn.execute(
            "UPDATE runs SET state=?, updated_at_ms=? WHERE id=?",
            params![state.as_str(), now_ms(), id],
        )?;
        if n == 0 {
            Err(StoreError::NotFound)
        } else {
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

    /// Cancel a run that has not reached a terminal state.
    pub fn cancel(&self, id: &str) -> Result<RunState> {
        match self.run_state(id)? {
            s if s.is_terminal() => Err(StoreError::InvalidTransition(format!(
                "run already {}",
                s.as_str()
            ))),
            _ => {
                self.set_run_state(id, RunState::Cancelled)?;
                self.cancel_nonterminal_nodes(id)?;
                Ok(RunState::Cancelled)
            }
        }
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
            match state.as_deref() {
                Some("done") => {}
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

    /// Combined count of all active work: running sessions, running verify steps,
    /// and guardian reviews currently building their stacked rebase (merging).
    /// This is the unified number shown in the concurrency counter.
    pub fn running_work_count(&self) -> Result<i64> {
        let sessions: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM sessions WHERE state='running'",
            [],
            |r| r.get(0),
        )?;
        let verifies: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM verifies WHERE state='running'",
            [],
            |r| r.get(0),
        )?;
        let reviews: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM guardians WHERE status='merging'",
            [],
            |r| r.get(0),
        )?;
        Ok(sessions + verifies + reviews)
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
        self.conn.execute(
            "UPDATE sessions SET state=? WHERE run_id=? AND task_idx=? AND idx=?",
            params![state.as_str(), run_id, task_idx, idx],
        )?;
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
        self.conn.execute(
            "UPDATE tasks SET state=? WHERE run_id=? AND idx=?",
            params![state.as_str(), run_id, task_idx],
        )?;
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
        }
        Ok(rows.into_iter().map(|(_, session)| session).collect())
    }

    /// Map each guardian branch of a run back to the reviews containing it, so a
    /// session can list the reviews its branch participates in (RAL-17).
    fn reviews_by_branch(&self, run_id: &str) -> Result<HashMap<String, Vec<RunReviewRef>>> {
        let mut map: HashMap<String, Vec<RunReviewRef>> = HashMap::new();
        for guardian in self.reviews_for_run(run_id)? {
            for ob in self.guardian_branches(&guardian.id)? {
                map.entry(ob.branch).or_default().push(guardian.clone());
            }
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
            "SELECT vid, kind, state, output, spec, model FROM verifies
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
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }
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
        "INSERT INTO verifies(run_id, task_idx, scope, session_idx, idx, vid, kind, spec, model, state, timeout_sec, budget_tokens)
         VALUES(?,?,?,?,?,?,?,?,?,?,?,?)",
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
    /// Task-level dependency references.
    pub depends_on: Vec<String>,
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
        let mut stmt = self
            .conn
            .prepare("SELECT idx, name, depends_on FROM tasks WHERE run_id=? ORDER BY idx")?;
        let rows = stmt
            .query_map(params![run_id], |r| {
                Ok(TaskRow {
                    idx: r.get(0)?,
                    name: r.get(1)?,
                    depends_on: from_json(&r.get::<_, String>(2)?),
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
        self.conn.execute(
            "UPDATE verifies SET state=?
             WHERE run_id=? AND task_idx=? AND scope=? AND session_idx=? AND idx=?",
            params![state.as_str(), run_id, task_idx, scope, session_idx, idx],
        )?;
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
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE verifies SET state=?, output=?
             WHERE run_id=? AND task_idx=? AND scope=? AND session_idx=? AND idx=?",
            params![
                state.as_str(),
                output,
                run_id,
                task_idx,
                scope,
                session_idx,
                idx
            ],
        )?;
        // Prefer the verifier's own id (e.g. `fmt`) over the bare index in the
        // event log, so the Logs view names which verifier moved.
        let vid: Option<String> = self
            .conn
            .query_row(
                "SELECT vid FROM verifies WHERE run_id=? AND task_idx=? AND scope=? AND session_idx=? AND idx=?",
                params![run_id, task_idx, scope, session_idx, idx],
                |r| r.get::<_, Option<String>>(0),
            )
            .ok()
            .flatten();
        let reference = match vid {
            Some(v) => format!("{scope} t{task_idx}/{v}"),
            None => format!("{scope} t{task_idx} #{idx}"),
        };
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

    /// Restart a whole run: reset it (and all its nodes) to Pending and dirty
    /// every run that transitively depends on it, so the dependents re-run once
    /// this run finishes again (RAL-19). Returns the dirtied dependent run ids.
    pub fn restart_run(&self, run_id: &str) -> Result<Vec<String>> {
        let exists: Option<String> = self
            .conn
            .query_row("SELECT id FROM runs WHERE id=?", params![run_id], |r| {
                r.get(0)
            })
            .optional()?;
        if exists.is_none() {
            return Err(StoreError::NotFound);
        }
        self.reset_run_to_pending(run_id)?;
        let _ = self.log_event(Some(run_id), None, "run", None, "restarted");
        self.dirty_dependents(run_id)
    }

    /// Restart a single session: reset it and every session downstream of it
    /// within the run to Pending, put the run (and each affected task) back to
    /// Pending, and dirty every run that depends on this one (RAL-19). Upstream
    /// sessions stay Done and are skipped on re-run. Returns the dirtied
    /// dependent run ids.
    pub fn restart_session(&self, run_id: &str, task_idx: i64, idx: i64) -> Result<Vec<String>> {
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

        let mut affected_tasks: HashSet<i64> = HashSet::new();
        for &pos in &affected {
            let s = &sessions[pos];
            self.conn.execute(
                "UPDATE sessions SET state='pending', error=NULL WHERE run_id=? AND task_idx=? AND idx=?",
                params![run_id, s.task_idx, s.idx],
            )?;
            self.conn.execute(
                "UPDATE verifies SET state='pending' WHERE run_id=? AND task_idx=? AND scope='session' AND session_idx=?",
                params![run_id, s.task_idx, s.idx],
            )?;
            affected_tasks.insert(s.task_idx);
        }
        for t in &affected_tasks {
            self.conn.execute(
                "UPDATE tasks SET state='pending' WHERE run_id=? AND idx=?",
                params![run_id, t],
            )?;
            self.conn.execute(
                "UPDATE verifies SET state='pending' WHERE run_id=? AND task_idx=? AND scope='task'",
                params![run_id, t],
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
        self.dirty_dependents(run_id)
    }

    /// Reset every run that transitively depends on `run_id` back to Pending, so
    /// it re-runs once the upstream completes again (RAL-19). Cross-run gating
    /// (`list_ready`) then holds each dependent until its upstreams are Done.
    /// Returns the dirtied run ids.
    pub fn dirty_dependents(&self, run_id: &str) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare("SELECT id, depends_on FROM runs")?;
        let all: Vec<(String, Vec<String>)> = stmt
            .query_map([], |r| {
                Ok((r.get::<_, String>(0)?, from_json(&r.get::<_, String>(1)?)))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        drop(stmt);
        let mut seen: HashSet<String> = HashSet::new();
        seen.insert(run_id.to_string());
        let mut frontier = vec![run_id.to_string()];
        let mut dirtied = Vec::new();
        while let Some(cur) = frontier.pop() {
            for (id, deps) in &all {
                if seen.contains(id) {
                    continue;
                }
                // A dep reference is `run-id` or `run-id/task/session`; the run
                // is the first path segment.
                let depends = deps.iter().any(|d| d.split('/').next().unwrap_or(d) == cur);
                if depends {
                    seen.insert(id.clone());
                    self.reset_run_to_pending(id)?;
                    let _ =
                        self.log_event(Some(id), None, "run", None, "dirtied by upstream restart");
                    dirtied.push(id.clone());
                    frontier.push(id.clone());
                }
            }
        }
        Ok(dirtied)
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
    pub fn record_session_result(
        &self,
        run_id: &str,
        task_idx: i64,
        idx: i64,
        outcome: &SessionOutcome,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET state=?, tokens_in=?, tokens_out=?, cost_usd=?, error=?, claude_session_id=?
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

    /// Fetch the `claude_session_id` for the open-terminal endpoint.
    ///
    /// Returns `Err(StoreError::NotFound)` when the run or session row does not
    /// exist, and `Ok(None)` when the session exists but has no UUID yet.
    pub fn get_session_claude_id(
        &self,
        run_id: &str,
        task_idx: i64,
        session_idx: i64,
    ) -> Result<Option<String>> {
        self.conn
            .query_row(
                "SELECT claude_session_id FROM sessions WHERE run_id=? AND task_idx=? AND idx=?",
                params![run_id, task_idx, session_idx],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()?
            .ok_or(StoreError::NotFound)
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
        assert!(store.cancel(&id).is_err()); // terminal
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
    fn session_and_task_state_transitions() {
        let mut store = Store::open_in_memory().unwrap();
        let id = store.insert_run(&parse(SAMPLE), None, false).unwrap();
        store.set_run_state(&id, RunState::Running).unwrap();
        store.set_session_state(&id, 0, 0, NodeState::Done).unwrap();
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
}
