//! The daemon's HTTP/JSON API (see `docs/daemon-api.md`).
//!
//! The routing/handler core (`route`) is a pure function over `(&Daemon, method,
//! path, body)` so it can be unit-tested without sockets. `serve` wraps it in a
//! blocking `tiny_http` loop and runs the scheduler on a second thread; both
//! share the store through an `Arc<Mutex<Store>>`.

use std::net::ToSocketAddrs;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use opentelemetry::trace::{SpanKind, Status};
use ralphus_core::validate::{ValidationError, validate_toml};
use serde::{Deserialize, Serialize};

use crate::cancel::Cancellations;
use crate::procreg::ProcRegistry;
use crate::runner::{Runner, SubprocessRunner};
use crate::scheduler::Semaphore;
use crate::store::{NodeState, RunState, Store, StoreError};
use crate::summary_worker::SummaryQueue;

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
    /// RAL-121: priority queue of guardians awaiting a preliminary (git-log)
    /// change-summary recompute. Its background worker threads are spawned in
    /// `serve()` (not here — plain `Daemon::new` is also used by unit tests
    /// that don't want live background threads); handlers only ever enqueue
    /// into it.
    summary_queue: Arc<SummaryQueue>,
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
            summary_queue: SummaryQueue::new(),
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

    /// A cloned handle to the guardian summary-recompute priority queue (for
    /// the scheduler thread and the background workers spawned in `serve()`).
    #[must_use]
    pub fn summary_queue_handle(&self) -> Arc<SummaryQueue> {
        Arc::clone(&self.summary_queue)
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

fn default_vcs() -> String {
    "git".to_string()
}

#[derive(Deserialize)]
struct RegisterProjectBody {
    name: String,
    #[serde(default)]
    description: String,
    path: String,
    #[serde(default = "default_vcs")]
    vcs: String,
}

#[derive(Serialize)]
struct ProjectsResponse {
    projects: Vec<crate::store::ProjectView>,
}

#[derive(Serialize)]
struct ProjectValidateResponse {
    valid: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
}

#[derive(Serialize)]
struct DaemonHealth<'a> {
    name: &'a str,
    version: &'a str,
    status: &'a str,
    db: &'a str,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    warnings: Vec<&'a str>,
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
    /// Whether a configured down-time window (RAL-122) is active right now —
    /// the board uses this to render `pending` runs as "waiting" rather than
    /// implying the scheduler is simply slow to pick them up.
    downtime_active: bool,
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
        ("GET", ["api", "tasks"]) => board(daemon, query),
        ("GET", ["api", "projects"]) => list_projects(daemon),
        ("POST", ["api", "projects"]) => register_project(daemon, body),
        ("GET", ["api", "projects", name]) => get_project(daemon, name),
        ("GET", ["api", "projects", name, "validate"]) => validate_project(daemon, name),
        ("GET", ["api", "resources"]) => resources(daemon),
        ("GET", ["api", "cartographer"]) => cartographer_query(daemon, query),
        ("GET", ["api", "cartographer", id]) => cartographer_get(daemon, id),
        ("POST", ["api", "ghosts", "copy"]) => ghost_copy(daemon, body),
        ("GET", ["api", "ghosts", owner_uri]) => ghost_get(daemon, owner_uri),
        ("POST", ["api", "runs", "validate"]) => validate_endpoint(body),
        ("POST", ["api", "runs"]) => submit(daemon, body),
        ("POST", ["api", "clear"]) => clear_all(daemon, body),
        ("GET", ["api", "queue"]) => queue(daemon),
        ("POST", ["api", "queue", "reorder"]) => queue_reorder(daemon, body),
        ("POST", ["api", "queue", "set-position"]) => queue_set_position(daemon, body),
        ("GET", ["api", "graph"]) => global_graph(daemon, query),
        ("GET", ["api", "runs", id]) => get_run(daemon, id),
        ("GET", ["api", "runs", id, "worktrees"]) => run_worktrees(daemon, id),
        ("GET", ["api", "runs", id, "logs"]) => run_logs(daemon, id),
        ("GET", ["api", "runs", id, "graph"]) => run_graph(daemon, id),
        ("POST", ["api", "runs", id, "activate"]) => activate(daemon, id),
        ("POST", ["api", "runs", id, "cancel", "preview"]) => cancel_run_preview(daemon, id),
        ("POST", ["api", "runs", id, "cancel"]) => cancel(daemon, id),
        ("POST", ["api", "runs", id, "set-status"]) => set_status(daemon, id, body),
        ("POST", ["api", "runs", id, "edit"]) => edit_run(daemon, id, body),
        ("POST", ["api", "runs", id, "retry"]) => retry_run(daemon, id),
        ("POST", ["api", "runs", id, "restart", "preview"]) => restart_run_preview(daemon, id),
        ("POST", ["api", "runs", id, "restart"]) => restart_run(daemon, id),
        ("POST", ["api", "runs", id, "add-dependency"]) => add_dependency(daemon, id, body),
        ("POST", ["api", "runs", id, "sessions", ti, si, "restart", "preview"]) => {
            restart_session_preview(daemon, id, ti, si)
        }
        ("POST", ["api", "runs", id, "sessions", ti, si, "restart"]) => {
            restart_session(daemon, id, ti, si)
        }
        (
            "POST",
            [
                "api",
                "runs",
                id,
                "sessions",
                ti,
                si,
                "verify",
                vi,
                "restart",
            ],
        ) => restart_session_verify(daemon, id, ti, si, vi),
        ("POST", ["api", "runs", id, "tasks", ti, "verify", vi, "restart"]) => {
            restart_task_verify(daemon, id, ti, vi)
        }
        ("POST", ["api", "runs", id, "sessions", ti, si, "open-terminal"]) => {
            open_terminal(daemon, id, ti, si, query)
        }
        ("GET", ["api", "runs", id, "sessions", ti, si, "pane"]) => {
            session_pane(daemon, id, ti, si, query)
        }
        (
            "POST",
            [
                "api",
                "runs",
                id,
                "verifies",
                task_idx,
                scope,
                session_idx,
                verify_idx,
                "open-terminal",
            ],
        ) => open_verify_terminal(daemon, id, task_idx, scope, session_idx, verify_idx, query),
        (
            "GET",
            [
                "api",
                "runs",
                id,
                "verifies",
                task_idx,
                scope,
                session_idx,
                verify_idx,
                "pane",
            ],
        ) => verify_pane(daemon, id, task_idx, scope, session_idx, verify_idx, query),
        ("DELETE", ["api", "runs", id]) => delete_run(daemon, id),
        ("GET", ["api", "guardians"]) => guardian_list(daemon),
        ("POST", ["api", "guardians"]) => guardian_create(daemon, body),
        ("GET", ["api", "guardians", id]) => guardian_get(daemon, id),
        ("GET", ["api", "guardians", id, "logs"]) => guardian_logs(daemon, id),
        ("POST", ["api", "guardians", id, "rename"]) => guardian_rename(daemon, id, body),
        ("POST", ["api", "guardians", id, "settings"]) => guardian_settings(daemon, id, body),
        ("POST", ["api", "guardians", id, "squash"]) => guardian_squash(daemon, id, body),
        ("DELETE", ["api", "guardians", id]) => guardian_delete(daemon, id),
        ("POST", ["api", "guardians", id, "branches"]) => guardian_add_branch(daemon, id, body),
        ("POST", ["api", "guardians", id, "branches", "reorder"]) => {
            guardian_reorder(daemon, id, body)
        }
        ("POST", ["api", "guardians", id, "branches", "arrange"]) => {
            guardian_arrange(daemon, id, body)
        }
        ("POST", ["api", "guardians", id, "branches", branch_id, "feedback"]) => {
            guardian_feedback(daemon, id, branch_id, body)
        }
        ("GET", ["api", "guardians", id, "messages"]) => guardian_messages(daemon, id),
        ("POST", ["api", "guardians", id, "chat"]) => guardian_chat(daemon, id, body),
        ("POST", ["api", "guardians", id, "chat", "fork"]) => guardian_chat_fork(daemon, id, body),
        ("GET", ["api", "guardians", id, "base-branches"]) => guardian_base_branches(daemon, id),
        ("POST", ["api", "guardians", id, "base"]) => guardian_change_base(daemon, id, body),
        ("POST", ["api", "guardians", id, "force_start"]) => guardian_force_start(daemon, id),
        (
            "POST",
            [
                "api",
                "guardians",
                id,
                "branches",
                branch_id,
                "dismiss_reenable",
            ],
        ) => guardian_dismiss_reenable(daemon, id, branch_id),
        ("POST", ["api", "guardians", id, "branches", branch_id, "move"]) => {
            guardian_move_branch(daemon, id, branch_id, body)
        }
        (
            "POST",
            [
                "api",
                "guardians",
                id,
                "branches",
                branch_id,
                "open-terminal",
            ],
        ) => open_guardian_branch_terminal(daemon, id, branch_id, query),
        ("GET", ["api", "guardians", id, "branches", branch_id, "pane"]) => {
            guardian_branch_pane(daemon, id, branch_id, query)
        }
        ("POST", ["api", "guardians", id, "merge"]) => guardian_merge(daemon, id),
        ("POST", ["api", "guardians", id, "cancel_and_merge"]) => {
            guardian_cancel_and_merge(daemon, id)
        }
        ("POST", ["api", "guardians", id, "approve"]) => guardian_approve(daemon, id),
        ("POST", ["api", "guardians", id, "cancel"]) => guardian_cancel(daemon, id),
        ("POST", ["api", "guardians", id, "run-manual-commands"]) => {
            guardian_run_manual_commands(daemon, id, body)
        }
        ("POST", ["api", "guardians", id, "run-action-hint"]) => {
            guardian_run_action_hint(daemon, id, body)
        }
        ("POST", ["api", "guardians", id, "pull-requests"]) => {
            guardian_submit_prs(daemon, id, body)
        }
        ("GET", ["api", "guardians", id, "pull-requests"]) => guardian_list_prs(daemon, id),
        ("GET", ["api", "pull-requests"]) => pr_find(daemon, query),
        ("GET", ["api", "pull-requests", pr_id]) => pr_get(daemon, pr_id),
        ("POST", ["api", "pull-requests", pr_id]) => pr_update(daemon, pr_id, body),
        ("GET", ["api", "pull-requests", pr_id, "comments"]) => pr_comments(daemon, pr_id),
        ("POST", ["api", "pull-requests", pr_id, "action-feedback"]) => {
            pr_action_feedback(daemon, pr_id)
        }
        _ => error(
            404,
            "not_found",
            &format!("no route for {method} {path}"),
            vec![],
        ),
    }
}

/// Like [`route`], but wraps the call in an OpenTelemetry HTTP server span
/// (RAL-96) built from the request's incoming W3C `traceparent` header (`None`
/// when absent — a fresh trace is started rather than treated as an error).
///
/// `route` itself is left untouched (and its ~100 existing unit tests with
/// it): this wrapper only adds tracing at the outer HTTP boundary. A `POST
/// /api/runs` that creates a run persists this request's trace context onto
/// the new run (via [`Store::set_run_trace_context`]) so the scheduler's
/// later, asynchronous execution of that run continues the *same* trace
/// instead of starting a disconnected one.
pub fn route_with_trace(
    daemon: &Daemon,
    method: &str,
    path: &str,
    body: &str,
    traceparent: Option<&str>,
) -> Reply {
    let cx = crate::otel::context_from_traceparent(traceparent);
    let span = crate::otel::start_span("daemon.http", &cx, SpanKind::Server);
    span.set_attribute("http.method", method.to_string());
    span.set_attribute("http.target", path.to_string());

    let reply = route(daemon, method, path, body);

    span.set_attribute("http.status_code", i64::from(reply.status));
    if reply.status >= 400 {
        span.set_status(Status::error(format!("http {}", reply.status)));
    } else {
        span.set_status(Status::Ok);
    }

    let path_only = path.split('?').next().unwrap_or(path);
    if method == "POST" && path_only.trim_matches('/') == "api/runs" && reply.status == 201 {
        if let (Some(run_id), Some(tp)) = (
            extract_run_id(&reply.body),
            crate::otel::traceparent_from_context(&span.cx),
        ) {
            let _ = daemon.lock().set_run_trace_context(&run_id, &tp);
        }
    }

    reply
}

/// Pull `"run_id"` out of a [`SubmitResponse`] JSON body without a full typed
/// deserialize — the caller only wants the one field.
fn extract_run_id(body: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()?
        .get("run_id")?
        .as_str()
        .map(str::to_string)
}

fn health(daemon: &Daemon) -> Reply {
    let db = if daemon.lock().running_count().is_ok() {
        "ok"
    } else {
        "error"
    };
    let mut warnings = Vec::new();
    if !crate::logging::logging_to_file() {
        warnings.push("no log_path configured — daemon logs go to stderr only");
    }
    json(
        200,
        &DaemonHealth {
            name: "ralphus-daemon",
            version: ralphus_core::version(),
            status: "ok",
            db,
            warnings,
        },
    )
}

/// Filter and sort a run list for `GET /api/tasks` (CLI_PARITY_PLAN.local.md Q1:
/// server-side so the board and CLI share one filter/sort implementation).
///
/// - `status`: comma-separated, case-insensitive match against `RunView::state`.
/// - `name`: case-insensitive substring match against `RunView::label` (a run
///   with no label never matches a non-empty filter).
/// - `sort`: `"name"` sorts by label (falling back to id) case-insensitively,
///   ascending; anything else (including absent) keeps `list_runs`'s existing
///   newest-first order.
fn filter_and_sort_runs(
    mut runs: Vec<crate::store::RunView>,
    status: Option<&str>,
    name: Option<&str>,
    sort: Option<&str>,
) -> Vec<crate::store::RunView> {
    if let Some(status) = status {
        let wanted: Vec<String> = status
            .split(',')
            .map(|s| s.trim().to_lowercase())
            .filter(|s| !s.is_empty())
            .collect();
        if !wanted.is_empty() {
            runs.retain(|r| wanted.iter().any(|w| w == &r.state.to_lowercase()));
        }
    }
    if let Some(name) = name {
        let needle = name.to_lowercase();
        runs.retain(|r| {
            r.label
                .as_deref()
                .unwrap_or("")
                .to_lowercase()
                .contains(&needle)
        });
    }
    if sort == Some("name") {
        runs.sort_by(|a, b| {
            let ak = a.label.as_deref().unwrap_or(&a.id).to_lowercase();
            let bk = b.label.as_deref().unwrap_or(&b.id).to_lowercase();
            ak.cmp(&bk)
        });
    }
    runs
}

/// `?status=queued,running&name=foo&sort=name` — see [`filter_and_sort_runs`].
fn board(daemon: &Daemon, query: &str) -> Reply {
    let store = daemon.lock();
    // Ground truth is the shared concurrency semaphore, not a DB row count:
    // a permit is held for a session/verify/review-merge's entire time in
    // flight, which outlasts the windows where any single row actually reads
    // `running` (see `Semaphore::in_use`) — counting DB rows undercounts.
    let running = daemon.sem.in_use(daemon.max_concurrent);
    let running_reviews: Vec<RunningReviewItem> = store
        .merging_guardians()
        .unwrap_or_default()
        .into_iter()
        .map(|(id, name)| RunningReviewItem { id, name })
        .collect();
    match store.list_runs() {
        Ok(runs) => {
            let status = query_filter(query, "status");
            let name = query_filter(query, "name");
            let sort = query_filter(query, "sort");
            let runs =
                filter_and_sort_runs(runs, status.as_deref(), name.as_deref(), sort.as_deref());
            json(
                200,
                &Board {
                    daemon: DaemonStatus {
                        running,
                        max_concurrent: daemon.max_concurrent,
                        running_reviews,
                        downtime_active: crate::config::scheduler_in_downtime(),
                    },
                    runs,
                },
            )
        }
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

/// For every task with at least one placeholder-`cwd` session, check that its
/// `project` field resolves against the project registry (RAL-100). Returns
/// the first failure's message; core's structural `validate_toml` already
/// guarantees `project` is set whenever a placeholder is used, so this only
/// needs to check registry membership (the one piece core can't see).
fn validate_projects_registered(
    store: &Store,
    file: &ralphus_core::schema::TaskFile,
) -> std::result::Result<(), String> {
    for task in &file.task {
        let needs_project = task.session.iter().any(|s| {
            s.cwd
                .as_deref()
                .and_then(ralphus_core::schema::parse_worktree_placeholder)
                .is_some()
        });
        if !needs_project {
            continue;
        }
        let Some(project_name) = task.project.as_deref() else {
            return Err(format!(
                "task \"{}\" uses a placeholder cwd but has no 'project' set",
                task.name
            ));
        };
        let resolved = store
            .resolve_project(project_name)
            .map_err(|e| e.to_string())?;
        if resolved.is_none() {
            return Err(format!(
                "task \"{}\" references unregistered project \"{project_name}\" -- register it \
                 first with `ralphus project git --path <dir> --name {project_name} \
                 --description <desc>`",
                task.name
            ));
        }
    }
    Ok(())
}

/// Validate a project's on-disk path/vcs kind without persisting anything.
/// Shared by `register_project` (validate-then-write) and the read-only
/// `GET /api/projects/{name}/validate` endpoint (validate-only, RAL-101) so
/// the two can never drift on what counts as a valid project location.
fn validate_project_location(path: &str, vcs: &str) -> Result<(), String> {
    if vcs != "git" {
        return Err(format!(
            "unsupported vcs kind \"{vcs}\" (only \"git\" is implemented)"
        ));
    }
    let dir = Path::new(path);
    if !dir.is_dir() {
        return Err(format!(
            "path \"{path}\" does not exist or is not a directory"
        ));
    }
    let is_repo = std::process::Command::new("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .current_dir(dir)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !is_repo {
        return Err(format!("path \"{path}\" is not a git repository"));
    }
    Ok(())
}

fn register_project(daemon: &Daemon, body: &str) -> Reply {
    let Ok(req) = serde_json::from_str::<RegisterProjectBody>(body) else {
        return error(
            400,
            "bad_request",
            "body must include \"name\" and \"path\" strings",
            vec![],
        );
    };
    if req.name.trim().is_empty() {
        return error(400, "invalid_value", "'name' must not be empty", vec![]);
    }
    if let Err(msg) = validate_project_location(&req.path, &req.vcs) {
        return error(400, "invalid_value", &msg, vec![]);
    }
    match daemon
        .lock()
        .register_project(&req.name, &req.description, &req.path, &req.vcs)
    {
        Ok(()) => json(201, &serde_json::json!({"name": req.name})),
        Err(e) => store_error(&e),
    }
}

fn list_projects(daemon: &Daemon) -> Reply {
    match daemon.lock().list_projects() {
        Ok(projects) => json(200, &ProjectsResponse { projects }),
        Err(e) => store_error(&e),
    }
}

/// A single registered project by its exact name (RAL-100).
fn get_project(daemon: &Daemon, name: &str) -> Reply {
    match daemon.lock().get_project(name) {
        Ok(Some(p)) => json(200, &p),
        Ok(None) => error(
            404,
            "not_found",
            &format!("project \"{name}\" is not registered"),
            vec![],
        ),
        Err(e) => store_error(&e),
    }
}

/// Re-check a registered project's on-disk path/vcs kind without writing
/// anything (RAL-101). Used by the Projects tab to flag rows whose git
/// repository has since moved, been deleted, or stopped being a repo.
fn validate_project(daemon: &Daemon, name: &str) -> Reply {
    match daemon.lock().get_project(name) {
        Ok(Some(p)) => match validate_project_location(&p.path, &p.vcs) {
            Ok(()) => json(
                200,
                &ProjectValidateResponse {
                    valid: true,
                    message: None,
                },
            ),
            Err(msg) => json(
                200,
                &ProjectValidateResponse {
                    valid: false,
                    message: Some(msg),
                },
            ),
        },
        Ok(None) => error(
            404,
            "not_found",
            &format!("project \"{name}\" is not registered"),
            vec![],
        ),
        Err(e) => store_error(&e),
    }
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

    // A task with a placeholder cwd (`ralphus:new-worktree/<branch>`) names a
    // project in its `project` field; `validate_toml` above already required
    // that field to be set (core has no DB access), so this preflight only
    // needs the registry lookup itself (RAL-100).
    if let Err(msg) = validate_projects_registered(&daemon.lock(), &file) {
        return error(400, "project_validation_failed", &msg, vec![]);
    }

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
    /// The read-only "upstream" to show for this session's git worktree —
    /// either the branch of a chained dependency (`upstream = "<<task:...>>"`,
    /// RAL-50) or the worktree's own git tracking branch (typically a
    /// non-worktree base branch, e.g. `main`). `null` when the cwd isn't a
    /// git worktree or no upstream can be resolved. See
    /// `crate::reviews::session_upstream_display`.
    upstream: Option<String>,
}

/// For each session in a run, its worktree (`cwd`), the derived project root
/// (the shared git dir), and its display upstream, so the detail pane can show
/// them as distinct read-only fields (CCTL-148; upstream row added later). The
/// project/upstream are `null` when the cwd is not a git worktree. Computed on
/// demand (runs git per session) rather than on the hot board path.
fn run_worktrees(daemon: &Daemon, id: &str) -> Reply {
    let run = match daemon.lock().get_run(id) {
        Ok(r) => r,
        Err(e) => return store_error(&e),
    };
    let rows = match daemon.lock().sessions_of(id) {
        Ok(r) => r,
        Err(e) => return store_error(&e),
    };
    let mut paths = Vec::new();
    for (ti, task) in run.tasks.iter().enumerate() {
        for (si, s) in task.sessions.iter().enumerate() {
            let project = s.cwd.as_deref().and_then(crate::reviews::project_root_of);
            let upstream = crate::reviews::session_upstream_display(
                s.cwd.as_deref(),
                &rows,
                i64::try_from(ti).unwrap_or(0),
                i64::try_from(si).unwrap_or(0),
            );
            paths.push(SessionPaths {
                task_idx: ti,
                session_idx: si,
                worktree: s.cwd.clone(),
                project,
                upstream,
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

/// A run's internal session dependency graph (CLI_PARITY_PLAN.local.md
/// Phase 6). 404 if the run does not exist; 500 on a dependency cycle (should
/// not happen for an already-submitted run -- submission itself rejects
/// cycles -- but `plan::graph` is fallible so this stays honest).
fn run_graph(daemon: &Daemon, id: &str) -> Reply {
    let store = daemon.lock();
    if let Err(e) = store.run_state(id) {
        return store_error(&e);
    }
    let sessions = match store.sessions_of(id) {
        Ok(s) => s,
        Err(e) => return store_error(&e),
    };
    let tasks = match store.tasks_of(id) {
        Ok(t) => t,
        Err(e) => return store_error(&e),
    };
    match crate::plan::graph(&sessions, &tasks) {
        Ok(g) => json(200, &g),
        Err(e) => error(500, "cycle", &e, vec![]),
    }
}

/// The cross-run `[[default]] depends_on` gating graph (CLI_PARITY_PLAN.local.md
/// Phase 6, `ralphus graph --global`). `?all=1` includes terminal
/// (done/failed/cancelled) runs; otherwise only active (queued/pending/running)
/// runs are included, per plan Q5.
fn global_graph(daemon: &Daemon, query: &str) -> Reply {
    let include_terminal = query_param(query, "all").is_some_and(|v| v == "1" || v == "true");
    match daemon.lock().global_graph(include_terminal) {
        Ok(g) => json(200, &g),
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

/// The global Cartographer log (RAL-98): a filtered, paginated, sorted view
/// over every structured event in the system. The same endpoint serves both
/// "show me everything" (no filters) and "show me this one run/session/
/// guardian's history" (`run_id`/`session_id`/`guardian_id` filters) — the
/// per-run/per-guardian Logs sub-tab in the board is just this view with a
/// filter applied, not a separate implementation.
///
/// Query params: `source`, `scope`, `level`, `run_id`, `guardian_id`,
/// `session_id`, `q` (substring match on message), `since_ms`, `until_ms`,
/// `limit` (default 100, max 1000), `offset`, `sort` (`asc`/`desc`, default `desc`).
fn cartographer_query(daemon: &Daemon, query: &str) -> Reply {
    let limit = query_param(query, "limit")
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(100);
    let offset = query_param(query, "offset")
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(0);
    let filter = crate::cartographer::CartographerFilter {
        source: query_filter(query, "source"),
        scope: query_filter(query, "scope"),
        level: query_filter(query, "level"),
        run_id: query_filter(query, "run_id"),
        guardian_id: query_filter(query, "guardian_id"),
        session_id: query_filter(query, "session_id"),
        q: query_filter(query, "q"),
        since_ms: query_param(query, "since_ms").and_then(|s| s.parse::<i64>().ok()),
        until_ms: query_param(query, "until_ms").and_then(|s| s.parse::<i64>().ok()),
        limit,
        offset,
        ascending: query_param(query, "sort") == Some("asc"),
    };
    match daemon.lock().cartographer_query(&filter) {
        Ok(page) => json(200, &page),
        Err(e) => store_error(&e),
    }
}

/// Fetch one Cartographer row's full detail (used when the table's payload
/// column is truncated and the UI needs the whole JSON body).
fn cartographer_get(daemon: &Daemon, id: &str) -> Reply {
    let Ok(row_id) = id.parse::<i64>() else {
        return error(400, "bad_request", "id must be an integer", vec![]);
    };
    match daemon.lock().cartographer_get(row_id) {
        Ok(Some(row)) => json(200, &row),
        Ok(None) => error(404, "not_found", "no such cartographer event", vec![]),
        Err(e) => store_error(&e),
    }
}

/// RAL-136: fetch one ghost (a session's or review's handoff note) by its
/// owner URI. Lets any consumer explicitly query a ghost beyond the
/// automatic one-level-up lookup the scheduler already does at session start.
fn ghost_get(daemon: &Daemon, owner_uri: &str) -> Reply {
    match daemon.lock().get_ghost(owner_uri) {
        Ok(Some(g)) => json(200, &g),
        Ok(None) => error(404, "not_found", "no such ghost", vec![]),
        Err(e) => store_error(&e),
    }
}

#[derive(Deserialize)]
struct GhostCopyBody {
    source_uri: String,
    target_uri: String,
}

/// RAL-136: explicitly copy `source_uri`'s ghost onto `target_uri`,
/// independent of the dependency graph. `target_uri`'s `session:`/`review:`
/// prefix determines the copy's `kind`/`run_id`/`guardian_id`
/// ([`crate::ghost::parse_owner_uri`]) so the caller only needs to name the
/// two URIs, not every column of the target row.
fn ghost_copy(daemon: &Daemon, body: &str) -> Reply {
    let Ok(req) = serde_json::from_str::<GhostCopyBody>(body) else {
        return error(
            400,
            "bad_request",
            "body must be {source_uri, target_uri}",
            vec![],
        );
    };
    let Some((kind, run_id, guardian_id)) = crate::ghost::parse_owner_uri(&req.target_uri) else {
        return error(
            400,
            "bad_request",
            "target_uri must be a session:... or review:... URI",
            vec![],
        );
    };
    match daemon
        .lock()
        .copy_ghost(&req.source_uri, &req.target_uri, kind, run_id, guardian_id)
    {
        Ok(g) => json(200, &g),
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

/// Edit a run's label, a task's name/project, or a session's fields.
///
/// A `run` edit (the label only) is purely cosmetic -- it isn't tied to any
/// node in the dependency graph or to the content executed, so it does not
/// touch execution state at all. A `task` edit resets the *whole* run back
/// to Pending: its fields aren't tied to one node in the dependency graph
/// either, but (unlike the label) they can affect what actually runs, so
/// there's no narrower scope to preserve. A `session` edit instead reuses
/// [`Store::restart_session`]'s downstream-only reset -- only the edited
/// session and whatever is downstream of it in the plan graph goes back to
/// Pending; upstream/sibling sessions that already finished stay Done.
/// (Previously every edit kind, including `session`, called the whole-run
/// reset -- so editing one session's prompt silently re-ran every
/// already-`done` upstream task in the run too. See
/// [`wait_for_worker_stop`]'s doc comment for why any in-flight worker this
/// touches must be cancelled *and waited out* first.)
fn edit_run(daemon: &Daemon, id: &str, body: &str) -> Reply {
    let Ok(req) = serde_json::from_str::<EditBody>(body) else {
        return error(400, "bad_request", "invalid edit body", vec![]);
    };
    match req.kind.as_str() {
        "run" => {
            let store = daemon.lock();
            if let Err(e) = store.edit_run_label(id, non_empty(req.label.as_ref())) {
                return store_error(&e);
            }
        }
        "task" => {
            let store = daemon.lock();
            let name = non_empty(req.name.as_ref()).unwrap_or("task");
            if let Err(e) =
                store.edit_task_fields(id, req.task_idx, name, non_empty(req.project.as_ref()))
            {
                return store_error(&e);
            }
            if let Err(e) = store.reset_run_to_pending(id) {
                return store_error(&e);
            }
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
            if let Err(e) =
                daemon
                    .lock()
                    .edit_session_fields(id, req.task_idx, req.session_idx, &edit)
            {
                return store_error(&e);
            }
            // Same double-dispatch guard the `restart_session` HTTP handler
            // uses: stop every worker this is about to reset *before*
            // resetting it, and never hold the daemon lock across the
            // (up to 5s) wait.
            if let Ok(impact) =
                daemon
                    .lock()
                    .compute_session_restart_impact(id, req.task_idx, req.session_idx)
            {
                for dep in &impact.dirtied_runs {
                    daemon.cancellations.cancel(&dep.id);
                }
                for dep in &impact.dirtied_runs {
                    wait_for_worker_stop(daemon, &dep.id);
                }
            }
            daemon.cancellations.cancel(id);
            wait_for_worker_stop(daemon, id);
            if let Err(e) = daemon
                .lock()
                .restart_session(id, req.task_idx, req.session_idx)
            {
                return store_error(&e);
            }
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
    match daemon.lock().get_run(id) {
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

/// Block until `run_id`'s in-flight worker (if any) has actually exited,
/// bounded so a wedged worker can never hang the HTTP thread forever.
///
/// `Cancellations::cancel` only flips a flag the worker polls at the top of
/// its dispatcher loop (~25ms) and its tmux capture-pane loop (`runner.rs`'s
/// `TMUX_POLL_INTERVAL`, 500ms) — cancelling alone does not make the worker
/// stop *before this function returns*. Without waiting here, a restart that
/// only calls `cancel()` and immediately resets the run to `Pending` still
/// leaves a window (up to ~500ms) where the scheduler's next tick claims the
/// run and spawns a *second* worker while the first is still mid-poll,
/// holding its own tmux session open — reproducing the exact double-dispatch
/// race `cancel()` was meant to close (two workers racing on the same
/// deterministic tmux/spec-file names; one's "kill stale session before
/// resuming" cleanup tears down the other's still-live session out from under
/// it, which then reports "runner produced no result file" since its python
/// process never got to finish). `Cancellations::remove` — called
/// unconditionally once `execute_run_inner` returns, cancelled or not — is
/// the reliable "actually stopped" signal this polls for.
fn wait_for_worker_stop(daemon: &Daemon, run_id: &str) {
    const TIMEOUT: Duration = Duration::from_secs(5);
    const POLL_INTERVAL: Duration = Duration::from_millis(50);
    let started = Instant::now();
    while daemon.cancellations.is_active(run_id) {
        if started.elapsed() >= TIMEOUT {
            break;
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// Restart a whole run and dirty every run that depends on it (RAL-19).
///
/// `Store::restart_run` forces the run back to `Pending` unconditionally, even
/// if a worker thread from a still-in-flight previous attempt hasn't exited
/// yet — that thread has no idea it's been restarted underneath it. See
/// [`wait_for_worker_stop`] for why cancelling alone isn't enough: this stops
/// the old worker *and* waits for it to actually exit before the restart's
/// fresh claim can begin.
fn restart_run(daemon: &Daemon, id: &str) -> Reply {
    // Stop every worker this restart is about to touch — the target run and
    // everything it will dirty — *before* any of them are reset to Pending,
    // so no old worker can still be mid-poll when the fresh claim lands.
    if let Ok(impact) = daemon.lock().compute_run_restart_impact(id) {
        for dep in &impact.dirtied_runs {
            daemon.cancellations.cancel(&dep.id);
        }
        for dep in &impact.dirtied_runs {
            wait_for_worker_stop(daemon, &dep.id);
        }
    }
    daemon.cancellations.cancel(id);
    wait_for_worker_stop(daemon, id);
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

#[derive(Deserialize)]
struct AddDependencyBody {
    target_id: String,
}

/// Add a cross-run dependency (RAL-105): `id` will not be scheduled until
/// `target_id` reaches a state that satisfies dependents (normally Done).
/// Appends to the same `[[default]] depends_on` list that `ralphus submit`
/// populates from TOML, so it is picked up by the existing whole-run gating
/// in `Store::list_ready` — no new scheduling path. Rejects a self-reference
/// or a reference that would create a cycle in the cross-run dependency
/// graph (409); an unknown `id`/`target_id` is 404.
fn add_dependency(daemon: &Daemon, id: &str, body: &str) -> Reply {
    let Ok(req) = serde_json::from_str::<AddDependencyBody>(body) else {
        return error(400, "bad_request", "invalid add-dependency body", vec![]);
    };
    if req.target_id.trim().is_empty() {
        return error(400, "bad_request", "target_id is required", vec![]);
    }
    let store = daemon.lock();
    if let Err(e) = store.add_run_dependency(id, &req.target_id) {
        return store_error(&e);
    }
    match store.get_run(id) {
        Ok(run) => json(200, &run),
        Err(e) => store_error(&e),
    }
}

/// Dry-run preview of [`restart_run`]: computes the same downstream-impact
/// set the real restart would dirty, without mutating anything (RAL-104).
fn restart_run_preview(daemon: &Daemon, id: &str) -> Reply {
    match daemon.lock().compute_run_restart_impact(id) {
        Ok(impact) => json(200, &impact),
        Err(e) => store_error(&e),
    }
}

/// Restart a single session (and its downstream), dirtying dependent runs.
///
/// See [`restart_run`]/[`wait_for_worker_stop`]'s doc comments for why the
/// owning run's worker (and every run this restart dirties) must be
/// cancelled *and waited out* first: `Store::restart_session` resets the run
/// to `Pending` too, and without stopping any still-in-flight worker first, a
/// restart mid-run causes a double-dispatch race on the deterministic
/// per-session tmux/spec-file names.
fn restart_session(daemon: &Daemon, id: &str, ti: &str, si: &str) -> Reply {
    let (Ok(task_idx), Ok(session_idx)) = (ti.parse::<i64>(), si.parse::<i64>()) else {
        return error(
            400,
            "bad_request",
            "task/session index must be integers",
            vec![],
        );
    };
    if let Ok(impact) = daemon
        .lock()
        .compute_session_restart_impact(id, task_idx, session_idx)
    {
        for dep in &impact.dirtied_runs {
            daemon.cancellations.cancel(&dep.id);
        }
        for dep in &impact.dirtied_runs {
            wait_for_worker_stop(daemon, &dep.id);
        }
    }
    daemon.cancellations.cancel(id);
    wait_for_worker_stop(daemon, id);
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

/// Dry-run preview of [`restart_session`]: computes the same downstream-impact
/// set the real restart would dirty, without mutating anything (RAL-104).
fn restart_session_preview(daemon: &Daemon, id: &str, ti: &str, si: &str) -> Reply {
    let (Ok(task_idx), Ok(session_idx)) = (ti.parse::<i64>(), si.parse::<i64>()) else {
        return error(
            400,
            "bad_request",
            "task/session index must be integers",
            vec![],
        );
    };
    match daemon
        .lock()
        .compute_session_restart_impact(id, task_idx, session_idx)
    {
        Ok(impact) => json(200, &impact),
        Err(e) => store_error(&e),
    }
}

/// Restart a single session's verify steps from `vi` onwards: resets
/// session-level verifies at index >= vi to Pending while leaving the session
/// body Done so the scheduler re-runs only the affected verify steps.
fn restart_session_verify(daemon: &Daemon, id: &str, ti: &str, si: &str, vi: &str) -> Reply {
    let (Ok(task_idx), Ok(session_idx), Ok(verify_from)) =
        (ti.parse::<i64>(), si.parse::<i64>(), vi.parse::<i64>())
    else {
        return error(
            400,
            "bad_request",
            "task/session/verify index must be integers",
            vec![],
        );
    };
    daemon.cancellations.cancel(id);
    wait_for_worker_stop(daemon, id);
    match daemon
        .lock()
        .restart_session_verify(id, task_idx, session_idx, verify_from)
    {
        Ok(dirtied) => {
            // No precomputed-impact endpoint exists for verify restarts, so
            // (unlike restart_run/restart_session above) dependents can only
            // be cancelled after the fact — a smaller, second-order version
            // of the same race remains for *their* worker, but the primary
            // run this handler targets is fully protected.
            for dep_id in &dirtied {
                daemon.cancellations.cancel(dep_id);
                wait_for_worker_stop(daemon, dep_id);
            }
            json(
                200,
                &RestartResponse {
                    state: "pending",
                    dirtied,
                },
            )
        }
        Err(e) => store_error(&e),
    }
}

/// Restart a task's verify steps from `vi` onwards: resets task-scope verifies
/// at index >= vi to Pending while leaving all sessions Done so the scheduler
/// re-runs only the affected task-level verifies.
fn restart_task_verify(daemon: &Daemon, id: &str, ti: &str, vi: &str) -> Reply {
    let (Ok(task_idx), Ok(verify_from)) = (ti.parse::<i64>(), vi.parse::<i64>()) else {
        return error(
            400,
            "bad_request",
            "task/verify index must be integers",
            vec![],
        );
    };
    daemon.cancellations.cancel(id);
    wait_for_worker_stop(daemon, id);
    match daemon.lock().restart_task_verify(id, task_idx, verify_from) {
        Ok(dirtied) => {
            for dep_id in &dirtied {
                daemon.cancellations.cancel(dep_id);
                wait_for_worker_stop(daemon, dep_id);
            }
            json(
                200,
                &RestartResponse {
                    state: "pending",
                    dirtied,
                },
            )
        }
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

/// Decode a `%XX`-escaped, `+`-for-space query parameter value (minimal
/// `application/x-www-form-urlencoded` decode — good enough for filter text,
/// no dependency on the `url` crate).
fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
                match hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                    Some(byte) => {
                        out.push(byte);
                        i += 3;
                    }
                    None => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A filter value from the query string, `%XX`-decoded and empty-string-as-absent.
fn query_filter(query: &str, key: &str) -> Option<String> {
    query_param(query, key)
        .map(url_decode)
        .filter(|s| !s.is_empty())
}

#[derive(Serialize)]
struct OpenTerminalResponse {
    ok: bool,
}

/// A pane's current content for the live "read-only terminal" peek view
/// (RAL-102). `active` is `false` once the underlying tmux session has ended
/// (the run/verify/resolver finished and the daemon tore it down, or it was
/// never started) — the board is expected to degrade gracefully in that case
/// rather than treat it as an error.
#[derive(Serialize)]
struct PaneResponse {
    active: bool,
    content: String,
}

/// Capture-pane content for the tmux session keyed by `(run_id, task,
/// session_id)` (see `crate::tmux::session_name`), or an inactive
/// [`PaneResponse`] if no such session currently exists. A tmux resolution
/// failure (no binary available at all) is the only case reported as a real
/// error, since that reflects a daemon configuration problem rather than
/// "this run finished".
fn capture_pane_reply(run_id: &str, task: &str, session_id: &str, query: &str) -> Reply {
    let lines: u32 = query_param(query, "lines")
        .and_then(|s| s.parse().ok())
        .unwrap_or(2000);
    let tmux = match crate::tmux::Tmux::resolve() {
        Ok(t) => t,
        Err(e) => return error(500, "tmux_error", &e.to_string(), vec![]),
    };
    let name = crate::tmux::session_name(run_id, task, session_id);
    if !tmux.has_session(&name) {
        return json(
            200,
            &PaneResponse {
                active: false,
                content: String::new(),
            },
        );
    }
    match tmux.capture_pane(&name, lines) {
        Ok(content) => json(
            200,
            &PaneResponse {
                active: true,
                content,
            },
        ),
        Err(_) => json(
            200,
            &PaneResponse {
                active: false,
                content: String::new(),
            },
        ),
    }
}

/// Open an interactive terminal attached to the tmux session keyed by
/// `(run_id, task, session_id)` — the "open terminal" button (RAL-102).
/// Requires the session to currently be live; a finished run/verify/resolver
/// has already had its tmux session torn down (see
/// `crate::runner::SubprocessRunner::run_via_tmux`), so this reports a clear
/// `409` rather than spawning a terminal attached to nothing.
fn attach_tmux_terminal(run_id: &str, task: &str, session_id: &str) -> Reply {
    let tmux = match crate::tmux::Tmux::resolve() {
        Ok(t) => t,
        Err(e) => return error(500, "tmux_error", &e.to_string(), vec![]),
    };
    let name = crate::tmux::session_name(run_id, task, session_id);
    if !tmux.has_session(&name) {
        return error(
            409,
            "no_tmux_session",
            "no live tmux session — it may not be running right now",
            vec![],
        );
    }
    let program = tmux.program().to_string();
    let mut args: Vec<String> = tmux.prefix_args().to_vec();
    args.extend(["attach-session".to_string(), "-t".to_string(), name]);
    match spawn_in_terminal(None, &program, &args) {
        Ok(()) => json(200, &OpenTerminalResponse { ok: true }),
        Err(msg) => error(500, "terminal_error", &msg, vec![]),
    }
}

/// Open a terminal for a task session — the "Open Terminal Log" / "Open
/// Agent" actions (RAL-102 follow-up).
///
/// `mode` (query param):
/// - `"open"` (default) — attach to the runner's tmux-wrapped session, which
///   shows its log/event stream (this is the original RAL-102 behavior,
///   itself a replacement for the old `claude --resume` spawn).
/// - `"agent"` — spawn a real, interactive `claude --resume <id>` session,
///   for actually continuing the conversation rather than watching its log.
fn open_terminal(daemon: &Daemon, id: &str, ti: &str, si: &str, query: &str) -> Reply {
    let (Ok(task_idx), Ok(session_idx)) = (ti.parse::<i64>(), si.parse::<i64>()) else {
        return error(
            400,
            "bad_request",
            "task/session index must be integers",
            vec![],
        );
    };
    if query_param(query, "mode") == Some("agent") {
        let (cwd, claude_session_id) =
            match daemon
                .lock()
                .get_session_agent_resume(id, task_idx, session_idx)
            {
                Ok(v) => v,
                Err(e) => return store_error(&e),
            };
        return open_agent_terminal(&cwd, claude_session_id.as_deref());
    }
    let session_id = match daemon.lock().get_session_id(id, task_idx, session_idx) {
        Ok(v) => v,
        Err(e) => return store_error(&e),
    };
    let task = match daemon.lock().get_task_name(id, task_idx) {
        Ok(v) => v,
        Err(e) => return store_error(&e),
    };
    attach_tmux_terminal(id, &task, &session_id)
}

/// The live pane content of a task session's tmux session, for the
/// auto-refreshing "read-only terminal" peek view (RAL-102).
fn session_pane(daemon: &Daemon, id: &str, ti: &str, si: &str, query: &str) -> Reply {
    let (Ok(task_idx), Ok(session_idx)) = (ti.parse::<i64>(), si.parse::<i64>()) else {
        return error(
            400,
            "bad_request",
            "task/session index must be integers",
            vec![],
        );
    };
    let session_id = match daemon.lock().get_session_id(id, task_idx, session_idx) {
        Ok(v) => v,
        Err(e) => return store_error(&e),
    };
    let task = match daemon.lock().get_task_name(id, task_idx) {
        Ok(v) => v,
        Err(e) => return store_error(&e),
    };
    capture_pane_reply(id, &task, &session_id, query)
}

/// The `(run_id, session_id)` pair a `prompt`-kind verify step's tmux session
/// is keyed by — mirrors exactly how `scheduler.rs::run_verifies` builds the
/// `RunnerSpec` for the same verify step (`run_id` is the owning run;
/// `session_id` is `verify-{scope}-{verify_idx}`, matching the verify step's
/// own position within its `(task_idx, scope, session_idx)` group — see the
/// doc comment on `Store::verify_specs`). The third leg of the key, `task`,
/// is fetched separately by each caller via `Store::get_task_name` since
/// `verify_idx` alone repeats across sibling tasks (RAL-102 collision bug —
/// see `crate::tmux::session_name`'s doc comment).
fn verify_tmux_keys(run_id: &str, scope: &str, verify_idx: &str) -> (String, String) {
    (run_id.to_string(), format!("verify-{scope}-{verify_idx}"))
}

/// Open a terminal for a `prompt`-kind verify step — the "Open Terminal Log"
/// / "Open Agent" actions (RAL-102 follow-up). `mode` query param semantics
/// mirror `open_terminal`'s doc comment.
#[allow(clippy::too_many_arguments)]
fn open_verify_terminal(
    daemon: &Daemon,
    id: &str,
    task_idx: &str,
    scope: &str,
    session_idx: &str,
    verify_idx: &str,
    query: &str,
) -> Reply {
    let (Ok(task_idx_n), Ok(session_idx_n), Ok(verify_idx_n)) = (
        task_idx.parse::<i64>(),
        session_idx.parse::<i64>(),
        verify_idx.parse::<i64>(),
    ) else {
        return error(
            400,
            "bad_request",
            "task/session/verify index must be integers",
            vec![],
        );
    };
    if let Err(e) = daemon
        .lock()
        .verify_specs(id, task_idx_n, scope, session_idx_n)
    {
        return store_error(&e);
    }
    if query_param(query, "mode") == Some("agent") {
        let claude_session_id = match daemon.lock().get_verify_claude_session_id(
            id,
            task_idx_n,
            scope,
            session_idx_n,
            verify_idx_n,
        ) {
            Ok(v) => v,
            Err(e) => return store_error(&e),
        };
        // A task-scope step's cwd is its task's first session's (see
        // `Store::get_task_first_session_cwd`'s doc comment); a session-scope
        // step's cwd is that exact session's.
        let cwd = if scope == "task" {
            daemon.lock().get_task_first_session_cwd(id, task_idx_n)
        } else {
            daemon
                .lock()
                .get_session_agent_resume(id, task_idx_n, session_idx_n)
                .map(|(cwd, _)| cwd)
        };
        let cwd = match cwd {
            Ok(v) => v,
            Err(e) => return store_error(&e),
        };
        return open_agent_terminal(&cwd, claude_session_id.as_deref());
    }
    let task = match daemon.lock().get_task_name(id, task_idx_n) {
        Ok(v) => v,
        Err(e) => return store_error(&e),
    };
    let (run_id, session_id) = verify_tmux_keys(id, scope, verify_idx);
    attach_tmux_terminal(&run_id, &task, &session_id)
}

/// The live pane content of a `prompt`-kind verify step's tmux session, for
/// the auto-refreshing "read-only terminal" peek view (RAL-102).
#[allow(clippy::too_many_arguments)]
fn verify_pane(
    daemon: &Daemon,
    id: &str,
    task_idx: &str,
    scope: &str,
    session_idx: &str,
    verify_idx: &str,
    query: &str,
) -> Reply {
    let (Ok(task_idx_n), Ok(session_idx_n), Ok(_verify_idx_n)) = (
        task_idx.parse::<i64>(),
        session_idx.parse::<i64>(),
        verify_idx.parse::<i64>(),
    ) else {
        return error(
            400,
            "bad_request",
            "task/session/verify index must be integers",
            vec![],
        );
    };
    if let Err(e) = daemon
        .lock()
        .verify_specs(id, task_idx_n, scope, session_idx_n)
    {
        return store_error(&e);
    }
    let task = match daemon.lock().get_task_name(id, task_idx_n) {
        Ok(v) => v,
        Err(e) => return store_error(&e),
    };
    let (run_id, session_id) = verify_tmux_keys(id, scope, verify_idx);
    capture_pane_reply(&run_id, &task, &session_id, query)
}

/// Open a terminal for a review branch.
///
/// `mode` (query param):
/// - `"open"` (default) — attach to the conflict-resolver's live tmux
///   session (RAL-102 — replaces the old `claude --resume` spawn; `409` if
///   the resolver isn't currently running)
/// - `"worktree"` — open a plain shell in the branch's review worktree
///   directory; available as soon as the worktree exists, even before the
///   first resolver pass
/// - `"agent"` — spawn a real, interactive `claude --resume <id>` session
///   resuming the conflict resolver's own conversation (RAL-102 follow-up)
fn open_guardian_branch_terminal(daemon: &Daemon, id: &str, branch_id: &str, query: &str) -> Reply {
    let mode = query_param(query, "mode").unwrap_or("open");
    if mode != "open" && mode != "worktree" && mode != "agent" {
        return error(
            400,
            "bad_request",
            "mode must be 'open', 'worktree', or 'agent'",
            vec![],
        );
    }

    // Worktree mode: open a plain shell in the branch's review worktree directory.
    // This is available as soon as the worktree exists, before any session ID.
    if mode == "worktree" {
        let worktree = match daemon.lock().get_branch_worktree(id, branch_id) {
            Ok(v) => v,
            Err(e) => return store_error(&e),
        };
        let Some(wt_path) = worktree else {
            return error(
                409,
                "no_worktree",
                "no worktree available for this branch yet",
                vec![],
            );
        };
        let safe_path = wt_path.replace('\'', "''");
        let shell_cmd = std::env::var("RALPHUS_SHELL_CMD").unwrap_or_else(|_| "pwsh".to_string());
        let shell_args = vec![
            "-NoExit".to_string(),
            "-NoProfile".to_string(),
            "-Command".to_string(),
            format!("Set-Location -LiteralPath '{safe_path}'"),
        ];
        return match spawn_in_terminal(None, &shell_cmd, &shell_args) {
            Ok(()) => json(200, &OpenTerminalResponse { ok: true }),
            Err(msg) => error(500, "terminal_error", &msg, vec![]),
        };
    }

    if mode == "agent" {
        let worktree = match daemon.lock().get_branch_worktree(id, branch_id) {
            Ok(v) => v,
            Err(e) => return store_error(&e),
        };
        let Some(wt_path) = worktree else {
            return error(
                409,
                "no_worktree",
                "no worktree available for this branch yet",
                vec![],
            );
        };
        let claude_session_id = match daemon
            .lock()
            .get_branch_resolver_claude_session_id(id, branch_id)
        {
            Ok(v) => v,
            Err(e) => return store_error(&e),
        };
        return open_agent_terminal(&wt_path, claude_session_id.as_deref());
    }

    // open: attach to the conflict-resolver's tmux session (RAL-102). Keyed the
    // same way `guardian_merge.rs::resolve_conflicts_with_agent` builds its
    // `RunnerSpec` for this exact branch position -- the tmux session name is
    // ephemeral internal naming (not persistent addressing), so it is still
    // resolved to the branch's current numeric position here.
    let position = match daemon.lock().guardian_branches(id) {
        Ok(branches) => match branches.iter().find(|b| b.id == branch_id) {
            Some(b) => b.position,
            None => return error(404, "not_found", "no such branch", vec![]),
        },
        Err(e) => return store_error(&e),
    };
    attach_tmux_terminal(
        &format!("guardian-{id}"),
        crate::guardian_merge::RESOLVER_TASK,
        &format!("resolver-{position}"),
    )
}

/// Spawn a real, interactive `claude --resume <id> --dangerously-skip-permissions`
/// session in a new terminal window, rooted at `cwd` — the "Open Agent"
/// terminal action (RAL-102 follow-up). Unlike `attach_tmux_terminal` (which
/// re-attaches to the runner's own tmux-wrapped subprocess and only shows its
/// log/event stream — see `claude_code_backend.py`'s `stream-json` parsing),
/// this launches the actual Claude Code CLI so a human can keep the
/// conversation going interactively, exactly as the pre-RAL-102 "resume" flow
/// did. `claude_session_id` is only ever populated for a session that
/// actually ran under the `claude-code` agent (see `Store::get_session_agent_resume`'s
/// doc comment), so this is always a claude-code resume -- `--dangerously-skip-permissions`
/// is applied unconditionally, matching the headless run's own invocation
/// (`claude_code_backend.py`), so the resumed conversation doesn't immediately
/// stall on a permission prompt the human has to notice and click through.
/// The PowerShell `-Command` string that resumes `session_id` via `program`
/// (single-quoted/escaped for embedding) -- split out from `open_agent_terminal`
/// so the exact command line is unit-testable without actually spawning a
/// terminal.
fn resume_agent_command(program: &str, session_id: &str) -> String {
    let safe_program = program.replace('\'', "''");
    let safe_session = session_id.replace('\'', "''");
    format!("& '{safe_program}' --resume '{safe_session}' --dangerously-skip-permissions")
}

fn open_agent_terminal(cwd: &str, claude_session_id: Option<&str>) -> Reply {
    let Some(session_id) = claude_session_id else {
        return error(
            409,
            "no_claude_session",
            "no Claude Code session id recorded yet — it may not have started, \
             or didn't run under the claude-code agent",
            vec![],
        );
    };
    // Mirrors `RALPHUS_CLAUDE_COMMAND` in `claude_code_backend.py` — the same
    // override point resolves both the headless run and this resumed one to
    // the same binary.
    let program = std::env::var("RALPHUS_CLAUDE_COMMAND").unwrap_or_else(|_| "claude".to_string());
    let shell_cmd = std::env::var("RALPHUS_SHELL_CMD").unwrap_or_else(|_| "pwsh".to_string());
    // No leading `Set-Location ...;` here -- `cwd` is passed as its own
    // argument below so it goes through wt's `-d` flag / `current_dir`
    // instead of a semicolon `wt.exe` can't pass through to `claude` (see
    // `spawn_in_terminal`'s doc comment).
    let shell_args = vec![
        "-NoExit".to_string(),
        "-NoProfile".to_string(),
        "-Command".to_string(),
        resume_agent_command(&program, session_id),
    ];
    match spawn_in_terminal(Some(cwd), &shell_cmd, &shell_args) {
        Ok(()) => json(200, &OpenTerminalResponse { ok: true }),
        Err(msg) => error(500, "terminal_error", &msg, vec![]),
    }
}

/// The live pane content of a review branch's conflict-resolver tmux
/// session, for the auto-refreshing "read-only terminal" peek view in the
/// Review tab (RAL-102).
fn guardian_branch_pane(daemon: &Daemon, id: &str, branch_id: &str, query: &str) -> Reply {
    if daemon.lock().get_guardian(id).is_err() {
        return error(404, "not_found", "no such guardian", vec![]);
    }
    // The tmux session name is ephemeral internal naming keyed on the branch's
    // current numeric position (see `open_guardian_branch_terminal`), so resolve
    // it from the addressed branch_id.
    let position = match daemon.lock().guardian_branches(id) {
        Ok(branches) => match branches.iter().find(|b| b.id == branch_id) {
            Some(b) => b.position,
            None => return error(404, "not_found", "no such branch", vec![]),
        },
        Err(e) => return store_error(&e),
    };
    capture_pane_reply(
        &format!("guardian-{id}"),
        crate::guardian_merge::RESOLVER_TASK,
        &format!("resolver-{position}"),
        query,
    )
}

/// Launch the given program+args in a new interactive terminal window,
/// optionally starting in `cwd`.
///
/// On Windows: tries `wt.exe` (Windows Terminal) first; falls back to opening
/// a new PowerShell console window via `CREATE_NEW_CONSOLE`. On other platforms
/// (not yet supported) returns an error.
///
/// `cwd` is a separate parameter rather than something callers bake into
/// `args` (e.g. a leading `Set-Location ...;`) because `wt.exe`'s own
/// command-line parser treats a bare `;` as a hard subcommand separator (its
/// new-tab/split-pane syntax) with **no way to escape it through to the
/// launched program** -- confirmed against Microsoft's own docs: every
/// documented "escaping" trick there (backtick, `--%`) is only about getting
/// a literal `;` past *PowerShell's* parsing and into `wt`'s argv, at which
/// point `wt` treats it as a genuine command boundary regardless, splitting
/// the command and launching the tail as its own program name. `spawn()`
/// still succeeds either way (wt.exe itself started fine), so the failure
/// only surfaced later as a "cannot find file" error printed inside the new
/// wt tab. Passing `cwd` here lets it go through wt's own `-d`/
/// `--startingDirectory` flag (and `Command::current_dir` for the
/// `CREATE_NEW_CONSOLE` fallback) instead, so no semicolon is ever needed.
#[cfg(target_os = "windows")]
fn spawn_in_terminal(
    cwd: Option<&str>,
    program: &str,
    args: &[String],
) -> std::result::Result<(), String> {
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
    let mut wt_cmd = Command::new("wt");
    if let Some(dir) = cwd {
        wt_cmd.args(["-d", dir]);
    }
    if wt_cmd.arg("--").args(&all).spawn().is_ok() {
        return Ok(());
    }

    // Fallback: open a new console window running PowerShell with the command.
    // Single-quote each arg (doubling embedded single quotes) for PS safety.
    let ps_parts: Vec<String> = all
        .iter()
        .map(|a| format!("'{}'", a.replace('\'', "''")))
        .collect();
    let ps_cmd = format!("& {}", ps_parts.join(" "));
    let mut console_cmd = Command::new("powershell");
    if let Some(dir) = cwd {
        console_cmd.current_dir(dir);
    }
    console_cmd
        .creation_flags(CREATE_NEW_CONSOLE)
        .args(["-NoExit", "-NoProfile", "-Command", &ps_cmd])
        .spawn()
        .map_err(|e| format!("could not open terminal: {e}"))?;
    Ok(())
}

#[cfg(not(target_os = "windows"))]
fn spawn_in_terminal(
    _cwd: Option<&str>,
    _program: &str,
    _args: &[String],
) -> std::result::Result<(), String> {
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

/// Directly, synchronously kill every live tmux session belonging to
/// `run_id`, as a robust backstop alongside `Cancellations::cancel`.
///
/// `Cancellations::cancel` only trips a flag a worker's polling loop checks
/// on its own schedule (up to `TMUX_POLL_INTERVAL`, currently 500ms) — and
/// is a silent no-op if that run's worker thread isn't currently alive at
/// all (already finished, or never started). Either way, a tmux pane that's
/// actually still running would be left orphaned, alive, and unresponsive to
/// "cancelled" until something else notices. This call doesn't wait on any
/// worker at all: it asks psmux directly what's running under this run's
/// deterministic `ralphus_<run_id>_...` session-name prefix and kills it
/// immediately, regardless of whether the token mechanism is working.
fn kill_run_tmux_sessions(run_id: &str) {
    let Ok(tmux) = crate::tmux::Tmux::resolve() else {
        return;
    };
    tmux.kill_sessions_with_prefix(&format!("ralphus_{run_id}_"));
}

#[derive(Serialize)]
struct CancelResponse {
    state: &'static str,
    cancelled: Vec<String>,
}

/// Cancel a run and cascade the cancellation to every run transitively
/// dependent on it (RAL-116). Always available and idempotent regardless of
/// the run's current state — even an already-terminal run is (re-)cancelled,
/// so it can never be picked up again by another trigger (a restart,
/// cross-run gating, etc).
fn cancel(daemon: &Daemon, id: &str) -> Reply {
    match daemon.lock().cancel_run(id, false) {
        Ok(impact) => {
            // The store has flipped every affected run (and their non-terminal
            // nodes) to cancelled; now stop each one's worker thread and kill
            // its subprocess, if it has one in flight.
            for r in &impact.runs {
                daemon.cancellations.cancel(&r.id);
                kill_run_tmux_sessions(&r.id);
            }
            json(
                200,
                &CancelResponse {
                    state: "cancelled",
                    cancelled: impact.runs.into_iter().map(|r| r.id).collect(),
                },
            )
        }
        Err(e) => store_error(&e),
    }
}

/// Dry-run preview of [`cancel`]: computes the same cascade-cancel impact set
/// the real cancel would affect, without mutating anything (RAL-116).
fn cancel_run_preview(daemon: &Daemon, id: &str) -> Reply {
    match daemon.lock().cancel_run(id, true) {
        Ok(impact) => json(200, &impact),
        Err(e) => store_error(&e),
    }
}

#[derive(Deserialize)]
struct SetStatusBody {
    kind: String,
    #[serde(default)]
    task_idx: i64,
    #[serde(default)]
    session_idx: i64,
    #[serde(default)]
    verify_idx: i64,
    #[serde(default)]
    verify_scope: String,
    state: String,
}

/// Manually override the state of a run, task, session, or verify step (RAL-74).
///
/// Routes through the same store setters used by natural transitions so that
/// audit log entries are written and the scheduler can observe the new state on
/// its next tick (e.g. a run moved to Pending will be claimed and re-executed).
/// Targeting a run at `cancelled` is special-cased to go through the exact
/// same cascading cancel path as the "Cancel Run" button (RAL-116) — not a
/// separate DB-only flip — so both entry points stop in-flight agents and
/// cascade to dependents identically.
fn set_status(daemon: &Daemon, id: &str, body: &str) -> Reply {
    let Ok(req) = serde_json::from_str::<SetStatusBody>(body) else {
        return error(400, "bad_request", "invalid set-status body", vec![]);
    };
    let store = daemon.lock();

    // A manual override to "cancelled" of a single task/session/verify within
    // a run reuses the dedicated cancel cascade (`Store::cancel` + tripping
    // the run's `CancelToken`). There is no finer-grained cancel signal than
    // the run-wide token, so cancelling one node also stops the rest of the
    // run's in-flight work and kills its subprocess; otherwise the DB row
    // would read "cancelled" while the real agent — and the concurrency-slot
    // permit it holds — kept running indefinitely, and the board's `running`
    // count would never drop. Targeting the run itself is handled separately
    // below via `Store::cancel_run`, which also cascades to dependents.
    let wants_cancel = matches!(req.kind.as_str(), "task" | "session" | "verify")
        && NodeState::parse(&req.state) == Some(NodeState::Cancelled);
    if wants_cancel {
        if let Err(e) = store.cancel(id) {
            return store_error(&e);
        }
        daemon.cancellations.cancel(id);
        kill_run_tmux_sessions(id);
        return match store.get_run(id) {
            Ok(run) => json(200, &run),
            Err(e) => store_error(&e),
        };
    }

    let result = match req.kind.as_str() {
        "run" => {
            let Some(state) = RunState::parse(&req.state) else {
                return error(
                    400,
                    "bad_request",
                    &format!("unknown run state '{}'", req.state),
                    vec![],
                );
            };
            if state == RunState::Cancelled {
                store.cancel_run(id, false).map(|impact| {
                    for r in &impact.runs {
                        daemon.cancellations.cancel(&r.id);
                        kill_run_tmux_sessions(&r.id);
                    }
                })
            } else {
                store.set_run_state(id, state)
            }
        }
        "task" => {
            let Some(state) = NodeState::parse(&req.state) else {
                return error(
                    400,
                    "bad_request",
                    &format!("unknown node state '{}'", req.state),
                    vec![],
                );
            };
            store.set_task_state(id, req.task_idx, state)
        }
        "session" => {
            let Some(state) = NodeState::parse(&req.state) else {
                return error(
                    400,
                    "bad_request",
                    &format!("unknown node state '{}'", req.state),
                    vec![],
                );
            };
            store.set_session_state(id, req.task_idx, req.session_idx, state)
        }
        "verify" => {
            let Some(state) = NodeState::parse(&req.state) else {
                return error(
                    400,
                    "bad_request",
                    &format!("unknown node state '{}'", req.state),
                    vec![],
                );
            };
            store.set_verify_state(
                id,
                req.task_idx,
                &req.verify_scope,
                req.session_idx,
                req.verify_idx,
                state,
            )
        }
        other => {
            return error(
                400,
                "bad_request",
                &format!("unknown kind '{other}'"),
                vec![],
            );
        }
    };
    if let Err(e) = result {
        return store_error(&e);
    }
    match store.get_run(id) {
        Ok(run) => json(200, &run),
        Err(e) => store_error(&e),
    }
}

// ── Queue (RAL Queue) ────────────────────────────────────────────────────────

#[derive(Serialize)]
struct QueueResponse {
    items: Vec<crate::store::QueueItem>,
}

#[derive(Serialize)]
struct QueueOrderResponse {
    order: Vec<String>,
    items: Vec<crate::store::QueueItem>,
}

#[derive(Deserialize)]
struct QueueReorderBody {
    order: Vec<String>,
}

#[derive(Deserialize)]
struct QueueSetPositionBody {
    items: Vec<String>,
    position: i64,
    #[serde(default)]
    absolute: bool,
}

/// The classified, ordered list of runnable work across all schedulable runs.
fn queue(daemon: &Daemon) -> Reply {
    match daemon.lock().queue() {
        Ok(items) => json(200, &QueueResponse { items }),
        Err(e) => store_error(&e),
    }
}

/// Persist a new queue order (server-side DAG-stabilized), returning the
/// repaired order plus the refreshed queue.
fn queue_reorder(daemon: &Daemon, body: &str) -> Reply {
    let Ok(req) = serde_json::from_str::<QueueReorderBody>(body) else {
        return error(400, "bad_request", "body must be {order:[paths]}", vec![]);
    };
    let store = daemon.lock();
    let order = match store.reorder_queue(&req.order) {
        Ok(o) => o,
        Err(e) => return store_error(&e),
    };
    match store.queue() {
        Ok(items) => json(200, &QueueOrderResponse { order, items }),
        Err(e) => store_error(&e),
    }
}

/// Move a selection of queue items to an absolute or relative position.
fn queue_set_position(daemon: &Daemon, body: &str) -> Reply {
    let Ok(req) = serde_json::from_str::<QueueSetPositionBody>(body) else {
        return error(
            400,
            "bad_request",
            "body must be {items:[paths], position, absolute?}",
            vec![],
        );
    };
    let store = daemon.lock();
    let order = match store.set_queue_position(&req.items, req.position, req.absolute) {
        Ok(o) => o,
        Err(e) => return store_error(&e),
    };
    match store.queue() {
        Ok(items) => json(200, &QueueOrderResponse { order, items }),
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
    skip_auto_build: bool,
    #[serde(default)]
    skip_worktree_checks: bool,
    #[serde(default)]
    skip_worktrees: bool,
    #[serde(default)]
    review_type: Option<String>,
}

#[derive(Deserialize)]
struct GuardianSettingsBody {
    #[serde(default)]
    skip_auto_build: Option<bool>,
    #[serde(default)]
    skip_worktree_checks: Option<bool>,
    #[serde(default)]
    skip_worktrees: Option<bool>,
    #[serde(default)]
    resolver_agent: Option<String>,
    #[serde(default)]
    resolver_model: Option<String>,
    #[serde(default)]
    base_branch: Option<String>,
    /// RAL-117: opt this review into automatically incorporating PR feedback
    /// comments instead of requiring the manual "Pull in PR feedback" action.
    #[serde(default)]
    auto_pr_feedback: Option<bool>,
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
        Ok(gs) => {
            // RAL-121: this list only ever needs to show summary DATA for the
            // one review the user actually opens (`guardian_get` promotes
            // that one to High priority); every other guardian still
            // `collecting` that's merely visible here gets its preliminary
            // git-log summary computed in the BACKGROUND at Low priority
            // instead -- this read itself never blocks on git subprocess
            // work either way.
            let queue = daemon.summary_queue_handle();
            for g in &gs {
                if g.status == "collecting" {
                    queue.enqueue(&g.id, crate::summary_worker::Priority::Low);
                }
            }
            json(200, &gs)
        }
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
            if req.skip_auto_build {
                let _ = store.set_guardian_skip_auto_build(&id, true);
            }
            if req.skip_worktree_checks {
                let _ = store.set_guardian_skip_worktree_checks(&id, true);
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
        Ok(g) => {
            // RAL-121: fetching a single guardian is the review page's "the
            // user is looking at this one now" signal -- promote its summary
            // job to High priority (or wake a cold one) rather than ever
            // computing it inline on this request thread.
            daemon.summary_queue_handle().promote(id);
            json(200, &g)
        }
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
        return error(
            400,
            "bad_request",
            "body must be {skip_auto_build?, skip_worktree_checks?}",
            vec![],
        );
    };
    let store = daemon.lock();
    if let Some(skip) = req.skip_auto_build {
        if let Err(e) = store.set_guardian_skip_auto_build(id, skip) {
            return store_error(&e);
        }
    }
    if let Some(skip) = req.skip_worktree_checks {
        if let Err(e) = store.set_guardian_skip_worktree_checks(id, skip) {
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
    if let Some(enabled) = req.auto_pr_feedback {
        if let Err(e) = store.set_guardian_auto_pr_feedback(id, enabled) {
            return store_error(&e);
        }
    }
    match store.get_guardian(id) {
        Ok(g) => json(200, &g),
        Err(e) => store_error(&e),
    }
}

// ── PR submission + feedback loop (RAL-117) ─────────────────────────────────

#[derive(Deserialize)]
struct SubmitPrsBody {
    prs: Vec<crate::pr::PrRequest>,
}

/// Submit one or more PRs/MRs for a guardian's stacked and/or combined
/// worktree(s). Kicks off in the background (git push + forge API calls);
/// poll `GET .../pull-requests` for the resulting rows.
fn guardian_submit_prs(daemon: &Daemon, id: &str, body: &str) -> Reply {
    let Ok(req) = serde_json::from_str::<SubmitPrsBody>(body) else {
        return error(400, "bad_request", "body must be {prs: [...]}", vec![]);
    };
    let runner: Arc<dyn Runner> =
        Arc::new(SubprocessRunner::from_env().with_cartographer(daemon.store_handle()));
    crate::pr::start_submit_pull_requests(daemon.store_handle(), runner, id, req.prs)
}

/// List every PR/MR submitted for a guardian, oldest first. Bare JSON array
/// (see the `GET /api/guardians` convention this mirrors).
fn guardian_list_prs(daemon: &Daemon, id: &str) -> Reply {
    match daemon.lock().list_pull_requests_for_guardian(id) {
        Ok(prs) => json(200, &prs),
        Err(e) => store_error(&e),
    }
}

/// Fetch one PR row by its ralphus-internal id (worktree → PR direction).
fn pr_get(daemon: &Daemon, pr_id: &str) -> Reply {
    match daemon.lock().get_pull_request(pr_id) {
        Ok(pr) => json(200, &pr),
        Err(e) => store_error(&e),
    }
}

/// Look up the ralphus PR row for a given forge PR/MR (PR → worktree
/// direction): `GET /api/pull-requests?forge=github&repo=acme%2Fwidget&pr_number=42`.
fn pr_find(daemon: &Daemon, query: &str) -> Reply {
    let (Some(forge), Some(repo), Some(num)) = (
        query_param(query, "forge"),
        query_param(query, "repo"),
        query_param(query, "pr_number").and_then(|s| s.parse::<i64>().ok()),
    ) else {
        return error(
            400,
            "bad_request",
            "query must include forge, repo, and pr_number",
            vec![],
        );
    };
    match daemon
        .lock()
        .find_pull_request_by_number(forge, &url_decode(repo), num)
    {
        Ok(Some(pr)) => json(200, &pr),
        Ok(None) => error(
            404,
            "not_found",
            "no PR recorded for that forge/repo/number",
            vec![],
        ),
        Err(e) => store_error(&e),
    }
}

#[derive(Deserialize)]
struct UpdatePrBody {
    #[serde(default)]
    pr_number: Option<i64>,
    #[serde(default)]
    pr_url: Option<String>,
    #[serde(default)]
    branch_alias: Option<String>,
    #[serde(default)]
    state: Option<String>,
}

/// Mutate the recorded PR mapping after the fact (RAL-117: PR numbers are not
/// permanently stable — a PR closed and reopened gets a new number). Only
/// fields present in the body are changed.
fn pr_update(daemon: &Daemon, pr_id: &str, body: &str) -> Reply {
    let Ok(req) = serde_json::from_str::<UpdatePrBody>(body) else {
        return error(
            400,
            "bad_request",
            "body must be {pr_number?, pr_url?, branch_alias?, state?}",
            vec![],
        );
    };
    let store = daemon.lock();
    let result = store.update_pull_request(
        pr_id,
        req.pr_number.map(Some),
        req.pr_url.as_deref().map(Some),
        req.branch_alias.as_deref(),
        req.state.as_deref(),
    );
    match result {
        Ok(()) => match store.get_pull_request(pr_id) {
            Ok(pr) => json(200, &pr),
            Err(e) => store_error(&e),
        },
        Err(e) => store_error(&e),
    }
}

#[derive(Serialize)]
struct PrCommentItem {
    external_id: String,
    author: String,
    body: String,
    created_at: String,
    /// Whether this comment has already been actioned into the worktree.
    actioned: bool,
}

/// Live-query the forge for this PR's comments (RAL-117: "an API must let PR
/// notes/comments be queried"), flagging which ones have already been
/// actioned into the worktree so the UI can highlight what's new.
fn pr_comments(daemon: &Daemon, pr_id: &str) -> Reply {
    let store = daemon.lock();
    let pr = match store.get_pull_request(pr_id) {
        Ok(pr) => pr,
        Err(e) => return store_error(&e),
    };
    let guardian = match store.get_guardian(&pr.guardian_id) {
        Ok(g) => g,
        Err(e) => return store_error(&e),
    };
    let Some(pr_number) = pr.pr_number else {
        return error(409, "no_pr_number", "PR has no recorded number yet", vec![]);
    };
    let root = Path::new(&guardian.git_root);
    let forge_cfg = crate::config::resolve_forge(root);
    let client = match crate::forge::resolve_remote(root, &forge_cfg) {
        Ok(c) => c,
        Err(e) => return error(502, "forge_error", &e, vec![]),
    };
    let comments = match client.list_pr_comments(pr_number) {
        Ok(c) => c,
        Err(e) => return error(502, "forge_error", &e, vec![]),
    };
    let actioned = store.actioned_pr_comment_ids(pr_id).unwrap_or_default();
    let items: Vec<PrCommentItem> = comments
        .into_iter()
        .map(|c| PrCommentItem {
            actioned: actioned.contains(&c.external_id),
            external_id: c.external_id,
            author: c.author,
            body: c.body,
            created_at: c.created_at,
        })
        .collect();
    json(200, &items)
}

/// Kick off actioning this PR's un-actioned feedback into the owning review
/// worktree in the background (RAL-117's "Pull in PR feedback" button).
fn pr_action_feedback(daemon: &Daemon, pr_id: &str) -> Reply {
    let runner: Arc<dyn Runner> =
        Arc::new(SubprocessRunner::from_env().with_cartographer(daemon.store_handle()));
    crate::pr::start_action_pr_feedback(daemon.store_handle(), runner, pr_id)
}

#[derive(Deserialize)]
struct SquashBody {
    /// The git project root to toggle. Must be one of the review's projects.
    project: String,
    /// Whether that project's task branches are squashed to a single commit.
    enabled: bool,
}

/// RAL-91: enable/disable per-commit squashing for one git project within a
/// review. The change is persisted and applied on the next rebase (like the
/// other per-review opt-out toggles); the updated guardian view is returned.
fn guardian_squash(daemon: &Daemon, id: &str, body: &str) -> Reply {
    let Ok(req) = serde_json::from_str::<SquashBody>(body) else {
        return error(
            400,
            "bad_request",
            "body must be {project, enabled}",
            vec![],
        );
    };
    let store = daemon.lock();
    let guardian = match store.get_guardian(id) {
        Ok(g) => g,
        Err(e) => return store_error(&e),
    };
    // Guard: the project must actually belong to this review.
    if !guardian.projects.iter().any(|p| p == &req.project) {
        return error(
            400,
            "bad_request",
            "project is not part of this review",
            vec![],
        );
    }
    if let Err(e) = store.set_guardian_project_squash(id, &req.project, req.enabled) {
        return store_error(&e);
    }
    let _ = store.log_event(
        None,
        Some(id),
        "guardian",
        None,
        &format!(
            "squash {} for project {} (applies on next rebase)",
            if req.enabled { "enabled" } else { "disabled" },
            req.project
        ),
    );
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

/// Atomically apply a new branch order/enabled arrangement and kick off the
/// rebase in one call (CLI_PARITY_PLAN.local.md C3). Equivalent to calling
/// `branches/reorder` then `merge`, but as a single request -- removing the
/// read-modify-write window between those two calls where a second writer's
/// concurrent reorder would be silently clobbered.
fn guardian_arrange(daemon: &Daemon, id: &str, body: &str) -> Reply {
    let Ok(req) = serde_json::from_str::<ReorderBody>(body) else {
        return error(
            400,
            "bad_request",
            "body must be {order:[...], enabled?:{...}}",
            vec![],
        );
    };
    {
        let mut store = daemon.lock();
        if let Err(e) = store.reorder_guardian_branches(id, &req.order) {
            return store_error(&e);
        }
        for (branch, &enabled) in &req.enabled {
            if let Err(e) = store.set_branch_enabled_by_name(id, branch, enabled) {
                return store_error(&e);
            }
        }
    }
    let runner: Arc<dyn Runner> =
        Arc::new(SubprocessRunner::from_env().with_cartographer(daemon.store_handle()));
    crate::guardian_merge::start_merge(daemon.store_handle(), runner, id, daemon.semaphore_handle())
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

    // Run against the built review worktree (e.g. `<id>-review`), not the
    // original repo — that's the checkout the commands are meant to verify.
    // Fall back to git_root only if the review hasn't been built yet.
    let cwd = g.combined_worktree.clone().unwrap_or(g.git_root.clone());
    let mut errors: Vec<String> = Vec::new();
    for cmd in &to_run {
        // cd /d sets both drive and directory on Windows before running the command.
        let full_cmd = format!("cd /d \"{cwd}\" && {cmd}");
        if let Err(e) = spawn_in_terminal(None, "cmd", &["/K".to_string(), full_cmd]) {
            errors.push(e);
        }
    }

    if errors.is_empty() {
        json(200, &OpenTerminalResponse { ok: true })
    } else {
        error(500, "terminal_error", &errors.join("; "), vec![])
    }
}

/// Run a user-declared action hint from `[[review.action]]` (RAL-77).
/// Body `{ "index": N }` runs the hint at position N (required).
/// `command`-kind hints are opened in a terminal; `prompt`-kind hints are not
/// yet executed (reserved for a future LLM-expansion step) and return 501.
fn guardian_run_action_hint(daemon: &Daemon, id: &str, body: &str) -> Reply {
    #[derive(Deserialize)]
    struct Body {
        index: usize,
    }
    let Ok(req) = serde_json::from_str::<Body>(body) else {
        return error(400, "bad_request", "body must be {\"index\": N}", vec![]);
    };

    let g = match daemon.lock().get_guardian(id) {
        Ok(g) => g,
        Err(e) => return store_error(&e),
    };

    let hint = match g.action_hints.get(req.index) {
        Some(h) => h.clone(),
        None => {
            return error(
                400,
                "out_of_range",
                "action hint index out of range",
                vec![],
            );
        }
    };

    if let Some(cmd) = &hint.command {
        // Same reasoning as guardian_run_manual_commands: prefer the built
        // review worktree over the original repo root.
        let cwd = g.combined_worktree.clone().unwrap_or(g.git_root.clone());
        let full_cmd = format!("cd /d \"{cwd}\" && {cmd}");
        match spawn_in_terminal(None, "cmd", &["/K".to_string(), full_cmd]) {
            Ok(()) => json(200, &OpenTerminalResponse { ok: true }),
            Err(e) => error(500, "terminal_error", &e, vec![]),
        }
    } else {
        // prompt-kind hints are stored for display only; LLM expansion is not yet implemented.
        error(
            501,
            "not_implemented",
            "prompt-kind action hints cannot be run directly yet",
            vec![],
        )
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
    let runner: Arc<dyn Runner> =
        Arc::new(SubprocessRunner::from_env().with_cartographer(daemon.store_handle()));
    crate::guardian_merge::start_merge(daemon.store_handle(), runner, id, daemon.semaphore_handle())
}

/// Force-start a collecting review (RAL-69): disable all enabled branches whose
/// source session is not yet done (or was never submitted), then kick off the
/// merge immediately with whatever branches remain enabled.
fn guardian_force_start(daemon: &Daemon, id: &str) -> Reply {
    let store = daemon.lock();
    let guardian = match store.get_guardian(id) {
        Ok(g) => g,
        Err(e) => return store_error(&e),
    };
    if guardian.status != "collecting" {
        return error(
            409,
            "invalid_transition",
            "force_start is only valid when the review is in collecting state",
            vec![],
        );
    }
    if let Err(e) = store.force_start_disable_branches(id) {
        return store_error(&e);
    }
    drop(store);
    let runner: Arc<dyn Runner> =
        Arc::new(SubprocessRunner::from_env().with_cartographer(daemon.store_handle()));
    crate::guardian_merge::start_merge(daemon.store_handle(), runner, id, daemon.semaphore_handle())
}

/// Permanently dismiss the "can re-enable" notification for a branch (RAL-69).
fn guardian_dismiss_reenable(daemon: &Daemon, id: &str, branch_id: &str) -> Reply {
    if let Err(e) = daemon.lock().dismiss_branch_reenable(id, branch_id) {
        return store_error(&e);
    }
    match daemon.lock().get_guardian(id) {
        Ok(g) => json(200, &g),
        Err(e) => store_error(&e),
    }
}

#[derive(Deserialize)]
struct MoveBranchBody {
    to_guardian_id: String,
}

/// Move a branch from this guardian to another review (RAL-118: "compose a
/// review from worktrees belonging to other reviews"). This is a move, not a
/// copy (RAL-118 Q3): the branch leaves this guardian's stack and is
/// appended to the destination's. `Store::move_guardian_branch` blocks the
/// move (409) while either review has a merge/rebase in flight (RAL-118 Q4),
/// and renumbers/records provenance (RAL-118 Q2) as part of the same call.
///
/// Triggers a rebuild on the source guardian first (best-effort, only if
/// branches remain there -- its stack lost a branch and needs renumbering
/// plus a fresh rebase), then returns the *destination* guardian's
/// merge-kickoff reply -- the same convention `guardian_change_base` uses:
/// the destination gained new work and is the review most callers care about
/// after a move.
fn guardian_move_branch(daemon: &Daemon, id: &str, branch_id: &str, body: &str) -> Reply {
    let Ok(req) = serde_json::from_str::<MoveBranchBody>(body) else {
        return error(400, "bad_request", "body must be {to_guardian_id}", vec![]);
    };
    let to_guardian_id = req.to_guardian_id.trim().to_string();
    if to_guardian_id.is_empty() {
        return error(
            400,
            "bad_request",
            "to_guardian_id must not be empty",
            vec![],
        );
    }

    let (source_remaining, source_git_root) = {
        let store = daemon.lock();
        match store.get_guardian(id) {
            Ok(g) => (
                g.branches.iter().filter(|b| b.id != branch_id).count(),
                g.git_root,
            ),
            Err(e) => return store_error(&e),
        }
    };

    if let Err(e) = daemon
        .lock()
        .move_guardian_branch(id, branch_id, &to_guardian_id)
    {
        return store_error(&e);
    }

    // The remaining source branches may have been stacked on top of the one
    // that just left; purge the source's worktrees/branches/carry-refs so the
    // rebuild below re-derives them from their own feature-branch tips
    // instead of replaying stale history that still contains the moved
    // branch's commits (see `guardian_merge::purge_worktrees`).
    crate::guardian_merge::purge_worktrees(&source_git_root, id);

    let runner: Arc<dyn Runner> =
        Arc::new(SubprocessRunner::from_env().with_cartographer(daemon.store_handle()));
    if source_remaining > 0 {
        let _ = crate::guardian_merge::start_merge(
            daemon.store_handle(),
            Arc::clone(&runner),
            id,
            daemon.semaphore_handle(),
        );
    }
    crate::guardian_merge::start_merge(
        daemon.store_handle(),
        runner,
        &to_guardian_id,
        daemon.semaphore_handle(),
    )
}

fn guardian_merge(daemon: &Daemon, id: &str) -> Reply {
    let runner: Arc<dyn Runner> =
        Arc::new(SubprocessRunner::from_env().with_cartographer(daemon.store_handle()));
    crate::guardian_merge::start_merge(daemon.store_handle(), runner, id, daemon.semaphore_handle())
}

/// Cancel an in-progress rebase (status `merging` or `in_review`) and
/// immediately start a fresh one. The in-flight background thread is
/// superseded: its eventual status writes will be overwritten by the new run.
fn guardian_cancel_and_merge(daemon: &Daemon, id: &str) -> Reply {
    if let Err(e) = daemon.lock().reset_guardian_to_collecting(id) {
        return store_error(&e);
    }
    let runner: Arc<dyn Runner> =
        Arc::new(SubprocessRunner::from_env().with_cartographer(daemon.store_handle()));
    crate::guardian_merge::start_merge(daemon.store_handle(), runner, id, daemon.semaphore_handle())
}

#[derive(Deserialize)]
struct FeedbackBody {
    #[serde(default)]
    feedback: String,
}

fn guardian_feedback(daemon: &Daemon, id: &str, branch_id: &str, body: &str) -> Reply {
    let Ok(req) = serde_json::from_str::<FeedbackBody>(body) else {
        return error(400, "bad_request", "body must be {feedback}", vec![]);
    };
    if req.feedback.trim().is_empty() {
        return error(400, "bad_request", "feedback must not be empty", vec![]);
    }
    let runner: Arc<dyn Runner> =
        Arc::new(SubprocessRunner::from_env().with_cartographer(daemon.store_handle()));
    crate::guardian_merge::start_feedback(
        daemon.store_handle(),
        runner,
        id,
        branch_id,
        req.feedback,
    )
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
    /// Optional base64 data-URI image attached to this message (RAL-59).
    image: Option<String>,
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
    let runner: Arc<dyn Runner> =
        Arc::new(SubprocessRunner::from_env().with_cartographer(daemon.store_handle()));
    crate::guardian_merge::start_chat(daemon.store_handle(), runner, id, req.text, req.image)
}

#[derive(Deserialize)]
struct ForkBody {
    /// The `seq` of the message to fork from: all messages with seq ≥ this
    /// value are deleted and the new text is posted in their place (RAL-59).
    seq: i64,
    #[serde(default)]
    text: String,
    /// Optional base64 data-URI image (RAL-59).
    image: Option<String>,
}

/// Fork the conversation at `seq`: delete everything from that message
/// onwards, post the new reviewer message, and re-trigger the triage agent
/// (RAL-59).
fn guardian_chat_fork(daemon: &Daemon, id: &str, body: &str) -> Reply {
    let Ok(req) = serde_json::from_str::<ForkBody>(body) else {
        return error(400, "bad_request", "body must be {seq, text}", vec![]);
    };
    if req.text.trim().is_empty() {
        return error(400, "bad_request", "message text must not be empty", vec![]);
    }
    crate::rlog!(
        INFO,
        "ralphus [guardian] review {id} chat fork from seq={}",
        req.seq
    );
    {
        let guard = daemon.lock();
        let _ = guard.cartographer_log(crate::cartographer::CartographerEntry {
            level: crate::logging::LogLevel::INFO,
            source: "guardian",
            message: "chat fork",
            scope: Some("guardian"),
            run_id: None,
            guardian_id: Some(id),
            session_id: None,
            task: None,
            payload: serde_json::json!({"seq": req.seq}),
        });
    }
    if let Err(e) = daemon.lock().delete_guardian_messages_from_seq(id, req.seq) {
        return store_error(&e);
    }
    let runner: Arc<dyn Runner> =
        Arc::new(SubprocessRunner::from_env().with_cartographer(daemon.store_handle()));
    crate::guardian_merge::start_chat(daemon.store_handle(), runner, id, req.text, req.image)
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
    // Bind the port before touching any run state. `recover_orphaned_runs`
    // assumes "nothing is executing yet, so any running row is orphaned" —
    // true only for the process that actually wins the port. A second
    // `serve()` invocation racing against an already-running daemon (e.g. an
    // agent session testing against the same shared db_path) would otherwise
    // stomp the live daemon's in-flight `running` rows to `pending` before
    // failing here on the bind, corrupting state without ever serving a
    // request. Binding first makes that race fail closed instead.
    let server = tiny_http::Server::http(addr).map_err(|e| std::io::Error::other(e.to_string()))?;
    // Crash recovery before anything schedules: a previous unclean shutdown may
    // have left runs `Running` with no worker. Reset them to `Pending` so the
    // scheduler resumes them; finished sessions are preserved and skipped, so
    // only the unfinished tail re-runs (RAL-19).
    match store.recover_orphaned_runs() {
        Ok(ids) if !ids.is_empty() => {
            crate::rlog!(
                WARNING,
                "ralphus [recovery] {} orphaned run(s) reset to pending: {}",
                ids.len(),
                ids.join(", ")
            );
            let _ = store.cartographer_log(crate::cartographer::CartographerEntry {
                level: crate::logging::LogLevel::WARNING,
                source: "recovery",
                message: "orphaned runs reset to pending",
                scope: Some("run"),
                run_id: None,
                guardian_id: None,
                session_id: None,
                task: None,
                payload: serde_json::json!({"run_ids": ids}),
            });
        }
        Ok(_) => {}
        Err(e) => crate::rlog!(ERROR, "ralphus [recovery] run recovery failed: {e}"),
    }
    // RAL-48: guardians stuck in `merging` after an unclean shutdown have no
    // background thread; reset them to `merge_failed` so the user can retry.
    match store.recover_orphaned_merges() {
        Ok(ids) if !ids.is_empty() => {
            crate::rlog!(
                WARNING,
                "ralphus [recovery] {} orphaned merge(s) reset to merge_failed: {}",
                ids.len(),
                ids.join(", ")
            );
            let _ = store.cartographer_log(crate::cartographer::CartographerEntry {
                level: crate::logging::LogLevel::WARNING,
                source: "recovery",
                message: "orphaned merges reset to merge_failed",
                scope: Some("guardian"),
                run_id: None,
                guardian_id: None,
                session_id: None,
                task: None,
                payload: serde_json::json!({"guardian_ids": ids}),
            });
        }
        Ok(_) => {}
        Err(e) => crate::rlog!(ERROR, "ralphus [recovery] merge recovery failed: {e}"),
    }
    // Zombie tmux.exe reaping: on the Windows tmux-alternative (psmux) this
    // project targets, `kill-session` frees a session's *name* but never
    // actually terminates the backing OS process (see
    // PSMUX_CRASH_NOTES.local.md's "kill-session leaks the underlying OS
    // process" finding) -- every session ever run, cancelled, or restarted
    // across this machine's history can leave one behind, and they persist
    // across daemon restarts. Safe to force-kill all `ralphus_`-named ones
    // unconditionally here, and only here: nothing has been dispatched yet
    // (same invariant `recover_orphaned_runs` above relies on), so anything
    // found is guaranteed orphaned. Never touches psmux's own `__warm__`
    // pool -- see `tmux::reap_orphaned_sessions_at_startup`'s doc comment.
    let reaped = crate::tmux::reap_orphaned_sessions_at_startup();
    if reaped > 0 {
        crate::rlog!(
            WARNING,
            "ralphus [recovery] force-killed {reaped} orphaned tmux.exe process(es) from before this daemon startup"
        );
        let _ = store.cartographer_log(crate::cartographer::CartographerEntry {
            level: crate::logging::LogLevel::WARNING,
            source: "recovery",
            message: "orphaned tmux.exe processes force-killed at startup",
            scope: None,
            run_id: None,
            guardian_id: None,
            session_id: None,
            task: None,
            payload: serde_json::json!({"count": reaped}),
        });
    }
    let daemon = Daemon::new(store, max_concurrent);

    // Scheduler runs on its own thread, sharing the store via Arc<Mutex> and the
    // cancellation registry so a `cancel` request can reach its workers. The
    // runner shares the daemon's PID registry so `/api/resources` can attribute
    // OS metrics to the sessions it spawns (RAL-11).
    let runner: Arc<dyn Runner> = Arc::new(
        SubprocessRunner::from_env()
            .with_registry(daemon.procs_handle())
            .with_cartographer(daemon.store_handle()),
    );
    let handle = daemon.store_handle();
    let cancellations = daemon.cancellations_handle();
    let sem = daemon.semaphore_handle();
    let summary_queue = daemon.summary_queue_handle();
    // RAL-121: background workers draining the guardian summary priority
    // queue, separate from the scheduler thread so a burst of git-log work
    // for several guardians never delays scheduling. A small fixed pool is
    // enough -- each job is a handful of `git log` subprocess calls, and the
    // queue's own High/Low ordering (not thread count) is what keeps the
    // currently-viewed review responsive.
    crate::summary_worker::spawn_workers(&summary_queue, &daemon.store_handle(), 2);
    std::thread::spawn(move || {
        crate::scheduler::run_loop(
            handle,
            runner,
            max_concurrent,
            cancellations,
            sem,
            summary_queue,
        );
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
        // `route()` splits `path` on `?` itself (it needs the query string for
        // filtered/paginated endpoints like `/api/cartographer`), so the full
        // URL is forwarded verbatim rather than pre-stripped here.
        let url = request.url().to_string();
        let traceparent = request
            .headers()
            .iter()
            .find(|h| h.field.equiv("traceparent"))
            .map(|h| h.value.as_str().to_string());

        let mut body = String::new();
        let _ = request.as_reader().read_to_string(&mut body);

        let reply = route_with_trace(daemon, &method, &url, &body, traceparent.as_deref());
        crate::rlog!(DEBUG, "ralphus [http] {method} {url} → {}", reply.status);
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

    // ── RAL-96: OpenTelemetry trace propagation ─────────────────────────────

    #[test]
    fn route_with_trace_behaves_identically_to_route_for_replies() {
        let d = daemon();
        let traced = route_with_trace(&d, "GET", "/api/daemon", "", None);
        let plain = route(&d, "GET", "/api/daemon", "");
        assert_eq!(traced.status, plain.status);
        assert_eq!(traced.body, plain.body);
    }

    #[test]
    fn route_with_trace_persists_incoming_traceparent_onto_the_new_run() {
        let d = daemon();
        let incoming = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
        let r = route_with_trace(&d, "POST", "/api/runs", &submit_body(GOOD), Some(incoming));
        assert_eq!(r.status, 201);

        let stored = d
            .lock()
            .run_trace_context("run-000000000001")
            .unwrap()
            .expect("trace context recorded on submit");
        // Same trace id (the middle hex segment) as the incoming header — the
        // stored value is the daemon's own child span, not a bare copy.
        assert_eq!(
            stored.split('-').nth(1),
            incoming.split('-').nth(1),
            "run must stay on the browser's trace"
        );
    }

    #[test]
    fn route_with_trace_with_no_incoming_header_and_no_exporter_records_nothing() {
        // With OTEL_EXPORTER_OTLP_ENDPOINT unset (as in this test process), the
        // global tracer is a genuine no-op: it can continue an already-valid
        // parent context but cannot mint a brand new trace id from nothing. So
        // a submit with no incoming header — and tracing not configured —
        // leaves `trace_context` unset rather than storing a bogus value.
        let d = daemon();
        let r = route_with_trace(&d, "POST", "/api/runs", &submit_body(GOOD), None);
        assert_eq!(r.status, 201);
        let stored = d.lock().run_trace_context("run-000000000001").unwrap();
        assert!(stored.is_none());
    }

    #[test]
    fn route_with_trace_does_not_record_trace_context_for_non_submit_routes() {
        let d = daemon();
        let _ = route_with_trace(&d, "GET", "/api/daemon", "", Some("bogus"));
        // No run exists at all, so looking one up is a NotFound error rather
        // than a panic — this just confirms the non-submit path is a no-op.
        assert!(d.lock().run_trace_context("run-000000000001").is_err());
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

    // ── Project registry (RAL-100) ───────────────────────────────────────────

    static PROJ_TEST_SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

    /// A fresh temp git repo with one commit, for project-registration tests.
    fn tmp_git_repo(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ral100-server-{tag}-{}-{}",
            std::process::id(),
            PROJ_TEST_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let run = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .args(args)
                .current_dir(&dir)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .status()
                .expect("git");
            assert!(status.success(), "git {args:?} failed in {}", dir.display());
        };
        run(&["init", "-b", "main"]);
        std::fs::write(dir.join("base.txt"), "base\n").unwrap();
        run(&["add", "."]);
        run(&["commit", "-m", "base"]);
        dir
    }

    fn register_body(name: &str, path: &str, description: &str) -> String {
        serde_json::to_string(&serde_json::json!({
            "name": name, "path": path, "description": description, "vcs": "git",
        }))
        .unwrap()
    }

    #[test]
    fn register_project_route_success() {
        let d = daemon();
        let repo = tmp_git_repo("register");
        let r = route(
            &d,
            "POST",
            "/api/projects",
            &register_body("proj", &repo.to_string_lossy(), "a test project"),
        );
        assert_eq!(r.status, 201, "{}", r.body);
        assert!(r.body.contains("proj"));
    }

    #[test]
    fn register_project_rejects_non_git_path() {
        let d = daemon();
        let dir = std::env::temp_dir().join(format!(
            "ral100-server-not-git-{}-{}",
            std::process::id(),
            PROJ_TEST_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let r = route(
            &d,
            "POST",
            "/api/projects",
            &register_body("proj", &dir.to_string_lossy(), ""),
        );
        assert_eq!(r.status, 400);
        assert!(r.body.contains("invalid_value"));
    }

    #[test]
    fn register_project_rejects_missing_path() {
        let d = daemon();
        let r = route(
            &d,
            "POST",
            "/api/projects",
            &register_body("proj", "C:/definitely/does/not/exist/anywhere", ""),
        );
        assert_eq!(r.status, 400);
    }

    #[test]
    fn list_projects_route_returns_registered() {
        let d = daemon();
        let repo = tmp_git_repo("list");
        route(
            &d,
            "POST",
            "/api/projects",
            &register_body("proj", &repo.to_string_lossy(), ""),
        );
        let r = route(&d, "GET", "/api/projects", "");
        assert_eq!(r.status, 200);
        assert!(r.body.contains("\"projects\""));
        assert!(r.body.contains("proj"));
    }

    #[test]
    fn get_project_route_returns_registered_project() {
        let d = daemon();
        let repo = tmp_git_repo("get");
        route(
            &d,
            "POST",
            "/api/projects",
            &register_body("proj", &repo.to_string_lossy(), "a test project"),
        );
        let r = route(&d, "GET", "/api/projects/proj", "");
        assert_eq!(r.status, 200, "{}", r.body);
        assert!(r.body.contains("\"name\":\"proj\""));
        assert!(r.body.contains("a test project"));
        assert!(r.body.contains("\"vcs\":\"git\""));
    }

    #[test]
    fn get_project_route_unregistered_name_is_404() {
        let d = daemon();
        let r = route(&d, "GET", "/api/projects/nope", "");
        assert_eq!(r.status, 404);
        assert!(r.body.contains("not_found"));
    }

    #[test]
    fn validate_project_route_reports_valid_for_intact_repo() {
        let d = daemon();
        let repo = tmp_git_repo("validate-ok");
        route(
            &d,
            "POST",
            "/api/projects",
            &register_body("proj", &repo.to_string_lossy(), ""),
        );
        let r = route(&d, "GET", "/api/projects/proj/validate", "");
        assert_eq!(r.status, 200, "{}", r.body);
        assert!(r.body.contains("\"valid\":true"));
    }

    #[test]
    fn validate_project_route_reports_invalid_after_path_removed() {
        let d = daemon();
        let repo = tmp_git_repo("validate-removed");
        route(
            &d,
            "POST",
            "/api/projects",
            &register_body("proj", &repo.to_string_lossy(), ""),
        );
        std::fs::remove_dir_all(&repo).unwrap();
        let r = route(&d, "GET", "/api/projects/proj/validate", "");
        assert_eq!(r.status, 200, "{}", r.body);
        assert!(r.body.contains("\"valid\":false"));
        assert!(r.body.contains("does not exist"));
    }

    #[test]
    fn validate_project_route_unregistered_name_is_404() {
        let d = daemon();
        let r = route(&d, "GET", "/api/projects/nope/validate", "");
        assert_eq!(r.status, 404);
        assert!(r.body.contains("not_found"));
    }

    #[test]
    fn submit_rejects_placeholder_cwd_with_unregistered_project() {
        let d = daemon();
        let toml = "[[task]]\nname=\"t\"\nproject=\"ghost\"\n[[task.session]]\ncwd=\"ralphus:new-worktree/feat\"\nprompt=\"p\"\n";
        let r = route(&d, "POST", "/api/runs", &submit_body(toml));
        assert_eq!(r.status, 400, "{}", r.body);
        assert!(r.body.contains("project_validation_failed"));
    }

    #[test]
    fn submit_accepts_placeholder_cwd_with_registered_project() {
        let d = daemon();
        let repo = tmp_git_repo("submit-ok");
        route(
            &d,
            "POST",
            "/api/projects",
            &register_body("proj", &repo.to_string_lossy(), ""),
        );
        let toml = "[[task]]\nname=\"t\"\nproject=\"proj\"\n[[task.session]]\ncwd=\"ralphus:new-worktree/feat\"\nprompt=\"p\"\n";
        let r = route(&d, "POST", "/api/runs", &submit_body(toml));
        assert_eq!(r.status, 201, "{}", r.body);
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
    fn edit_session_dirties_only_the_target_and_its_downstream_not_upstream() {
        // Regression: editing one session used to call `reset_run_to_pending`
        // unconditionally, resetting *every* task/session in the run -- so
        // editing `mid`'s prompt below used to also flip the already-`done`,
        // unrelated `up` task back to running. up -> mid -> down (task-level
        // `depends_on`); editing mid's session must dirty mid and down (its
        // downstream) but leave up's `done` state untouched.
        let d = daemon();
        let toml = "[[task]]\nname=\"up\"\n[[task.session]]\ncwd=\"/r\"\nprompt=\"p\"\n\
            [[task]]\nname=\"mid\"\ndepends_on=[\"up\"]\n[[task.session]]\ncwd=\"/r\"\nprompt=\"p\"\n\
            [[task]]\nname=\"down\"\ndepends_on=[\"mid\"]\n[[task.session]]\ncwd=\"/r\"\nprompt=\"p\"\n";
        route(&d, "POST", "/api/runs", &submit_body(toml));
        {
            let store = d.lock();
            for task_idx in 0..3 {
                store
                    .set_session_state("run-000000000001", task_idx, 0, NodeState::Done)
                    .unwrap();
                store
                    .set_task_state("run-000000000001", task_idx, NodeState::Done)
                    .unwrap();
            }
            store
                .set_run_state("run-000000000001", RunState::Done)
                .unwrap();
        }

        let body = serde_json::json!({
            "kind": "session", "task_idx": 1, "session_idx": 0, "prompt": "changed"
        })
        .to_string();
        let r = route(&d, "POST", "/api/runs/run-000000000001/edit", &body);
        assert_eq!(r.status, 200, "{}", r.body);

        let run = d.lock().get_run("run-000000000001").unwrap();
        assert_eq!(run.tasks[0].name, "up");
        assert_eq!(run.tasks[0].state, "done", "upstream task must stay done");
        assert_eq!(run.tasks[1].name, "mid");
        assert_eq!(run.tasks[1].state, "pending", "edited task itself resets");
        assert_eq!(run.tasks[2].name, "down");
        assert_eq!(
            run.tasks[2].state, "pending",
            "downstream of the edited task must still reset"
        );
    }

    #[test]
    fn resume_agent_command_always_skips_permissions() {
        // "Open Agent" is only ever reachable for a session that actually ran
        // under claude-code (a claude_session_id is required to get here at
        // all -- see open_agent_terminal), so the resumed conversation should
        // never immediately stall on a permission prompt a human has to
        // notice and click through.
        let cmd = resume_agent_command("claude", "abc-123");
        assert!(cmd.contains("--dangerously-skip-permissions"));
        assert!(cmd.contains("--resume 'abc-123'"));
        assert!(cmd.contains("& 'claude'"));
    }

    #[test]
    fn resume_agent_command_escapes_single_quotes() {
        let cmd = resume_agent_command("my'claude", "sess'123");
        assert!(cmd.contains("my''claude"));
        assert!(cmd.contains("sess''123"));
    }

    #[test]
    fn edit_run_label_does_not_reset_run_state() {
        // Regression: renaming a run (kind "run", label only) used to also
        // call `reset_run_to_pending` unconditionally, so renaming a
        // finished (or in-progress) run silently kicked its tasks back to
        // Pending and the scheduler re-ran them -- even though the label is
        // purely cosmetic and unrelated to execution state or content.
        let d = daemon();
        route(&d, "POST", "/api/runs", &submit_body(GOOD));
        d.lock()
            .set_run_state("run-000000000001", RunState::Done)
            .unwrap();
        let body = serde_json::json!({"kind": "run", "label": "renamed"}).to_string();
        let r = route(&d, "POST", "/api/runs/run-000000000001/edit", &body);
        assert_eq!(r.status, 200);
        assert!(r.body.contains("\"label\":\"renamed\""));
        assert!(r.body.contains("\"state\":\"done\""));
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
    fn cartographer_endpoint_returns_events_from_run_logs() {
        let d = daemon();
        route(&d, "POST", "/api/runs", &submit_body(GOOD));
        d.lock()
            .set_run_state("run-000000000001", RunState::Done)
            .unwrap();
        let r = route(&d, "GET", "/api/cartographer", "");
        assert_eq!(r.status, 200);
        assert!(r.body.contains("run → done"));
        assert!(r.body.contains("\"total\":"));
    }

    #[test]
    fn cartographer_endpoint_filters_by_run_id() {
        let d = daemon();
        route(&d, "POST", "/api/runs", &submit_body(GOOD));
        d.lock()
            .set_run_state("run-000000000001", RunState::Done)
            .unwrap();
        let r = route(&d, "GET", "/api/cartographer?run_id=run-000000000001", "");
        assert_eq!(r.status, 200);
        assert!(r.body.contains("run-000000000001"));

        let r_missing = route(&d, "GET", "/api/cartographer?run_id=run-nope", "");
        assert_eq!(r_missing.status, 200);
        assert!(r_missing.body.contains("\"total\":0"));
    }

    #[test]
    fn cartographer_endpoint_paginates() {
        let d = daemon();
        route(&d, "POST", "/api/runs", &submit_body(GOOD));
        let r = route(&d, "GET", "/api/cartographer?limit=1&offset=0", "");
        assert_eq!(r.status, 200);
        let r2 = route(&d, "GET", "/api/cartographer?limit=1&offset=100", "");
        assert_eq!(r2.status, 200);
        assert!(r2.body.contains("\"rows\":[]"));
    }

    #[test]
    fn cartographer_get_by_id() {
        let d = daemon();
        route(&d, "POST", "/api/runs", &submit_body(GOOD));
        d.lock()
            .set_run_state("run-000000000001", RunState::Done)
            .unwrap();
        let page = d
            .lock()
            .cartographer_query(&crate::cartographer::CartographerFilter::recent(1))
            .unwrap();
        let id = page.rows[0].id;
        let r = route(&d, "GET", &format!("/api/cartographer/{id}"), "");
        assert_eq!(r.status, 200);
        assert_eq!(
            route(&d, "GET", "/api/cartographer/999999999", "").status,
            404
        );
        assert_eq!(
            route(&d, "GET", "/api/cartographer/not-a-number", "").status,
            400
        );
    }

    // ── Ghost memory (RAL-136) ───────────────────────────────────────────────

    #[test]
    fn ghost_get_returns_published_ghost_or_404() {
        let d = daemon();
        route(&d, "POST", "/api/runs", &submit_body(GOOD));
        let uri = crate::ghost::session_uri("run-000000000001", 0, 0);
        d.lock()
            .upsert_ghost(
                &uri,
                crate::ghost::KIND_SESSION,
                Some("run-000000000001"),
                None,
                "watch out for the flaky test",
                None,
            )
            .unwrap();

        let r = route(&d, "GET", &format!("/api/ghosts/{uri}"), "");
        assert_eq!(r.status, 200);
        assert!(r.body.contains("watch out for the flaky test"));

        let missing = route(&d, "GET", "/api/ghosts/session:nope:0:0", "");
        assert_eq!(missing.status, 404);
    }

    #[test]
    fn ghost_copy_seeds_another_session_and_validates_target_uri() {
        let d = daemon();
        route(&d, "POST", "/api/runs", &submit_body(GOOD));
        route(&d, "POST", "/api/runs", &submit_body(GOOD));
        let src = crate::ghost::session_uri("run-000000000001", 0, 0);
        let dst = crate::ghost::session_uri("run-000000000002", 0, 0);
        d.lock()
            .upsert_ghost(
                &src,
                crate::ghost::KIND_SESSION,
                Some("run-000000000001"),
                None,
                "handoff note",
                None,
            )
            .unwrap();

        let body =
            serde_json::to_string(&serde_json::json!({ "source_uri": src, "target_uri": dst }))
                .unwrap();
        let r = route(&d, "POST", "/api/ghosts/copy", &body);
        assert_eq!(r.status, 200, "body={}", r.body);
        assert!(r.body.contains("handoff note"));
        assert_eq!(
            d.lock().get_ghost(&dst).unwrap().unwrap().content,
            "handoff note"
        );

        let bad_target = serde_json::to_string(
            &serde_json::json!({ "source_uri": src, "target_uri": "bogus:xyz" }),
        )
        .unwrap();
        assert_eq!(
            route(&d, "POST", "/api/ghosts/copy", &bad_target).status,
            400
        );

        let missing_source = serde_json::to_string(
            &serde_json::json!({ "source_uri": "session:nope:0:0", "target_uri": dst }),
        )
        .unwrap();
        assert_eq!(
            route(&d, "POST", "/api/ghosts/copy", &missing_source).status,
            404
        );
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
        // The GOOD fixture's session cwd is "/r"; not a git worktree -> project/upstream null.
        assert!(r.body.contains("\"worktree\":\"/r\""));
        assert!(r.body.contains("\"project\":null"));
        assert!(r.body.contains("\"upstream\":null"));
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
        assert!(c.body.contains("\"cancelled\":[\"run-000000000001\"]"));
    }

    #[test]
    fn cancel_run_is_always_available_even_when_already_terminal() {
        let d = daemon();
        route(&d, "POST", "/api/runs", &submit_body(GOOD));
        d.lock()
            .set_run_state("run-000000000001", crate::store::RunState::Done)
            .unwrap();
        // Per RAL-116, the cancel action must still succeed (not 4xx) on an
        // already-terminal run, locking it out of ever being picked up again.
        let c = route(&d, "POST", "/api/runs/run-000000000001/cancel", "");
        assert_eq!(c.status, 200);
        assert!(c.body.contains("\"state\":\"cancelled\""));
    }

    #[test]
    fn cancel_run_cascades_to_dependent_run() {
        let d = daemon();
        route(&d, "POST", "/api/runs", &submit_body(GOOD)); // run-1
        let dependent = "[[default]]\ndepends_on=[\"run-000000000001\"]\n".to_string() + GOOD;
        route(&d, "POST", "/api/runs", &submit_body(&dependent)); // run-2

        let c = route(&d, "POST", "/api/runs/run-000000000001/cancel", "");
        assert_eq!(c.status, 200);
        assert!(c.body.contains("run-000000000001"));
        assert!(c.body.contains("run-000000000002"));
        assert!(
            route(&d, "GET", "/api/runs/run-000000000002", "")
                .body
                .contains("\"state\":\"cancelled\"")
        );
    }

    #[test]
    fn cancel_run_preview_reports_impact_without_mutating() {
        let d = daemon();
        route(&d, "POST", "/api/runs", &submit_body(GOOD)); // run-1
        let dependent = "[[default]]\ndepends_on=[\"run-000000000001\"]\n".to_string() + GOOD;
        route(&d, "POST", "/api/runs", &submit_body(&dependent)); // run-2

        let p = route(&d, "POST", "/api/runs/run-000000000001/cancel/preview", "");
        assert_eq!(p.status, 200);
        assert!(p.body.contains("run-000000000001"));
        assert!(p.body.contains("run-000000000002"));
        // Nothing was actually mutated.
        assert!(
            route(&d, "GET", "/api/runs/run-000000000001", "")
                .body
                .contains("\"state\":\"pending\"")
        );
        assert!(
            route(&d, "GET", "/api/runs/run-000000000002", "")
                .body
                .contains("\"state\":\"pending\"")
        );
    }

    #[test]
    fn cancel_run_preview_missing_is_404() {
        let d = daemon();
        assert_eq!(
            route(&d, "POST", "/api/runs/run-999/cancel/preview", "").status,
            404
        );
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
    fn set_status_run() {
        let d = daemon();
        route(&d, "POST", "/api/runs", &submit_body(GOOD));
        let body = serde_json::json!({"kind": "run", "state": "done"}).to_string();
        let r = route(&d, "POST", "/api/runs/run-000000000001/set-status", &body);
        assert_eq!(r.status, 200);
        assert!(r.body.contains("\"state\":\"done\""));
    }

    #[test]
    fn set_status_run_cancelled_cascades_like_cancel_button() {
        // Set Status -> cancelled must go through the exact same cascading
        // cancel path as the "Cancel Run" button (RAL-116), not a bare DB flip.
        let d = daemon();
        route(&d, "POST", "/api/runs", &submit_body(GOOD)); // run-1
        let dependent = "[[default]]\ndepends_on=[\"run-000000000001\"]\n".to_string() + GOOD;
        route(&d, "POST", "/api/runs", &submit_body(&dependent)); // run-2

        let body = serde_json::json!({"kind": "run", "state": "cancelled"}).to_string();
        let r = route(&d, "POST", "/api/runs/run-000000000001/set-status", &body);
        assert_eq!(r.status, 200);
        assert!(r.body.contains("\"state\":\"cancelled\""));
        // The dependent run was cascade-cancelled too, same as the button.
        assert!(
            route(&d, "GET", "/api/runs/run-000000000002", "")
                .body
                .contains("\"state\":\"cancelled\"")
        );
    }

    #[test]
    fn set_status_run_cancelled_works_even_when_already_terminal() {
        let d = daemon();
        route(&d, "POST", "/api/runs", &submit_body(GOOD));
        d.lock()
            .set_run_state("run-000000000001", crate::store::RunState::Done)
            .unwrap();
        let body = serde_json::json!({"kind": "run", "state": "cancelled"}).to_string();
        let r = route(&d, "POST", "/api/runs/run-000000000001/set-status", &body);
        assert_eq!(r.status, 200);
        assert!(r.body.contains("\"state\":\"cancelled\""));
    }

    #[test]
    fn set_status_task() {
        let d = daemon();
        route(&d, "POST", "/api/runs", &submit_body(GOOD));
        let body = serde_json::json!({"kind": "task", "task_idx": 0, "state": "done"}).to_string();
        let r = route(&d, "POST", "/api/runs/run-000000000001/set-status", &body);
        assert_eq!(r.status, 200);
        let v: serde_json::Value = serde_json::from_str(&r.body).unwrap();
        assert_eq!(v["tasks"][0]["state"].as_str(), Some("done"));
    }

    #[test]
    fn set_status_session() {
        let d = daemon();
        route(&d, "POST", "/api/runs", &submit_body(GOOD));
        let body = serde_json::json!({
            "kind": "session", "task_idx": 0, "session_idx": 0, "state": "done"
        })
        .to_string();
        let r = route(&d, "POST", "/api/runs/run-000000000001/set-status", &body);
        assert_eq!(r.status, 200);
        let v: serde_json::Value = serde_json::from_str(&r.body).unwrap();
        assert_eq!(v["tasks"][0]["sessions"][0]["state"].as_str(), Some("done"));
    }

    #[test]
    fn set_status_task_cancelled_cancels_whole_run() {
        // Regression: forcing a single task/session/verify to "cancelled"
        // through set-status must reuse the real cancel cascade (Store::cancel
        // + tripping the run's CancelToken), not just write the one DB row —
        // otherwise the board's `running` count never drops and any live
        // subprocess for that run keeps running with its permit held forever.
        let d = daemon();
        route(&d, "POST", "/api/runs", &submit_body(GOOD));
        let body =
            serde_json::json!({"kind": "task", "task_idx": 0, "state": "cancelled"}).to_string();
        let r = route(&d, "POST", "/api/runs/run-000000000001/set-status", &body);
        assert_eq!(r.status, 200);
        let v: serde_json::Value = serde_json::from_str(&r.body).unwrap();
        assert_eq!(
            v["state"].as_str(),
            Some("cancelled"),
            "cancelling one task must cancel the whole run: {}",
            r.body
        );
        assert_eq!(v["tasks"][0]["state"].as_str(), Some("cancelled"));
    }

    #[test]
    fn set_status_bad_state_is_400() {
        let d = daemon();
        route(&d, "POST", "/api/runs", &submit_body(GOOD));
        let body = serde_json::json!({"kind": "run", "state": "nope"}).to_string();
        let r = route(&d, "POST", "/api/runs/run-000000000001/set-status", &body);
        assert_eq!(r.status, 400);
    }

    #[test]
    fn set_status_bad_kind_is_400() {
        let d = daemon();
        route(&d, "POST", "/api/runs", &submit_body(GOOD));
        let body = serde_json::json!({"kind": "wat", "state": "done"}).to_string();
        let r = route(&d, "POST", "/api/runs/run-000000000001/set-status", &body);
        assert_eq!(r.status, 400);
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
    fn guardian_move_branch_route_moves_and_compacts_source() {
        // RAL-118: move a branch from one review to another over HTTP. As with
        // `guardian_arrange_persists_order_and_enabled_before_merging`,
        // git_root here isn't a real repo, so the rebuild kicked off on both
        // guardians runs (and fails) on a background thread -- what matters
        // for this test is that the move itself is persisted correctly
        // regardless of that async merge outcome.
        let d = daemon();
        for name in ["r1", "r2"] {
            let body = serde_json::json!({"name":name,"base_branch":"main","git_root":"/repo"})
                .to_string();
            route(&d, "POST", "/api/guardians", &body);
        }
        let src = "guardian-000000000001";
        let dst = "guardian-000000000002";
        for br in ["a", "b"] {
            let body = serde_json::json!({ "branch": br }).to_string();
            route(&d, "POST", &format!("/api/guardians/{src}/branches"), &body);
        }
        route(
            &d,
            "POST",
            &format!("/api/guardians/{dst}/branches"),
            &serde_json::json!({ "branch": "x" }).to_string(),
        );

        let src_before = route(&d, "GET", &format!("/api/guardians/{src}"), "");
        let src_before_json: serde_json::Value = serde_json::from_str(&src_before.body).unwrap();
        let branch_id_a = src_before_json["branches"][0]["id"].as_str().unwrap();

        let move_body = serde_json::json!({"to_guardian_id": dst}).to_string();
        let r = route(
            &d,
            "POST",
            &format!("/api/guardians/{src}/branches/{branch_id_a}/move"),
            &move_body,
        );
        assert_ne!(r.status, 404, "route must be wired up");
        assert_ne!(r.status, 400, "move itself must succeed: {}", r.body);

        let source_view = route(&d, "GET", &format!("/api/guardians/{src}"), "");
        let source_json: serde_json::Value = serde_json::from_str(&source_view.body).unwrap();
        let source_branches: Vec<&str> = source_json["branches"]
            .as_array()
            .unwrap()
            .iter()
            .map(|b| b["branch"].as_str().unwrap())
            .collect();
        assert_eq!(
            source_branches,
            vec!["b"],
            "source must renumber to just b at position 0"
        );
        assert_eq!(source_json["branches"][0]["position"], 0);

        let dest_view = route(&d, "GET", &format!("/api/guardians/{dst}"), "");
        let dest_json: serde_json::Value = serde_json::from_str(&dest_view.body).unwrap();
        let dest_branches = dest_json["branches"].as_array().unwrap();
        assert_eq!(dest_branches.len(), 2);
        let moved = dest_branches.iter().find(|b| b["branch"] == "a").unwrap();
        assert_eq!(moved["position"], 1);
        assert_eq!(moved["moved_from_guardian_id"], src);
    }

    #[test]
    fn guardian_move_branch_route_bad_body_is_400() {
        let d = daemon();
        let body =
            serde_json::json!({"name":"r","base_branch":"main","git_root":"/repo"}).to_string();
        route(&d, "POST", "/api/guardians", &body);
        let gid = "guardian-000000000001";
        route(
            &d,
            "POST",
            &format!("/api/guardians/{gid}/branches"),
            &serde_json::json!({ "branch": "a" }).to_string(),
        );
        let r = route(
            &d,
            "POST",
            &format!("/api/guardians/{gid}/branches/0/move"),
            "not json",
        );
        assert_eq!(r.status, 400);
    }

    #[test]
    fn guardian_move_branch_route_blocked_while_destination_is_merging() {
        // RAL-118 Q4: moving into a review with an active merge/rebase is
        // blocked with a clear 409, not silently corrupted state.
        let d = daemon();
        for name in ["r1", "r2"] {
            let body = serde_json::json!({"name":name,"base_branch":"main","git_root":"/repo"})
                .to_string();
            route(&d, "POST", "/api/guardians", &body);
        }
        let src = "guardian-000000000001";
        let dst = "guardian-000000000002";
        route(
            &d,
            "POST",
            &format!("/api/guardians/{src}/branches"),
            &serde_json::json!({ "branch": "a" }).to_string(),
        );
        assert!(d.lock().claim_guardian_merge(dst).unwrap());

        let move_body = serde_json::json!({"to_guardian_id": dst}).to_string();
        let r = route(
            &d,
            "POST",
            &format!("/api/guardians/{src}/branches/0/move"),
            &move_body,
        );
        assert_eq!(r.status, 409);
    }

    #[test]
    fn guardian_arrange_persists_order_and_enabled_before_merging() {
        // C3 (CLI_PARITY_PLAN.local.md): `branches/arrange` must apply the new
        // arrangement atomically with kicking off the merge -- unlike the old
        // reorder-then-merge two-call sequence, there is no window where a second
        // writer's reorder could be lost. The merge itself may fail immediately
        // (git_root here is not a real repo); what matters is the request doesn't
        // panic and the arrangement is persisted regardless of merge outcome.
        let d = daemon();
        let body =
            serde_json::json!({"name":"r","base_branch":"main","git_root":"/repo"}).to_string();
        route(&d, "POST", "/api/guardians", &body);
        let gid = "guardian-000000000001";
        for br in ["a", "b", "c"] {
            let b = serde_json::json!({ "branch": br }).to_string();
            route(&d, "POST", &format!("/api/guardians/{gid}/branches"), &b);
        }
        let arrange = serde_json::json!({"order":["c","a","b"],"enabled":{"b":false}}).to_string();
        let r = route(
            &d,
            "POST",
            &format!("/api/guardians/{gid}/branches/arrange"),
            &arrange,
        );
        assert_ne!(r.status, 404, "route must be wired up");

        let g = route(&d, "GET", &format!("/api/guardians/{gid}"), "");
        let g: serde_json::Value = serde_json::from_str(&g.body).unwrap();
        let branches = g["branches"].as_array().unwrap();
        let order: Vec<&str> = branches
            .iter()
            .map(|b| b["branch"].as_str().unwrap())
            .collect();
        assert_eq!(
            order,
            vec!["c", "a", "b"],
            "arrangement must persist regardless of merge outcome"
        );
        let b_enabled = branches.iter().find(|b| b["branch"] == "b").unwrap()["enabled"]
            .as_bool()
            .unwrap();
        assert!(!b_enabled, "b must be disabled");
    }

    #[test]
    fn guardian_arrange_bad_body_is_400() {
        let d = daemon();
        let body =
            serde_json::json!({"name":"r","base_branch":"main","git_root":"/repo"}).to_string();
        route(&d, "POST", "/api/guardians", &body);
        let gid = "guardian-000000000001";
        let r = route(
            &d,
            "POST",
            &format!("/api/guardians/{gid}/branches/arrange"),
            "not json",
        );
        assert_eq!(r.status, 400);
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
    fn add_dependency_route_appends_and_gates_readiness() {
        let d = daemon();
        route(&d, "POST", "/api/runs", &submit_body(GOOD)); // run-000000000001
        route(&d, "POST", "/api/runs", &submit_body(GOOD)); // run-000000000002
        let body = serde_json::json!({"target_id": "run-000000000001"}).to_string();
        let r = route(
            &d,
            "POST",
            "/api/runs/run-000000000002/add-dependency",
            &body,
        );
        assert_eq!(r.status, 200);
        assert!(r.body.contains("run-000000000002"));

        let ready = route(&d, "GET", "/api/graph", "");
        assert_eq!(ready.status, 200);
        assert!(ready.body.contains("run-000000000001"));
    }

    #[test]
    fn add_dependency_route_rejects_self_reference() {
        let d = daemon();
        route(&d, "POST", "/api/runs", &submit_body(GOOD));
        let body = serde_json::json!({"target_id": "run-000000000001"}).to_string();
        let r = route(
            &d,
            "POST",
            "/api/runs/run-000000000001/add-dependency",
            &body,
        );
        assert_eq!(r.status, 409);
    }

    #[test]
    fn add_dependency_route_rejects_cycle() {
        let d = daemon();
        route(&d, "POST", "/api/runs", &submit_body(GOOD)); // run-1
        route(&d, "POST", "/api/runs", &submit_body(GOOD)); // run-2
        let body = serde_json::json!({"target_id": "run-000000000001"}).to_string();
        route(
            &d,
            "POST",
            "/api/runs/run-000000000002/add-dependency",
            &body,
        );
        let reverse = serde_json::json!({"target_id": "run-000000000002"}).to_string();
        let r = route(
            &d,
            "POST",
            "/api/runs/run-000000000001/add-dependency",
            &reverse,
        );
        assert_eq!(r.status, 409);
    }

    #[test]
    fn add_dependency_route_missing_target_is_404() {
        let d = daemon();
        route(&d, "POST", "/api/runs", &submit_body(GOOD));
        let body = serde_json::json!({"target_id": "run-999"}).to_string();
        let r = route(
            &d,
            "POST",
            "/api/runs/run-000000000001/add-dependency",
            &body,
        );
        assert_eq!(r.status, 404);
    }

    #[test]
    fn add_dependency_route_bad_body_is_400() {
        let d = daemon();
        route(&d, "POST", "/api/runs", &submit_body(GOOD));
        let r = route(
            &d,
            "POST",
            "/api/runs/run-000000000001/add-dependency",
            "not json",
        );
        assert_eq!(r.status, 400);
    }

    #[test]
    fn restart_run_preview_reports_impact_without_mutating() {
        // RAL-104: the dry-run preview must report the same sessions/tasks a
        // real restart would touch, and must leave the run's state untouched.
        let d = daemon();
        route(&d, "POST", "/api/runs", &submit_body(GOOD));
        let r = route(&d, "POST", "/api/runs/run-000000000001/restart/preview", "");
        assert_eq!(r.status, 200);
        let body: serde_json::Value = serde_json::from_str(&r.body).unwrap();
        assert_eq!(body["sessions"].as_array().unwrap().len(), 1);
        assert_eq!(body["tasks"].as_array().unwrap().len(), 1);
        assert_eq!(body["sessions"][0]["task_name"], "t");

        // Previewing did not mutate anything: the run is still fresh/pending,
        // not reset via the restart path.
        let run = route(&d, "GET", "/api/runs/run-000000000001", "");
        let run: serde_json::Value = serde_json::from_str(&run.body).unwrap();
        assert_eq!(run["state"], "pending");
    }

    #[test]
    fn restart_run_preview_missing_is_404() {
        let d = daemon();
        assert_eq!(
            route(&d, "POST", "/api/runs/run-999/restart/preview", "").status,
            404
        );
    }

    #[test]
    fn restart_session_preview_reports_impact_without_mutating() {
        let d = daemon();
        route(&d, "POST", "/api/runs", &submit_body(GOOD));
        let r = route(
            &d,
            "POST",
            "/api/runs/run-000000000001/sessions/0/0/restart/preview",
            "",
        );
        assert_eq!(r.status, 200);
        let body: serde_json::Value = serde_json::from_str(&r.body).unwrap();
        assert_eq!(body["sessions"].as_array().unwrap().len(), 1);
        assert_eq!(body["sessions"][0]["task_idx"], 0);
        assert_eq!(body["sessions"][0]["idx"], 0);
    }

    #[test]
    fn restart_session_preview_bad_index_is_400() {
        let d = daemon();
        let r = route(
            &d,
            "POST",
            "/api/runs/run-1/sessions/x/y/restart/preview",
            "",
        );
        assert_eq!(r.status, 400);
    }

    #[test]
    fn restart_session_preview_missing_is_404() {
        let d = daemon();
        route(&d, "POST", "/api/runs", &submit_body(GOOD));
        let r = route(
            &d,
            "POST",
            "/api/runs/run-000000000001/sessions/9/9/restart/preview",
            "",
        );
        assert_eq!(r.status, 404);
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
    fn guardian_chat_fork_empty_text_is_400() {
        let d = daemon();
        let body =
            serde_json::json!({"name":"r","base_branch":"main","git_root":"/repo"}).to_string();
        route(&d, "POST", "/api/guardians", &body);
        let r = route(
            &d,
            "POST",
            "/api/guardians/guardian-000000000001/chat/fork",
            "{\"seq\":1,\"text\":\"  \"}",
        );
        assert_eq!(r.status, 400);
    }

    #[test]
    fn guardian_chat_fork_missing_guardian_is_404() {
        let d = daemon();
        let r = route(
            &d,
            "POST",
            "/api/guardians/guardian-000000000099/chat/fork",
            "{\"seq\":1,\"text\":\"retry\"}",
        );
        assert_eq!(r.status, 404);
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
    fn guardian_skip_auto_build_via_create_and_settings() {
        let d = daemon();
        let body = serde_json::json!({"name":"r","base_branch":"main","git_root":"/repo","skip_auto_build":true})
            .to_string();
        route(&d, "POST", "/api/guardians", &body);
        let gid = "guardian-000000000001";
        assert!(
            route(&d, "GET", &format!("/api/guardians/{gid}"), "")
                .body
                .contains("\"skip_auto_build\":true")
        );
        // toggle back off via settings
        let r = route(
            &d,
            "POST",
            &format!("/api/guardians/{gid}/settings"),
            "{\"skip_auto_build\":false}",
        );
        assert_eq!(r.status, 200);
        assert!(r.body.contains("\"skip_auto_build\":false"));
    }

    #[test]
    fn guardian_skip_worktree_checks_via_create_and_settings() {
        // RAL-110: the two split flags round-trip independently.
        let d = daemon();
        let body = serde_json::json!({"name":"r","base_branch":"main","git_root":"/repo","skip_worktree_checks":true})
            .to_string();
        route(&d, "POST", "/api/guardians", &body);
        let gid = "guardian-000000000001";
        let get_body = route(&d, "GET", &format!("/api/guardians/{gid}"), "").body;
        assert!(get_body.contains("\"skip_worktree_checks\":true"));
        assert!(get_body.contains("\"skip_auto_build\":false"));
        let r = route(
            &d,
            "POST",
            &format!("/api/guardians/{gid}/settings"),
            "{\"skip_worktree_checks\":false}",
        );
        assert_eq!(r.status, 200);
        assert!(r.body.contains("\"skip_worktree_checks\":false"));
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

    #[test]
    fn restart_session_verify_route_returns_pending() {
        let d = daemon();
        route(&d, "POST", "/api/runs", &submit_body(GOOD));
        let r = route(
            &d,
            "POST",
            "/api/runs/run-000000000001/sessions/0/0/verify/0/restart",
            "",
        );
        assert_eq!(r.status, 200);
        assert!(r.body.contains("\"state\":\"pending\""));
    }

    #[test]
    fn restart_session_verify_bad_index_is_400() {
        let d = daemon();
        let r = route(
            &d,
            "POST",
            "/api/runs/run-1/sessions/x/y/verify/z/restart",
            "",
        );
        assert_eq!(r.status, 400);
    }

    #[test]
    fn restart_task_verify_route_returns_pending() {
        let d = daemon();
        route(&d, "POST", "/api/runs", &submit_body(GOOD));
        let r = route(
            &d,
            "POST",
            "/api/runs/run-000000000001/tasks/0/verify/0/restart",
            "",
        );
        assert_eq!(r.status, 200);
        assert!(r.body.contains("\"state\":\"pending\""));
    }

    #[test]
    fn restart_task_verify_bad_index_is_400() {
        let d = daemon();
        let r = route(&d, "POST", "/api/runs/run-1/tasks/x/verify/y/restart", "");
        assert_eq!(r.status, 400);
    }

    // ── CLI_PARITY_PLAN.local.md Phase 1 / Q1: server-side `/api/tasks` filter ──

    #[test]
    fn board_filters_runs_by_status() {
        let d = daemon();
        route(&d, "POST", "/api/runs", &submit_body(GOOD)); // run-1: pending
        route(&d, "POST", "/api/runs", &submit_body(GOOD)); // run-2: pending
        d.lock()
            .set_run_state("run-000000000002", crate::store::RunState::Done)
            .unwrap();
        let r = route(&d, "GET", "/api/tasks?status=done", "");
        assert_eq!(r.status, 200);
        assert!(r.body.contains("run-000000000002"));
        assert!(!r.body.contains("run-000000000001"));
    }

    #[test]
    fn board_filters_runs_by_status_is_case_insensitive_and_comma_separated() {
        let d = daemon();
        route(&d, "POST", "/api/runs", &submit_body(GOOD));
        let r = route(&d, "GET", "/api/tasks?status=Done,PENDING", "");
        assert_eq!(r.status, 200);
        assert!(r.body.contains("run-000000000001"));
    }

    #[test]
    fn board_filters_runs_by_name_substring() {
        let d = daemon();
        let body = serde_json::to_string(
            &serde_json::json!({ "toml": GOOD, "label": "fix the flaky test" }),
        )
        .unwrap();
        route(&d, "POST", "/api/runs", &body);
        route(&d, "POST", "/api/runs", &submit_body(GOOD)); // no label
        let r = route(&d, "GET", "/api/tasks?name=flaky", "");
        assert_eq!(r.status, 200);
        assert!(r.body.contains("run-000000000001"));
        assert!(!r.body.contains("run-000000000002"));
    }

    #[test]
    fn board_sort_by_name_orders_labels_ascending() {
        let d = daemon();
        for label in ["zebra", "apple"] {
            let body = serde_json::to_string(&serde_json::json!({ "toml": GOOD, "label": label }))
                .unwrap();
            route(&d, "POST", "/api/runs", &body);
        }
        let r = route(&d, "GET", "/api/tasks?sort=name", "");
        assert_eq!(r.status, 200);
        // "apple" (run-2) must appear before "zebra" (run-1) in the serialized array.
        let apple_pos = r.body.find("apple").unwrap();
        let zebra_pos = r.body.find("zebra").unwrap();
        assert!(apple_pos < zebra_pos);
    }

    #[test]
    fn board_with_no_query_keeps_newest_first_order() {
        let d = daemon();
        route(&d, "POST", "/api/runs", &submit_body(GOOD));
        route(&d, "POST", "/api/runs", &submit_body(GOOD));
        let r = route(&d, "GET", "/api/tasks", "");
        assert_eq!(r.status, 200);
        let first_pos = r.body.find("run-000000000002").unwrap();
        let second_pos = r.body.find("run-000000000001").unwrap();
        assert!(first_pos < second_pos, "newest run must be listed first");
    }

    // ── CLI_PARITY_PLAN.local.md Phase 6: graph endpoints ───────────────────────

    #[test]
    fn run_graph_route_returns_nodes_and_edges() {
        let d = daemon();
        let toml = "[[task]]\nname=\"build\"\n[[task.session]]\ncwd=\"/r\"\nprompt=\"p\"\n\
             [[task]]\nname=\"test\"\ndepends_on=[\"build\"]\n[[task.session]]\ncwd=\"/r\"\nprompt=\"p\"\n";
        route(&d, "POST", "/api/runs", &submit_body(toml));
        let r = route(&d, "GET", "/api/runs/run-000000000001/graph", "");
        assert_eq!(r.status, 200);
        assert!(r.body.contains("\"id\":\"t0s0\""));
        assert!(r.body.contains("\"id\":\"t1s0\""));
        assert!(r.body.contains("\"from\":\"t0s0\""));
        assert!(r.body.contains("\"to\":\"t1s0\""));
    }

    #[test]
    fn run_graph_missing_run_is_404() {
        let d = daemon();
        let r = route(&d, "GET", "/api/runs/run-nope/graph", "");
        assert_eq!(r.status, 404);
    }

    #[test]
    fn global_graph_edge_from_dependency_to_dependent() {
        let d = daemon();
        route(&d, "POST", "/api/runs", &submit_body(GOOD)); // run-1
        let dependent = "[[default]]\ndepends_on=[\"run-000000000001\"]\n".to_string() + GOOD;
        route(&d, "POST", "/api/runs", &submit_body(&dependent)); // run-2
        let r = route(&d, "GET", "/api/graph", "");
        assert_eq!(r.status, 200);
        assert!(r.body.contains("\"from\":\"run-000000000001\""));
        assert!(r.body.contains("\"to\":\"run-000000000002\""));
    }

    #[test]
    fn global_graph_excludes_terminal_runs_by_default_but_all_includes_them() {
        let d = daemon();
        route(&d, "POST", "/api/runs", &submit_body(GOOD)); // run-1: pending
        route(&d, "POST", "/api/runs", &submit_body(GOOD)); // run-2: pending
        d.lock()
            .set_run_state("run-000000000002", crate::store::RunState::Done)
            .unwrap();
        let default_view = route(&d, "GET", "/api/graph", "");
        assert!(default_view.body.contains("run-000000000001"));
        assert!(!default_view.body.contains("run-000000000002"));
        let all_view = route(&d, "GET", "/api/graph?all=1", "");
        assert!(all_view.body.contains("run-000000000001"));
        assert!(all_view.body.contains("run-000000000002"));
    }

    // ── RAL-117: PR submission + feedback loop ──────────────────────────────

    fn make_guardian(d: &Daemon) -> String {
        let body =
            serde_json::json!({"name":"r","base_branch":"main","git_root":"/repo"}).to_string();
        route(d, "POST", "/api/guardians", &body);
        "guardian-000000000001".to_string()
    }

    #[test]
    fn submit_prs_rejects_bad_body() {
        let d = daemon();
        let gid = make_guardian(&d);
        let r = route(
            &d,
            "POST",
            &format!("/api/guardians/{gid}/pull-requests"),
            "not json",
        );
        assert_eq!(r.status, 400);
    }

    #[test]
    fn submit_prs_rejects_empty_list() {
        let d = daemon();
        let gid = make_guardian(&d);
        let body = serde_json::json!({"prs": []}).to_string();
        let r = route(
            &d,
            "POST",
            &format!("/api/guardians/{gid}/pull-requests"),
            &body,
        );
        assert_eq!(r.status, 400);
    }

    #[test]
    fn submit_prs_missing_guardian_is_404() {
        let d = daemon();
        let body = serde_json::json!({"prs": [{"branch_id": "branch-000000000001"}]}).to_string();
        let r = route(
            &d,
            "POST",
            "/api/guardians/guardian-999/pull-requests",
            &body,
        );
        assert_eq!(r.status, 404);
    }

    #[test]
    fn list_prs_empty_for_new_guardian() {
        let d = daemon();
        let gid = make_guardian(&d);
        let r = route(
            &d,
            "GET",
            &format!("/api/guardians/{gid}/pull-requests"),
            "",
        );
        assert_eq!(r.status, 200);
        assert_eq!(r.body, "[]");
    }

    #[test]
    fn pr_get_update_and_find_roundtrip() {
        let d = daemon();
        let gid = make_guardian(&d);
        let pr_id = d
            .lock()
            .create_pull_request(
                &gid,
                Some("branch-000000000001"),
                "github",
                "acme/widget",
                "feature/foo",
                "main",
                "Add foo",
                "Adds the foo thing.",
                Some(42),
                Some("https://github.com/acme/widget/pull/42"),
            )
            .unwrap();

        let get = route(&d, "GET", &format!("/api/pull-requests/{pr_id}"), "");
        assert_eq!(get.status, 200);
        assert!(get.body.contains("\"pr_number\":42"));

        let find = route(
            &d,
            "GET",
            "/api/pull-requests?forge=github&repo=acme%2Fwidget&pr_number=42",
            "",
        );
        assert_eq!(find.status, 200);
        assert!(find.body.contains(&pr_id));

        let update_body = serde_json::json!({"pr_number": 43, "state": "closed"}).to_string();
        let updated = route(
            &d,
            "POST",
            &format!("/api/pull-requests/{pr_id}"),
            &update_body,
        );
        assert_eq!(updated.status, 200);
        assert!(updated.body.contains("\"pr_number\":43"));
        assert!(updated.body.contains("\"state\":\"closed\""));
    }

    #[test]
    fn pr_get_missing_is_404() {
        let d = daemon();
        assert_eq!(
            route(&d, "GET", "/api/pull-requests/pr-999", "").status,
            404
        );
    }

    #[test]
    fn pr_find_requires_all_query_params() {
        let d = daemon();
        let r = route(&d, "GET", "/api/pull-requests?forge=github", "");
        assert_eq!(r.status, 400);
    }

    #[test]
    fn pr_comments_missing_number_is_conflict() {
        let d = daemon();
        let gid = make_guardian(&d);
        let pr_id = d
            .lock()
            .create_pull_request(
                &gid,
                Some("branch-000000000001"),
                "github",
                "acme/widget",
                "a",
                "main",
                "T",
                "D",
                None,
                None,
            )
            .unwrap();
        let r = route(
            &d,
            "GET",
            &format!("/api/pull-requests/{pr_id}/comments"),
            "",
        );
        assert_eq!(r.status, 409);
    }

    #[test]
    fn action_feedback_missing_pr_is_404() {
        let d = daemon();
        let r = route(&d, "POST", "/api/pull-requests/pr-999/action-feedback", "");
        assert_eq!(r.status, 404);
    }

    #[test]
    fn guardian_settings_toggles_auto_pr_feedback() {
        let d = daemon();
        let gid = make_guardian(&d);
        let body = serde_json::json!({"auto_pr_feedback": true}).to_string();
        let r = route(&d, "POST", &format!("/api/guardians/{gid}/settings"), &body);
        assert_eq!(r.status, 200);
        assert!(r.body.contains("\"auto_pr_feedback\":true"));
    }
}
