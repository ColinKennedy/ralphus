//! SQLite-backed task store — the daemon's authoritative state.
//!
//! Only the daemon opens this database (WAL mode); the CLI and librarian reach
//! it through the HTTP API. On submission a task file is fully ingested into
//! these tables, so the database — not any on-disk TOML — is the source of truth
//! (the predecessor learned this the hard way; see `FINDINGS.local.md` §2.4 and
//! CCTL-149).

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

    fn parse(s: &str) -> Option<Self> {
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

/// A verify step as shown in the board.
#[derive(Debug, Clone, Serialize)]
pub struct VerifyView {
    /// Optional step id.
    pub id: Option<String>,
    /// One of `command` / `brain` / `agent` / `approval`.
    pub kind: String,
    /// Current state string.
    pub state: String,
}

/// A session as shown in the board.
#[derive(Debug, Clone, Serialize)]
pub struct SessionView {
    /// Session id (or a generated `session-N`).
    pub id: String,
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
                prompt     TEXT,
                command    TEXT,
                agent      TEXT NOT NULL,
                model      TEXT,
                state      TEXT NOT NULL,
                depends_on TEXT NOT NULL DEFAULT '[]',
                tokens_in  INTEGER NOT NULL DEFAULT 0,
                tokens_out INTEGER NOT NULL DEFAULT 0,
                cost_usd   REAL NOT NULL DEFAULT 0,
                error      TEXT,
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
                state       TEXT NOT NULL,
                output      TEXT,
                PRIMARY KEY (run_id, task_idx, scope, session_idx, idx)
            );
            CREATE TABLE IF NOT EXISTS guardians (
                id            TEXT PRIMARY KEY,
                name          TEXT NOT NULL,
                base_branch   TEXT NOT NULL,
                git_root      TEXT NOT NULL,
                review_branch TEXT,
                status        TEXT NOT NULL,
                detail        TEXT,
                checks        TEXT NOT NULL DEFAULT '[]',
                run_id        TEXT,
                created_at_ms INTEGER NOT NULL,
                updated_at_ms INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS guardian_branches (
                guardian_id  TEXT NOT NULL REFERENCES guardians(id) ON DELETE CASCADE,
                position     INTEGER NOT NULL,
                branch       TEXT NOT NULL,
                merge_status TEXT NOT NULL,
                detail       TEXT,
                PRIMARY KEY (guardian_id, position)
            );
            ",
        )?;
        // Best-effort migration for databases created before `run_id` existed.
        // Fails harmlessly (duplicate column) once the column is present.
        let _ = self
            .conn
            .execute("ALTER TABLE guardians ADD COLUMN run_id TEXT", []);
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
                tx.execute(
                    "INSERT INTO sessions(run_id, task_idx, idx, sid, cwd, prompt, command, agent, model, state, depends_on)
                     VALUES(?,?,?,?,?,?,?,?,?,?,?)",
                    params![
                        run_id,
                        t_idx_i,
                        i64::try_from(s_idx).unwrap_or(0),
                        sid,
                        session.cwd,
                        session.prompt,
                        session.command,
                        resolved.program,
                        resolved.model,
                        NodeState::Pending.as_str(),
                        to_json(&session.depends_on),
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
                    )?;
                }
            }

            for (v_idx, v) in task.verify.iter().enumerate() {
                insert_verify(&tx, &run_id, t_idx_i, "task", -1, v_idx, v)?;
            }
        }

        tx.commit()?;
        Ok(run_id)
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
                Ok(RunState::Cancelled)
            }
        }
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
        Ok(())
    }

    /// Set a task node's state.
    pub fn set_task_state(&self, run_id: &str, task_idx: i64, state: NodeState) -> Result<()> {
        self.conn.execute(
            "UPDATE tasks SET state=? WHERE run_id=? AND idx=?",
            params![state.as_str(), run_id, task_idx],
        )?;
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
        let mut stmt = self.conn.prepare(
            "SELECT id, label, state, created_at_ms FROM runs ORDER BY created_at_ms DESC",
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

        let mut tasks = Vec::with_capacity(task_rows.len());
        for (t_idx, name, project, tstate, deps) in task_rows {
            tasks.push(TaskView {
                name,
                project,
                state: tstate,
                sessions: self.sessions_for(&id, t_idx)?,
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

    fn sessions_for(&self, run_id: &str, task_idx: i64) -> Result<Vec<SessionView>> {
        let mut stmt = self.conn.prepare(
            "SELECT idx, sid, cwd, agent, model, state, tokens_in, tokens_out, cost_usd, error, prompt, command, depends_on
             FROM sessions WHERE run_id=? AND task_idx=? ORDER BY idx",
        )?;
        let rows = stmt
            .query_map(params![run_id, task_idx], |r| {
                Ok(SessionView {
                    id: r.get::<_, String>(1)?,
                    cwd: r.get::<_, Option<String>>(2)?,
                    agent: r.get::<_, String>(3)?,
                    model: r.get::<_, Option<String>>(4)?,
                    state: r.get::<_, String>(5)?,
                    tokens_in: r.get::<_, i64>(6)?,
                    tokens_out: r.get::<_, i64>(7)?,
                    cost_usd: r.get::<_, f64>(8)?,
                    error: r.get::<_, Option<String>>(9)?,
                    prompt: r.get::<_, Option<String>>(10)?,
                    command: r.get::<_, Option<String>>(11)?,
                    depends_on: from_json(&r.get::<_, String>(12)?),
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    fn verifies_for(
        &self,
        run_id: &str,
        task_idx: i64,
        scope: &str,
        session_idx: i64,
    ) -> Result<Vec<VerifyView>> {
        let mut stmt = self.conn.prepare(
            "SELECT vid, kind, state FROM verifies
             WHERE run_id=? AND task_idx=? AND scope=? AND session_idx=? ORDER BY idx",
        )?;
        let rows = stmt
            .query_map(params![run_id, task_idx, scope, session_idx], |r| {
                Ok(VerifyView {
                    id: r.get::<_, Option<String>>(0)?,
                    kind: r.get::<_, String>(1)?,
                    state: r.get::<_, String>(2)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }
}

fn insert_verify(
    tx: &rusqlite::Transaction<'_>,
    run_id: &str,
    task_idx: i64,
    scope: &str,
    session_idx: i64,
    v_idx: usize,
    v: &ralphus_core::schema::VerifyStep,
) -> Result<()> {
    let (kind, spec) = if let Some(c) = &v.command {
        ("command", c.clone())
    } else if let Some(b) = &v.brain {
        ("brain", b.clone())
    } else if let Some(a) = &v.agent {
        ("agent", a.clone())
    } else if v.requires_approval {
        ("approval", String::new())
    } else {
        ("unknown", String::new())
    };
    tx.execute(
        "INSERT INTO verifies(run_id, task_idx, scope, session_idx, idx, vid, kind, spec, state)
         VALUES(?,?,?,?,?,?,?,?,?)",
        params![
            run_id,
            task_idx,
            scope,
            session_idx,
            i64::try_from(v_idx).unwrap_or(0),
            v.id,
            kind,
            spec,
            NodeState::Pending.as_str(),
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
    /// AI prompt (mutually exclusive with `command`).
    pub prompt: Option<String>,
    /// Deterministic command (mutually exclusive with `prompt`).
    pub command: Option<String>,
    /// Resolved agent program.
    pub agent: String,
    /// Resolved model.
    pub model: Option<String>,
    /// This session's dependency references (within-task session ids or
    /// cross-task `task/session`).
    pub depends_on: Vec<String>,
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
            "SELECT s.task_idx, s.idx, t.name, s.sid, s.cwd, s.prompt, s.command, s.agent, s.model, s.depends_on
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
                    prompt: r.get(5)?,
                    command: r.get(6)?,
                    agent: r.get(7)?,
                    model: r.get(8)?,
                    depends_on: from_json(&r.get::<_, String>(9)?),
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

    /// The verify steps of a scope (`"task"` with `session_idx = -1`, or
    /// `"session"` with the session's index), in order: `(idx, kind, spec)`.
    pub fn verify_specs(
        &self,
        run_id: &str,
        task_idx: i64,
        scope: &str,
        session_idx: i64,
    ) -> Result<Vec<(i64, String, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT idx, kind, spec FROM verifies
             WHERE run_id=? AND task_idx=? AND scope=? AND session_idx=? ORDER BY idx",
        )?;
        let rows = stmt
            .query_map(params![run_id, task_idx, scope, session_idx], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
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

    /// Delete a run and all of its child rows. Children are removed explicitly
    /// (rather than relying on `ON DELETE CASCADE`, which is off for in-memory
    /// test databases) inside one transaction.
    pub fn delete_run(&mut self, run_id: &str) -> Result<()> {
        let tx = self.conn.transaction()?;
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

    /// Record a session's final outcome (state, usage, and any error).
    pub fn record_session_result(
        &self,
        run_id: &str,
        task_idx: i64,
        idx: i64,
        outcome: &SessionOutcome,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET state=?, tokens_in=?, tokens_out=?, cost_usd=?, error=?
             WHERE run_id=? AND task_idx=? AND idx=?",
            params![
                outcome.state.as_str(),
                outcome.tokens_in,
                outcome.tokens_out,
                outcome.cost_usd,
                outcome.error.as_deref(),
                run_id,
                task_idx,
                idx,
            ],
        )?;
        Ok(())
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
        assert_eq!(task.verify.len(), 1); // task-level verify
        assert_eq!(task.verify[0].kind, "command");
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
    fn list_runs_newest_first() {
        let mut store = Store::open_in_memory().unwrap();
        let _a = store.insert_run(&parse(SAMPLE), Some("a"), false).unwrap();
        let b = store.insert_run(&parse(SAMPLE), Some("b"), false).unwrap();
        let runs = store.list_runs().unwrap();
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].id, b); // newest first
    }
}
