//! The daemon's HTTP/JSON API (see `docs/daemon-api.md`).
//!
//! The routing/handler core (`route`) is a pure function over `(&Daemon, method,
//! path, body)` so it can be unit-tested without sockets. `serve` wraps it in a
//! blocking `tiny_http` loop and runs the scheduler on a second thread; both
//! share the store through an `Arc<Mutex<Store>>`.

use std::net::ToSocketAddrs;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

use ralphus_core::validate::{ValidationError, validate_toml};
use serde::{Deserialize, Serialize};

use crate::cancel::Cancellations;
use crate::procreg::ProcRegistry;
use crate::runner::{Runner, SubprocessRunner};
use crate::scheduler::Semaphore;
use crate::store::{RunState, Store, StoreError};

/// The running daemon: its store handle plus configuration.
pub struct Daemon {
    store: Arc<Mutex<Store>>,
    max_concurrent: i64,
    /// Cancel tokens of in-flight runs, shared with the scheduler's workers so a
    /// `cancel` request can stop the running worker and its subprocess.
    cancellations: Cancellations,
    /// Live registry of session subprocess PIDs, shared with the runner so the
    /// resource-usage endpoint can attribute OS metrics to running tasks (RAL-11).
    procs: ProcRegistry,
    /// Global concurrency semaphore shared by the scheduler, task-level verifies,
    /// and guardian review merges so all three count against `max_concurrent`.
    sem: Arc<Semaphore>,
}

impl Daemon {
    /// Build a daemon around an already-open store.
    #[must_use]
    pub fn new(store: Store, max_concurrent: i64) -> Self {
        Self {
            store: Arc::new(Mutex::new(store)),
            max_concurrent,
            cancellations: Cancellations::new(),
            procs: ProcRegistry::new(),
            sem: Arc::new(Semaphore::new(max_concurrent)),
        }
    }

    /// A cloned handle to the shared store (for the scheduler thread).
    #[must_use]
    pub fn store_handle(&self) -> Arc<Mutex<Store>> {
        Arc::clone(&self.store)
    }

    /// A cloned handle to the cancellation registry (for the scheduler thread).
    #[must_use]
    pub fn cancellations_handle(&self) -> Cancellations {
        self.cancellations.clone()
    }

    /// A cloned handle to the subprocess PID registry (for the session runner).
    #[must_use]
    pub fn procs_handle(&self) -> ProcRegistry {
        self.procs.clone()
    }

    /// A cloned handle to the global semaphore (for the scheduler thread and
    /// guardian merge workers so they all share the same concurrency cap).
    #[must_use]
    pub fn semaphore_handle(&self) -> Arc<Semaphore> {
        Arc::clone(&self.sem)
    }

    fn lock(&self) -> MutexGuard<'_, Store> {
        self.store.lock().expect("store mutex poisoned")
    }
}

/// A ready-to-send HTTP reply.
#[derive(Debug, PartialEq, Eq)]
pub struct Reply {
    /// HTTP status code.
    pub status: u16,
    /// JSON body.
    pub body: String,
}

fn json<T: Serialize>(status: u16, value: &T) -> Reply {
    Reply {
        status,
        body: serde_json::to_string(value).unwrap_or_else(|_| "{}".to_string()),
    }
}

fn error(status: u16, code: &str, message: &str, details: Vec<ValidationError>) -> Reply {
    json(
        status,
        &ErrorEnvelope {
            error: ErrorBody {
                code: code.to_string(),
                message: message.to_string(),
                details,
            },
        },
    )
}

fn store_error(e: &StoreError) -> Reply {
    match e {
        StoreError::NotFound => error(404, "not_found", &e.to_string(), vec![]),
        StoreError::InvalidTransition(_) => {
            error(409, "invalid_transition", &e.to_string(), vec![])
        }
        StoreError::Sqlite(_) => error(500, "internal", &e.to_string(), vec![]),
    }
}

// ── request/response bodies ──────────────────────────────────────────────────

#[derive(Deserialize)]
struct ValidateBody {
    toml: String,
}

#[derive(Deserialize)]
struct SubmitBody {
    toml: String,
    #[serde(default)]
    hold: bool,
    #[serde(default)]
    label: Option<String>,
}

#[derive(Serialize)]
struct DaemonHealth<'a> {
    name: &'a str,
    version: &'a str,
    status: &'a str,
    db: &'a str,
}

#[derive(Serialize)]
struct RunningReviewItem {
    id: String,
    name: String,
}

#[derive(Serialize)]
struct DaemonStatus {
    running: i64,
    max_concurrent: i64,
    /// Guardian reviews currently building their stacked rebase, for the
    /// concurrency-counter dropdown.
    running_reviews: Vec<RunningReviewItem>,
}

#[derive(Serialize)]
struct Board {
    daemon: DaemonStatus,
    runs: Vec<crate::store::RunView>,
}

#[derive(Serialize)]
struct ResourcesResponse {
    resources: Vec<crate::resources::ResourceRow>,
}

#[derive(Serialize)]
struct ValidateResponse<'a> {
    valid: bool,
    errors: &'a [ValidationError],
    warnings: &'a [ValidationError],
}

#[derive(Serialize)]
struct SubmitResponse<'a> {
    run_id: String,
    state: &'a str,
}

#[derive(Serialize)]
struct StateResponse<'a> {
    state: &'a str,
}

#[derive(Serialize)]
struct ErrorEnvelope {
    error: ErrorBody,
}

#[derive(Serialize)]
struct ErrorBody {
    code: String,
    message: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    details: Vec<ValidationError>,
}

// ── routing ──────────────────────────────────────────────────────────────────

/// Handle one request. Pure over the daemon state so it is unit-testable.
pub fn route(daemon: &Daemon, method: &str, path: &str, body: &str) -> Reply {
    let (path_only, query) = path.split_once('?').unwrap_or((path, ""));
    let segs: Vec<&str> = path_only.trim_matches('/').split('/').collect();
    match (method, segs.as_slice()) {
        ("GET", ["api", "daemon"]) => health(daemon),
        ("GET", ["api", "tasks"]) => board(daemon),
        ("GET", ["api", "resources"]) => resources(daemon),
        ("POST", ["api", "runs", "validate"]) => validate_endpoint(body),
        ("POST", ["api", "runs"]) => submit(daemon, body),
        ("POST", ["api", "clear"]) => clear_all(daemon, body),
        ("GET", ["api", "runs", id]) => get_run(daemon, id),
        ("GET", ["api", "runs", id, "worktrees"]) => run_worktrees(daemon, id),
        ("GET", ["api", "runs", id, "logs"]) => run_logs(daemon, id),
        ("POST", ["api", "runs", id, "activate"]) => activate(daemon, id),
        ("POST", ["api", "runs", id, "cancel"]) => cancel(daemon, id),
        ("POST", ["api", "runs", id, "edit"]) => edit_run(daemon, id, body),
        ("POST", ["api", "runs", id, "retry"]) => retry_run(daemon, id),
        ("POST", ["api", "runs", id, "restart"]) => restart_run(daemon, id),
        ("POST", ["api", "runs", id, "sessions", ti, si, "restart"]) => {
            restart_session(daemon, id, ti, si)
        }
        ("POST", ["api", "runs", id, "sessions", ti, si, "open-terminal"]) => {
            open_terminal(daemon, id, ti, si, query)
        }
        ("DELETE", ["api", "runs", id]) => delete_run(daemon, id),
        ("GET", ["api", "guardians"]) => guardian_list(daemon),
        ("POST", ["api", "guardians"]) => guardian_create(daemon, body),
        ("GET", ["api", "guardians", id]) => guardian_get(daemon, id),
        ("GET", ["api", "guardians", id, "logs"]) => guardian_logs(daemon, id),
        ("POST", ["api", "guardians", id, "rename"]) => guardian_rename(daemon, id, body),
        ("POST", ["api", "guardians", id, "settings"]) => guardian_settings(daemon, id, body),
        ("DELETE", ["api", "guardians", id]) => guardian_delete(daemon, id),
        ("POST", ["api", "guardians", id, "branches"]) => guardian_add_branch(daemon, id, body),
        ("POST", ["api", "guardians", id, "branches", "reorder"]) => {
            guardian_reorder(daemon, id, body)
        }
        ("POST", ["api", "guardians", id, "branches", pos, "feedback"]) => {
            guardian_feedback(daemon, id, pos, body)
        }
        ("GET", ["api", "guardians", id, "messages"]) => guardian_messages(daemon, id),
        ("POST", ["api", "guardians", id, "chat"]) => guardian_chat(daemon, id, body),
        ("GET", ["api", "guardians", id, "base-branches"]) => guardian_base_branches(daemon, id),
        ("POST", ["api", "guardians", id, "base"]) => guardian_change_base(daemon, id, body),
        ("POST", ["api", "guardians", id, "merge"]) => guardian_merge(daemon, id),
        ("POST", ["api", "guardians", id, "cancel_and_merge"]) => {
            guardian_cancel_and_merge(daemon, id)
        }
        ("POST", ["api", "guardians", id, "approve"]) => guardian_approve(daemon, id),
        ("POST", ["api", "guardians", id, "cancel"]) => guardian_cancel(daemon, id),
        ("POST", ["api", "guardians", id, "run-manual-commands"]) => {
            guardian_run_manual_commands(daemon, id, body)
        }
        _ => error(
            404,
            "not_found",
            &format!("no route for {method} {path}"),
            vec![],
        ),
    }
}

fn health(daemon: &Daemon) -> Reply {
    let db = if daemon.lock().running_count().is_ok() {
        "ok"
    } else {
        "error"
    };
    json(
        200,
        &DaemonHealth {
            name: "ralphus-daemon",
            version: ralphus_core::version(),
            status: "ok",
            db,
        },
    )
}

fn board(daemon: &Daemon) -> Reply {
    let store = daemon.lock();
    let running = match store.running_work_count() {
        Ok(n) => n,
        Err(e) => return store_error(&e),
    };
    let running_reviews: Vec<RunningReviewItem> = store
        .merging_guardians()
        .unwrap_or_default()
        .into_iter()
        .map(|(id, name)| RunningReviewItem { id, name })
        .collect();
    match store.list_runs() {
        Ok(runs) => json(
            200,
            &Board {
                daemon: DaemonStatus {
                    running,
                    max_concurrent: daemon.max_concurrent,
                    running_reviews,
                },
                runs,
            },
        ),
        Err(e) => store_error(&e),
    }
}

/// Per-task resource usage for every running session with a live subprocess
/// (RAL-11). Snapshots the running set + PIDs under the lock, then samples
/// CPU/RAM/GPU with the lock released (sampling briefly sleeps to measure CPU).
fn resources(daemon: &Daemon) -> Reply {
    let runs = {
        let store = daemon.lock();
        match store.list_runs() {
            Ok(runs) => runs,
            Err(e) => return store_error(&e),
        }
    };
    let procs = daemon.procs_handle();
    let rows = crate::resources::build(&runs, &procs);
    json(200, &ResourcesResponse { resources: rows })
}

fn validate_endpoint(body: &str) -> Reply {
    let Ok(req) = serde_json::from_str::<ValidateBody>(body) else {
        return error(
            400,
            "bad_request",
            "body must be {\"toml\": \"...\"}",
            vec![],
        );
    };
    let report = validate_toml(&req.toml);
    json(
        200,
        &ValidateResponse {
            valid: report.is_ok(),
            errors: &report.errors,
            warnings: &report.warnings,
        },
    )
}

fn submit(daemon: &Daemon, body: &str) -> Reply {
    let Ok(req) = serde_json::from_str::<SubmitBody>(body) else {
        return error(
            400,
            "bad_request",
            "body must include a \"toml\" string",
            vec![],
        );
    };

    let report = validate_toml(&req.toml);
    if !report.is_ok() {
        return error(
            400,
            "validation_failed",
            "the submitted TOML is invalid",
            report.errors,
        );
    }

    let file = match toml::from_str(&req.toml) {
        Ok(f) => f,
        Err(e) => {
            return error(
                400,
                "parse_failed",
                &format!("could not parse TOML: {e}"),
                vec![],
            );
        }
    };

    let mut store = daemon.lock();
    let run_id = match store.insert_run(&file, req.label.as_deref(), req.hold) {
        Ok(id) => id,
        Err(e) => return store_error(&e),
    };
    // Derive per-project review guardians. A preflight failure (bad worktree, no
    // upstream for a `<<upstream>>` base) rolls the run back and rejects the submit.
    if let Err(e) = crate::reviews::derive_reviews(&store, &run_id, &file) {
        let _ = store.delete_run(&run_id);
        return error(400, "review_preflight_failed", &e.message, vec![]);
    }
    let state = if req.hold {
        RunState::Queued
    } else {
        RunState::Pending
    };
    json(
        201,
        &SubmitResponse {
            run_id,
            state: state.as_str(),
        },
    )
}

fn get_run(daemon: &Daemon, id: &str) -> Reply {
    match daemon.lock().get_run(id) {
        Ok(run) => json(200, &run),
        Err(e) => store_error(&e),
    }
}

#[derive(Serialize)]
struct SessionPaths {
    task_idx: usize,
    session_idx: usize,
    worktree: Option<String>,
    project: Option<String>,
}

/// For each session in a run, its worktree (`cwd`) and the derived project root
/// (the shared git dir), so the detail pane can show them as distinct fields
/// (CCTL-148). The project is `null` when the cwd is not a git worktree. Computed
/// on demand (runs git per session) rather than on the hot board path.
fn run_worktrees(daemon: &Daemon, id: &str) -> Reply {
    let run = match daemon.lock().get_run(id) {
        Ok(r) => r,
        Err(e) => return store_error(&e),
    };
    let mut paths = Vec::new();
    for (ti, task) in run.tasks.iter().enumerate() {
        for (si, s) in task.sessions.iter().enumerate() {
            let project = s.cwd.as_deref().and_then(crate::reviews::project_root_of);
            paths.push(SessionPaths {
                task_idx: ti,
                session_idx: si,
                worktree: s.cwd.clone(),
                project,
            });
        }
    }
    json(200, &paths)
}

/// The execution/transition log for a run (CCTL-99). Latest 500 entries,
/// oldest-first. 404 if the run does not exist.
fn run_logs(daemon: &Daemon, id: &str) -> Reply {
    let store = daemon.lock();
    if let Err(e) = store.run_state(id) {
        return store_error(&e);
    }
    match store.events_for_run(id, 500) {
        Ok(events) => json(200, &events),
        Err(e) => store_error(&e),
    }
}

/// The full audit log of a review cycle (CCTL-99). 404 if it does not exist.
fn guardian_logs(daemon: &Daemon, id: &str) -> Reply {
    let store = daemon.lock();
    if store.get_guardian(id).is_err() {
        return error(404, "not_found", "no such guardian", vec![]);
    }
    match store.events_for_guardian(id, 500) {
        Ok(events) => json(200, &events),
        Err(e) => store_error(&e),
    }
}

fn activate(daemon: &Daemon, id: &str) -> Reply {
    match daemon.lock().activate(id) {
        Ok(state) => json(
            200,
            &StateResponse {
                state: state.as_str(),
            },
        ),
        Err(e) => store_error(&e),
    }
}

#[derive(Deserialize)]
struct EditBody {
    kind: String,
    #[serde(default)]
    task_idx: i64,
    #[serde(default)]
    session_idx: i64,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    project: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    agent: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default)]
    command: Option<String>,
}

fn non_empty(v: Option<&String>) -> Option<&str> {
    v.map(String::as_str).filter(|s| !s.is_empty())
}

/// Edit a run/task/session's fields, then reset the run to Pending so it
/// re-executes with the new values (stopping any in-flight work).
fn edit_run(daemon: &Daemon, id: &str, body: &str) -> Reply {
    let Ok(req) = serde_json::from_str::<EditBody>(body) else {
        return error(400, "bad_request", "invalid edit body", vec![]);
    };
    let store = daemon.lock();
    let result = match req.kind.as_str() {
        "run" => store.edit_run_label(id, non_empty(req.label.as_ref())),
        "task" => {
            let name = non_empty(req.name.as_ref()).unwrap_or("task");
            store.edit_task_fields(id, req.task_idx, name, non_empty(req.project.as_ref()))
        }
        "session" => {
            // Keep prompt XOR command: prefer command if given, else prompt.
            let (prompt, command) = match (
                non_empty(req.prompt.as_ref()),
                non_empty(req.command.as_ref()),
            ) {
                (_, Some(c)) => (None, Some(c)),
                (p, None) => (p, None),
            };
            let edit = crate::store::SessionEdit {
                cwd: non_empty(req.cwd.as_ref()),
                agent: non_empty(req.agent.as_ref()).unwrap_or("claude"),
                model: non_empty(req.model.as_ref()),
                prompt,
                command,
            };
            store.edit_session_fields(id, req.task_idx, req.session_idx, &edit)
        }
        other => {
            return error(
                400,
                "bad_request",
                &format!("unknown edit kind '{other}'"),
                vec![],
            );
        }
    };
    if let Err(e) = result {
        return store_error(&e);
    }
    if let Err(e) = store.reset_run_to_pending(id) {
        return store_error(&e);
    }
    match store.get_run(id) {
        Ok(run) => json(200, &run),
        Err(e) => store_error(&e),
    }
}

/// Re-run a run with its existing parameters by resetting it (and its tasks,
/// sessions, and verifies) back to Pending so the scheduler picks it up again.
fn retry_run(daemon: &Daemon, id: &str) -> Reply {
    let store = daemon.lock();
    if let Err(e) = store.run_state(id) {
        return store_error(&e);
    }
    if let Err(e) = store.reset_run_to_pending(id) {
        return store_error(&e);
    }
    match store.get_run(id) {
        Ok(run) => json(200, &run),
        Err(e) => store_error(&e),
    }
}

#[derive(Serialize)]
struct RestartResponse {
    state: &'static str,
    dirtied: Vec<String>,
}

/// Restart a whole run and dirty every run that depends on it (RAL-19).
fn restart_run(daemon: &Daemon, id: &str) -> Reply {
    match daemon.lock().restart_run(id) {
        Ok(dirtied) => json(
            200,
            &RestartResponse {
                state: "pending",
                dirtied,
            },
        ),
        Err(e) => store_error(&e),
    }
}

/// Restart a single session (and its downstream), dirtying dependent runs.
fn restart_session(daemon: &Daemon, id: &str, ti: &str, si: &str) -> Reply {
    let (Ok(task_idx), Ok(session_idx)) = (ti.parse::<i64>(), si.parse::<i64>()) else {
        return error(
            400,
            "bad_request",
            "task/session index must be integers",
            vec![],
        );
    };
    match daemon.lock().restart_session(id, task_idx, session_idx) {
        Ok(dirtied) => json(
            200,
            &RestartResponse {
                state: "pending",
                dirtied,
            },
        ),
        Err(e) => store_error(&e),
    }
}

/// Parse a single named parameter from a URL query string (e.g. `"mode=readonly"`).
fn query_param<'a>(query: &'a str, key: &str) -> Option<&'a str> {
    query.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        if k == key { Some(v) } else { None }
    })
}

#[derive(Serialize)]
struct OpenTerminalResponse {
    ok: bool,
}

/// Open a resumed `claude --resume <session-uuid>` session in a new terminal
/// window. `mode` (query param) is `"readonly"` or `"open"`.
fn open_terminal(daemon: &Daemon, id: &str, ti: &str, si: &str, query: &str) -> Reply {
    let mode = query_param(query, "mode").unwrap_or("open");
    if mode != "readonly" && mode != "open" {
        return error(
            400,
            "bad_request",
            "mode must be 'readonly' or 'open'",
            vec![],
        );
    }
    let (Ok(task_idx), Ok(session_idx)) = (ti.parse::<i64>(), si.parse::<i64>()) else {
        return error(
            400,
            "bad_request",
            "task/session index must be integers",
            vec![],
        );
    };
    let claude_id = match daemon
        .lock()
        .get_session_claude_id(id, task_idx, session_idx)
    {
        Ok(v) => v,
        Err(e) => return store_error(&e),
    };
    let Some(session_uuid) = claude_id else {
        return error(
            409,
            "no_session_id",
            "claude_session_id is not available — the session may not have completed yet",
            vec![],
        );
    };
    let claude_cmd = std::env::var("RALPHUS_CLAUDE_CMD").unwrap_or_else(|_| "claude".to_string());
    let claude_args: Vec<String> = if mode == "readonly" {
        vec![
            "--resume".to_string(),
            session_uuid,
            "--dangerously-skip-permissions".to_string(),
            "--append-system-prompt".to_string(),
            "You are in read-only mode. You may only read files. Do NOT write, edit, delete, commit, or push anything.".to_string(),
        ]
    } else {
        vec!["--resume".to_string(), session_uuid]
    };
    match spawn_in_terminal(&claude_cmd, &claude_args) {
        Ok(()) => json(200, &OpenTerminalResponse { ok: true }),
        Err(msg) => error(500, "terminal_error", &msg, vec![]),
    }
}

/// Launch the given program+args in a new interactive terminal window.
///
/// On Windows: tries `wt.exe` (Windows Terminal) first; falls back to opening
/// a new PowerShell console window via `CREATE_NEW_CONSOLE`. On other platforms
/// (not yet supported) returns an error.
#[cfg(target_os = "windows")]
fn spawn_in_terminal(program: &str, args: &[String]) -> std::result::Result<(), String> {
    use std::os::windows::process::CommandExt;
    use std::process::Command;

    const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;

    let mut all: Vec<String> = std::iter::once(program.to_string())
        .chain(args.iter().cloned())
        .collect();
    // Resolve the program through PATH so wt.exe gets the full command name.
    if let Some(resolved) = which_program(program) {
        all[0] = resolved;
    }

    // Try Windows Terminal (wt.exe) — opens the command in a new tab.
    if Command::new("wt").arg("--").args(&all).spawn().is_ok() {
        return Ok(());
    }

    // Fallback: open a new console window running PowerShell with the command.
    // Single-quote each arg (doubling embedded single quotes) for PS safety.
    let ps_parts: Vec<String> = all
        .iter()
        .map(|a| format!("'{}'", a.replace('\'', "''")))
        .collect();
    let ps_cmd = format!("& {}", ps_parts.join(" "));
    Command::new("powershell")
        .creation_flags(CREATE_NEW_CONSOLE)
        .args(["-NoExit", "-NoProfile", "-Command", &ps_cmd])
        .spawn()
        .map_err(|e| format!("could not open terminal: {e}"))?;
    Ok(())
}

#[cfg(not(target_os = "windows"))]
fn spawn_in_terminal(_program: &str, _args: &[String]) -> std::result::Result<(), String> {
    Err("terminal launching is only supported on Windows".to_string())
}

/// Resolve a program name through `PATH` using `where.exe`, returning the
/// first match or `None`. Windows-only; used by `spawn_in_terminal`.
#[cfg(target_os = "windows")]
fn which_program(name: &str) -> Option<String> {
    std::process::Command::new("where")
        .arg(name)
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                String::from_utf8(o.stdout)
                    .ok()
                    .and_then(|s| s.lines().next().map(str::trim).map(str::to_string))
            } else {
                None
            }
        })
}

fn cancel(daemon: &Daemon, id: &str) -> Reply {
    match daemon.lock().cancel(id) {
        Ok(state) => {
            // The store has flipped the run + its non-terminal nodes to
            // cancelled; now stop the worker thread and kill its subprocess.
            daemon.cancellations.cancel(id);
            json(
                200,
                &StateResponse {
                    state: state.as_str(),
                },
            )
        }
        Err(e) => store_error(&e),
    }
}

/// Delete a run and all of its child rows.
fn delete_run(daemon: &Daemon, id: &str) -> Reply {
    match daemon.lock().delete_run(id) {
        Ok(()) => json(200, &StateResponse { state: "deleted" }),
        Err(e) => store_error(&e),
    }
}

#[derive(Deserialize)]
struct ClearBody {
    /// Optional run-state filter; when empty, everything is cleared.
    #[serde(default)]
    states: Vec<String>,
    /// Keep on-disk review worktrees instead of purging them.
    #[serde(default)]
    keep_temporary: bool,
}

#[derive(Serialize)]
struct ClearResponse {
    runs_deleted: usize,
    guardians_deleted: usize,
    worktrees_purged: usize,
}

/// Bulk-clear tasks and reviews (RAL-13). Body: `{states?:[...], keep_temporary?}`.
/// An unknown status in `states` is a 400 so the caller fails fast.
fn clear_all(daemon: &Daemon, body: &str) -> Reply {
    let req: ClearBody = if body.trim().is_empty() {
        ClearBody {
            states: Vec::new(),
            keep_temporary: false,
        }
    } else {
        match serde_json::from_str(body) {
            Ok(r) => r,
            Err(_) => {
                return error(
                    400,
                    "bad_request",
                    "body must be {states?:[...], keep_temporary?:bool}",
                    vec![],
                );
            }
        }
    };
    let mut states = Vec::with_capacity(req.states.len());
    for s in &req.states {
        match RunState::parse(s) {
            Some(state) => states.push(state),
            None => {
                return error(
                    400,
                    "bad_request",
                    &format!(
                        "unknown status {s:?} (valid: queued, pending, running, done, failed, cancelled)"
                    ),
                    vec![],
                );
            }
        }
    }
    let outcome = match daemon.lock().clear_all(&states) {
        Ok(o) => o,
        Err(e) => return store_error(&e),
    };
    let mut worktrees_purged = 0;
    if !req.keep_temporary {
        for (gid, root) in &outcome.guardian_roots {
            crate::guardian_merge::purge_worktrees(root, gid);
            worktrees_purged += 1;
        }
    }
    json(
        200,
        &ClearResponse {
            runs_deleted: outcome.runs_deleted,
            guardians_deleted: outcome.guardians_deleted,
            worktrees_purged,
        },
    )
}

// ── guardian endpoints ─────────────────────────────────────────────────────

#[derive(Deserialize)]
struct CreateGuardianBody {
    name: String,
    base_branch: String,
    git_root: String,
    #[serde(default)]
    checks: Vec<String>,
    #[serde(default)]
    skip_checks: bool,
    #[serde(default)]
    skip_worktrees: bool,
    #[serde(default)]
    review_type: Option<String>,
}

#[derive(Deserialize)]
struct GuardianSettingsBody {
    #[serde(default)]
    skip_checks: Option<bool>,
    #[serde(default)]
    skip_worktrees: Option<bool>,
    #[serde(default)]
    resolver_agent: Option<String>,
    #[serde(default)]
    resolver_model: Option<String>,
    #[serde(default)]
    base_branch: Option<String>,
}

#[derive(Deserialize)]
struct AddBranchBody {
    branch: String,
}

#[derive(Deserialize)]
struct ReorderBody {
    order: Vec<String>,
    /// Per-branch enabled states (RAL-43). When present, each entry updates the
    /// branch's `enabled` flag atomically with the reorder. Unknown branch names
    /// are silently ignored (matching the `order` array semantics).
    #[serde(default)]
    enabled: std::collections::HashMap<String, bool>,
}

#[derive(Serialize)]
struct IdResponse {
    id: String,
}

#[derive(Serialize)]
struct PositionResponse {
    position: i64,
}

fn guardian_list(daemon: &Daemon) -> Reply {
    match daemon.lock().list_guardians() {
        Ok(gs) => json(200, &gs),
        Err(e) => store_error(&e),
    }
}

fn guardian_create(daemon: &Daemon, body: &str) -> Reply {
    let Ok(req) = serde_json::from_str::<CreateGuardianBody>(body) else {
        return error(
            400,
            "bad_request",
            "body must be {name, base_branch, git_root}",
            vec![],
        );
    };
    let store = daemon.lock();
    match store.create_guardian(&req.name, &req.base_branch, &req.git_root) {
        Ok(id) => {
            if !req.checks.is_empty() {
                let _ = store.set_guardian_checks(&id, &req.checks);
            }
            if req.skip_checks {
                let _ = store.set_guardian_skip_checks(&id, true);
            }
            if req.skip_worktrees {
                let _ = store.set_guardian_skip_worktrees(&id, true);
            }
            if let Some(t) = req
                .review_type
                .as_deref()
                .map(str::trim)
                .filter(|t| !t.is_empty())
            {
                let _ = store.set_guardian_type(&id, t);
            }
            json(201, &IdResponse { id })
        }
        Err(e) => store_error(&e),
    }
}

fn guardian_get(daemon: &Daemon, id: &str) -> Reply {
    match daemon.lock().get_guardian(id) {
        Ok(g) => json(200, &g),
        Err(e) => store_error(&e),
    }
}

#[derive(Deserialize)]
struct RenameBody {
    name: String,
}

fn guardian_rename(daemon: &Daemon, id: &str, body: &str) -> Reply {
    let Ok(req) = serde_json::from_str::<RenameBody>(body) else {
        return error(400, "bad_request", "body must be {name}", vec![]);
    };
    if req.name.trim().is_empty() {
        return error(400, "bad_request", "name must not be empty", vec![]);
    }
    let store = daemon.lock();
    match store.rename_guardian(id, req.name.trim()) {
        Ok(()) => match store.get_guardian(id) {
            Ok(g) => json(200, &g),
            Err(e) => store_error(&e),
        },
        Err(e) => store_error(&e),
    }
}

/// Update per-review settings (opt-out flags). Only the fields present in the
/// body are changed; the updated guardian view is returned.
fn guardian_settings(daemon: &Daemon, id: &str, body: &str) -> Reply {
    let Ok(req) = serde_json::from_str::<GuardianSettingsBody>(body) else {
        return error(400, "bad_request", "body must be {skip_checks?}", vec![]);
    };
    let store = daemon.lock();
    if let Some(skip) = req.skip_checks {
        if let Err(e) = store.set_guardian_skip_checks(id, skip) {
            return store_error(&e);
        }
    }
    if let Some(skip) = req.skip_worktrees {
        if let Err(e) = store.set_guardian_skip_worktrees(id, skip) {
            return store_error(&e);
        }
    }
    if req.resolver_agent.is_some() || req.resolver_model.is_some() {
        let agent = req.resolver_agent.as_deref().filter(|s| !s.is_empty());
        let model = req.resolver_model.as_deref().filter(|s| !s.is_empty());
        if let Err(e) = store.set_guardian_resolver(id, agent, model) {
            return store_error(&e);
        }
    }
    if let Some(branch) = req.base_branch.as_deref().filter(|s| !s.is_empty()) {
        if let Err(e) = store.set_guardian_base_branch(id, branch) {
            return store_error(&e);
        }
    }
    match store.get_guardian(id) {
        Ok(g) => json(200, &g),
        Err(e) => store_error(&e),
    }
}

/// Delete a review and purge its review worktrees/branches from every project.
fn guardian_delete(daemon: &Daemon, id: &str) -> Reply {
    let store = daemon.lock();
    // Collect all project roots before deleting (multi-project guardians have >1).
    let projects = store.get_guardian(id).ok().map(|g| g.projects);
    match store.delete_guardian(id) {
        Ok(()) => {
            drop(store);
            if let Some(roots) = projects {
                for root in &roots {
                    crate::guardian_merge::purge_worktrees(root, id);
                }
            }
            json(200, &StateResponse { state: "deleted" })
        }
        Err(e) => store_error(&e),
    }
}

fn guardian_add_branch(daemon: &Daemon, id: &str, body: &str) -> Reply {
    let Ok(req) = serde_json::from_str::<AddBranchBody>(body) else {
        return error(400, "bad_request", "body must be {branch}", vec![]);
    };
    match daemon.lock().add_guardian_branch(id, &req.branch) {
        Ok(position) => json(200, &PositionResponse { position }),
        Err(e) => store_error(&e),
    }
}

fn guardian_reorder(daemon: &Daemon, id: &str, body: &str) -> Reply {
    let Ok(req) = serde_json::from_str::<ReorderBody>(body) else {
        return error(400, "bad_request", "body must be {order:[...]}", vec![]);
    };
    let mut store = daemon.lock();
    if let Err(e) = store.reorder_guardian_branches(id, &req.order) {
        return store_error(&e);
    }
    // RAL-43: apply any staged enabled/disabled states atomically with the reorder.
    for (branch, &enabled) in &req.enabled {
        if let Err(e) = store.set_branch_enabled_by_name(id, branch, enabled) {
            return store_error(&e);
        }
    }
    match store.get_guardian(id) {
        Ok(g) => json(200, &g),
        Err(e) => store_error(&e),
    }
}

fn guardian_approve(daemon: &Daemon, id: &str) -> Reply {
    match daemon.lock().approve_guardian(id) {
        Ok(status) => json(
            200,
            &StateResponse {
                state: status.as_str(),
            },
        ),
        Err(e) => store_error(&e),
    }
}

fn guardian_cancel(daemon: &Daemon, id: &str) -> Reply {
    match daemon.lock().cancel_guardian(id) {
        Ok(status) => json(
            200,
            &StateResponse {
                state: status.as_str(),
            },
        ),
        Err(e) => store_error(&e),
    }
}

/// Run one or all LLM-generated manual review commands as fire-and-forget
/// terminal subprocesses (RAL-27). Body `{ "index": N }` runs command N only;
/// no body (or `{}`) runs all commands. Opens each in a new terminal window.
fn guardian_run_manual_commands(daemon: &Daemon, id: &str, body: &str) -> Reply {
    #[derive(Deserialize, Default)]
    struct Body {
        index: Option<usize>,
    }
    let req: Body = serde_json::from_str(body).unwrap_or_default();

    let g = match daemon.lock().get_guardian(id) {
        Ok(g) => g,
        Err(e) => return store_error(&e),
    };

    if g.manual_commands.is_empty() {
        return error(
            400,
            "no_commands",
            "no manual review commands available — review may still be building",
            vec![],
        );
    }

    let to_run: Vec<String> = match req.index {
        Some(i) => {
            if i >= g.manual_commands.len() {
                return error(400, "out_of_range", "command index out of range", vec![]);
            }
            vec![g.manual_commands[i].clone()]
        }
        None => g.manual_commands.clone(),
    };

    let git_root = g.git_root.clone();
    let mut errors: Vec<String> = Vec::new();
    for cmd in &to_run {
        // cd /d sets both drive and directory on Windows before running the command.
        let full_cmd = format!("cd /d \"{git_root}\" && {cmd}");
        if let Err(e) = spawn_in_terminal("cmd", &["/K".to_string(), full_cmd]) {
            errors.push(e);
        }
    }

    if errors.is_empty() {
        json(200, &OpenTerminalResponse { ok: true })
    } else {
        error(500, "terminal_error", &errors.join("; "), vec![])
    }
}

/// List candidate base branches for the given guardian, scoped to the same
/// remote as its current base branch (e.g. `origin/*`).
fn guardian_base_branches(daemon: &Daemon, id: &str) -> Reply {
    let guardian = match daemon.lock().get_guardian(id) {
        Ok(g) => g,
        Err(e) => return store_error(&e),
    };
    let branches =
        crate::guardian_merge::list_base_branches(&guardian.git_root, &guardian.base_branch);
    json(200, &branches)
}

#[derive(Deserialize)]
struct ChangeBaseBody {
    branch: String,
}

/// Change the base branch of a review and trigger a rebuild. Guards against
/// `approved`/`deployed` status; all other states are permitted.
fn guardian_change_base(daemon: &Daemon, id: &str, body: &str) -> Reply {
    let Ok(req) = serde_json::from_str::<ChangeBaseBody>(body) else {
        return error(400, "bad_request", "body must be {branch}", vec![]);
    };
    let branch = req.branch.trim();
    if branch.is_empty() {
        return error(400, "bad_request", "branch must not be empty", vec![]);
    }
    let store = daemon.lock();
    let guardian = match store.get_guardian(id) {
        Ok(g) => g,
        Err(e) => return store_error(&e),
    };
    if matches!(guardian.status.as_str(), "approved" | "deployed") {
        return error(
            409,
            "invalid_transition",
            "cannot change the base of an approved or deployed review",
            vec![],
        );
    }
    if let Err(e) = store.set_guardian_base_branch(id, branch) {
        return store_error(&e);
    }
    let has_branches = !guardian.branches.is_empty();
    drop(store);
    if !has_branches {
        return match daemon.lock().get_guardian(id) {
            Ok(g) => json(200, &g),
            Err(e) => store_error(&e),
        };
    }
    let runner: Arc<dyn Runner> = Arc::new(SubprocessRunner::from_env());
    crate::guardian_merge::start_merge(daemon.store_handle(), runner, id, daemon.semaphore_handle())
}

fn guardian_merge(daemon: &Daemon, id: &str) -> Reply {
    let runner: Arc<dyn Runner> = Arc::new(SubprocessRunner::from_env());
    crate::guardian_merge::start_merge(daemon.store_handle(), runner, id, daemon.semaphore_handle())
}

/// Cancel an in-progress rebase (status `merging` or `in_review`) and
/// immediately start a fresh one. The in-flight background thread is
/// superseded: its eventual status writes will be overwritten by the new run.
fn guardian_cancel_and_merge(daemon: &Daemon, id: &str) -> Reply {
    if let Err(e) = daemon.lock().reset_guardian_to_collecting(id) {
        return store_error(&e);
    }
    let runner: Arc<dyn Runner> = Arc::new(SubprocessRunner::from_env());
    crate::guardian_merge::start_merge(daemon.store_handle(), runner, id, daemon.semaphore_handle())
}

#[derive(Deserialize)]
struct FeedbackBody {
    #[serde(default)]
    feedback: String,
}

fn guardian_feedback(daemon: &Daemon, id: &str, pos: &str, body: &str) -> Reply {
    let Ok(req) = serde_json::from_str::<FeedbackBody>(body) else {
        return error(400, "bad_request", "body must be {feedback}", vec![]);
    };
    let Ok(position) = pos.parse::<i64>() else {
        return error(
            400,
            "bad_request",
            "branch position must be an integer",
            vec![],
        );
    };
    if req.feedback.trim().is_empty() {
        return error(400, "bad_request", "feedback must not be empty", vec![]);
    }
    let runner: Arc<dyn Runner> = Arc::new(SubprocessRunner::from_env());
    crate::guardian_merge::start_feedback(daemon.store_handle(), runner, id, position, req.feedback)
}

#[derive(Serialize)]
struct MessagesResponse {
    messages: Vec<crate::guardian::MessageView>,
}

/// The guardian's global feedback thread (RAL-22).
fn guardian_messages(daemon: &Daemon, id: &str) -> Reply {
    match daemon.lock().guardian_messages(id) {
        Ok(messages) => json(200, &MessagesResponse { messages }),
        Err(e) => store_error(&e),
    }
}

#[derive(Deserialize)]
struct ChatBody {
    #[serde(default)]
    text: String,
}

/// Post a reviewer message to the global feedback thread; the triage agent
/// replies in the background (RAL-22).
fn guardian_chat(daemon: &Daemon, id: &str, body: &str) -> Reply {
    let Ok(req) = serde_json::from_str::<ChatBody>(body) else {
        return error(400, "bad_request", "body must be {text}", vec![]);
    };
    if req.text.trim().is_empty() {
        return error(400, "bad_request", "message text must not be empty", vec![]);
    }
    let runner: Arc<dyn Runner> = Arc::new(SubprocessRunner::from_env());
    crate::guardian_merge::start_chat(daemon.store_handle(), runner, id, req.text)
}

// ── blocking server ──────────────────────────────────────────────────────────

/// Open the store at `db_path`, start the scheduler, and serve the API on `addr`
/// until the process is killed.
///
/// # Errors
/// Returns an error if the store cannot be opened or the listener cannot bind.
pub fn serve<A: ToSocketAddrs>(
    addr: A,
    db_path: &Path,
    max_concurrent: i64,
) -> std::io::Result<()> {
    let store = Store::open(db_path).map_err(|e| std::io::Error::other(e.to_string()))?;
    // Crash recovery before anything schedules: a previous unclean shutdown may
    // have left runs `Running` with no worker. Reset them to `Pending` so the
    // scheduler resumes them; finished sessions are preserved and skipped, so
    // only the unfinished tail re-runs (RAL-19).
    match store.recover_orphaned_runs() {
        Ok(ids) if !ids.is_empty() => {
            eprintln!(
                "recovered {} orphaned run(s): {}",
                ids.len(),
                ids.join(", ")
            );
        }
        Ok(_) => {}
        Err(e) => eprintln!("crash recovery failed: {e}"),
    }
    let server = tiny_http::Server::http(addr).map_err(|e| std::io::Error::other(e.to_string()))?;
    let daemon = Daemon::new(store, max_concurrent);

    // Scheduler runs on its own thread, sharing the store via Arc<Mutex> and the
    // cancellation registry so a `cancel` request can reach its workers. The
    // runner shares the daemon's PID registry so `/api/resources` can attribute
    // OS metrics to the sessions it spawns (RAL-11).
    let runner: Arc<dyn Runner> =
        Arc::new(SubprocessRunner::from_env().with_registry(daemon.procs_handle()));
    let handle = daemon.store_handle();
    let cancellations = daemon.cancellations_handle();
    let sem = daemon.semaphore_handle();
    std::thread::spawn(move || {
        crate::scheduler::run_loop(handle, runner, max_concurrent, cancellations, sem);
    });

    run_http_loop(server, &daemon);
    Ok(())
}

/// Serve requests from an already-bound server against an already-open store.
/// Does NOT start the scheduler — exposed so tests can bind an ephemeral port
/// and drive the API without runs executing underneath them.
pub fn serve_with(server: tiny_http::Server, store: Store, max_concurrent: i64) {
    let daemon = Daemon::new(store, max_concurrent);
    run_http_loop(server, &daemon);
}

fn run_http_loop(server: tiny_http::Server, daemon: &Daemon) {
    for mut request in server.incoming_requests() {
        let method = request.method().as_str().to_string();
        let url = request.url().to_string();
        let path = url.split('?').next().unwrap_or(&url).to_string();

        let mut body = String::new();
        let _ = request.as_reader().read_to_string(&mut body);

        let reply = route(daemon, &method, &path, &body);
        let header = tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..])
            .expect("valid header");
        let response = tiny_http::Response::from_string(reply.body)
            .with_status_code(reply.status)
            .with_header(header);
        let _ = request.respond(response);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = "[[task]]\nname=\"t\"\n[[task.session]]\ncwd=\"/r\"\nprompt=\"p\"\n";

    fn daemon() -> Daemon {
        Daemon::new(Store::open_in_memory().unwrap(), 12)
    }

    fn submit_body(toml: &str) -> String {
        serde_json::to_string(&serde_json::json!({ "toml": toml })).unwrap()
    }

    #[test]
    fn health_ok() {
        let d = daemon();
        let r = route(&d, "GET", "/api/daemon", "");
        assert_eq!(r.status, 200);
        assert!(r.body.contains("\"status\":\"ok\""));
    }

    #[test]
    fn validate_reports_invalid() {
        let d = daemon();
        let r = route(&d, "POST", "/api/runs/validate", &submit_body(""));
        assert_eq!(r.status, 200);
        assert!(r.body.contains("\"valid\":false"));
    }

    #[test]
    fn validate_reports_valid() {
        let d = daemon();
        let r = route(&d, "POST", "/api/runs/validate", &submit_body(GOOD));
        assert_eq!(r.status, 200);
        assert!(r.body.contains("\"valid\":true"));
    }

    #[test]
    fn submit_rejects_invalid() {
        let d = daemon();
        let r = route(&d, "POST", "/api/runs", &submit_body("nope = true"));
        assert_eq!(r.status, 400);
        assert!(r.body.contains("validation_failed"));
    }

    #[test]
    fn submit_then_list_and_get() {
        let d = daemon();
        let r = route(&d, "POST", "/api/runs", &submit_body(GOOD));
        assert_eq!(r.status, 201);
        assert!(r.body.contains("run-000000000001"));
        assert!(r.body.contains("\"state\":\"pending\""));

        let board = route(&d, "GET", "/api/tasks", "");
        assert_eq!(board.status, 200);
        assert!(board.body.contains("run-000000000001"));
        assert!(board.body.contains("\"max_concurrent\":12"));

        let got = route(&d, "GET", "/api/runs/run-000000000001", "");
        assert_eq!(got.status, 200);
        assert!(got.body.contains("\"name\":\"t\""));
    }

    #[test]
    fn get_missing_run_is_404() {
        let d = daemon();
        let r = route(&d, "GET", "/api/runs/run-999", "");
        assert_eq!(r.status, 404);
        assert!(r.body.contains("not_found"));
    }

    #[test]
    fn hold_then_activate() {
        let d = daemon();
        let body =
            serde_json::to_string(&serde_json::json!({ "toml": GOOD, "hold": true })).unwrap();
        let r = route(&d, "POST", "/api/runs", &body);
        assert!(r.body.contains("\"state\":\"queued\""));

        let act = route(&d, "POST", "/api/runs/run-000000000001/activate", "");
        assert_eq!(act.status, 200);
        assert!(act.body.contains("\"state\":\"pending\""));
    }

    #[test]
    fn activate_pending_conflicts() {
        let d = daemon();
        route(&d, "POST", "/api/runs", &submit_body(GOOD));
        let act = route(&d, "POST", "/api/runs/run-000000000001/activate", "");
        assert_eq!(act.status, 409);
    }

    #[test]
    fn edit_session_resets_run_to_pending() {
        let d = daemon();
        route(&d, "POST", "/api/runs", &submit_body(GOOD));
        // mark it done, then edit -> should go back to pending
        d.lock()
            .set_run_state("run-000000000001", RunState::Done)
            .unwrap();
        let body = serde_json::json!({
            "kind": "session", "task_idx": 0, "session_idx": 0,
            "cwd": "/new", "agent": "ollama", "model": "qwen3:8b", "prompt": "changed"
        })
        .to_string();
        let r = route(&d, "POST", "/api/runs/run-000000000001/edit", &body);
        assert_eq!(r.status, 200);
        assert!(r.body.contains("\"state\":\"pending\""));
        assert!(r.body.contains("\"cwd\":\"/new\""));
        assert!(r.body.contains("\"agent\":\"ollama\""));
    }

    #[test]
    fn edit_task_project() {
        let d = daemon();
        route(&d, "POST", "/api/runs", &submit_body(GOOD));
        let body = serde_json::json!({"kind":"task","task_idx":0,"name":"t","project":"myproj"})
            .to_string();
        let r = route(&d, "POST", "/api/runs/run-000000000001/edit", &body);
        assert_eq!(r.status, 200);
        assert!(r.body.contains("\"project\":\"myproj\""));
    }

    #[test]
    fn edit_unknown_kind_is_400() {
        let d = daemon();
        route(&d, "POST", "/api/runs", &submit_body(GOOD));
        let r = route(
            &d,
            "POST",
            "/api/runs/run-000000000001/edit",
            "{\"kind\":\"wat\"}",
        );
        assert_eq!(r.status, 400);
    }

    #[test]
    fn retry_resets_run_to_pending() {
        let d = daemon();
        route(&d, "POST", "/api/runs", &submit_body(GOOD));
        d.lock()
            .set_run_state("run-000000000001", RunState::Failed)
            .unwrap();
        let r = route(&d, "POST", "/api/runs/run-000000000001/retry", "");
        assert_eq!(r.status, 200);
        assert!(r.body.contains("\"state\":\"pending\""));
    }

    #[test]
    fn retry_missing_run_is_404() {
        let d = daemon();
        assert_eq!(route(&d, "POST", "/api/runs/run-999/retry", "").status, 404);
    }

    #[test]
    fn run_logs_records_transitions() {
        let d = daemon();
        route(&d, "POST", "/api/runs", &submit_body(GOOD));
        d.lock()
            .set_run_state("run-000000000001", RunState::Done)
            .unwrap();
        let r = route(&d, "GET", "/api/runs/run-000000000001/logs", "");
        assert_eq!(r.status, 200);
        assert!(r.body.contains("run → done"));
        assert!(r.body.contains("\"scope\":\"run\""));
    }

    #[test]
    fn logs_missing_run_is_404() {
        let d = daemon();
        assert_eq!(route(&d, "GET", "/api/runs/run-999/logs", "").status, 404);
    }

    #[test]
    fn guardian_logs_records_status() {
        let d = daemon();
        let body =
            serde_json::json!({"name":"r","base_branch":"main","git_root":"/repo"}).to_string();
        route(&d, "POST", "/api/guardians", &body);
        let gid = "guardian-000000000001";
        d.lock()
            .set_guardian_status(gid, crate::guardian::GuardianStatus::InReview, None)
            .unwrap();
        let r = route(&d, "GET", &format!("/api/guardians/{gid}/logs"), "");
        assert_eq!(r.status, 200);
        assert!(r.body.contains("review → in_review"));
        assert_eq!(
            route(&d, "GET", "/api/guardians/guardian-000000000009/logs", "").status,
            404
        );
    }

    #[test]
    fn run_worktrees_lists_sessions() {
        let d = daemon();
        route(&d, "POST", "/api/runs", &submit_body(GOOD));
        let r = route(&d, "GET", "/api/runs/run-000000000001/worktrees", "");
        assert_eq!(r.status, 200);
        // The GOOD fixture's session cwd is "/r"; not a git worktree -> project null.
        assert!(r.body.contains("\"worktree\":\"/r\""));
        assert!(r.body.contains("\"project\":null"));
    }

    #[test]
    fn worktrees_missing_run_is_404() {
        let d = daemon();
        assert_eq!(
            route(&d, "GET", "/api/runs/run-999/worktrees", "").status,
            404
        );
    }

    #[test]
    fn cancel_run() {
        let d = daemon();
        route(&d, "POST", "/api/runs", &submit_body(GOOD));
        let c = route(&d, "POST", "/api/runs/run-000000000001/cancel", "");
        assert_eq!(c.status, 200);
        assert!(c.body.contains("\"state\":\"cancelled\""));
    }

    #[test]
    fn delete_run_route() {
        let d = daemon();
        route(&d, "POST", "/api/runs", &submit_body(GOOD));
        let del = route(&d, "DELETE", "/api/runs/run-000000000001", "");
        assert_eq!(del.status, 200);
        assert!(del.body.contains("\"state\":\"deleted\""));
        // Gone now.
        assert_eq!(
            route(&d, "GET", "/api/runs/run-000000000001", "").status,
            404
        );
    }

    #[test]
    fn delete_missing_run_is_404() {
        let d = daemon();
        assert_eq!(route(&d, "DELETE", "/api/runs/run-999", "").status, 404);
    }

    #[test]
    fn guardian_reorder_route() {
        let d = daemon();
        let body =
            serde_json::json!({"name":"r","base_branch":"main","git_root":"/repo"}).to_string();
        route(&d, "POST", "/api/guardians", &body);
        let gid = "guardian-000000000001";
        for br in ["a", "b", "c"] {
            let b = serde_json::json!({ "branch": br }).to_string();
            route(&d, "POST", &format!("/api/guardians/{gid}/branches"), &b);
        }
        let reorder = serde_json::json!({"order":["c","a","b"]}).to_string();
        let r = route(
            &d,
            "POST",
            &format!("/api/guardians/{gid}/branches/reorder"),
            &reorder,
        );
        assert_eq!(r.status, 200);
        // "c" is now first in the returned view.
        let ci = r.body.find("\"branch\":\"c\"").unwrap();
        let ai = r.body.find("\"branch\":\"a\"").unwrap();
        assert!(ci < ai);
    }

    #[test]
    fn guardian_reorder_with_enabled_states_route() {
        // RAL-43: reorder body can include per-branch enabled flags; the returned
        // guardian view reflects them.
        let d = daemon();
        let body =
            serde_json::json!({"name":"r","base_branch":"main","git_root":"/repo"}).to_string();
        route(&d, "POST", "/api/guardians", &body);
        let gid = "guardian-000000000001";
        for br in ["a", "b", "c"] {
            let b = serde_json::json!({ "branch": br }).to_string();
            route(&d, "POST", &format!("/api/guardians/{gid}/branches"), &b);
        }
        // Disable "b", keep order unchanged.
        let body = serde_json::json!({"order":["a","b","c"],"enabled":{"b":false}}).to_string();
        let r = route(
            &d,
            "POST",
            &format!("/api/guardians/{gid}/branches/reorder"),
            &body,
        );
        assert_eq!(r.status, 200);
        // Parse the returned guardian view and check the enabled flag per branch.
        let g: serde_json::Value = serde_json::from_str(&r.body).unwrap();
        let branches = g["branches"].as_array().unwrap();
        let a_enabled = branches.iter().find(|b| b["branch"] == "a").unwrap()["enabled"]
            .as_bool()
            .unwrap();
        let b_enabled = branches.iter().find(|b| b["branch"] == "b").unwrap()["enabled"]
            .as_bool()
            .unwrap();
        let c_enabled = branches.iter().find(|b| b["branch"] == "c").unwrap()["enabled"]
            .as_bool()
            .unwrap();
        assert!(a_enabled, "a must stay enabled");
        assert!(!b_enabled, "b must be disabled");
        assert!(c_enabled, "c must stay enabled");
    }

    #[test]
    fn restart_run_route_returns_dirtied_and_is_pending() {
        let d = daemon();
        let r = route(&d, "POST", "/api/runs", &submit_body(GOOD));
        assert_eq!(r.status, 201);
        // First submitted run has the deterministic id run-000000000001.
        let rr = route(&d, "POST", "/api/runs/run-000000000001/restart", "");
        assert_eq!(rr.status, 200);
        assert!(rr.body.contains("\"state\":\"pending\""));
        assert!(rr.body.contains("\"dirtied\":"));
    }

    #[test]
    fn restart_run_missing_is_404() {
        let d = daemon();
        assert_eq!(
            route(&d, "POST", "/api/runs/run-999/restart", "").status,
            404
        );
    }

    #[test]
    fn restart_session_bad_index_is_400() {
        let d = daemon();
        let r = route(&d, "POST", "/api/runs/run-1/sessions/x/y/restart", "");
        assert_eq!(r.status, 400);
    }

    #[test]
    fn clear_all_wipes_runs_and_resets_ids() {
        let d = daemon();
        route(&d, "POST", "/api/runs", &submit_body(GOOD));
        route(&d, "POST", "/api/runs", &submit_body(GOOD));
        let r = route(&d, "POST", "/api/clear", "");
        assert_eq!(r.status, 200);
        assert!(r.body.contains("\"runs_deleted\":2"));
        // Both runs are gone.
        assert_eq!(
            route(&d, "GET", "/api/runs/run-000000000001", "").status,
            404
        );
        // Id sequence reset: the next submitted run is run-...001 again.
        let again = route(&d, "POST", "/api/runs", &submit_body(GOOD));
        assert!(again.body.contains("run-000000000001"));
    }

    #[test]
    fn clear_rejects_unknown_status() {
        let d = daemon();
        let body = serde_json::json!({"states": ["done", "bogus"]}).to_string();
        let r = route(&d, "POST", "/api/clear", &body);
        assert_eq!(r.status, 400);
        assert!(r.body.contains("unknown status"));
    }

    #[test]
    fn clear_with_status_filter_keeps_unmatched_runs() {
        let d = daemon();
        route(&d, "POST", "/api/runs", &submit_body(GOOD));
        // Submitted runs are Pending; filtering on `done` matches nothing.
        let body = serde_json::json!({"states": ["done"]}).to_string();
        let r = route(&d, "POST", "/api/clear", &body);
        assert_eq!(r.status, 200);
        assert!(r.body.contains("\"runs_deleted\":0"));
        assert_eq!(
            route(&d, "GET", "/api/runs/run-000000000001", "").status,
            200
        );
    }

    #[test]
    fn feedback_on_missing_guardian_is_404() {
        let d = daemon();
        let body = serde_json::json!({"feedback": "do x"}).to_string();
        let r = route(
            &d,
            "POST",
            "/api/guardians/guardian-000000000009/branches/0/feedback",
            &body,
        );
        assert_eq!(r.status, 404);
    }

    #[test]
    fn guardian_messages_endpoint_starts_empty() {
        let d = daemon();
        let body =
            serde_json::json!({"name":"r","base_branch":"main","git_root":"/repo"}).to_string();
        route(&d, "POST", "/api/guardians", &body);
        let r = route(
            &d,
            "GET",
            "/api/guardians/guardian-000000000001/messages",
            "",
        );
        assert_eq!(r.status, 200);
        assert!(r.body.contains("\"messages\":[]"));
    }

    #[test]
    fn guardian_chat_empty_text_is_400() {
        let d = daemon();
        let body =
            serde_json::json!({"name":"r","base_branch":"main","git_root":"/repo"}).to_string();
        route(&d, "POST", "/api/guardians", &body);
        let r = route(
            &d,
            "POST",
            "/api/guardians/guardian-000000000001/chat",
            "{\"text\":\"  \"}",
        );
        assert_eq!(r.status, 400);
    }

    #[test]
    fn feedback_empty_is_400() {
        let d = daemon();
        let body =
            serde_json::json!({"name":"r","base_branch":"main","git_root":"/repo"}).to_string();
        route(&d, "POST", "/api/guardians", &body);
        let r = route(
            &d,
            "POST",
            "/api/guardians/guardian-000000000001/branches/0/feedback",
            "{\"feedback\":\"  \"}",
        );
        assert_eq!(r.status, 400);
    }

    #[test]
    fn guardian_rename_and_delete() {
        let d = daemon();
        let body =
            serde_json::json!({"name":"r","base_branch":"main","git_root":"/repo"}).to_string();
        route(&d, "POST", "/api/guardians", &body);
        let gid = "guardian-000000000001";
        // rename
        let rn = route(
            &d,
            "POST",
            &format!("/api/guardians/{gid}/rename"),
            "{\"name\":\"renamed\"}",
        );
        assert_eq!(rn.status, 200);
        assert!(rn.body.contains("\"name\":\"renamed\""));
        // empty rename rejected
        assert_eq!(
            route(
                &d,
                "POST",
                &format!("/api/guardians/{gid}/rename"),
                "{\"name\":\"  \"}"
            )
            .status,
            400
        );
        // delete
        let del = route(&d, "DELETE", &format!("/api/guardians/{gid}"), "");
        assert_eq!(del.status, 200);
        assert!(del.body.contains("deleted"));
        assert_eq!(
            route(&d, "GET", &format!("/api/guardians/{gid}"), "").status,
            404
        );
    }

    #[test]
    fn guardian_skip_checks_via_create_and_settings() {
        let d = daemon();
        let body = serde_json::json!({"name":"r","base_branch":"main","git_root":"/repo","skip_checks":true})
            .to_string();
        route(&d, "POST", "/api/guardians", &body);
        let gid = "guardian-000000000001";
        assert!(
            route(&d, "GET", &format!("/api/guardians/{gid}"), "")
                .body
                .contains("\"skip_checks\":true")
        );
        // toggle back off via settings
        let r = route(
            &d,
            "POST",
            &format!("/api/guardians/{gid}/settings"),
            "{\"skip_checks\":false}",
        );
        assert_eq!(r.status, 200);
        assert!(r.body.contains("\"skip_checks\":false"));
    }

    #[test]
    fn delete_missing_guardian_is_404() {
        let d = daemon();
        assert_eq!(
            route(&d, "DELETE", "/api/guardians/guardian-000000000009", "").status,
            404
        );
    }

    #[test]
    fn resources_endpoint_is_empty_without_running_sessions() {
        let d = daemon();
        route(&d, "POST", "/api/runs", &submit_body(GOOD));
        // Submitted run is Pending and no subprocess is tracked, so nothing is
        // reported (and the endpoint does not error).
        let r = route(&d, "GET", "/api/resources", "");
        assert_eq!(r.status, 200);
        assert!(r.body.contains("\"resources\":[]"));
    }

    #[test]
    fn unknown_route_is_404() {
        let d = daemon();
        assert_eq!(route(&d, "GET", "/nope", "").status, 404);
    }
}
