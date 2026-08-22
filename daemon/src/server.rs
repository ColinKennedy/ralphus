//! The daemon's HTTP/JSON API (see `docs/daemon-api.md`).
//!
//! The routing/handler core (`route`) is a pure function over `(&Daemon, method,
//! path, body)` so it can be unit-tested without sockets. `serve` wraps it in a
//! blocking `tiny_http` loop and runs the scheduler on a second thread; both
//! share the store through an `Arc<Mutex<Store>>`.

use std::collections::{BTreeSet, HashSet};
use std::io::Write;
use std::net::ToSocketAddrs;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::RecvTimeoutError;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use opentelemetry::trace::{SpanKind, Status};
use ralphus_core::uri::{RalphusUri, Segment, Token, parse_uri};
use ralphus_core::validate::{ValidationError, validate_toml};
use serde::{Deserialize, Serialize};

use crate::cancel::Cancellations;
use crate::procreg::ProcRegistry;
use crate::runner::{Runner, SubprocessRunner};
use crate::scheduler::Semaphore;
use crate::store::{NodeState, RunState, Store, StoreError};
use crate::summary_worker::SummaryQueue;
use crate::vcs;

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
    /// Set by the `POST /api/daemon/shutdown` handler; polled by
    /// `run_http_loop` (never by `route()`'s own ~100 in-process unit tests)
    /// to break out of the blocking `tiny_http` accept loop and let `serve()`
    /// return, so `main()` can exit normally and drop `_job_guard` — see
    /// `jobobject.rs` for why that's what actually kills every subprocess.
    shutdown: Arc<AtomicBool>,
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
            shutdown: Arc::new(AtomicBool::new(false)),
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

    /// Request that `run_http_loop` stop accepting new requests and return,
    /// so `serve()` returns and the daemon process exits. See the `shutdown`
    /// field's doc comment.
    fn request_shutdown(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
    }

    /// Whether shutdown has been requested — polled by `run_http_loop` only.
    fn shutdown_requested(&self) -> bool {
        self.shutdown.load(Ordering::SeqCst)
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

/// `POST /api/machines` body (RAL-185). Registering a machine provider is an
/// administrative action, deliberately reachable only over this endpoint (and
/// the CLI wrapping it) and never declarable inside a submitted task file —
/// see `crate::machines` for why.
#[derive(Deserialize)]
struct RegisterMachineBody {
    scheme: String,
    #[serde(default)]
    description: String,
    program: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default = "default_protocol_version")]
    protocol_version: i64,
    /// Whether this provider implements the `channel` verb (RAL-185 D7).
    /// Defaults false — a provider is spawned per command unless it says
    /// otherwise.
    #[serde(default)]
    supports_channel: bool,
}

fn default_protocol_version() -> i64 {
    crate::machines::PROTOCOL_VERSION
}

#[derive(Serialize)]
struct MachinesResponse {
    machines: Vec<crate::machines::MachineProviderView>,
    /// Schemes that always resolve without a registry row (`local`,
    /// `ralphus-daemon`), so a client can render them alongside the
    /// registered ones instead of appearing to be missing.
    builtin: Vec<String>,
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
        ("POST", ["api", "daemon", "shutdown"]) => shutdown(daemon, body),
        ("GET", ["api", "tasks"]) => board(daemon, query),
        ("GET", ["api", "projects"]) => list_projects(daemon),
        ("POST", ["api", "projects"]) => register_project(daemon, body),
        ("GET", ["api", "projects", name]) => get_project(daemon, name),
        ("GET", ["api", "projects", name, "validate"]) => validate_project(daemon, name),
        // Machine provider registry (RAL-185).
        ("GET", ["api", "machines"]) => list_machines(daemon),
        ("POST", ["api", "machines"]) => register_machine(daemon, body),
        ("GET", ["api", "machines", scheme]) => get_machine(daemon, scheme),
        ("DELETE", ["api", "machines", scheme]) => deregister_machine(daemon, scheme),
        ("POST", ["api", "machines", scheme, "check"]) => check_machine(daemon, scheme),
        ("POST", ["api", "machines", "cleanup"]) => cleanup_machine(daemon, body),
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
        ("GET", ["api", "resolve"]) => resolve_uri_endpoint(daemon, query),
        ("GET", ["api", "runs", id]) => get_run(daemon, id),
        ("GET", ["api", "runs", id, "worktrees"]) => run_worktrees(daemon, id),
        ("GET", ["api", "runs", id, "logs"]) => run_logs(daemon, id),
        ("GET", ["api", "runs", id, "timeline"]) => run_timeline(daemon, id),
        ("GET", ["api", "runs", id, "graph"]) => run_graph(daemon, id),
        ("POST", ["api", "runs", id, "activate"]) => activate(daemon, id),
        ("POST", ["api", "runs", id, "cancel", "preview"]) => cancel_run_preview(daemon, id),
        ("POST", ["api", "runs", id, "cancel"]) => cancel(daemon, id),
        ("POST", ["api", "runs", id, "set-status"]) => set_status(daemon, id, body),
        ("POST", ["api", "runs", id, "edit"]) => edit_run(daemon, id, body),
        ("POST", ["api", "runs", id, "retry"]) => retry_run(daemon, id),
        ("POST", ["api", "runs", id, "restart", "preview"]) => restart_run_preview(daemon, id),
        ("POST", ["api", "runs", id, "restart"]) => restart_run(daemon, id, body),
        ("POST", ["api", "runs", id, "add-dependency"]) => add_dependency(daemon, id, body),
        ("POST", ["api", "runs", id, "sessions", ti, si, "restart", "preview"]) => {
            restart_session_preview(daemon, id, ti, si)
        }
        ("POST", ["api", "runs", id, "sessions", ti, si, "restart"]) => {
            restart_session(daemon, id, ti, si, body)
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
        ) => restart_session_verify(daemon, id, ti, si, vi, body),
        ("POST", ["api", "runs", id, "tasks", ti, "verify", vi, "restart"]) => {
            restart_task_verify(daemon, id, ti, vi, body)
        }
        ("POST", ["api", "runs", id, "tasks", ti, "restart", "preview"]) => {
            restart_task_preview(daemon, id, ti)
        }
        ("POST", ["api", "runs", id, "tasks", ti, "restart"]) => restart_task(daemon, id, ti, body),
        ("POST", ["api", "runs", id, "env"]) => set_run_env(daemon, id, body),
        // RAL-191: the per-step routes must precede the scope-wide ones, since
        // `["verify", "env"]` and `["verify", vi, "env"]` are otherwise
        // ambiguous to a reader (they are not to the matcher, which is
        // length-sensitive -- but keeping them adjacent and ordered narrow-first
        // makes the layering obvious).
        ("POST", ["api", "runs", id, "tasks", ti, "verify", vi, "env"]) => {
            set_task_verify_step_env(daemon, id, ti, vi, body)
        }
        ("POST", ["api", "runs", id, "tasks", ti, "verify", "env"]) => {
            set_task_verify_env(daemon, id, ti, body)
        }
        ("POST", ["api", "runs", id, "tasks", ti, "env"]) => set_task_env(daemon, id, ti, body),
        ("POST", ["api", "runs", id, "sessions", ti, si, "verify", vi, "env"]) => {
            set_session_verify_step_env(daemon, id, ti, si, vi, body)
        }
        ("POST", ["api", "runs", id, "sessions", ti, si, "verify", "env"]) => {
            set_session_verify_env(daemon, id, ti, si, body)
        }
        ("POST", ["api", "runs", id, "sessions", ti, si, "env"]) => {
            set_session_env(daemon, id, ti, si, body)
        }
        ("POST", ["api", "runs", id, "tasks", ti, "solo"]) => solo_task(daemon, id, ti),
        ("POST", ["api", "runs", id, "tasks", ti, "unsolo"]) => unsolo_task(daemon, id, ti),
        ("POST", ["api", "runs", id, "sessions", ti, si, "open-terminal"]) => {
            open_terminal(daemon, id, ti, si, query)
        }
        ("GET", ["api", "runs", id, "sessions", ti, si, "pane"]) => {
            session_pane(daemon, id, ti, si, query)
        }
        (
            "GET",
            [
                "api",
                "runs",
                id,
                "sessions",
                ti,
                si,
                "terminal-log-attempts",
            ],
        ) => session_terminal_log_attempts(daemon, id, ti, si),
        (
            "GET",
            [
                "api",
                "runs",
                id,
                "sessions",
                ti,
                si,
                "terminal-log-attempts",
                attempt,
            ],
        ) => session_terminal_log_attempt(daemon, id, ti, si, attempt),
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
                "terminal-log-attempts",
            ],
        ) => verify_terminal_log_attempts(daemon, id, task_idx, scope, session_idx, verify_idx),
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
                "terminal-log-attempts",
                attempt,
            ],
        ) => verify_terminal_log_attempt(
            daemon,
            id,
            task_idx,
            scope,
            session_idx,
            verify_idx,
            attempt,
        ),
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
        ("POST", ["api", "guardians", id, "branches", branch_id, "env"]) => {
            set_guardian_branch_env(daemon, id, branch_id, body)
        }
        ("POST", ["api", "guardians", id, "build-env"]) => set_guardian_build_env(daemon, id, body),
        ("POST", ["api", "guardians", id, "manual-checks-env"]) => {
            set_guardian_manual_checks_env(daemon, id, body)
        }
        ("GET", ["api", "guardians", id, "branches", branch_id, "pane"]) => {
            guardian_branch_pane(daemon, id, branch_id, query)
        }
        ("GET", ["api", "guardians", id, "branches", branch_id, "conflicts"]) => {
            guardian_branch_conflicts(daemon, id, branch_id)
        }
        (
            "GET",
            [
                "api",
                "guardians",
                id,
                "branches",
                branch_id,
                "terminal-log-attempts",
            ],
        ) => guardian_branch_terminal_log_attempts(daemon, id, branch_id),
        (
            "GET",
            [
                "api",
                "guardians",
                id,
                "branches",
                branch_id,
                "terminal-log-attempts",
                attempt,
            ],
        ) => guardian_branch_terminal_log_attempt(daemon, id, branch_id, attempt),
        ("POST", ["api", "guardians", id, "manual-checks", "open-terminal"]) => {
            open_guardian_manual_checks_terminal(daemon, id, query)
        }
        ("GET", ["api", "guardians", id, "manual-checks", "pane"]) => {
            guardian_manual_checks_pane(daemon, id, query)
        }
        (
            "GET",
            [
                "api",
                "guardians",
                id,
                "manual-checks",
                "terminal-log-attempts",
            ],
        ) => guardian_manual_checks_terminal_log_attempts(daemon, id),
        (
            "GET",
            [
                "api",
                "guardians",
                id,
                "manual-checks",
                "terminal-log-attempts",
                attempt,
            ],
        ) => guardian_manual_checks_terminal_log_attempt(daemon, id, attempt),
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
        ("POST", ["api", "guardians", id, "resolve-input"]) => {
            guardian_resolve_input(daemon, id, body)
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
        ("GET", ["api", "pull-requests", pr_id, "sync-status"]) => pr_sync_status(daemon, pr_id),
        ("POST", ["api", "pull-requests", pr_id, "pull-from-pr"]) => pr_pull_from_pr(daemon, pr_id),
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

/// Require the declarative inputs a remote-fed review needs, before anything is
/// provisioned (RAL-185 Phase 3b).
///
/// A review normally *infers* each contributing branch and its base by running
/// the VCS against the session's worktree. That read only works on the machine
/// holding it, so a review fed by a remote session must instead be told:
///
/// - the branch, via a `ralphus:new-worktree/<branch>` cwd, and
/// - the base, via `[[review]] base`.
///
/// Checked here rather than inside `crate::reviews::derive_reviews` so it fails
/// before the provider is asked to provision a workspace — a missing `base` is
/// a static authoring mistake, and there is no reason to spend a remote
/// checkout discovering it. `derive_reviews` keeps its own equivalent errors as
/// a backstop for paths that don't come through submit.
fn validate_remote_reviews_are_declarative(
    store: &Store,
    file: &ralphus_core::schema::TaskFile,
) -> std::result::Result<(), String> {
    for task in &file.task {
        for (idx, session) in task.session.iter().enumerate() {
            let Some(review_id) = session.review.as_deref().filter(|r| !r.trim().is_empty()) else {
                continue;
            };
            let machine = ralphus_core::schema::resolve_session_machine(task, session);
            if store
                .resolve_machine(machine.as_deref())
                .is_ok_and(|m| m.is_local())
            {
                continue;
            }
            let sid = session
                .id
                .clone()
                .unwrap_or_else(|| format!("session-{idx}"));
            let machine_label = machine.as_deref().unwrap_or("local");
            if session
                .cwd
                .as_deref()
                .and_then(ralphus_core::schema::parse_worktree_placeholder)
                .is_none()
            {
                return Err(format!(
                    "session \"{sid}\" runs on machine \"{machine_label}\" and opts into review                      \"{review_id}\", so its branch must be knowable without reading that                      machine's filesystem. Give it a cwd of the form                      \"ralphus:new-worktree/<branch>\" instead of a literal path."
                ));
            }
            let declared = file
                .review
                .iter()
                .find(|r| r.id.as_deref() == Some(review_id))
                .and_then(|r| r.base.as_deref())
                .map(str::trim)
                .filter(|b| !b.is_empty());
            if declared.is_none() {
                return Err(format!(
                    "review \"{review_id}\" is fed by session \"{sid}\" running on machine                      \"{machine_label}\", so its base branch cannot be read from that worktree's                      upstream — this daemon cannot see another machine's filesystem. Declare it                      explicitly: [[review]] base = \"main\"."
                ));
            }
        }
    }
    Ok(())
}

/// Enforce the within-task machine affinity rule (RAL-185 Phase 3a).
///
/// Every session under one task — and every verify step under those sessions,
/// plus the task's own verify steps — must resolve to the **same** machine.
/// A task is one unit of work sharing one workspace: sessions hand off through
/// files on disk, and a task-scope verify runs against whatever its sessions
/// produced. Splitting that across machines would silently verify an empty or
/// stale directory.
///
/// Different *tasks* may freely use different machines, and a review's machine
/// is independent of all of them — that is what makes a fan-out across a build
/// farm possible in the first place.
///
/// Checked at submit rather than discovered at run time: by the point the
/// scheduler noticed, the first session would already have run somewhere.
fn validate_machine_affinity(
    file: &ralphus_core::schema::TaskFile,
) -> std::result::Result<(), String> {
    for task in &file.task {
        // The task's own value is the baseline every member must agree with;
        // when it is unset, the first session that names one sets the
        // expectation for the rest.
        let mut expected: Option<(String, String)> = task
            .machine
            .as_deref()
            .map(str::trim)
            .filter(|m| !m.is_empty())
            .map(|m| (m.to_string(), format!("task \"{}\"", task.name)));

        let mut check = |machine: Option<String>,
                         whose: String|
         -> std::result::Result<(), String> {
            let Some(m) = machine.filter(|m| !m.trim().is_empty()) else {
                return Ok(());
            };
            match &expected {
                Some((first, first_whose)) if first != &m => Err(format!(
                    "task \"{}\" spans two machines: {first_whose} runs on \"{first}\" but {whose} \
                     runs on \"{m}\". Every session and verify step under one task must resolve to \
                     the same machine — they share one workspace, so splitting them would verify a \
                     directory the other half never wrote to. Different tasks may use different \
                     machines.",
                    task.name
                )),
                Some(_) => Ok(()),
                None => {
                    expected = Some((m, whose));
                    Ok(())
                }
            }
        };

        for (idx, session) in task.session.iter().enumerate() {
            let sid = session
                .id
                .clone()
                .unwrap_or_else(|| format!("session-{idx}"));
            check(
                ralphus_core::schema::resolve_session_machine(task, session),
                format!("session \"{sid}\""),
            )?;
            for (vi, verify) in session.verify.iter().enumerate() {
                check(
                    ralphus_core::schema::resolve_session_verify_machine(task, session, verify),
                    format!("session \"{sid}\" verify step #{vi}"),
                )?;
            }
        }
        for (vi, verify) in task.verify.iter().enumerate() {
            check(
                ralphus_core::schema::resolve_task_verify_machine(task, verify),
                format!("task verify step #{vi}"),
            )?;
        }
    }
    Ok(())
}

fn validate_machines_registered(
    store: &Store,
    file: &ralphus_core::schema::TaskFile,
) -> std::result::Result<(), String> {
    let check = |machine: Option<&str>, whose: &str| -> std::result::Result<(), String> {
        store
            .resolve_machine(machine)
            .map(|_| ())
            .map_err(|e| format!("{whose}: {e}"))
    };
    for task in &file.task {
        check(task.machine.as_deref(), &format!("task \"{}\"", task.name))?;
        for verify in &task.verify {
            check(
                verify.machine.as_deref(),
                &format!("task \"{}\" verify step", task.name),
            )?;
        }
        for session in &task.session {
            let sid = session.id.as_deref().unwrap_or("<unnamed>");
            check(
                session.machine.as_deref(),
                &format!("task \"{}\" session \"{sid}\"", task.name),
            )?;
            for verify in &session.verify {
                check(
                    verify.machine.as_deref(),
                    &format!("task \"{}\" session \"{sid}\" verify step", task.name),
                )?;
            }
        }
    }
    for review in &file.review {
        let rid = review.id.as_deref().unwrap_or("<unnamed>");
        check(review.machine.as_deref(), &format!("review \"{rid}\""))?;
    }
    Ok(())
}

/// Validate a project's on-disk path/vcs kind without persisting anything.
/// Shared by `register_project` (validate-then-write) and the read-only
/// `GET /api/projects/{name}/validate` endpoint (validate-only, RAL-101) so
/// the two can never drift on what counts as a valid project location.
fn validate_project_location(path: &str, kind: &str) -> Result<(), String> {
    let adapter = vcs::for_kind(kind).ok_or_else(|| {
        format!(
            "unsupported vcs kind \"{kind}\" (only \"{}\" is implemented)",
            vcs::DEFAULT_KIND
        )
    })?;
    let dir = Path::new(path);
    if !dir.is_dir() {
        return Err(format!(
            "path \"{path}\" does not exist or is not a directory"
        ));
    }
    // Routed through the VCS adapter (RAL-213) rather than a raw `git`
    // invocation, so a project registered with a different `vcs` kind is
    // checked by that kind's own adapter instead of assuming git.
    if !adapter.is_repository(dir) {
        return Err(format!("path \"{path}\" is not a {kind} repository"));
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

/// `POST /api/machines`: register (or update) a machine provider (RAL-185).
fn register_machine(daemon: &Daemon, body: &str) -> Reply {
    let Ok(req) = serde_json::from_str::<RegisterMachineBody>(body) else {
        return error(
            400,
            "bad_request",
            "body must include \"scheme\" and \"program\" strings",
            vec![],
        );
    };
    let scheme = req.scheme.trim();
    if scheme.is_empty() {
        return error(400, "invalid_value", "'scheme' must not be empty", vec![]);
    }
    // The scheme has to be parseable as the left half of a `machine` value, or
    // it could be registered but never referenceable.
    if let Err(e) = ralphus_core::schema::parse_machine(&format!("{scheme}:probe")) {
        return error(
            400,
            "invalid_value",
            &format!("'scheme' {scheme:?} is not a usable machine scheme ({e:?})"),
            vec![],
        );
    }
    if crate::machines::is_unregistrable_scheme(scheme) {
        return error(
            400,
            "invalid_value",
            &format!(
                "'{scheme}' cannot be registered: built-in schemes are {}, and {} are reserved for other meanings",
                crate::machines::BUILTIN_SCHEMES.join(", "),
                crate::machines::RESERVED_SCHEMES.join(", ")
            ),
            vec![],
        );
    }
    if req.program.trim().is_empty() {
        return error(400, "invalid_value", "'program' must not be empty", vec![]);
    }
    match daemon.lock().register_machine_provider(
        scheme,
        &req.description,
        req.program.trim(),
        &req.args,
        req.protocol_version,
        req.supports_channel,
    ) {
        Ok(()) => json(201, &serde_json::json!({"scheme": scheme.to_lowercase()})),
        Err(e) => store_error(&e),
    }
}

/// `POST /api/machines/{scheme}/check`: probe a provider for reachability
/// (RAL-185 Q3).
///
/// Explicit and on demand rather than polled: a probe spawns the provider
/// program, and a board refreshing every couple of seconds must not turn that
/// into steady load on a build farm. The result is stored, so the tab shows the
/// last known answer with its timestamp instead of implying live truth.
///
/// The built-in `local` scheme is not probeable — the daemon's own host is
/// reachable by definition, and pretending otherwise would be theatre.
fn check_machine(daemon: &Daemon, scheme: &str) -> Reply {
    let provider = {
        let store = daemon.lock();
        match crate::remote_runner::provider_from_store(&store, &format!("{scheme}:probe")) {
            Ok(Some(p)) => p,
            Ok(None) => {
                return json(
                    200,
                    &serde_json::json!({"ok": true, "note": "local host — always reachable"}),
                );
            }
            Err(e) => return error(400, "invalid_value", &e, vec![]),
        }
    };
    // A probe belongs to no run; the spec exists only to satisfy the invocation
    // shape (env overrides, ids for the provider's own logging).
    let spec = crate::runner::RunnerSpec::for_command_verify(
        "machine-check",
        "machine-check",
        scheme,
        ".",
        "",
        "claude",
        Some(30),
    );
    let (ok, note) = match provider.ping(&spec) {
        Ok(detail) => (true, detail),
        Err(e) => (false, Some(e)),
    };
    {
        let store = daemon.lock();
        let _ = store.record_machine_check(scheme, ok, note.as_deref());
        // RAL-201: `ping` previously only wrote to `machine_providers`'
        // reachability columns -- no Cartographer record at all, so a probe
        // never showed up in the queryable event log.
        crate::cartographer::Note::new("remote")
            .level(if ok {
                crate::logging::LogLevel::INFO
            } else {
                crate::logging::LogLevel::WARNING
            })
            .scope("machine")
            .emit(
                &store,
                "machine ping",
                serde_json::json!({"scheme": scheme, "ok": ok, "note": note}),
            );
    }
    json(200, &serde_json::json!({"ok": ok, "note": note}))
}

#[derive(Deserialize)]
struct CleanupMachineBody {
    /// Full `<scheme>:<uri>` value naming the workspace to tear down -- not
    /// just a scheme, since a provider may back many independent workspaces
    /// (one per `uri`) and `cleanup` operates on exactly one of them.
    machine: String,
}

/// `POST /api/machines/cleanup`: tear down one provisioned workspace
/// (RAL-201). Body `{"machine": "<scheme>:<uri>"}`.
///
/// **Never invoked automatically** — see [`crate::remote_runner::ProviderRunner::cleanup`]'s
/// doc for the retention-on-failure policy this mirrors. This is the explicit,
/// operator-initiated action an admin takes once they are actually done with a
/// workspace, the same way nobody automatically deletes a local
/// `.git/.ralphus_worktrees/<branch>` directory either.
fn cleanup_machine(daemon: &Daemon, body: &str) -> Reply {
    let Ok(req) = serde_json::from_str::<CleanupMachineBody>(body) else {
        return error(
            400,
            "bad_request",
            "body must be {\"machine\": \"...\"}",
            vec![],
        );
    };
    let machine = req.machine.trim();
    let provider = {
        let store = daemon.lock();
        match crate::remote_runner::provider_from_store(&store, machine) {
            Ok(Some(p)) => p,
            Ok(None) => {
                return error(
                    400,
                    "invalid_value",
                    "\"local\" has no workspace to clean up -- cleanup only applies to a \
                     registered machine provider",
                    vec![],
                );
            }
            Err(e) => return error(400, "invalid_value", &e, vec![]),
        }
    };
    let spec = crate::runner::RunnerSpec::for_command_verify(
        "machine-cleanup",
        "machine-cleanup",
        machine,
        ".",
        "",
        "claude",
        Some(60),
    );
    let result = provider.cleanup(&spec);
    let guard = daemon.lock();
    let _ = guard.cartographer_log(crate::cartographer::CartographerEntry {
        level: if result.is_ok() {
            crate::logging::LogLevel::INFO
        } else {
            crate::logging::LogLevel::WARNING
        },
        source: "remote",
        message: "machine cleanup",
        scope: Some("machine"),
        run_id: None,
        guardian_id: None,
        session_id: None,
        task: None,
        log_path: None,
        payload: serde_json::json!({
            "machine": machine,
            "ok": result.is_ok(),
            "error": result.as_ref().err(),
        }),
    });
    match result {
        Ok(()) => json(200, &serde_json::json!({"ok": true})),
        // RAL-201: the workspace is left exactly as it was on failure -- there
        // is no daemon-side record of it to roll back or discard (`provision`
        // re-derives the same workspace deterministically every time), so the
        // only responsibility here is surfacing the real reason rather than
        // swallowing it.
        Err(e) => error(502, "provider_error", &e, vec![]),
    }
}

/// `GET /api/machines`: every registered provider plus the built-in schemes.
fn list_machines(daemon: &Daemon) -> Reply {
    match daemon.lock().list_machine_providers() {
        Ok(machines) => json(
            200,
            &MachinesResponse {
                machines,
                builtin: crate::machines::BUILTIN_SCHEMES
                    .iter()
                    .map(|s| (*s).to_string())
                    .collect(),
            },
        ),
        Err(e) => store_error(&e),
    }
}

/// `GET /api/machines/{scheme}`.
fn get_machine(daemon: &Daemon, scheme: &str) -> Reply {
    match daemon.lock().get_machine_provider(scheme) {
        Ok(Some(m)) => json(200, &m),
        Ok(None) => error(
            404,
            "not_found",
            &format!("machine provider \"{scheme}\" is not registered"),
            vec![],
        ),
        Err(e) => store_error(&e),
    }
}

/// `DELETE /api/machines/{scheme}`.
fn deregister_machine(daemon: &Daemon, scheme: &str) -> Reply {
    match daemon.lock().deregister_machine_provider(scheme) {
        Ok(true) => json(200, &serde_json::json!({"deleted": true})),
        Ok(false) => error(
            404,
            "not_found",
            &format!("machine provider \"{scheme}\" is not registered"),
            vec![],
        ),
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

    // Every `machine` value must name a registered provider at a supported
    // contract version (RAL-185). Core validated the syntax offline; only the
    // daemon can see the registry.
    if let Err(msg) = validate_machines_registered(&daemon.lock(), &file) {
        return error(400, "machine_validation_failed", &msg, vec![]);
    }
    // Phase 3a: every session/verify under one task must share a machine.
    if let Err(msg) = validate_machine_affinity(&file) {
        return error(400, "machine_validation_failed", &msg, vec![]);
    }
    // Phase 3b: a remote-fed review must declare what cannot be discovered.
    if let Err(msg) = validate_remote_reviews_are_declarative(&daemon.lock(), &file) {
        return error(400, "machine_validation_failed", &msg, vec![]);
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

// ── RAL-188: ralphus URI resolution ──────────────────────────────────────────

/// The positional coordinates a ralphus URI resolves to — the addressing the
/// rest of this API is built on (`/api/runs/{id}/sessions/{ti}/{si}/pane` and
/// friends).
///
/// RAL-188 §C.7: the URI itself can never *be* a REST path, because the `/`
/// between its segments cannot sit in a path segment without percent-encoding
/// as `%2F` (routinely normalized or rejected by HTTP stacks and proxies). So
/// it travels as a query value and this endpoint hands back the coordinates;
/// every existing positional route is untouched.
#[derive(Serialize)]
struct ResolvedUri {
    /// The canonical URI for whatever was resolved, rebuilt from the entity's
    /// *current* labels and always carrying the `?id=` sidecar (§C.3). A
    /// caller that resolved a stale/renamed label gets the fresh form back.
    uri: String,
    /// `run` | `task` | `session` | `verify` | `review`.
    kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    run_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    task_idx: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_idx: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    verify_idx: Option<usize>,
    /// `task` or `session` — which scope the addressed verify step lives in.
    #[serde(skip_serializing_if = "Option::is_none")]
    verify_scope: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    guardian_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    branch_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    branch: Option<String>,
    /// The URI addressed the review's combined worktree (`?combined`) rather
    /// than one stacked branch.
    combined: bool,
}

impl ResolvedUri {
    fn new(kind: &str, uri: String) -> Self {
        Self {
            uri,
            kind: kind.to_string(),
            run_id: None,
            task_idx: None,
            session_idx: None,
            verify_idx: None,
            verify_scope: None,
            guardian_id: None,
            branch_id: None,
            branch: None,
            combined: false,
        }
    }
}

/// A URI was well-formed but did not resolve. Carried separately from
/// [`UriError`] so a *parse* failure (400) and an *ambiguous or missing entity*
/// (404 / 409) get different status codes.
struct ResolveError {
    status: u16,
    code: &'static str,
    message: String,
}

fn not_found(message: String) -> ResolveError {
    ResolveError {
        status: 404,
        code: "not_found",
        message,
    }
}

fn ambiguous(message: String) -> ResolveError {
    ResolveError {
        status: 409,
        code: "ambiguous_uri",
        message,
    }
}

/// Read the `uri=` query value, taking **everything after it** rather than
/// stopping at the next `&`.
///
/// A ralphus URI legitimately contains `&` (`?id=…&combined`), and callers
/// hand-writing one by pasting it into a URL is the common case this scheme
/// exists to support. Percent-encoded input works identically, since `%26`
/// simply decodes back to `&`. Any trailing `&key=` a caller appends therefore
/// becomes part of the URI and fails loudly as an unknown query key rather
/// than being silently dropped.
fn uri_query_value(query: &str) -> Option<&str> {
    if let Some(rest) = query.strip_prefix("uri=") {
        return Some(rest);
    }
    query.find("&uri=").map(|at| &query[at + 5..])
}

/// The addressable name for each verify step: its author-supplied `id`, or
/// empty for an anonymous step — addressable only as `~N` (RAL-188 §C.4).
fn verify_candidates(steps: &[crate::store::VerifyView]) -> Vec<String> {
    steps
        .iter()
        .map(|s| s.id.clone().unwrap_or_default())
        .collect()
}

/// Find the run a `RUN[label]` segment names when the URI carried no `?id=`.
///
/// Matches a run's label *or* its id, since a run with no label renders as its
/// id (§C.2). Ambiguity lists the candidates and points at `?id=`; it is never
/// silently resolved to the most recent (§C.3).
fn run_id_for_label(store: &MutexGuard<'_, Store>, label: &str) -> Result<String, ResolveError> {
    let runs = store.list_runs().map_err(|e| ResolveError {
        status: 500,
        code: "internal",
        message: e.to_string(),
    })?;
    let matches: Vec<&crate::store::RunView> = runs
        .iter()
        .filter(|r| r.id == label || r.label.as_deref() == Some(label))
        .collect();
    match matches.as_slice() {
        [] => Err(not_found(format!("no run labelled '{label}'"))),
        [only] => Ok(only.id.clone()),
        many => {
            let ids: Vec<&str> = many.iter().map(|r| r.id.as_str()).collect();
            Err(ambiguous(format!(
                "'{label}' matches {} runs ({}) -- add '?id=<run id>' to say which one",
                many.len(),
                ids.join(", ")
            )))
        }
    }
}

/// Rebuild the canonical URI for a resolved run-family entity from the run's
/// current names, falling back to `~N` at any level that has no name of its own.
fn canonical_run_uri(
    run: &crate::store::RunView,
    task_idx: Option<usize>,
    session_idx: Option<usize>,
    verify_idx: Option<usize>,
) -> String {
    let label = run
        .label
        .clone()
        .filter(|l| !l.is_empty())
        .unwrap_or_else(|| run.id.clone());
    let mut segments = vec![Segment::named("RUN", label)];
    if let Some(ti) = task_idx {
        segments.push(match run.tasks.get(ti) {
            Some(t) if !t.name.is_empty() => Segment::named("TASK", t.name.clone()),
            _ => Segment::positional("TASK", ti),
        });
        let steps = if let Some(si) = session_idx {
            let session = run.tasks.get(ti).and_then(|t| t.sessions.get(si));
            segments.push(match session {
                Some(s) => {
                    let name = s
                        .name
                        .clone()
                        .filter(|n| !n.is_empty())
                        .unwrap_or_else(|| s.id.clone());
                    if name.is_empty() {
                        Segment::positional("SESSION", si)
                    } else {
                        Segment::named("SESSION", name)
                    }
                }
                None => Segment::positional("SESSION", si),
            });
            session.map(|s| s.verify.as_slice()).unwrap_or_default()
        } else {
            run.tasks
                .get(ti)
                .map(|t| t.verify.as_slice())
                .unwrap_or_default()
        };
        if let Some(vi) = verify_idx {
            segments.push(match steps.get(vi).and_then(|s| s.id.as_deref()) {
                Some(id) if !id.is_empty() => Segment::named("VERIFY", id),
                _ => Segment::positional("VERIFY", vi),
            });
        }
    }
    RalphusUri {
        segments,
        query: vec![("id".to_string(), Some(run.id.clone()))],
    }
    .to_string()
}

fn resolve_run_uri(
    store: &MutexGuard<'_, Store>,
    uri: &RalphusUri,
) -> Result<ResolvedUri, ResolveError> {
    let run_seg = uri
        .segment("RUN")
        .ok_or_else(|| not_found("no RUN[...] segment".to_string()))?;
    if run_seg.index.is_some() {
        return Err(ResolveError {
            status: 400,
            code: "bad_uri",
            message: "RUN[~N] is not addressable -- a run has no stable position. \
                      Use its label or id (and ideally '?id=<run id>')."
                .to_string(),
        });
    }
    let run_id = match uri.id() {
        Some(id) => id.to_string(),
        None => run_id_for_label(store, run_seg.name.as_deref().unwrap_or(""))?,
    };
    let run = store
        .get_run(&run_id)
        .map_err(|_| not_found(format!("no run '{run_id}'")))?;

    let Some(task_seg) = uri.segment("TASK") else {
        let mut out = ResolvedUri::new("run", canonical_run_uri(&run, None, None, None));
        out.run_id = Some(run.id.clone());
        return Ok(out);
    };
    let task_names: Vec<String> = run.tasks.iter().map(|t| t.name.clone()).collect();
    let task_idx = task_seg
        .token()
        .resolve(&task_names, "task")
        .map_err(|e| ambiguous(e.to_string()))?;
    let task = &run.tasks[task_idx];

    let verify_seg = uri.segment("VERIFY");
    let Some(session_seg) = uri.segment("SESSION") else {
        // No SESSION segment: either the task itself, or one of its own
        // task-scope verify steps.
        let Some(verify_seg) = verify_seg else {
            let mut out =
                ResolvedUri::new("task", canonical_run_uri(&run, Some(task_idx), None, None));
            out.run_id = Some(run.id.clone());
            out.task_idx = Some(task_idx);
            return Ok(out);
        };
        let verify_idx = verify_seg
            .token()
            .resolve(&verify_candidates(&task.verify), "task verify")
            .map_err(|e| ambiguous(e.to_string()))?;
        let mut out = ResolvedUri::new(
            "verify",
            canonical_run_uri(&run, Some(task_idx), None, Some(verify_idx)),
        );
        out.run_id = Some(run.id.clone());
        out.task_idx = Some(task_idx);
        out.verify_idx = Some(verify_idx);
        out.verify_scope = Some("task".to_string());
        return Ok(out);
    };

    let session_names: Vec<String> = task
        .sessions
        .iter()
        .map(|s| {
            s.name
                .clone()
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| s.id.clone())
        })
        .collect();
    let session_idx = session_seg
        .token()
        .resolve(&session_names, "session")
        .map_err(|e| ambiguous(e.to_string()))?;

    let Some(verify_seg) = verify_seg else {
        let mut out = ResolvedUri::new(
            "session",
            canonical_run_uri(&run, Some(task_idx), Some(session_idx), None),
        );
        out.run_id = Some(run.id.clone());
        out.task_idx = Some(task_idx);
        out.session_idx = Some(session_idx);
        return Ok(out);
    };
    let session = &task.sessions[session_idx];
    let verify_idx = verify_seg
        .token()
        .resolve(&verify_candidates(&session.verify), "session verify")
        .map_err(|e| ambiguous(e.to_string()))?;
    let mut out = ResolvedUri::new(
        "verify",
        canonical_run_uri(&run, Some(task_idx), Some(session_idx), Some(verify_idx)),
    );
    out.run_id = Some(run.id.clone());
    out.task_idx = Some(task_idx);
    out.session_idx = Some(session_idx);
    out.verify_idx = Some(verify_idx);
    out.verify_scope = Some("session".to_string());
    Ok(out)
}

fn resolve_review_uri(
    store: &MutexGuard<'_, Store>,
    uri: &RalphusUri,
) -> Result<ResolvedUri, ResolveError> {
    let seg = uri
        .segment("REVIEW")
        .ok_or_else(|| not_found("no REVIEW[...] segment".to_string()))?;
    if seg.index.is_some() {
        return Err(ResolveError {
            status: 400,
            code: "bad_uri",
            message: "REVIEW[~N] is not addressable -- a review has no stable position. \
                      Use its name or id (and ideally '?id=<guardian id>')."
                .to_string(),
        });
    }
    let name = seg.name.clone().unwrap_or_default();
    let guardian_id = match uri.id() {
        Some(id) => id.to_string(),
        None => {
            let guardians = store.list_guardians().map_err(|e| ResolveError {
                status: 500,
                code: "internal",
                message: e.to_string(),
            })?;
            let matches: Vec<&crate::guardian::GuardianView> = guardians
                .iter()
                .filter(|g| g.id == name || g.name == name)
                .collect();
            match matches.as_slice() {
                [] => {
                    let options: Vec<&str> = guardians.iter().map(|g| g.name.as_str()).collect();
                    let options = if options.is_empty() {
                        "(none)".to_string()
                    } else {
                        options.join(", ")
                    };
                    return Err(not_found(format!(
                        "no review named '{name}' (available: {options})"
                    )));
                }
                [only] => only.id.clone(),
                many => {
                    let ids: Vec<&str> = many.iter().map(|g| g.id.as_str()).collect();
                    return Err(ambiguous(format!(
                        "'{name}' matches {} reviews -- add '?id=<guardian id>' to say which one ({})",
                        many.len(),
                        ids.join(", ")
                    )));
                }
            }
        }
    };
    let guardian = store
        .get_guardian(&guardian_id)
        .map_err(|_| not_found(format!("no review '{guardian_id}'")))?;

    let display = if guardian.name.is_empty() {
        guardian.id.clone()
    } else {
        guardian.name.clone()
    };
    let mut query = vec![("id".to_string(), Some(guardian.id.clone()))];
    let combined = uri.has("combined");

    let Some(worktree) = uri.get("worktree") else {
        if combined {
            query.push(("combined".to_string(), None));
        }
        let canonical = RalphusUri {
            segments: vec![Segment::named("REVIEW", display)],
            query,
        }
        .to_string();
        let mut out = ResolvedUri::new("review", canonical);
        out.guardian_id = Some(guardian.id.clone());
        out.combined = combined;
        return Ok(out);
    };

    let token = Token::parse(worktree).map_err(|e| ResolveError {
        status: 400,
        code: "bad_uri",
        message: format!("'?worktree={worktree}': {e}"),
    })?;
    let branch = match &token {
        // A branch's `position` is a display/reorder attribute, so `~N` means
        // "the branch currently at position N", matched against `position`
        // rather than the vector index they normally agree with.
        Token::Index(position) => guardian
            .branches
            .iter()
            .find(|b| usize::try_from(b.position).ok() == Some(*position))
            .ok_or_else(|| not_found(format!("no branch at position {position} in this review")))?,
        // A worktree's label is its feature branch name -- what the Reviews UI
        // lists it under. Its stable id (`branch-000000000001`, RAL-122) is
        // accepted too, mirroring §C.2's "the label, falling back to its id"
        // at every other level of the grammar; an id match wins outright,
        // since ids are unique and a branch name need not be.
        Token::Name(name) => match guardian.branches.iter().find(|b| &b.id == name) {
            Some(by_id) => by_id,
            None => {
                let names: Vec<String> =
                    guardian.branches.iter().map(|b| b.branch.clone()).collect();
                let idx = token
                    .resolve(&names, "branch")
                    .map_err(|e| ambiguous(e.to_string()))?;
                &guardian.branches[idx]
            }
        },
    };

    query.push(("worktree".to_string(), Some(branch.branch.clone())));
    let canonical = RalphusUri {
        segments: vec![Segment::named("REVIEW", display)],
        query,
    }
    .to_string();
    let mut out = ResolvedUri::new("review", canonical);
    out.guardian_id = Some(guardian.id.clone());
    out.branch_id = Some(branch.id.clone());
    out.branch = Some(branch.branch.clone());
    Ok(out)
}

/// `GET /api/resolve?uri=<ralphus URI>` — turn a RAL-188 URI into the
/// positional coordinates the rest of this API speaks (§C.7).
fn resolve_uri_endpoint(daemon: &Daemon, query: &str) -> Reply {
    let Some(raw) = uri_query_value(query) else {
        return error(
            400,
            "bad_request",
            "GET /api/resolve needs a '?uri=' parameter",
            vec![],
        );
    };
    let decoded = url_decode(raw);
    let uri = match parse_uri(&decoded) {
        Ok(uri) => uri,
        Err(e) => return error(400, "bad_uri", &e.to_string(), vec![]),
    };
    let store = daemon.lock();
    let result = if uri.kinds().first() == Some(&"REVIEW") {
        resolve_review_uri(&store, &uri)
    } else {
        resolve_run_uri(&store, &uri)
    };
    match result {
        Ok(resolved) => json(200, &resolved),
        Err(e) => error(e.status, e.code, &e.message, vec![]),
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
/// `session_id`, `task`, `q` (substring match on message), `since_ms`,
/// `until_ms`, `limit` (default 100, max 1000), `offset`, `sort` (`asc`/`desc`,
/// default `desc`) — plus `entity`, a single-string URI (RAL-155, see
/// `crate::entity_uri`) that addresses a run/task/session/verify/guardian
/// uniformly. `entity` is resolved into the equivalent `run_id`/`task`/
/// `session_id`/`guardian_id` filter fields (via `Store::task_name_at`/
/// `session_sid_at`, since Cartographer's `task`/`session_id` columns store
/// the task's *name*/session's *sid*, not the index an entity URI addresses
/// by) and composes with any of those fields also given explicitly — an
/// explicit field always wins if both are present and disagree, since it's
/// the more specific ask.
fn cartographer_query(daemon: &Daemon, query: &str) -> Reply {
    let limit = query_param(query, "limit")
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(100);
    let offset = query_param(query, "offset")
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(0);
    let mut filter = crate::cartographer::CartographerFilter {
        source: query_filter(query, "source"),
        scope: query_filter(query, "scope"),
        level: query_filter(query, "level"),
        run_id: query_filter(query, "run_id"),
        guardian_id: query_filter(query, "guardian_id"),
        session_id: query_filter(query, "session_id"),
        task: query_filter(query, "task"),
        q: query_filter(query, "q"),
        since_ms: query_param(query, "since_ms").and_then(|s| s.parse::<i64>().ok()),
        until_ms: query_param(query, "until_ms").and_then(|s| s.parse::<i64>().ok()),
        limit,
        offset,
        ascending: query_param(query, "sort") == Some("asc"),
    };
    let guard = daemon.lock();
    if let Some(entity) = query_filter(query, "entity") {
        let Some(parsed) = crate::entity_uri::parse(&entity) else {
            return error(400, "bad_request", "invalid entity uri", vec![]);
        };
        if let Err(e) = apply_entity_filter(&guard, &mut filter, &parsed) {
            return store_error(&e);
        }
    }
    match guard.cartographer_query(&filter) {
        Ok(page) => json(200, &page),
        Err(e) => store_error(&e),
    }
}

/// Fold an [`crate::entity_uri::EntityUri`]'s addressing coordinates into a
/// [`crate::cartographer::CartographerFilter`] in place, resolving the
/// index-addressed task/session down to the name/sid Cartographer's columns
/// actually store. Only overwrites a field the caller didn't already set
/// explicitly (see `cartographer_query`'s doc comment).
fn apply_entity_filter(
    store: &Store,
    filter: &mut crate::cartographer::CartographerFilter,
    entity: &crate::entity_uri::EntityUri,
) -> crate::store::Result<()> {
    use crate::entity_uri::EntityUri;
    if let Some(run_id) = entity.run_id() {
        filter.run_id.get_or_insert_with(|| run_id.to_string());
    }
    match entity {
        EntityUri::Run { .. } => {}
        EntityUri::Task { run_id, task_idx }
        | EntityUri::Verify {
            run_id, task_idx, ..
        } => {
            if filter.task.is_none() {
                filter.task = store.task_name_at(run_id, *task_idx)?;
            }
            if let EntityUri::Verify {
                run_id,
                task_idx,
                session_idx,
                ..
            } = entity
            {
                if filter.session_id.is_none() && *session_idx >= 0 {
                    filter.session_id = store.session_sid_at(run_id, *task_idx, *session_idx)?;
                }
            }
        }
        EntityUri::Session {
            run_id,
            task_idx,
            session_idx,
        } => {
            if filter.task.is_none() {
                filter.task = store.task_name_at(run_id, *task_idx)?;
            }
            if filter.session_id.is_none() {
                filter.session_id = store.session_sid_at(run_id, *task_idx, *session_idx)?;
            }
        }
        EntityUri::Guardian { guardian_id } => {
            filter
                .guardian_id
                .get_or_insert_with(|| guardian_id.to_string());
        }
    }
    Ok(())
}

/// Generate and return the merged, chronological "uber-log-viewer" timeline
/// for a whole run (RAL-155). A side effect of every call: the rendered text
/// is (best-effort) written to a temp file — see `crate::timeline::RunTimeline::file_path`
/// — so the board's "generate to file, then display" button and this JSON
/// response come from the exact same generation, not two separate code paths.
fn run_timeline(daemon: &Daemon, id: &str) -> Reply {
    match crate::timeline::build_run_timeline(&daemon.lock(), id) {
        Ok(timeline) => json(200, &timeline),
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

/// Solo a task within a run (RAL-157): pauses every other task in the run
/// (their not-yet-started sessions won't be dispatched) until un-soloed.
/// Returns the refreshed [`RunView`] so the board reflects the new `soloed`
/// flag in the same round trip.
fn solo_task(daemon: &Daemon, id: &str, ti: &str) -> Reply {
    let Ok(task_idx) = ti.parse::<i64>() else {
        return error(400, "bad_request", "task index must be an integer", vec![]);
    };
    let guard = daemon.lock();
    match guard.solo_task(id, task_idx) {
        Ok(()) => match guard.get_run(id) {
            Ok(run) => json(200, &run),
            Err(e) => store_error(&e),
        },
        Err(e) => store_error(&e),
    }
}

/// Un-solo a task (RAL-157) — the reverse of [`solo_task`]. Returns the
/// refreshed [`RunView`].
fn unsolo_task(daemon: &Daemon, id: &str, ti: &str) -> Reply {
    let Ok(task_idx) = ti.parse::<i64>() else {
        return error(400, "bad_request", "task index must be an integer", vec![]);
    };
    let guard = daemon.lock();
    match guard.unsolo_task(id, task_idx) {
        Ok(()) => match guard.get_run(id) {
            Ok(run) => json(200, &run),
            Err(e) => store_error(&e),
        },
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

#[derive(Deserialize, Default)]
struct EnvOverridesBody {
    #[serde(default)]
    set: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    unset: Vec<String>,
}

/// Add/replace (`set`) and remove (`unset`) a run's persistent
/// environment-variable overrides (RAL-150, ticket Q1/Q4). Does *not* itself
/// retry anything — the overrides just sit on the run until the scheduler
/// next executes one of its sessions/verify steps (see
/// `RunnerSpec::env_overrides` and `run_command_verify_capture`'s `env`
/// param); pair this with `POST .../retry` or `POST .../restart` to actually
/// re-run something under the new values, exactly as the CLI's `ralphus
/// retry <selector> --environment ...` does.
///
/// Every key in `set`/`unset` must be a valid environment-variable
/// identifier (`[A-Za-z_][A-Za-z0-9_]*`) — rejected with 400 otherwise, both
/// to catch obvious typos early and because
/// `crate::tmux::build_command_line_with_env` relies on this same validation
/// as a shell-injection backstop for the tmux-wrapped runner path.
///
/// Logs a Cartographer record with the changed key names in the clear but
/// **values redacted unless the key is in the project's
/// `[env_overrides].allowlist`** (ticket Q3) — a values-in-plaintext audit
/// log would defeat the entire point of the allowlist for a secret-carrying
/// override (an API key, a token).
fn set_run_env(daemon: &Daemon, id: &str, body: &str) -> Reply {
    let Ok(req) = serde_json::from_str::<EnvOverridesBody>(body) else {
        return error(400, "bad_request", "invalid env body", vec![]);
    };
    if req.set.is_empty() && req.unset.is_empty() {
        return error(
            400,
            "bad_request",
            "at least one of `set`/`unset` is required",
            vec![],
        );
    }
    for key in req.set.keys().chain(req.unset.iter()) {
        if !crate::config::is_valid_env_key(key) {
            return error(
                400,
                "bad_request",
                &format!("invalid environment variable name: {key:?}"),
                vec![],
            );
        }
    }
    let store = daemon.lock();
    if let Err(e) = store.run_state(id) {
        return store_error(&e);
    }
    let result = match store.set_run_env_overrides(id, &req.set, &req.unset) {
        Ok(m) => m,
        Err(e) => return store_error(&e),
    };
    let allow = crate::config::load_env_overrides_config();
    let redacted_set: serde_json::Map<String, serde_json::Value> = req
        .set
        .iter()
        .map(|(k, v)| {
            let shown = if allow.is_allowed(k) {
                v.clone()
            } else {
                "<redacted>".to_string()
            };
            (k.clone(), serde_json::Value::String(shown))
        })
        .collect();
    let _ = store.cartographer_log(crate::cartographer::CartographerEntry {
        level: crate::logging::LogLevel::INFO,
        source: "server",
        message: "run env overrides changed",
        scope: Some("run"),
        run_id: Some(id),
        guardian_id: None,
        session_id: None,
        task: None,
        log_path: None,
        payload: serde_json::json!({"set": redacted_set, "unset": req.unset}),
    });
    json(200, &result)
}

/// Shared body for the hierarchical-env-overrides extension's four
/// `POST .../env` endpoints (task / task-verify / session / session-verify):
/// parses `{set, unset}`, validates every key is a syntactically valid
/// env-var name, applies via `apply` (which does its own existence check —
/// every `set_*_env_overrides` store method resolves to `StoreError::NotFound`
/// when the owning task/session doesn't exist, exactly like
/// `set_run_env_overrides` does for a missing run), and logs an
/// allowlist-redacted Cartographer record, mirroring [`set_run_env`] in every
/// respect except which store method `apply` calls and which entity refs the
/// Cartographer entry carries.
fn set_env_overrides(
    daemon: &Daemon,
    body: &str,
    scope: &'static str,
    run_id: Option<&str>,
    task: Option<&str>,
    session_id: Option<&str>,
    apply: impl FnOnce(
        &Store,
        &std::collections::BTreeMap<String, String>,
        &[String],
    )
        -> std::result::Result<std::collections::BTreeMap<String, String>, StoreError>,
) -> Reply {
    let Ok(req) = serde_json::from_str::<EnvOverridesBody>(body) else {
        return error(400, "bad_request", "invalid env body", vec![]);
    };
    if req.set.is_empty() && req.unset.is_empty() {
        return error(
            400,
            "bad_request",
            "at least one of `set`/`unset` is required",
            vec![],
        );
    }
    for key in req.set.keys().chain(req.unset.iter()) {
        if !crate::config::is_valid_env_key(key) {
            return error(
                400,
                "bad_request",
                &format!("invalid environment variable name: {key:?}"),
                vec![],
            );
        }
    }
    let store = daemon.lock();
    let result = match apply(&store, &req.set, &req.unset) {
        Ok(m) => m,
        Err(e) => return store_error(&e),
    };
    let allow = crate::config::load_env_overrides_config();
    let redacted_set: serde_json::Map<String, serde_json::Value> = req
        .set
        .iter()
        .map(|(k, v)| {
            let shown = if allow.is_allowed(k) {
                v.clone()
            } else {
                "<redacted>".to_string()
            };
            (k.clone(), serde_json::Value::String(shown))
        })
        .collect();
    let _ = store.cartographer_log(crate::cartographer::CartographerEntry {
        level: crate::logging::LogLevel::INFO,
        source: "server",
        message: "env overrides changed",
        scope: Some(scope),
        run_id,
        guardian_id: None,
        session_id,
        task,
        log_path: None,
        payload: serde_json::json!({"set": redacted_set, "unset": req.unset}),
    });
    json(200, &result)
}

/// Add/replace/remove a task's own persistent environment-variable overrides
/// (hierarchical env overrides, extending RAL-150) — see
/// [`Store::resolve_session_env_overrides`] for how this merges under the
/// run's and over into every session under this task.
fn set_task_env(daemon: &Daemon, id: &str, ti: &str, body: &str) -> Reply {
    let Ok(task_idx) = ti.parse::<i64>() else {
        return error(400, "bad_request", "task index must be an integer", vec![]);
    };
    let label = format!("t{task_idx}");
    set_env_overrides(
        daemon,
        body,
        "task",
        Some(id),
        Some(&label),
        None,
        |store, set, unset| store.set_task_env_overrides(id, task_idx, set, unset),
    )
}

/// Add/replace/remove the environment-variable overrides applied only to a
/// task's own (task-scoped) verify steps — see
/// [`Store::resolve_task_verify_env_overrides`].
fn set_task_verify_env(daemon: &Daemon, id: &str, ti: &str, body: &str) -> Reply {
    let Ok(task_idx) = ti.parse::<i64>() else {
        return error(400, "bad_request", "task index must be an integer", vec![]);
    };
    let label = format!("t{task_idx}");
    set_env_overrides(
        daemon,
        body,
        "task-verify",
        Some(id),
        Some(&label),
        None,
        |store, set, unset| store.set_task_verify_env_overrides(id, task_idx, set, unset),
    )
}

/// Add/replace/remove a session's own persistent environment-variable
/// overrides — see [`Store::resolve_session_env_overrides`].
fn set_session_env(daemon: &Daemon, id: &str, ti: &str, si: &str, body: &str) -> Reply {
    let (Ok(task_idx), Ok(session_idx)) = (ti.parse::<i64>(), si.parse::<i64>()) else {
        return error(
            400,
            "bad_request",
            "task/session index must be integers",
            vec![],
        );
    };
    let task_label = format!("t{task_idx}");
    let session_label = format!("s{session_idx}");
    set_env_overrides(
        daemon,
        body,
        "session",
        Some(id),
        Some(&task_label),
        Some(&session_label),
        |store, set, unset| store.set_session_env_overrides(id, task_idx, session_idx, set, unset),
    )
}

/// Add/replace/remove the environment-variable overrides applied only to a
/// session's own (session-scoped) verify steps — see
/// [`Store::resolve_session_verify_env_overrides`].
fn set_session_verify_env(daemon: &Daemon, id: &str, ti: &str, si: &str, body: &str) -> Reply {
    let (Ok(task_idx), Ok(session_idx)) = (ti.parse::<i64>(), si.parse::<i64>()) else {
        return error(
            400,
            "bad_request",
            "task/session index must be integers",
            vec![],
        );
    };
    let task_label = format!("t{task_idx}");
    let session_label = format!("s{session_idx}");
    set_env_overrides(
        daemon,
        body,
        "session-verify",
        Some(id),
        Some(&task_label),
        Some(&session_label),
        |store, set, unset| {
            store.set_session_verify_env_overrides(id, task_idx, session_idx, set, unset)
        },
    )
}

/// Add/replace/remove the environment-variable overrides applied to **one
/// individual task-scoped verify step** (RAL-191) — the narrowest layer, see
/// [`Store::resolve_task_verify_step_env_overrides`].
fn set_task_verify_step_env(daemon: &Daemon, id: &str, ti: &str, vi: &str, body: &str) -> Reply {
    let (Ok(task_idx), Ok(verify_idx)) = (ti.parse::<i64>(), vi.parse::<i64>()) else {
        return error(
            400,
            "bad_request",
            "task/verify index must be integers",
            vec![],
        );
    };
    let label = format!("t{task_idx}/verify#{verify_idx}");
    set_env_overrides(
        daemon,
        body,
        "task-verify-step",
        Some(id),
        Some(&label),
        None,
        |store, set, unset| {
            // Task-scoped verify rows are stored with `session_idx = -1`.
            store.set_verify_step_env_overrides(id, task_idx, "task", -1, verify_idx, set, unset)
        },
    )
}

/// Add/replace/remove the environment-variable overrides applied to **one
/// individual session-scoped verify step** (RAL-191) — see
/// [`Store::resolve_session_verify_step_env_overrides`].
fn set_session_verify_step_env(
    daemon: &Daemon,
    id: &str,
    ti: &str,
    si: &str,
    vi: &str,
    body: &str,
) -> Reply {
    let (Ok(task_idx), Ok(session_idx), Ok(verify_idx)) =
        (ti.parse::<i64>(), si.parse::<i64>(), vi.parse::<i64>())
    else {
        return error(
            400,
            "bad_request",
            "task/session/verify index must be integers",
            vec![],
        );
    };
    let task_label = format!("t{task_idx}/verify#{verify_idx}");
    let session_label = format!("s{session_idx}");
    set_env_overrides(
        daemon,
        body,
        "session-verify-step",
        Some(id),
        Some(&task_label),
        Some(&session_label),
        |store, set, unset| {
            store.set_verify_step_env_overrides(
                id,
                task_idx,
                "session",
                session_idx,
                verify_idx,
                set,
                unset,
            )
        },
    )
}

/// Body for `POST /api/guardians/{id}/branches/{bid}/env` (RAL-191) and the
/// guardian-level `POST /api/guardians/{id}/build-env` /
/// `.../manual-checks-env` endpoints (RAL-203). Unlike the run/task/session
/// layers this one has three operations, because these values are *inherited*
/// (from a source session, or from the combined worktree's branch union)
/// rather than being the target's own to begin with:
///
/// - `set` — override an inherited value (or add a new variable).
/// - `unset` — tombstone: remove the inherited variable from the effective
///   environment entirely.
/// - `clear` — drop this layer's own entry, so the key goes back to
///   inheriting whatever the layer below resolves to.
#[derive(Deserialize)]
struct InheritedEnvOverridesBody {
    #[serde(default)]
    set: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    unset: Vec<String>,
    #[serde(default)]
    clear: Vec<String>,
}

/// Set/unset/clear a review branch's own environment-variable overrides
/// (RAL-191).
///
/// A review worktree is built from a session's work and inherits that
/// session's resolved environment; this endpoint layers per-branch changes on
/// top, which then apply to that branch's conflict resolver, final-verify
/// pass, feedback routing, and check gates. Mirrors [`set_env_overrides`]'s
/// key validation and allowlist-redacted Cartographer logging.
fn set_guardian_branch_env(daemon: &Daemon, id: &str, branch_id: &str, body: &str) -> Reply {
    let Ok(req) = serde_json::from_str::<InheritedEnvOverridesBody>(body) else {
        return error(400, "bad_request", "invalid env body", vec![]);
    };
    if req.set.is_empty() && req.unset.is_empty() && req.clear.is_empty() {
        return error(
            400,
            "bad_request",
            "at least one of `set`/`unset`/`clear` is required",
            vec![],
        );
    }
    for key in req
        .set
        .keys()
        .chain(req.unset.iter())
        .chain(req.clear.iter())
    {
        if !crate::config::is_valid_env_key(key) {
            return error(
                400,
                "bad_request",
                &format!("invalid environment variable name: {key:?}"),
                vec![],
            );
        }
    }
    let store = daemon.lock();
    let result = match store
        .set_guardian_branch_env_overrides(id, branch_id, &req.set, &req.unset, &req.clear)
    {
        Ok(m) => m,
        Err(e) => return store_error(&e),
    };
    let allow = crate::config::load_env_overrides_config();
    let redacted_set: serde_json::Map<String, serde_json::Value> = req
        .set
        .iter()
        .map(|(k, v)| {
            let shown = if allow.is_allowed(k) {
                v.clone()
            } else {
                "<redacted>".to_string()
            };
            (k.clone(), serde_json::Value::String(shown))
        })
        .collect();
    let _ = store.cartographer_log(crate::cartographer::CartographerEntry {
        level: crate::logging::LogLevel::INFO,
        source: "server",
        message: "review branch env overrides changed",
        scope: Some("guardian-branch"),
        run_id: None,
        guardian_id: Some(id),
        session_id: None,
        task: Some(branch_id),
        log_path: None,
        payload: serde_json::json!({
            "set": redacted_set,
            "unset": req.unset,
            "clear": req.clear,
        }),
    });
    json(200, &result)
}

/// Which guardian-level env-override layer [`set_guardian_scoped_env`]
/// targets (RAL-203): the finalize-time build/check-gate step run against
/// the combined worktree, or the manual-checks step (the LLM-suggested
/// commands run via `ralphus review checks run` / the board's "Run all").
/// The two are independent -- mutating one never touches the other.
#[derive(Clone, Copy)]
enum GuardianEnvSection {
    Build,
    ManualChecks,
}

impl GuardianEnvSection {
    fn cartographer_scope(self) -> &'static str {
        match self {
            Self::Build => "guardian-build-env",
            Self::ManualChecks => "guardian-manual-checks-env",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Build => "build",
            Self::ManualChecks => "manual-checks",
        }
    }
}

/// Set/unset/clear this review's own environment-variable overrides for
/// `section` (RAL-203) -- the finalize-time build/check-gate step or the
/// manual-checks step, whichever `section` names. Both are layered on top of
/// `combined_env` (the last enabled branch's own resolved environment),
/// which the combined worktree borrows in place of a source session of its
/// own. Shares [`InheritedEnvOverridesBody`]'s
/// `set`/`unset`/`clear` shape and [`set_guardian_branch_env`]'s key
/// validation and allowlist-redacted Cartographer logging.
fn set_guardian_scoped_env(
    daemon: &Daemon,
    id: &str,
    body: &str,
    section: GuardianEnvSection,
) -> Reply {
    let Ok(req) = serde_json::from_str::<InheritedEnvOverridesBody>(body) else {
        return error(400, "bad_request", "invalid env body", vec![]);
    };
    if req.set.is_empty() && req.unset.is_empty() && req.clear.is_empty() {
        return error(
            400,
            "bad_request",
            "at least one of `set`/`unset`/`clear` is required",
            vec![],
        );
    }
    for key in req
        .set
        .keys()
        .chain(req.unset.iter())
        .chain(req.clear.iter())
    {
        if !crate::config::is_valid_env_key(key) {
            return error(
                400,
                "bad_request",
                &format!("invalid environment variable name: {key:?}"),
                vec![],
            );
        }
    }
    let store = daemon.lock();
    let result = match section {
        GuardianEnvSection::Build => {
            store.set_guardian_build_env_overrides(id, &req.set, &req.unset, &req.clear)
        }
        GuardianEnvSection::ManualChecks => {
            store.set_guardian_manual_checks_env_overrides(id, &req.set, &req.unset, &req.clear)
        }
    };
    let result = match result {
        Ok(m) => m,
        Err(e) => return store_error(&e),
    };
    let allow = crate::config::load_env_overrides_config();
    let redacted_set: serde_json::Map<String, serde_json::Value> = req
        .set
        .iter()
        .map(|(k, v)| {
            let shown = if allow.is_allowed(k) {
                v.clone()
            } else {
                "<redacted>".to_string()
            };
            (k.clone(), serde_json::Value::String(shown))
        })
        .collect();
    let _ = store.cartographer_log(crate::cartographer::CartographerEntry {
        level: crate::logging::LogLevel::INFO,
        source: "server",
        message: "review env overrides changed",
        scope: Some(section.cartographer_scope()),
        run_id: None,
        guardian_id: Some(id),
        session_id: None,
        task: Some(section.label()),
        log_path: None,
        payload: serde_json::json!({
            "set": redacted_set,
            "unset": req.unset,
            "clear": req.clear,
        }),
    });
    json(200, &result)
}

/// `POST /api/guardians/{id}/build-env` (RAL-203) -- see
/// [`set_guardian_scoped_env`].
fn set_guardian_build_env(daemon: &Daemon, id: &str, body: &str) -> Reply {
    set_guardian_scoped_env(daemon, id, body, GuardianEnvSection::Build)
}

/// `POST /api/guardians/{id}/manual-checks-env` (RAL-203) -- see
/// [`set_guardian_scoped_env`].
fn set_guardian_manual_checks_env(daemon: &Daemon, id: &str, body: &str) -> Reply {
    set_guardian_scoped_env(daemon, id, body, GuardianEnvSection::ManualChecks)
}

#[derive(Serialize)]
struct RestartResponse {
    state: &'static str,
    dirtied: Vec<String>,
}

/// A human-authored restart note (RAL-174), parsed from a restart endpoint's
/// JSON body: `{"note": "...", "apply_to_all": false}`. Both fields are
/// optional and an empty/malformed body parses to the all-defaults case (no
/// note, narrow scope) — restarting without opting into this feature must
/// behave exactly as it did before (ticket AC).
#[derive(Deserialize, Default)]
struct RestartNoteBody {
    #[serde(default)]
    note: Option<String>,
    /// The "Apply To All Children" checkbox: when set, the note is written to
    /// every session downstream of the restart's target(s) within the run,
    /// not just the exact target(s).
    #[serde(default)]
    apply_to_all: bool,
}

impl RestartNoteBody {
    fn parse(body: &str) -> Self {
        if body.trim().is_empty() {
            return Self::default();
        }
        serde_json::from_str(body).unwrap_or_default()
    }

    /// The note text to attach, trimmed and capped to the same size ghost
    /// content is capped to (`ghost::MAX_CONTENT_CHARS`) so arbitrarily long
    /// user input can't blow up prompt construction. `None` when the user
    /// left the box blank — callers skip writing a ghost note entirely.
    fn trimmed_note(&self) -> Option<String> {
        let note = self.note.as_deref()?.trim();
        if note.is_empty() {
            return None;
        }
        Some(note.chars().take(crate::ghost::MAX_CONTENT_CHARS).collect())
    }
}

/// Attach a restart note (if any) to the session(s) a restart request
/// targets. Best-effort: by the time this runs the restart's own state
/// transition has already succeeded, so a failure to write the (purely
/// advisory) ghost note must never fail the restart response itself.
fn apply_restart_note(daemon: &Daemon, run_id: &str, roots: &[(i64, i64)], req: &RestartNoteBody) {
    if let Some(note) = req.trimmed_note() {
        let _ = daemon
            .lock()
            .apply_restart_user_note(run_id, roots, req.apply_to_all, &note);
    }
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
fn restart_run(daemon: &Daemon, id: &str, body: &str) -> Reply {
    let note_req = RestartNoteBody::parse(body);
    // Stop every worker this restart is about to touch — the target run and
    // everything it will dirty — *before* any of them are reset to Pending,
    // so no old worker can still be mid-poll when the fresh claim lands.
    let impact = daemon.lock().compute_run_restart_impact(id);
    if let Ok(impact) = &impact {
        for dep in &impact.dirtied_runs {
            daemon.cancellations.cancel(&dep.id);
        }
        for dep in &impact.dirtied_runs {
            wait_for_worker_stop(daemon, &dep.id);
        }
    }
    daemon.cancellations.cancel(id);
    wait_for_worker_stop(daemon, id);
    // Bound to a `let` (not matched directly) so the `MutexGuard` `.lock()`
    // returns is dropped at the end of *this* statement -- matching on
    // `daemon.lock().restart_run(id)` directly would keep that guard alive
    // for the whole arm body below (Rust extends a match scrutinee's
    // temporaries to the arm), and `apply_restart_note` below takes its own
    // lock, which would then deadlock against the still-held one.
    let restarted = daemon.lock().restart_run(id);
    match restarted {
        Ok(dirtied) => {
            // A whole-run restart's target IS every session in the run — there
            // is no narrower "exact target" to restrict to, so the note always
            // reaches all of them regardless of "Apply To All Children".
            if let Ok(impact) = &impact {
                let roots: Vec<(i64, i64)> = impact
                    .sessions
                    .iter()
                    .map(|s| (s.task_idx, s.idx))
                    .collect();
                apply_restart_note(daemon, id, &roots, &note_req);
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
/// owning run's worker sometimes must be cancelled *and waited out* first:
/// `Store::restart_session` resets the run to `Pending` too, and without
/// stopping a still-in-flight worker for the *restarted session itself*
/// first, a restart mid-run causes a double-dispatch race on the
/// deterministic per-session tmux/spec-file names.
///
/// That cancellation is scoped to whether the restart target is actually
/// still running, though — not unconditional. A run's worker thread drives
/// every one of that run's sessions concurrently over one shared
/// [`crate::cancel::CancelToken`] (see `scheduler::execute_run_inner`), so
/// cancelling it to restart one *already-terminal* (done/failed) session
/// used to also kill every other still-running, unrelated sibling session in
/// the same run as collateral damage (RAL-1xx) — restarting a batch run's
/// one failed task could take down several of its perfectly healthy
/// siblings. Skipping the cancel when the target isn't `Running` avoids
/// that; `claim_ready` (`scheduler.rs`) now defers re-claiming a `Pending`
/// run until any worker still registered for it exits naturally, so this
/// stays double-dispatch-safe even without the cancel.
fn restart_session(daemon: &Daemon, id: &str, ti: &str, si: &str, body: &str) -> Reply {
    let note_req = RestartNoteBody::parse(body);
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
    // Cancel only when we can't positively confirm the target has already
    // stopped on its own — an unknown/error result stays on the safe
    // (cancel) side, same as the old unconditional behavior.
    let target_already_stopped = matches!(
        daemon.lock().session_state(id, task_idx, session_idx),
        Ok(Some(state)) if state != NodeState::Running
    );
    if !target_already_stopped {
        daemon.cancellations.cancel(id);
        wait_for_worker_stop(daemon, id);
    }
    // See `restart_run`'s comment on why this is a `let` and not matched
    // directly -- `apply_restart_note` below takes its own lock, which would
    // deadlock against a guard still held by the match scrutinee.
    let restarted = daemon.lock().restart_session(id, task_idx, session_idx);
    match restarted {
        Ok(dirtied) => {
            apply_restart_note(daemon, id, &[(task_idx, session_idx)], &note_req);
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

/// Restart a whole task within a run (and its downstream sessions), dirtying
/// dependent runs (RAL-150) — the task-granularity counterpart of
/// [`restart_run`]/[`restart_session`], backing the CLI's generic `ralphus
/// retry <selector>` when the selector resolves to a task. See
/// [`wait_for_worker_stop`]'s doc comment for why the owning run's worker
/// (and every run this restart dirties) must be cancelled *and waited out*
/// first.
fn restart_task(daemon: &Daemon, id: &str, ti: &str, body: &str) -> Reply {
    let note_req = RestartNoteBody::parse(body);
    let Ok(task_idx) = ti.parse::<i64>() else {
        return error(400, "bad_request", "task index must be an integer", vec![]);
    };
    let impact = daemon.lock().compute_task_restart_impact(id, task_idx);
    if let Ok(impact) = &impact {
        for dep in &impact.dirtied_runs {
            daemon.cancellations.cancel(&dep.id);
        }
        for dep in &impact.dirtied_runs {
            wait_for_worker_stop(daemon, &dep.id);
        }
    }
    daemon.cancellations.cancel(id);
    wait_for_worker_stop(daemon, id);
    // See `restart_run`'s comment on why this is a `let` and not matched
    // directly -- `apply_restart_note` below takes its own lock, which would
    // deadlock against a guard still held by the match scrutinee.
    let restarted = daemon.lock().restart_task(id, task_idx);
    match restarted {
        Ok(dirtied) => {
            // The task's own (directly-owned) sessions are exactly the ones
            // in `impact.sessions` with this task_idx — every session
            // downstream of them (possibly in other tasks) is a "child" only
            // reached when "Apply To All Children" is set.
            if let Ok(impact) = &impact {
                let roots: Vec<(i64, i64)> = impact
                    .sessions
                    .iter()
                    .filter(|s| s.task_idx == task_idx)
                    .map(|s| (s.task_idx, s.idx))
                    .collect();
                apply_restart_note(daemon, id, &roots, &note_req);
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

/// Dry-run preview of [`restart_task`]: computes the same downstream-impact
/// set the real restart would dirty, without mutating anything (RAL-104-style).
fn restart_task_preview(daemon: &Daemon, id: &str, ti: &str) -> Reply {
    let Ok(task_idx) = ti.parse::<i64>() else {
        return error(400, "bad_request", "task index must be an integer", vec![]);
    };
    match daemon.lock().compute_task_restart_impact(id, task_idx) {
        Ok(impact) => json(200, &impact),
        Err(e) => store_error(&e),
    }
}

/// Restart a single session's verify steps from `vi` onwards: resets
/// session-level verifies at index >= vi to Pending while leaving the session
/// body Done so the scheduler re-runs only the affected verify steps.
///
/// Like [`restart_session`], only cancels the run's worker when one of the
/// targeted verify steps is actually still `Running` — see that function's
/// doc comment for why an unconditional cancel collaterally kills unrelated
/// sibling sessions in the same run (RAL-1xx).
fn restart_session_verify(
    daemon: &Daemon,
    id: &str,
    ti: &str,
    si: &str,
    vi: &str,
    body: &str,
) -> Reply {
    let note_req = RestartNoteBody::parse(body);
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
    let target_running = daemon
        .lock()
        .session_verify_running_from(id, task_idx, session_idx, verify_from)
        .unwrap_or(true);
    if target_running {
        daemon.cancellations.cancel(id);
        wait_for_worker_stop(daemon, id);
    }
    // See `restart_run`'s comment on why this is a `let` and not matched
    // directly -- `apply_restart_note` below takes its own lock, which would
    // deadlock against a guard still held by the match scrutinee.
    let restarted = daemon
        .lock()
        .restart_session_verify(id, task_idx, session_idx, verify_from);
    match restarted {
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
            apply_restart_note(daemon, id, &[(task_idx, session_idx)], &note_req);
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
///
/// Like [`restart_session`], only cancels the run's worker when one of the
/// targeted verify steps is actually still `Running` — see that function's
/// doc comment for why an unconditional cancel collaterally kills unrelated
/// sibling sessions in the same run (RAL-1xx).
fn restart_task_verify(daemon: &Daemon, id: &str, ti: &str, vi: &str, body: &str) -> Reply {
    let note_req = RestartNoteBody::parse(body);
    let (Ok(task_idx), Ok(verify_from)) = (ti.parse::<i64>(), vi.parse::<i64>()) else {
        return error(
            400,
            "bad_request",
            "task/verify index must be integers",
            vec![],
        );
    };
    let target_running = daemon
        .lock()
        .task_verify_running_from(id, task_idx, verify_from)
        .unwrap_or(true);
    if target_running {
        daemon.cancellations.cancel(id);
        wait_for_worker_stop(daemon, id);
    }
    // See `restart_run`'s comment on why this is a `let` and not matched
    // directly -- both `sessions_of` and `apply_restart_note` below take
    // their own lock, which would deadlock against a guard still held by the
    // match scrutinee.
    let restarted = daemon.lock().restart_task_verify(id, task_idx, verify_from);
    match restarted {
        Ok(dirtied) => {
            for dep_id in &dirtied {
                daemon.cancellations.cancel(dep_id);
                wait_for_worker_stop(daemon, dep_id);
            }
            // The task's own directly-owned sessions -- mirrors restart_task
            // above (no separate impact-preview endpoint exists here, so
            // filter sessions_of directly instead of an already-computed
            // impact set).
            // Same `let`-before-`if let` reasoning as above: `sessions_of`'s
            // guard must be dropped before `apply_restart_note` takes its own.
            let sessions = daemon.lock().sessions_of(id);
            if let Ok(sessions) = sessions {
                let roots: Vec<(i64, i64)> = sessions
                    .iter()
                    .filter(|s| s.task_idx == task_idx)
                    .map(|s| (s.task_idx, s.idx))
                    .collect();
                apply_restart_note(daemon, id, &roots, &note_req);
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
/// rather than treat it as an error. When inactive, `content` is the
/// persisted last-pane-content snapshot (`crate::tmux::read_pane_snapshot`,
/// written by `SubprocessRunner::run_via_tmux_attempt` as each attempt ends)
/// if one exists — a read-only historical record of what the pane last
/// showed, not fresh output — or empty if the session never ran under tmux
/// at all. The board distinguishes the two by whether `content` is non-empty.
///
/// `last_activity_ms` (RAL-170) is the Unix-epoch-milliseconds time the
/// daemon last observed *fresh* pane output (more lines than the previous
/// poll) for this session — the liveness signal that lets the Live View
/// distinguish "still working, just quiet" from "hasn't produced a line in
/// a suspiciously long time". Tracked continuously and in-memory by
/// `SubprocessRunner::run_via_tmux_attempt` for every running tmux-wrapped
/// session regardless of whether a Live View is open (see
/// `Store::note_live_activity`'s doc comment for the scale tradeoff behind
/// that decision) — so this is always fresh at the moment it's read, never
/// backfilled or stale from before the view opened. `None` when the session
/// has produced no output yet, or has already ended (`active: false` here
/// too, in that case).
#[derive(Serialize)]
struct PaneResponse {
    active: bool,
    content: String,
    last_activity_ms: Option<i64>,
}

/// An inactive [`PaneResponse`] falling back to the persisted snapshot for
/// `session_name`, if one exists — shared by every "no live session" branch
/// in [`capture_pane_reply`] so the read-only historical record degrades the
/// same way regardless of *why* the session isn't live right now. Liveness
/// tracking (RAL-170) only covers currently-running sessions (see
/// `Store::clear_live_activity`), so `last_activity_ms` is always `None`
/// here.
fn inactive_pane_reply(session_name: &str) -> Reply {
    json(
        200,
        &PaneResponse {
            active: false,
            content: crate::tmux::read_pane_snapshot(session_name).unwrap_or_default(),
            last_activity_ms: None,
        },
    )
}

/// Capture-pane content for the tmux session keyed by `(run_id, task,
/// session_id)` (see `crate::tmux::session_name`), or an inactive
/// [`PaneResponse`] if no such session currently exists. A tmux resolution
/// failure (no binary available at all) is the only case reported as a real
/// error, since that reflects a daemon configuration problem rather than
/// "this run finished".
fn capture_pane_reply(
    daemon: &Daemon,
    run_id: &str,
    task: &str,
    session_id: &str,
    query: &str,
) -> Reply {
    let lines: u32 = query_param(query, "lines")
        .and_then(|s| s.parse().ok())
        .unwrap_or(2000);
    let tmux = match crate::tmux::Tmux::resolve() {
        Ok(t) => t,
        Err(e) => return error(500, "tmux_error", &e.to_string(), vec![]),
    };
    let name = crate::tmux::session_name(run_id, task, session_id);
    if !tmux.has_session(&name) {
        return inactive_pane_reply(&name);
    }
    match tmux.capture_pane(&name, lines) {
        Ok(content) => json(
            200,
            &PaneResponse {
                active: true,
                content,
                last_activity_ms: daemon.lock().live_activity_ms(&name),
            },
        ),
        Err(_) => inactive_pane_reply(&name),
    }
}

/// List of persisted terminal-log attempts for a tmux session, for the
/// board's "historical attempts" picker (RAL-154).
#[derive(Serialize)]
struct TerminalLogAttemptsResponse {
    attempts: Vec<crate::terminal_log::AttemptMeta>,
}

/// List every durably-persisted terminal-log attempt for the tmux session
/// keyed by `(run_id, task, session_id)` (see `crate::tmux::session_name`),
/// ascending by attempt number. Empty (never an error) when the session never
/// ran under tmux, or no attempt has finished writing its log yet. Shared by
/// every "Open Terminal Log" context (task session, verify step, guardian
/// branch resolver, guardian manual-checks) the same way [`capture_pane_reply`]
/// already is.
fn terminal_log_attempts_reply(run_id: &str, task: &str, session_id: &str) -> Reply {
    let name = crate::tmux::session_name(run_id, task, session_id);
    json(
        200,
        &TerminalLogAttemptsResponse {
            attempts: crate::terminal_log::list_attempts(&name),
        },
    )
}

/// Raw content of one historical terminal-log attempt (RAL-154), readable
/// outside the GUI just as easily as the underlying file
/// (`crate::terminal_log::read_attempt`) is.
#[derive(Serialize)]
struct TerminalLogAttemptContentResponse {
    attempt: u32,
    content: String,
}

/// Content of one persisted terminal-log attempt for the tmux session keyed
/// by `(run_id, task, session_id)`. `404` when that attempt was never
/// written, or has since been pruned/deleted (`crate::terminal_log::prune`,
/// or the owning run/guardian's deletion).
fn terminal_log_attempt_content_reply(
    run_id: &str,
    task: &str,
    session_id: &str,
    attempt: &str,
) -> Reply {
    let Ok(attempt_n) = attempt.parse::<u32>() else {
        return error(
            400,
            "bad_request",
            "attempt must be a non-negative integer",
            vec![],
        );
    };
    let name = crate::tmux::session_name(run_id, task, session_id);
    match crate::terminal_log::read_attempt(&name, attempt_n) {
        Some(content) => json(
            200,
            &TerminalLogAttemptContentResponse {
                attempt: attempt_n,
                content,
            },
        ),
        None => error(
            404,
            "not_found",
            "no terminal log recorded for that attempt",
            vec![],
        ),
    }
}

/// Open an interactive terminal attached to the tmux session keyed by
/// `(run_id, task, session_id)` — the "open terminal" button (RAL-102). When
/// the session is currently live, attaches to it directly. Otherwise
/// degrades to [`open_readonly_snapshot_terminal`] — a finished
/// run/verify/resolver has already had its tmux session torn down (see
/// `crate::runner::SubprocessRunner::run_via_tmux`), but its last pane
/// content may still be available as a persisted, read-only historical
/// record.
fn attach_tmux_terminal(run_id: &str, task: &str, session_id: &str) -> Reply {
    let tmux = match crate::tmux::Tmux::resolve() {
        Ok(t) => t,
        Err(e) => return error(500, "tmux_error", &e.to_string(), vec![]),
    };
    let name = crate::tmux::session_name(run_id, task, session_id);
    if !tmux.has_session(&name) {
        return open_readonly_snapshot_terminal(&name);
    }
    let program = tmux.program().to_string();
    let mut args: Vec<String> = tmux.prefix_args().to_vec();
    args.extend(["attach-session".to_string(), "-t".to_string(), name]);
    match spawn_in_terminal(None, &program, &args, &std::collections::BTreeMap::new()) {
        Ok(()) => json(200, &OpenTerminalResponse { ok: true }),
        Err(msg) => error(500, "terminal_error", &msg, vec![]),
    }
}

/// Degraded fallback for [`attach_tmux_terminal`] once the live tmux session
/// for `session_name` is gone: if a persisted pane snapshot exists
/// (`crate::tmux::read_pane_snapshot`, written as each attempt of
/// `SubprocessRunner::run_via_tmux_attempt` ends), copy it to a disposable
/// temp file and open that copy with the user's preferred external viewer
/// (RAL-153) — instead of hard-failing. Still reports `409` when there is
/// genuinely nothing to show: the session never ran under tmux at all, or
/// produced no pane output before ending.
///
/// RAL-153: this used to spawn a `pwsh -NoExit -Command "...Get-Content..."`
/// string through [`spawn_in_terminal`], which on the `wt.exe` path
/// re-tokenizes that whole quoted string a second time (the same class of
/// bug [`spawn_in_terminal`]'s own doc comment describes for cwd/semicolon
/// handling) — the result was a terminal window that opened but failed to
/// find/display the file. Writing a plain file and handing its *path* to a
/// viewer sidesteps that class of bug entirely: there is no complex string
/// for any layer to re-tokenize.
///
/// The viewer is resolved in order: `$VISUAL`, then `$EDITOR` (each split on
/// whitespace so e.g. `code -w` works, not just a bare command), then the
/// OS's own default handler for the file type (`start`/`open`/`xdg-open`).
/// A fresh, uniquely-named copy is written per click into its own directory
/// under the OS temp dir — distinct from the durable, authoritative
/// `pane_snapshots/<session>.txt` under `state_dir()` — so a stray edit in
/// the viewer can never corrupt the real log, and
/// [`prune_stale_readonly_viewer_copies`] can freely delete old copies from
/// that directory without ever touching the persisted record.
fn open_readonly_snapshot_terminal(session_name: &str) -> Reply {
    let Some(content) = crate::tmux::read_pane_snapshot(session_name) else {
        return error(
            409,
            "no_tmux_session",
            "no live tmux session, and no historical pane record was saved for it",
            vec![],
        );
    };
    let dir = readonly_viewer_dir();
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return error(
            500,
            "terminal_error",
            &format!("could not create terminal-log viewer directory: {e}"),
            vec![],
        );
    }
    prune_stale_readonly_viewer_copies(&dir);
    let snapshot_file = readonly_viewer_copy_path(&dir, session_name);
    if let Err(e) = std::fs::write(&snapshot_file, &content) {
        return error(
            500,
            "terminal_error",
            &format!("could not write snapshot file: {e}"),
            vec![],
        );
    }
    let result = if let Some((program, args)) = resolve_preferred_editor() {
        std::process::Command::new(&program)
            .args(&args)
            .arg(&snapshot_file)
            .spawn()
            .map(|_| ())
            .map_err(|e| format!("could not open viewer '{program}': {e}"))
    } else {
        open_with_os_default(&snapshot_file)
    };
    match result {
        Ok(()) => json(200, &OpenTerminalResponse { ok: true }),
        Err(msg) => error(500, "terminal_error", &msg, vec![]),
    }
}

/// Directory disposable per-click terminal-log viewer copies live under
/// (RAL-153) — kept separate from `crate::tmux::pane_snapshot_dir` (the
/// durable, authoritative snapshot under `state_dir()`) so pruning this
/// directory can never touch that record.
fn readonly_viewer_dir() -> std::path::PathBuf {
    std::env::temp_dir().join("ralphus-terminal-logs")
}

/// How long a disposable per-click viewer copy is kept before
/// [`prune_stale_readonly_viewer_copies`] deletes it (RAL-153 Q2) — long
/// enough that a slow-to-open `$EDITOR`/OS handler still finds the file,
/// short enough that repeated "Open Terminal Log" clicks over a project's
/// lifetime don't leak files into the OS temp directory indefinitely.
const READONLY_VIEWER_COPY_MAX_AGE: Duration = Duration::from_secs(3600);

/// Best-effort cleanup of [`readonly_viewer_dir`]: delete every file older
/// than [`READONLY_VIEWER_COPY_MAX_AGE`]. Called each time a fresh copy is
/// written rather than on a timer, so cleanup happens without the daemon
/// needing a background sweep thread; a failure to read the directory or
/// remove any one file is silently ignored — this is disk-space hygiene,
/// never load-bearing for the current click's own copy.
fn prune_stale_readonly_viewer_copies(dir: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let now = std::time::SystemTime::now();
    for entry in entries.flatten() {
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        let Ok(modified) = metadata.modified() else {
            continue;
        };
        if is_stale(modified, now, READONLY_VIEWER_COPY_MAX_AGE) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Whether a file last modified at `modified` counts as stale as of `now`,
/// given `max_age` — pulled out of [`prune_stale_readonly_viewer_copies`] so
/// the staleness rule itself is unit-testable without touching real
/// filesystem timestamps. `modified` at or after `now` (freshly written, or
/// clock skew) is never stale.
fn is_stale(
    modified: std::time::SystemTime,
    now: std::time::SystemTime,
    max_age: Duration,
) -> bool {
    now.duration_since(modified).is_ok_and(|age| age > max_age)
}

/// A fresh, unique path for this click's disposable viewer copy of
/// `session_name`'s snapshot within `dir` — unique per call (not reused
/// across clicks like the old fixed `{session_name}.readonly.txt` path) so
/// two "Open Terminal Log" clicks in a row, or a slow viewer still holding
/// the first copy open, never race each other over the same file.
fn readonly_viewer_copy_path(dir: &std::path::Path, session_name: &str) -> std::path::PathBuf {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    dir.join(format!("{session_name}-{unique}.txt"))
}

/// Resolve the user's preferred external viewer for a read-only terminal-log
/// copy (RAL-153): `$VISUAL`, then `$EDITOR`, each split on whitespace (the
/// same convention `RALPHUS_RUNNER_CMD`/`RALPHUS_TMUX_CMD` already use — see
/// `crate::tmux`'s `split_command`) so e.g. `code -w` or a bare `vim` both
/// resolve to a program plus its fixed leading arguments. `None` when
/// neither is set (or set to blank), so the caller falls back to the OS's
/// own default file-type handler.
fn resolve_preferred_editor() -> Option<(String, Vec<String>)> {
    resolve_preferred_editor_from(std::env::var("VISUAL").ok(), std::env::var("EDITOR").ok())
}

/// Implementation behind [`resolve_preferred_editor`], taking the two env
/// values explicitly so it's unit-testable without mutating real process
/// environment.
fn resolve_preferred_editor_from(
    visual: Option<String>,
    editor: Option<String>,
) -> Option<(String, Vec<String>)> {
    for value in [visual, editor].into_iter().flatten() {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            continue;
        }
        let mut parts = trimmed.split_whitespace().map(str::to_string);
        let program = parts.next()?;
        return Some((program, parts.collect()));
    }
    None
}

/// Open `path` with the OS's default handler for its file type — the
/// fallback when neither `$VISUAL` nor `$EDITOR` is set (RAL-153).
#[cfg(target_os = "windows")]
fn open_with_os_default(path: &std::path::Path) -> std::result::Result<(), String> {
    // `start` is a `cmd.exe` builtin, not its own executable. The empty ""
    // argument is the (usually-omitted) window-title parameter -- required
    // here because `start` treats a leading quoted argument as the title,
    // which would otherwise try to "start" the file path as a window title
    // instead of opening it.
    std::process::Command::new("cmd")
        .args(["/C", "start", "", &path.to_string_lossy()])
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("could not open default viewer: {e}"))
}

/// See the Windows overload's doc comment.
#[cfg(target_os = "macos")]
fn open_with_os_default(path: &std::path::Path) -> std::result::Result<(), String> {
    std::process::Command::new("open")
        .arg(path)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("could not open default viewer: {e}"))
}

/// See the Windows overload's doc comment.
#[cfg(all(unix, not(target_os = "macos")))]
fn open_with_os_default(path: &std::path::Path) -> std::result::Result<(), String> {
    std::process::Command::new("xdg-open")
        .arg(path)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("could not open default viewer: {e}"))
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
        let (cwd, agent, agent_session_id) =
            match daemon
                .lock()
                .get_session_agent_resume(id, task_idx, session_idx)
            {
                Ok(v) => v,
                Err(e) => return store_error(&e),
            };
        return open_agent_terminal(&cwd, Some(agent.as_str()), agent_session_id.as_deref());
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
    capture_pane_reply(daemon, id, &task, &session_id, query)
}

/// List a task session's persisted historical terminal-log attempts (RAL-154)
/// — the board's "Open Terminal Log" button uses this to list attempts beyond
/// just the current live one.
fn session_terminal_log_attempts(daemon: &Daemon, id: &str, ti: &str, si: &str) -> Reply {
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
    terminal_log_attempts_reply(id, &task, &session_id)
}

/// Content of one of a task session's historical terminal-log attempts (RAL-154).
fn session_terminal_log_attempt(
    daemon: &Daemon,
    id: &str,
    ti: &str,
    si: &str,
    attempt: &str,
) -> Reply {
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
    terminal_log_attempt_content_reply(id, &task, &session_id, attempt)
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
        let (agent, agent_session_id) = match daemon.lock().get_verify_agent_session_id(
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
                .map(|(cwd, _, _)| cwd)
        };
        let cwd = match cwd {
            Ok(v) => v,
            Err(e) => return store_error(&e),
        };
        return open_agent_terminal(&cwd, Some(agent.as_str()), agent_session_id.as_deref());
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
    capture_pane_reply(daemon, &run_id, &task, &session_id, query)
}

/// List a `prompt`-kind verify step's persisted historical terminal-log
/// attempts (RAL-154).
fn verify_terminal_log_attempts(
    daemon: &Daemon,
    id: &str,
    task_idx: &str,
    scope: &str,
    session_idx: &str,
    verify_idx: &str,
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
    terminal_log_attempts_reply(&run_id, &task, &session_id)
}

/// Content of one of a `prompt`-kind verify step's historical terminal-log
/// attempts (RAL-154).
#[allow(clippy::too_many_arguments)]
fn verify_terminal_log_attempt(
    daemon: &Daemon,
    id: &str,
    task_idx: &str,
    scope: &str,
    session_idx: &str,
    verify_idx: &str,
    attempt: &str,
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
    terminal_log_attempt_content_reply(&run_id, &task, &session_id, attempt)
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
        return match spawn_in_terminal(
            None,
            &shell_cmd,
            &shell_args,
            &std::collections::BTreeMap::new(),
        ) {
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
        let agent_session_id = match daemon
            .lock()
            .get_branch_resolver_agent_session_id(id, branch_id)
        {
            Ok(v) => v,
            Err(e) => return store_error(&e),
        };
        // The guardian's resolver agent is set once for the whole guardian
        // (not per-branch) -- see `guardians.resolver_agent`.
        let resolver_agent = daemon
            .lock()
            .get_guardian(id)
            .ok()
            .and_then(|g| g.resolver_agent);
        return open_agent_terminal(
            &wt_path,
            resolver_agent.as_deref(),
            agent_session_id.as_deref(),
        );
    }

    // open: attach to the conflict-resolver's tmux session (RAL-102). Keyed the
    // same way `guardian_merge.rs::resolve_conflicts_with_agent` builds its
    // `RunnerSpec` for this exact branch -- RAL-192: the tmux session name is
    // derived from the branch's stable id (not its mutable stack position),
    // so it stays resolvable here even after a later reorder/add/remove.
    // RAL-149: while the branch is `verify_pending`, the live session is the
    // dedicated final-verify call, not the fix pass -- attach to that one
    // instead.
    let (task, session_id) = match resolver_task_and_session_id(daemon, id, branch_id) {
        Ok(v) => v,
        Err(e) => return e,
    };
    attach_tmux_terminal(&format!("guardian-{id}"), task, &session_id)
}

/// The `(task, session_id)` pair addressing a review branch's conflict-resolver
/// tmux session -- live or historical (RAL-102, extended for RAL-149's
/// fix/verify split and RAL-192's stable-id keying) -- shared by
/// [`open_guardian_branch_terminal`] (attach) and [`guardian_branch_pane`]
/// (read-only peek) so the two never drift on which call's session they
/// resolve to.
fn resolver_task_and_session_id(
    daemon: &Daemon,
    id: &str,
    branch_id: &str,
) -> std::result::Result<(&'static str, String), Reply> {
    let g = daemon
        .lock()
        .get_guardian(id)
        .map_err(|e| store_error(&e))?;
    let Some(b) = g.branches.iter().find(|b| b.id == branch_id) else {
        return Err(error(404, "not_found", "no such branch", vec![]));
    };
    Ok(if b.merge_status == "verify_pending" {
        (
            crate::guardian_merge::RESOLVER_VERIFY_TASK,
            format!("resolver-verify-{}", b.id),
        )
    } else {
        (
            crate::guardian_merge::RESOLVER_TASK,
            format!("resolver-{}", b.id),
        )
    })
}

/// Spawn a real, interactive resumed CLI session (`claude --resume <id>
/// --dangerously-skip-permissions` or `codex exec resume <id>
/// --dangerously-bypass-approvals-and-sandbox`, depending on which agent the
/// session actually ran under) in a new terminal window, rooted at `cwd` —
/// the "Open Agent" terminal action (RAL-102 follow-up). Unlike
/// `attach_tmux_terminal` (which re-attaches to the runner's own
/// tmux-wrapped subprocess and only shows its log/event stream — see
/// `claude_code_backend.py`/`codex_backend.py`'s streaming-JSON parsing),
/// this launches the actual CLI so a human can keep the conversation going
/// interactively, exactly as the pre-RAL-102 "resume" flow did. Permission
/// prompts are bypassed unconditionally, matching each backend's own
/// headless invocation, so the resumed conversation doesn't immediately
/// stall on a prompt the human has to notice and click through.
// Shared with `cli-rs`'s session/review-terminal commands, so both sides
// build the exact same resume command line -- see `ralphus_core::agent_resume`
// for the logic and its tests.
use ralphus_core::agent_resume::{
    is_codex_agent, resume_agent_command, resume_codex_agent_command,
};

fn open_agent_terminal(cwd: &str, agent: Option<&str>, agent_session_id: Option<&str>) -> Reply {
    let Some(session_id) = agent_session_id else {
        return error(
            409,
            "no_claude_session",
            "no CLI-agent session id recorded yet — it may not have started, \
             or didn't run under an agent that supports resume",
            vec![],
        );
    };
    let shell_cmd = std::env::var("RALPHUS_SHELL_CMD").unwrap_or_else(|_| "pwsh".to_string());
    // No leading `Set-Location ...;` here -- `cwd` is passed as its own
    // argument below so it goes through wt's `-d` flag / `current_dir`
    // instead of a semicolon `wt.exe` can't pass through to the agent (see
    // `spawn_in_terminal`'s doc comment).
    let command = if is_codex_agent(agent) {
        // Mirrors `RALPHUS_CODEX_COMMAND` in `codex_backend.py` — the same
        // override point resolves both the headless run and this resumed
        // one to the same binary.
        let program =
            std::env::var("RALPHUS_CODEX_COMMAND").unwrap_or_else(|_| "codex".to_string());
        resume_codex_agent_command(&program, session_id)
    } else {
        // Mirrors `RALPHUS_CLAUDE_COMMAND` in `claude_code_backend.py` — the
        // same override point resolves both the headless run and this
        // resumed one to the same binary.
        let program =
            std::env::var("RALPHUS_CLAUDE_COMMAND").unwrap_or_else(|_| "claude".to_string());
        resume_agent_command(&program, session_id)
    };
    let shell_args = vec![
        "-NoExit".to_string(),
        "-NoProfile".to_string(),
        "-Command".to_string(),
        command,
    ];
    match spawn_in_terminal(
        Some(cwd),
        &shell_cmd,
        &shell_args,
        &std::collections::BTreeMap::new(),
    ) {
        Ok(()) => json(200, &OpenTerminalResponse { ok: true }),
        Err(msg) => error(500, "terminal_error", &msg, vec![]),
    }
}

/// The live pane content of a review branch's conflict-resolver tmux
/// session, for the auto-refreshing "read-only terminal" peek view in the
/// Review tab (RAL-102).
fn guardian_branch_pane(daemon: &Daemon, id: &str, branch_id: &str, query: &str) -> Reply {
    // RAL-192: the tmux session name is keyed on the branch's stable id (see
    // `resolver_task_and_session_id`), so it resolves correctly here even if
    // the branch's stack position has since changed -- including falling
    // back to the persisted historical snapshot (`inactive_pane_reply`, via
    // `capture_pane_reply`) once the live session is gone. RAL-149:
    // `resolver_task_and_session_id` also picks the dedicated final-verify
    // session while the branch is `verify_pending`, so this peek view tracks
    // whichever call is actually live.
    let (task, session_id) = match resolver_task_and_session_id(daemon, id, branch_id) {
        Ok(v) => v,
        Err(e) => return e,
    };
    capture_pane_reply(daemon, &format!("guardian-{id}"), task, &session_id, query)
}

/// Live list of files with unresolved merge conflicts (`git diff
/// --diff-filter=U`) in a review branch's worktree, for the auto-refreshing
/// conflicting-files panel in the Reviews UI (RAL-148).
#[derive(Debug, Clone, Serialize)]
struct BranchConflictsView {
    /// Paths (relative to the worktree root) still carrying unresolved
    /// `<<<<<<<` conflict markers. Empty once every file is resolved, the
    /// branch has no worktree yet, or the worktree has been torn down (e.g.
    /// a base-branch shift triggered a rebuild) -- callers should treat an
    /// empty list as "no live conflicts to show", not an error.
    files: Vec<String>,
    /// Whether the worktree is currently mid-`git rebase` (a `rebase-merge`/
    /// `rebase-apply` state directory exists). `files` can be non-empty with
    /// this `false` between rebase steps, so the board should not assume
    /// "no active rebase" means the conflicts are stale.
    rebase_in_progress: bool,
}

/// The live conflicting-files list for one review branch (RAL-148). Computed
/// on demand (one `git diff --diff-filter=U` call in the branch's worktree)
/// rather than persisted -- mirrors `run_worktrees`'s "computed on demand,
/// not on the hot board path" precedent. Returns an empty, non-error list
/// once the branch has no worktree yet or the worktree directory no longer
/// exists, so a poller can just stop rendering the entry.
fn guardian_branch_conflicts(daemon: &Daemon, id: &str, branch_id: &str) -> Reply {
    if daemon.lock().get_guardian(id).is_err() {
        return error(404, "not_found", "no such guardian", vec![]);
    }
    match daemon.lock().guardian_branches(id) {
        Ok(branches) => {
            if !branches.iter().any(|b| b.id == branch_id) {
                return error(404, "not_found", "no such branch", vec![]);
            }
        }
        Err(e) => return store_error(&e),
    }
    let worktree = match daemon.lock().get_branch_worktree(id, branch_id) {
        Ok(v) => v,
        Err(e) => return store_error(&e),
    };
    let empty = || {
        json(
            200,
            &BranchConflictsView {
                files: vec![],
                rebase_in_progress: false,
            },
        )
    };
    let Some(worktree) = worktree else {
        return empty();
    };
    let wt = std::path::Path::new(&worktree);
    if !wt.exists() {
        return empty();
    }
    json(
        200,
        &BranchConflictsView {
            files: crate::guardian_merge::conflicted_files(&crate::workspace::Workspace::local(wt)),
            rebase_in_progress: crate::guardian_merge::rebase_in_progress(
                &crate::workspace::Workspace::local(wt),
            ),
        },
    )
}

/// List a review branch's conflict-resolver's persisted historical
/// terminal-log attempts (RAL-154).
fn guardian_branch_terminal_log_attempts(daemon: &Daemon, id: &str, branch_id: &str) -> Reply {
    if daemon.lock().get_guardian(id).is_err() {
        return error(404, "not_found", "no such guardian", vec![]);
    }
    let position = match daemon.lock().guardian_branches(id) {
        Ok(branches) => match branches.iter().find(|b| b.id == branch_id) {
            Some(b) => b.position,
            None => return error(404, "not_found", "no such branch", vec![]),
        },
        Err(e) => return store_error(&e),
    };
    terminal_log_attempts_reply(
        &format!("guardian-{id}"),
        crate::guardian_merge::RESOLVER_TASK,
        &format!("resolver-{position}"),
    )
}

/// Content of one of a review branch's conflict-resolver's historical
/// terminal-log attempts (RAL-154).
fn guardian_branch_terminal_log_attempt(
    daemon: &Daemon,
    id: &str,
    branch_id: &str,
    attempt: &str,
) -> Reply {
    if daemon.lock().get_guardian(id).is_err() {
        return error(404, "not_found", "no such guardian", vec![]);
    }
    let position = match daemon.lock().guardian_branches(id) {
        Ok(branches) => match branches.iter().find(|b| b.id == branch_id) {
            Some(b) => b.position,
            None => return error(404, "not_found", "no such branch", vec![]),
        },
        Err(e) => return store_error(&e),
    };
    terminal_log_attempt_content_reply(
        &format!("guardian-{id}"),
        crate::guardian_merge::RESOLVER_TASK,
        &format!("resolver-{position}"),
        attempt,
    )
}

/// Open a terminal for a review's manual-checks generation pass (RAL-88
/// follow-up). `mode` (query param) mirrors `open_guardian_branch_terminal`'s:
/// - `"open"` (default) — attach to the generation run's live tmux session;
///   `409` if it isn't currently running.
/// - `"agent"` — spawn a real, interactive resumed session resuming that
///   generation's own conversation, if it ran under an agent that supports
///   resume (see `open_agent_terminal`'s doc comment).
fn open_guardian_manual_checks_terminal(daemon: &Daemon, id: &str, query: &str) -> Reply {
    let mode = query_param(query, "mode").unwrap_or("open");
    if mode != "open" && mode != "agent" {
        return error(400, "bad_request", "mode must be 'open' or 'agent'", vec![]);
    }
    let (cwd, agent, agent_session_id) =
        match daemon.lock().get_guardian_manual_commands_agent_resume(id) {
            Ok(v) => v,
            Err(e) => return store_error(&e),
        };
    if mode == "agent" {
        return open_agent_terminal(&cwd, agent.as_deref(), agent_session_id.as_deref());
    }
    // open: attach to the generation pass's tmux session — keyed the same way
    // `guardian_merge.rs::generate_manual_commands` builds its `RunnerSpec`.
    attach_tmux_terminal(
        &format!("guardian-{id}"),
        crate::guardian_merge::MANUAL_COMMANDS_TASK,
        crate::guardian_merge::MANUAL_COMMANDS_SESSION,
    )
}

/// The live pane content of a review's manual-checks generation tmux session,
/// for the auto-refreshing "read-only terminal" peek view (RAL-88 follow-up).
fn guardian_manual_checks_pane(daemon: &Daemon, id: &str, query: &str) -> Reply {
    if daemon.lock().get_guardian(id).is_err() {
        return error(404, "not_found", "no such guardian", vec![]);
    }
    capture_pane_reply(
        daemon,
        &format!("guardian-{id}"),
        crate::guardian_merge::MANUAL_COMMANDS_TASK,
        crate::guardian_merge::MANUAL_COMMANDS_SESSION,
        query,
    )
}

/// List a review's manual-checks generation pass's persisted historical
/// terminal-log attempts (RAL-154).
fn guardian_manual_checks_terminal_log_attempts(daemon: &Daemon, id: &str) -> Reply {
    if daemon.lock().get_guardian(id).is_err() {
        return error(404, "not_found", "no such guardian", vec![]);
    }
    terminal_log_attempts_reply(
        &format!("guardian-{id}"),
        crate::guardian_merge::MANUAL_COMMANDS_TASK,
        crate::guardian_merge::MANUAL_COMMANDS_SESSION,
    )
}

/// Content of one of a review's manual-checks generation pass's historical
/// terminal-log attempts (RAL-154).
fn guardian_manual_checks_terminal_log_attempt(daemon: &Daemon, id: &str, attempt: &str) -> Reply {
    if daemon.lock().get_guardian(id).is_err() {
        return error(404, "not_found", "no such guardian", vec![]);
    }
    terminal_log_attempt_content_reply(
        &format!("guardian-{id}"),
        crate::guardian_merge::MANUAL_COMMANDS_TASK,
        crate::guardian_merge::MANUAL_COMMANDS_SESSION,
        attempt,
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
///
/// `env` (RAL-203) is applied on top of the daemon's own inherited
/// environment via `Command::envs` -- both the `wt.exe` and PowerShell-console
/// fallback paths spawn the actual command as a *child* of the process this
/// starts, so variables set here propagate down to it either way. Empty for
/// callers with nothing to add (e.g. attaching to an already-running tmux
/// session, whose environment was fixed at spawn time).
#[cfg(target_os = "windows")]
fn spawn_in_terminal(
    cwd: Option<&str>,
    program: &str,
    args: &[String],
    env: &std::collections::BTreeMap<String, String>,
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
    if wt_cmd.envs(env).arg("--").args(&all).spawn().is_ok() {
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
        .envs(env)
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
    _env: &std::collections::BTreeMap<String, String>,
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
fn kill_run_tmux_sessions(run_id: &str) -> usize {
    let Ok(tmux) = crate::tmux::Tmux::resolve() else {
        return 0;
    };
    tmux.kill_sessions_with_prefix(&format!("ralphus_{run_id}_"))
}

/// The guardian-session counterpart to [`kill_run_tmux_sessions`]: kills
/// every live tmux session belonging to guardian `guardian_id`. Guardian
/// resolver/PR-description/summary sessions are named via
/// `tmux::session_name(&format!("guardian-{guardian_id}"), ...)` (see the
/// `RunnerSpec` construction sites in `guardian_merge.rs`), so the same
/// `ralphus_{run_id}_` scoping `kill_run_tmux_sessions` uses applies here
/// with that formatted id in place of a plain run id.
fn kill_guardian_tmux_sessions(guardian_id: &str) -> usize {
    let Ok(tmux) = crate::tmux::Tmux::resolve() else {
        return 0;
    };
    tmux.kill_sessions_with_prefix(&format!("ralphus_guardian-{guardian_id}_"))
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

#[derive(Deserialize, Default)]
struct ShutdownBody {
    /// When true, every non-terminal run and cancellable guardian is also
    /// marked `cancelled` in the store before the daemon exits, so history
    /// records an intentional stop and nothing auto-resumes on next
    /// `ralphus-daemon serve`. When false (the default), DB state is left
    /// alone — killed processes' rows stay `running`/`merging`/etc, and the
    /// existing crash-recovery path (`recover_orphaned_runs`/
    /// `recover_orphaned_merges`, run at every `serve()` startup) resumes
    /// them on the next start.
    #[serde(default)]
    auto_cancel: bool,
}

#[derive(Serialize)]
struct ShutdownResponse {
    state: &'static str,
    auto_cancel: bool,
    cancelled_runs: Vec<String>,
    cancelled_guardians: Vec<String>,
}

/// Kill every process this daemon has spawned — session/verify/review/chat/
/// summary subprocesses, tmux panes, everything — and request that the
/// daemon exit once this response is sent.
///
/// Unconditionally (regardless of `auto_cancel`): trips every registered
/// cancellation token (stops scheduler-run sessions' subprocesses via their
/// existing polling loop) and kills every tmux session belonging to a
/// currently-known run or guardian, one entity at a time, via
/// [`kill_run_tmux_sessions`] / [`kill_guardian_tmux_sessions`] — each
/// scoped to that single entity's own deterministic session-name prefix
/// (see `tmux::session_name`). With `auto_cancel: true`, also
/// cascade-cancels every non-terminal run and cancellable guardian so their
/// DB state reflects an intentional stop rather than being left for
/// crash-recovery to resume.
///
/// Previously called `tmux::reap_orphaned_sessions_at_startup` here instead
/// — a machine-wide, unscoped kill of *every* `ralphus_`-prefixed tmux.exe
/// process, sound only at actual daemon startup (its own doc comment says
/// so). Called from this live endpoint, it matches — and kills — the very
/// pane this handler happens to be running in whenever that pane's own
/// session name starts with `ralphus_` (which every real daemon-spawned
/// session's name does, by construction). See `PSMUX_CRASH_NOTES.local.md`'s
/// "SOLVED" section for the full incident.
///
/// Never terminates the process itself — see the `shutdown` field's doc
/// comment on [`Daemon`] for why that has to happen outside `route()`.
fn shutdown(daemon: &Daemon, body: &str) -> Reply {
    let req: ShutdownBody = serde_json::from_str(body).unwrap_or_default();

    daemon.cancellations.cancel_all();

    let runs = daemon.lock().list_runs().unwrap_or_default();
    let guardians = daemon.lock().list_guardians().unwrap_or_default();
    let tmux_killed: usize = runs
        .iter()
        .map(|r| kill_run_tmux_sessions(&r.id))
        .sum::<usize>()
        + guardians
            .iter()
            .map(|g| kill_guardian_tmux_sessions(&g.id))
            .sum::<usize>();

    let mut cancelled_runs = Vec::new();
    let mut cancelled_guardians = Vec::new();
    if req.auto_cancel {
        let active_run_ids: Vec<String> = runs
            .into_iter()
            .filter(|r| !RunState::parse(&r.state).is_some_and(RunState::is_terminal))
            .map(|r| r.id)
            .collect();
        for id in active_run_ids {
            if let Ok(impact) = daemon.lock().cancel_run(&id, false) {
                for r in &impact.runs {
                    daemon.cancellations.cancel(&r.id);
                }
                cancelled_runs.extend(impact.runs.into_iter().map(|r| r.id));
            }
        }

        for g in guardians {
            if daemon.lock().cancel_guardian(&g.id).is_ok() {
                cancelled_guardians.push(g.id);
            }
        }
    }

    let _ = daemon
        .lock()
        .cartographer_log(crate::cartographer::CartographerEntry {
            level: crate::logging::LogLevel::WARNING,
            source: "daemon",
            message: "shutdown requested",
            scope: None,
            run_id: None,
            guardian_id: None,
            session_id: None,
            task: None,
            log_path: None,
            payload: serde_json::json!({
                "auto_cancel": req.auto_cancel,
                "tmux_killed": tmux_killed,
                "cancelled_runs": cancelled_runs.len(),
                "cancelled_guardians": cancelled_guardians.len(),
            }),
        });
    crate::rlog!(
        WARNING,
        "ralphus [daemon] shutdown requested: auto_cancel={} tmux_killed={tmux_killed} \
         cancelled_runs={} cancelled_guardians={}",
        req.auto_cancel,
        cancelled_runs.len(),
        cancelled_guardians.len()
    );

    daemon.request_shutdown();
    json(
        200,
        &ShutdownResponse {
            state: "stopping",
            auto_cancel: req.auto_cancel,
            cancelled_runs,
            cancelled_guardians,
        },
    )
}

#[derive(Clone, Deserialize)]
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

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct StopTarget {
    pane_name: String,
    ghost_task_idx: i64,
    ghost_session_idx: i64,
}

#[derive(Default)]
struct StopCascadePlan {
    state_tasks: BTreeSet<i64>,
    state_sessions: BTreeSet<(i64, i64)>,
    state_verifies: BTreeSet<(i64, String, i64, i64)>,
    stop_sessions: BTreeSet<(i64, i64)>,
    stop_verifies: BTreeSet<(i64, String, i64, i64)>,
}

impl StopCascadePlan {
    fn add_task_state(&mut self, task_idx: i64) {
        self.state_tasks.insert(task_idx);
    }

    fn add_session(&mut self, task_idx: i64, session_idx: i64) {
        self.state_sessions.insert((task_idx, session_idx));
        self.stop_sessions.insert((task_idx, session_idx));
    }

    fn add_verify(&mut self, task_idx: i64, scope: &str, session_idx: i64, verify_idx: i64) {
        let key = (task_idx, scope.to_string(), session_idx, verify_idx);
        self.state_verifies.insert(key.clone());
        self.stop_verifies.insert(key);
    }

    fn state_requests(&self) -> Vec<SetStatusBody> {
        let mut reqs = Vec::new();
        for &task_idx in &self.state_tasks {
            reqs.push(SetStatusBody {
                kind: "task".to_string(),
                task_idx,
                session_idx: 0,
                verify_idx: 0,
                verify_scope: String::new(),
                state: "cancelled".to_string(),
            });
        }
        for &(task_idx, session_idx) in &self.state_sessions {
            reqs.push(SetStatusBody {
                kind: "session".to_string(),
                task_idx,
                session_idx,
                verify_idx: 0,
                verify_scope: String::new(),
                state: "cancelled".to_string(),
            });
        }
        for (task_idx, scope, session_idx, verify_idx) in &self.state_verifies {
            reqs.push(SetStatusBody {
                kind: "verify".to_string(),
                task_idx: *task_idx,
                session_idx: *session_idx,
                verify_idx: *verify_idx,
                verify_scope: scope.clone(),
                state: "cancelled".to_string(),
            });
        }
        reqs
    }

    fn stop_requests(&self) -> Vec<SetStatusBody> {
        let mut reqs = Vec::new();
        for &(task_idx, session_idx) in &self.stop_sessions {
            reqs.push(SetStatusBody {
                kind: "session".to_string(),
                task_idx,
                session_idx,
                verify_idx: 0,
                verify_scope: String::new(),
                state: "cancelled".to_string(),
            });
        }
        for (task_idx, scope, session_idx, verify_idx) in &self.stop_verifies {
            reqs.push(SetStatusBody {
                kind: "verify".to_string(),
                task_idx: *task_idx,
                session_idx: *session_idx,
                verify_idx: *verify_idx,
                verify_scope: scope.clone(),
                state: "cancelled".to_string(),
            });
        }
        reqs
    }
}

fn add_verify_range(
    store: &Store,
    run_id: &str,
    plan: &mut StopCascadePlan,
    task_idx: i64,
    scope: &str,
    session_idx: i64,
    from_idx: i64,
) -> Result<(), StoreError> {
    let start = usize::try_from(from_idx).unwrap_or(usize::MAX);
    for (verify_idx, _) in store
        .verifies_for(run_id, task_idx, scope, session_idx)?
        .iter()
        .enumerate()
        .skip(start)
    {
        plan.add_verify(
            task_idx,
            scope,
            session_idx,
            i64::try_from(verify_idx).unwrap_or(i64::MAX),
        );
    }
    Ok(())
}

fn all_task_sessions(
    store: &Store,
    run_id: &str,
    task_idx: i64,
) -> Result<Vec<(i64, i64)>, StoreError> {
    Ok(store
        .get_task_session_ids(run_id, task_idx)?
        .into_iter()
        .map(|(session_idx, _)| (task_idx, session_idx))
        .collect())
}

fn stop_cascade_plan(
    store: &Store,
    run_id: &str,
    req: &SetStatusBody,
) -> Result<StopCascadePlan, StoreError> {
    let mut plan = StopCascadePlan::default();
    match req.kind.as_str() {
        "task" => {
            let impact = store.compute_task_restart_impact(run_id, req.task_idx)?;
            for task in impact.tasks {
                plan.add_task_state(task.idx);
                add_verify_range(store, run_id, &mut plan, task.idx, "task", -1, 0)?;
            }
            for session in impact.sessions {
                plan.add_session(session.task_idx, session.idx);
                add_verify_range(
                    store,
                    run_id,
                    &mut plan,
                    session.task_idx,
                    "session",
                    session.idx,
                    0,
                )?;
            }
        }
        "session" => {
            let impact =
                store.compute_session_restart_impact(run_id, req.task_idx, req.session_idx)?;
            for task in impact.tasks {
                plan.add_task_state(task.idx);
                add_verify_range(store, run_id, &mut plan, task.idx, "task", -1, 0)?;
            }
            for session in impact.sessions {
                plan.add_session(session.task_idx, session.idx);
                add_verify_range(
                    store,
                    run_id,
                    &mut plan,
                    session.task_idx,
                    "session",
                    session.idx,
                    0,
                )?;
            }
        }
        "verify" if req.verify_scope == "session" => {
            let impact =
                store.compute_session_restart_impact(run_id, req.task_idx, req.session_idx)?;
            for task in &impact.tasks {
                plan.add_task_state(task.idx);
                add_verify_range(store, run_id, &mut plan, task.idx, "task", -1, 0)?;
            }
            add_verify_range(
                store,
                run_id,
                &mut plan,
                req.task_idx,
                "session",
                req.session_idx,
                req.verify_idx,
            )?;
            for session in impact.sessions {
                if session.task_idx == req.task_idx && session.idx == req.session_idx {
                    continue;
                }
                plan.add_session(session.task_idx, session.idx);
                add_verify_range(
                    store,
                    run_id,
                    &mut plan,
                    session.task_idx,
                    "session",
                    session.idx,
                    0,
                )?;
            }
        }
        "verify" if req.verify_scope == "task" => {
            let impact = store.compute_task_restart_impact(run_id, req.task_idx)?;
            for task in &impact.tasks {
                plan.add_task_state(task.idx);
            }
            add_verify_range(
                store,
                run_id,
                &mut plan,
                req.task_idx,
                "task",
                -1,
                req.verify_idx,
            )?;
            for task in impact.tasks {
                if task.idx == req.task_idx {
                    continue;
                }
                add_verify_range(store, run_id, &mut plan, task.idx, "task", -1, 0)?;
            }
            let root_sessions: HashSet<(i64, i64)> =
                all_task_sessions(store, run_id, req.task_idx)?
                    .into_iter()
                    .collect();
            for session in impact.sessions {
                let key = (session.task_idx, session.idx);
                if root_sessions.contains(&key) {
                    continue;
                }
                plan.add_session(session.task_idx, session.idx);
                add_verify_range(
                    store,
                    run_id,
                    &mut plan,
                    session.task_idx,
                    "session",
                    session.idx,
                    0,
                )?;
            }
        }
        "verify" => {
            add_verify_range(
                store,
                run_id,
                &mut plan,
                req.task_idx,
                &req.verify_scope,
                req.session_idx,
                req.verify_idx,
            )?;
        }
        _ => {}
    }
    Ok(plan)
}

fn apply_stop_cascade(
    store: &Store,
    run_id: &str,
    reqs: &[SetStatusBody],
) -> Result<(), StoreError> {
    for req in reqs {
        match req.kind.as_str() {
            "task" => store.set_task_state(run_id, req.task_idx, NodeState::Cancelled)?,
            "session" => store.set_session_state(
                run_id,
                req.task_idx,
                req.session_idx,
                NodeState::Cancelled,
            )?,
            "verify" => store.set_verify_state(
                run_id,
                req.task_idx,
                &req.verify_scope,
                req.session_idx,
                req.verify_idx,
                NodeState::Cancelled,
            )?,
            _ => {}
        }
    }
    Ok(())
}

/// The tmux pane name(s) and owning session that a manual status-set
/// (RAL-163) on a task/session/verify step must capture and stop, or `None`
/// if the target doesn't resolve to a real node.
fn stop_targets_for_status_change_full(
    store: &Store,
    run_id: &str,
    req: &SetStatusBody,
) -> Vec<StopTarget> {
    let Ok(task) = store.get_task_name(run_id, req.task_idx) else {
        return Vec::new();
    };
    match req.kind.as_str() {
        "session" => {
            let Ok(sid) = store.get_session_id(run_id, req.task_idx, req.session_idx) else {
                return Vec::new();
            };
            vec![StopTarget {
                pane_name: crate::tmux::session_name(run_id, &task, &sid),
                ghost_task_idx: req.task_idx,
                ghost_session_idx: req.session_idx,
            }]
        }
        "task" => store
            .get_task_session_ids(run_id, req.task_idx)
            .unwrap_or_default()
            .into_iter()
            .map(|(idx, sid)| StopTarget {
                pane_name: crate::tmux::session_name(run_id, &task, &sid),
                ghost_task_idx: req.task_idx,
                ghost_session_idx: idx,
            })
            .collect(),
        "verify" => {
            let ghost_session_idx = if req.verify_scope == "task" {
                store
                    .get_task_session_ids(run_id, req.task_idx)
                    .unwrap_or_default()
                    .into_iter()
                    .next()
                    .map(|(idx, _)| idx)
            } else {
                Some(req.session_idx)
            };
            let Some(ghost_session_idx) = ghost_session_idx else {
                return Vec::new();
            };
            let (vrun_id, vsession_id) =
                verify_tmux_keys(run_id, &req.verify_scope, &req.verify_idx.to_string());
            vec![StopTarget {
                pane_name: crate::tmux::session_name(&vrun_id, &task, &vsession_id),
                ghost_task_idx: req.task_idx,
                ghost_session_idx,
            }]
        }
        _ => Vec::new(),
    }
}

#[cfg(test)]
/// The tmux pane name(s) and owning-session index that a manual status-set
/// (RAL-163) on a task/session/verify step must capture and stop. Kept as the
/// compact `(pane_name, ghost_session_idx)` shape for the existing unit tests.
fn stop_targets_for_status_change(
    store: &Store,
    run_id: &str,
    req: &SetStatusBody,
) -> Vec<(String, i64)> {
    stop_targets_for_status_change_full(store, run_id, req)
        .into_iter()
        .map(|target| (target.pane_name, target.ghost_session_idx))
        .collect()
}

/// RAL-163: before a manual status change away from `pending` takes effect on
/// a task/session/verify step, capture whatever the agent running under it
/// has produced so far and stop it — turning a hard cutoff into a
/// recoverable checkpoint instead of leaving the agent running in the
/// background with no relationship to the new status. Capture goes into the
/// owning session's ghost (`ghost::upsert_ghost`, merged onto whatever that
/// ghost already held), which `ghost::format_context_block` automatically
/// prepends to that session's prompt on its next attempt — so no separate
/// "pass it to the next worker" plumbing is needed.
///
/// Best-effort throughout, and never blocks the status change itself: most
/// of the time there's no live agent to capture (the common case is
/// overriding an already-finished node), and even a tmux resolution failure
/// shouldn't stop a user from being able to force a status.
fn capture_and_stop_nodes(store: &Store, run_id: &str, reqs: &[SetStatusBody]) {
    let Ok(tmux) = crate::tmux::Tmux::resolve() else {
        return;
    };
    let mut seen = HashSet::new();
    let mut targets = Vec::new();
    for req in reqs {
        for target in stop_targets_for_status_change_full(store, run_id, req) {
            if seen.insert(target.clone()) {
                targets.push(target);
            }
        }
    }
    for target in targets {
        if !tmux.has_session(&target.pane_name) {
            continue;
        }
        if let Ok(content) = tmux.capture_pane(&target.pane_name, 2000) {
            let trimmed = content.trim();
            if !trimmed.is_empty() {
                let uri = crate::ghost::session_uri(
                    run_id,
                    target.ghost_task_idx,
                    target.ghost_session_idx,
                );
                let cwd = store
                    .get_session_agent_resume(
                        run_id,
                        target.ghost_task_idx,
                        target.ghost_session_idx,
                    )
                    .map(|(cwd, _, _)| cwd)
                    .unwrap_or_default();
                let revision = crate::ghost::current_revision(&cwd);
                if store
                    .upsert_ghost(
                        &uri,
                        crate::ghost::KIND_SESSION,
                        Some(run_id),
                        None,
                        trimmed,
                        revision.as_deref(),
                    )
                    .is_ok()
                {
                    crate::cartographer::Note::new("set_status")
                        .run(run_id)
                        .scope("cascade-stop")
                        .emit(
                            store,
                            "captured in-progress agent output before manual status change",
                            serde_json::json!({
                                "pane": target.pane_name,
                                "len": trimmed.len(),
                                "target_state": "cancelled",
                            }),
                        );
                }
            }
        }
        let _ = tmux.kill_session(&target.pane_name);
    }
}

fn capture_and_stop_node(store: &Store, run_id: &str, req: &SetStatusBody) {
    capture_and_stop_nodes(store, run_id, std::slice::from_ref(req));
}

/// Manually override the state of a run, task, session, or verify step (RAL-74).
///
/// Routes through the same store setters used by natural transitions so that
/// audit log entries are written and the scheduler can observe the new state on
/// its next tick (e.g. a run moved to Pending will be claimed and re-executed).
/// Targeting the *run itself* at `cancelled` goes through the same cascading
/// cancel path as the "Cancel Run" button (RAL-116) — not a separate DB-only
/// flip — so both entry points stop every in-flight agent in the run and
/// cascade to dependent runs identically. Targeting a task/session/verify at
/// `cancelled` is the board's scoped "Stop" path (RAL-181): cancel that node
/// plus everything downstream of it *within the same run*, while leaving the
/// rest of the run alive. This is intentionally narrower than the run-wide
/// `Cancel Run` action and intentionally broader than an arbitrary one-row DB
/// flip — it follows the run's existing dependency graph rather than treating
/// sibling branches as collateral damage.
fn set_status(daemon: &Daemon, id: &str, body: &str) -> Reply {
    let Ok(req) = serde_json::from_str::<SetStatusBody>(body) else {
        return error(400, "bad_request", "invalid set-status body", vec![]);
    };
    let store = daemon.lock();
    let stop_plan =
        if matches!(req.kind.as_str(), "task" | "session" | "verify") && req.state == "cancelled" {
            match stop_cascade_plan(&store, id, &req) {
                Ok(plan) => Some(plan),
                Err(StoreError::NotFound) => {
                    return error(404, "not_found", "run or node not found", vec![]);
                }
                Err(StoreError::InvalidTransition(msg)) => {
                    return error(400, "bad_request", &msg, vec![]);
                }
                Err(e) => {
                    return error(500, "internal_error", &e.to_string(), vec![]);
                }
            }
        } else {
            None
        };

    // RAL-163: any status other than "pending" on a task/session/verify step
    // means "stop the agent running there" (including `ignored`, which is
    // deliberately non-terminal in the state machine but still means "stop
    // and save what happened" per the ticket, and `cancelled`). Capture +
    // kill happens before the state change itself is applied below. For a
    // cascading RAL-181 stop, stop every impacted pane in that branch; for any
    // other manual status change, keep the old single-node behavior.
    if matches!(req.kind.as_str(), "task" | "session" | "verify") {
        if let Some(state) = NodeState::parse(&req.state) {
            if state != NodeState::Pending {
                if let Some(plan) = &stop_plan {
                    capture_and_stop_nodes(&store, id, &plan.stop_requests());
                } else {
                    capture_and_stop_node(&store, id, &req);
                }
            }
        }
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
            if state == NodeState::Cancelled {
                if let Some(plan) = &stop_plan {
                    apply_stop_cascade(&store, id, &plan.state_requests())
                } else {
                    store.set_task_state(id, req.task_idx, state)
                }
            } else {
                store.set_task_state(id, req.task_idx, state)
            }
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
            if state == NodeState::Cancelled {
                if let Some(plan) = &stop_plan {
                    apply_stop_cascade(&store, id, &plan.state_requests())
                } else {
                    store.set_session_state(id, req.task_idx, req.session_idx, state)
                }
            } else {
                store.set_session_state(id, req.task_idx, req.session_idx, state)
            }
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
            if state == NodeState::Cancelled {
                if let Some(plan) = &stop_plan {
                    apply_stop_cascade(&store, id, &plan.state_requests())
                } else {
                    store.set_verify_state(
                        id,
                        req.task_idx,
                        &req.verify_scope,
                        req.session_idx,
                        req.verify_idx,
                        state,
                    )
                }
            } else {
                store.set_verify_state(
                    id,
                    req.task_idx,
                    &req.verify_scope,
                    req.session_idx,
                    req.verify_idx,
                    state,
                )
            }
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
        Ok(()) => {
            // RAL-154: a deleted run's durable terminal logs must not outlive
            // the run itself — same `ralphus_<run_id>_` prefix scoping
            // `kill_run_tmux_sessions` already uses to find its live tmux
            // sessions.
            crate::terminal_log::delete_with_prefix(&format!("ralphus_{id}_"));
            json(200, &StateResponse { state: "deleted" })
        }
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
    // RAL-154: same per-entity cleanup `delete_run`/`guardian_delete` do,
    // applied to every run/guardian this bulk clear actually removed.
    for run_id in &outcome.run_ids {
        crate::terminal_log::delete_with_prefix(&format!("ralphus_{run_id}_"));
    }
    let mut seen_guardians = std::collections::HashSet::new();
    for (gid, _) in &outcome.guardian_roots {
        if seen_guardians.insert(gid.clone()) {
            crate::terminal_log::delete_with_prefix(&format!("ralphus_guardian-{gid}_"));
        }
    }
    let mut worktrees_purged = 0;
    if !req.keep_temporary {
        for (gid, root) in &outcome.guardian_roots {
            crate::guardian_merge::purge_worktrees(&daemon.store_handle(), root, gid);
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
    /// RAL-168: this review's own Verify-scope override -- `"each_branch"`,
    /// `"final_branch"`, or `"nothing"`. An empty string resets it to
    /// "inherit the project default" (same empty-string-means-unset
    /// convention `resolver_agent`/`resolver_model` above use).
    #[serde(default)]
    verify_scope: Option<String>,
    /// RAL-168: this review's own override for `"each_branch"` scope's
    /// auto-clean-skip sub-option. Always an explicit override when present
    /// -- a boolean has no natural "reset to inherit" sentinel the way an
    /// empty string works for `verify_scope`.
    #[serde(default)]
    verify_skip_auto_clean: Option<bool>,
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
    if let Some(scope) = req.verify_scope.as_deref() {
        let scope = if scope.is_empty() { None } else { Some(scope) };
        if let Err(e) = store.set_guardian_verify_scope(id, scope) {
            return store_error(&e);
        }
    }
    if let Some(skip) = req.verify_skip_auto_clean {
        if let Err(e) = store.set_guardian_verify_skip_auto_clean(id, Some(skip)) {
            return store_error(&e);
        }
    }
    // RAL-213: every setting above is a plain DB column write that a running
    // merge never re-reads mid-flight -- restart it now so the new setting
    // actually takes effect on this build instead of only the next one.
    // Best-effort, same convention as `apply_restart_note`: the settings
    // writes above have already succeeded and must not be undone by a
    // restart hiccup, so a failure here is logged, not surfaced to the caller.
    let status = store.get_guardian(id).map(|g| g.status).ok();
    drop(store);
    if status.as_deref() == Some("merging") {
        let runner = guardian_agent_runner(daemon);
        let restarted = crate::guardian_merge::restart_guardian_merge(
            daemon.store_handle(),
            daemon.cancellations_handle(),
            runner,
            id,
            daemon.semaphore_handle(),
        );
        if restarted.status >= 400 {
            crate::rlog!(
                WARNING,
                "ralphus [guardian] review {id} settings-triggered merge restart failed: {}",
                restarted.body
            );
        }
    }
    match daemon.lock().get_guardian(id) {
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

/// Live drift check between this PR's remote branch and its owning review
/// worktree (RAL-190) — see [`crate::pr::PrSyncStatus`].
fn pr_sync_status(daemon: &Daemon, pr_id: &str) -> Reply {
    match crate::pr::compute_sync_status(&daemon.store_handle(), pr_id) {
        Ok(status) => json(200, &status),
        Err(e) => error(502, "forge_error", &e, vec![]),
    }
}

/// Kick off pulling the PR branch's fetched commits into its owning review
/// worktree in the background (RAL-190's "Pull PR commits" button) — see
/// [`crate::pr::pull_pr_commits`].
fn pr_pull_from_pr(daemon: &Daemon, pr_id: &str) -> Reply {
    let runner: Arc<dyn Runner> =
        Arc::new(SubprocessRunner::from_env().with_cartographer(daemon.store_handle()));
    crate::pr::start_pull_pr_commits(daemon.store_handle(), runner, pr_id)
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
    // Snapshot everything needed for cleanup before the row is gone
    // (multi-project guardians span >1 root).
    let snapshot = store.get_guardian(id).ok();
    match store.delete_guardian(id) {
        Ok(()) => {
            drop(store);
            if let Some(g) = snapshot {
                for root in &g.projects {
                    crate::guardian_merge::purge_worktrees(&daemon.store_handle(), root, id);
                }
            }
            // RAL-154: same scoping as `kill_guardian_tmux_sessions` — a
            // deleted guardian's durable terminal logs (resolver/manual-checks
            // sessions) must not outlive it.
            crate::terminal_log::delete_with_prefix(&format!("ralphus_guardian-{id}_"));
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
    let result = match store.get_guardian(id) {
        Ok(g) => json(200, &g),
        Err(e) => return store_error(&e),
    };
    drop(store);
    // RAL-190: a reorder can change which branch precedes a stacked PR, so
    // its forge base needs to follow -- backgrounded since it makes network
    // calls (see pr::resync_pr_bases's doc comment).
    crate::pr::start_resync_pr_bases(daemon.store_handle(), id);
    result
}

/// The runner for a guardian review's own agent invocations -- the merge's
/// conflict resolver, verify-instruction synthesis, summary/manual-commands
/// generation, chat triage, feedback and "set it for me" input resolution
/// (RAL-201).
///
/// Wrapped in [`crate::remote_runner::MachineRouter`] rather than handed the
/// bare local runner directly: every `RunnerSpec` these call sites build
/// carries the review's actual `machine` (see the RAL-201 fixes in
/// `guardian_merge.rs`), and without this router that field had nowhere to
/// go -- a bare [`SubprocessRunner`] ignores it and always runs locally. A
/// local review pays nothing extra: `MachineRouter`'s local path is the same
/// `SubprocessRunner::run` call this always was.
fn guardian_agent_runner(daemon: &Daemon) -> Arc<dyn Runner> {
    let local: Arc<dyn Runner> =
        Arc::new(SubprocessRunner::from_env().with_cartographer(daemon.store_handle()));
    Arc::new(crate::remote_runner::MachineRouter::new(
        local,
        daemon.store_handle(),
    ))
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
    // RAL-190: see the identical call in `guardian_reorder`.
    crate::pr::start_resync_pr_bases(daemon.store_handle(), id);
    let runner = guardian_agent_runner(daemon);
    crate::guardian_merge::start_merge(
        daemon.store_handle(),
        runner,
        id,
        daemon.semaphore_handle(),
        daemon.cancellations_handle(),
    )
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

/// Resolve `{name}` placeholders in `text` from a check's declared inputs
/// (RAL-164): a value submitted with this run wins, then a previously
/// resolved/submitted value stored on the guardian, then the input's own
/// literal default.
fn substitute_check_inputs(
    text: &str,
    inputs: &[crate::guardian::CheckInput],
    submitted: &std::collections::HashMap<String, String>,
    stored: &std::collections::HashMap<String, String>,
) -> String {
    let mut out = text.to_string();
    for input in inputs {
        let value = submitted
            .get(&input.name)
            .or_else(|| stored.get(&input.name))
            .unwrap_or(&input.default);
        out = out.replace(&format!("{{{}}}", input.name), value);
    }
    out
}

/// Build the full `cmd /K` command line for running a check (RAL-164):
/// substitutes input placeholders (see [`substitute_check_inputs`]) and, when
/// `run_cleanup` is set and the check declares a `cleanup_command`, chains it
/// before the main command with `&` (not `&&`) so a cleanup that "fails"
/// because there was nothing to kill doesn't block the main command from
/// running. `None` when the check has no `command` (e.g. a prompt-kind
/// action hint).
fn build_check_command_line(
    cwd: &str,
    check: &crate::guardian::GuardianCheck,
    submitted_inputs: &std::collections::HashMap<String, String>,
    stored_inputs: &std::collections::HashMap<String, String>,
    run_cleanup: bool,
) -> Option<String> {
    let cmd = check.command.as_ref()?;
    let resolved = substitute_check_inputs(cmd, &check.inputs, submitted_inputs, stored_inputs);
    let body = if run_cleanup {
        check.cleanup_command.as_ref().map_or_else(
            || resolved.clone(),
            |cleanup| {
                let resolved_cleanup = substitute_check_inputs(
                    cleanup,
                    &check.inputs,
                    submitted_inputs,
                    stored_inputs,
                );
                format!("({resolved_cleanup}) & {resolved}")
            },
        )
    } else {
        resolved
    };
    // cd /d sets both drive and directory on Windows before running the command.
    Some(format!("cd /d \"{cwd}\" && {body}"))
}

/// Run one or all LLM-generated manual review commands as fire-and-forget
/// terminal subprocesses (RAL-27). Body `{ "index": N, "inputs": {...},
/// "run_cleanup": bool }` runs command N only; no body (or `{}`) runs all
/// commands. Opens each in a new terminal window, under this review's
/// `manual_checks_env` (RAL-203) -- the union of every enabled branch's own
/// resolved environment, with this review's manual-checks-step overrides
/// applied on top, so the "Run all" launcher exercises the same variables
/// the code was written and reviewed under. `inputs` (RAL-164) substitutes
/// named `{name}` placeholders and, when non-empty, is persisted as the new
/// default for those inputs on this guardian.
fn guardian_run_manual_commands(daemon: &Daemon, id: &str, body: &str) -> Reply {
    #[derive(Deserialize, Default)]
    struct Body {
        index: Option<usize>,
        #[serde(default)]
        inputs: std::collections::HashMap<String, String>,
        #[serde(default)]
        run_cleanup: bool,
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

    let to_run: Vec<crate::guardian::GuardianCheck> = match req.index {
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
    for check in &to_run {
        let Some(full_cmd) =
            build_check_command_line(&cwd, check, &req.inputs, &g.input_values, req.run_cleanup)
        else {
            continue;
        };
        if let Err(e) = spawn_in_terminal(
            None,
            "cmd",
            &["/K".to_string(), full_cmd],
            &g.manual_checks_env,
        ) {
            errors.push(e);
        }
    }

    if !req.inputs.is_empty() {
        let _ = daemon.lock().merge_guardian_input_values(id, &req.inputs);
    }

    if errors.is_empty() {
        json(200, &OpenTerminalResponse { ok: true })
    } else {
        error(500, "terminal_error", &errors.join("; "), vec![])
    }
}

/// Run a user-declared action hint from `[[review.action]]` (RAL-77).
/// Body `{ "index": N, "inputs": {...}, "run_cleanup": bool }` runs the hint
/// at position N (required). `command`-kind hints are opened in a terminal;
/// `prompt`-kind hints are not yet executed (reserved for a future
/// LLM-expansion step) and return 501. `inputs` (RAL-164) substitutes named
/// `{name}` placeholders and, when non-empty, is persisted as the new default
/// for those inputs on this guardian.
fn guardian_run_action_hint(daemon: &Daemon, id: &str, body: &str) -> Reply {
    #[derive(Deserialize)]
    struct Body {
        index: usize,
        #[serde(default)]
        inputs: std::collections::HashMap<String, String>,
        #[serde(default)]
        run_cleanup: bool,
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

    // Same reasoning as guardian_run_manual_commands, including the RAL-203
    // manual-checks environment -- action hints run in the same combined
    // worktree via the same terminal-spawning mechanism, so they inherit it
    // too: prefer the built review worktree over the original repo root.
    let cwd = g.combined_worktree.clone().unwrap_or(g.git_root.clone());
    if let Some(full_cmd) =
        build_check_command_line(&cwd, &hint, &req.inputs, &g.input_values, req.run_cleanup)
    {
        let result = spawn_in_terminal(
            None,
            "cmd",
            &["/K".to_string(), full_cmd],
            &g.manual_checks_env,
        );
        if !req.inputs.is_empty() {
            let _ = daemon.lock().merge_guardian_input_values(id, &req.inputs);
        }
        match result {
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

/// Kick off "set it for me" AI resolution of one named check input
/// (RAL-164). Body `{ "input_name": "..." }`. Returns `202` immediately —
/// the board picks up completion through its existing guardian-view poll
/// (`GuardianView.input_resolutions`), not a dedicated polling endpoint.
fn guardian_resolve_input(daemon: &Daemon, id: &str, body: &str) -> Reply {
    #[derive(Deserialize)]
    struct Body {
        input_name: String,
    }
    let Ok(req) = serde_json::from_str::<Body>(body) else {
        return error(
            400,
            "bad_request",
            "body must be {\"input_name\": \"...\"}",
            vec![],
        );
    };
    let runner = guardian_agent_runner(daemon);
    crate::guardian_merge::start_resolve_input(
        daemon.store_handle(),
        runner,
        id,
        &req.input_name,
        daemon.semaphore_handle(),
    )
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
    let runner = guardian_agent_runner(daemon);
    crate::guardian_merge::start_merge(
        daemon.store_handle(),
        runner,
        id,
        daemon.semaphore_handle(),
        daemon.cancellations_handle(),
    )
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
    let runner = guardian_agent_runner(daemon);
    crate::guardian_merge::start_merge(
        daemon.store_handle(),
        runner,
        id,
        daemon.semaphore_handle(),
        daemon.cancellations_handle(),
    )
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
    crate::guardian_merge::purge_worktrees(&daemon.store_handle(), &source_git_root, id);

    let runner = guardian_agent_runner(daemon);
    if source_remaining > 0 {
        let _ = crate::guardian_merge::start_merge(
            daemon.store_handle(),
            Arc::clone(&runner),
            id,
            daemon.semaphore_handle(),
            daemon.cancellations_handle(),
        );
    }
    crate::guardian_merge::start_merge(
        daemon.store_handle(),
        runner,
        &to_guardian_id,
        daemon.semaphore_handle(),
        daemon.cancellations_handle(),
    )
}

fn guardian_merge(daemon: &Daemon, id: &str) -> Reply {
    let runner = guardian_agent_runner(daemon);
    crate::guardian_merge::start_merge(
        daemon.store_handle(),
        runner,
        id,
        daemon.semaphore_handle(),
        daemon.cancellations_handle(),
    )
}

/// Cancel an in-progress rebase (status `merging` or `in_review`) and
/// immediately start a fresh one.
///
/// RAL-213: routed through [`crate::guardian_merge::restart_guardian_merge`],
/// which actually stops the in-flight merge worker (cancel + bounded wait)
/// before resetting state and starting the new one -- this used to just reset
/// the DB status and spawn a second merge thread without stopping the first,
/// which raced on the same worktree paths/branch names/carry refs the two
/// threads shared (RAL-213's second bug: `cancel_and_merge` was already
/// unsafe before this fix).
fn guardian_cancel_and_merge(daemon: &Daemon, id: &str) -> Reply {
    let runner = guardian_agent_runner(daemon);
    crate::guardian_merge::restart_guardian_merge(
        daemon.store_handle(),
        daemon.cancellations_handle(),
        runner,
        id,
        daemon.semaphore_handle(),
    )
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
    let runner = guardian_agent_runner(daemon);
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
    let runner = guardian_agent_runner(daemon);
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
            log_path: None,
            payload: serde_json::json!({"seq": req.seq}),
        });
    }
    if let Err(e) = daemon.lock().delete_guardian_messages_from_seq(id, req.seq) {
        return store_error(&e);
    }
    let runner = guardian_agent_runner(daemon);
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
                log_path: None,
                payload: serde_json::json!({"run_ids": ids}),
            });
        }
        Ok(_) => {}
        Err(e) => crate::rlog!(ERROR, "ralphus [recovery] run recovery failed: {e}"),
    }
    // RAL-201: reconcile any remote `exec` handle left behind by the crash
    // `recover_orphaned_runs` just reset above -- same "nothing is executing
    // yet" invariant, so it belongs immediately alongside it rather than
    // deferred to first use.
    crate::remote_runner::reconcile_remote_exec_handles(&store);
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
                log_path: None,
                payload: serde_json::json!({"guardian_ids": ids}),
            });
        }
        Ok(_) => {}
        Err(e) => crate::rlog!(ERROR, "ralphus [recovery] merge recovery failed: {e}"),
    }
    // RAL-164: "set it for me" input resolutions left `resolving` after an
    // unclean shutdown have no background thread; reset them to `failed` so
    // the UI never shows a permanently-stuck spinner.
    match store.recover_orphaned_input_resolutions() {
        Ok(pairs) if !pairs.is_empty() => {
            crate::rlog!(
                WARNING,
                "ralphus [recovery] {} orphaned input resolution(s) reset to failed: {}",
                pairs.len(),
                pairs
                    .iter()
                    .map(|(g, n)| format!("{g}/{n}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            let _ = store.cartographer_log(crate::cartographer::CartographerEntry {
                level: crate::logging::LogLevel::WARNING,
                source: "recovery",
                message: "orphaned input resolutions reset to failed",
                scope: Some("guardian"),
                run_id: None,
                guardian_id: None,
                session_id: None,
                task: None,
                log_path: None,
                payload: serde_json::json!({"pairs": pairs}),
            });
        }
        Ok(_) => {}
        Err(e) => crate::rlog!(
            ERROR,
            "ralphus [recovery] input resolution recovery failed: {e}"
        ),
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
            log_path: None,
            payload: serde_json::json!({"count": reaped}),
        });
    }
    let daemon = Daemon::new(store, max_concurrent);

    // Scheduler runs on its own thread, sharing the store via Arc<Mutex> and the
    // cancellation registry so a `cancel` request can reach its workers. The
    // runner shares the daemon's PID registry so `/api/resources` can attribute
    // OS metrics to the sessions it spawns (RAL-11).
    let local_runner: Arc<dyn Runner> = Arc::new(
        SubprocessRunner::from_env()
            .with_registry(daemon.procs_handle())
            .with_cartographer(daemon.store_handle()),
    );
    // RAL-185: the scheduler holds a router rather than the local runner
    // directly, so a session carrying a `machine` is dispatched to its provider
    // while every local session takes exactly the path it always did.
    let runner: Arc<dyn Runner> = Arc::new(crate::remote_runner::MachineRouter::new(
        local_runner,
        daemon.store_handle(),
    ));
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

/// The SSE push endpoint (RAL-167). Handled specially in `run_http_loop`
/// below rather than through `route()`: every other endpoint returns a
/// bounded `Reply` immediately, but this one holds the connection open
/// indefinitely.
const EVENTS_PATH: &str = "/api/events";

fn run_http_loop(server: tiny_http::Server, daemon: &Daemon) {
    for mut request in server.incoming_requests() {
        let method = request.method().as_str().to_string();
        // `route()` splits `path` on `?` itself (it needs the query string for
        // filtered/paginated endpoints like `/api/cartographer`), so the full
        // URL is forwarded verbatim rather than pre-stripped here.
        let url = request.url().to_string();

        if method == "GET" && url.split('?').next().unwrap_or(&url) == EVENTS_PATH {
            // A long-lived SSE stream must never block this accept loop from
            // serving anyone else -- tiny_http is otherwise a synchronous,
            // one-request-at-a-time server (RAL-167's own risk list). Hand it
            // off to its own thread; it lives until the client disconnects.
            let store = daemon.store_handle();
            std::thread::spawn(move || serve_events_stream(request, &store));
            continue;
        }

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
        // Requested by `POST /api/daemon/shutdown` (`shutdown()` above).
        // Breaking here — rather than calling `server.unblock()` — is
        // sufficient: this request has already been answered and we simply
        // never call `.next()` again, so `serve()` returns and `main()` can
        // exit normally, dropping `_job_guard` to kill every remaining
        // subprocess (see `jobobject.rs`).
        if daemon.shutdown_requested() {
            crate::rlog!(
                INFO,
                "ralphus [http] shutdown requested — stopping HTTP loop"
            );
            break;
        }
    }
}

/// How often [`serve_events_stream`] writes an SSE comment/keepalive line
/// when no real event has arrived — long enough to stay well clear of
/// "constant chatter," short enough that a proxy/browser doesn't decide the
/// connection is dead and a disconnect is noticed promptly (RAL-167).
const SSE_HEARTBEAT: Duration = Duration::from_secs(15);

/// Serve one SSE connection until the client disconnects (RAL-167).
///
/// Writes the HTTP response headers manually via `Request::into_writer()` —
/// tiny_http's escape hatch for a response whose length isn't known upfront
/// and that isn't produced by a single call to `respond()` — then loops
/// forever: each event from the subscriber channel becomes one `event: ...\n
/// data: ...\n\n` block (`event:` is the entity kind — `run`/`guardian`/
/// `other`, see `crate::events::EventKind` — so the browser can react
/// differently per surface without a full-page refetch on every event); a
/// receive timeout with nothing pending becomes an SSE comment line so idle
/// proxies/browsers don't time the connection out. Any write failure (the
/// client went away) ends the loop and unsubscribes.
fn serve_events_stream(request: tiny_http::Request, store: &Arc<Mutex<Store>>) {
    let (sub_id, rx) = store.lock().unwrap().event_bus().subscribe();
    crate::rlog!(DEBUG, "ralphus [http] SSE client connected sub_id={sub_id}");
    let mut writer = request.into_writer();
    let preamble = b"HTTP/1.1 200 OK\r\n\
Content-Type: text/event-stream\r\n\
Cache-Control: no-cache\r\n\
Connection: keep-alive\r\n\
X-Accel-Buffering: no\r\n\
\r\n";
    let mut ok = writer.write_all(preamble).is_ok() && writer.flush().is_ok();
    while ok {
        ok = match rx.recv_timeout(SSE_HEARTBEAT) {
            Ok(event) => {
                let payload =
                    serde_json::to_string(&event.row).unwrap_or_else(|_| "{}".to_string());
                let line = format!("event: {}\ndata: {payload}\n\n", event.kind.as_str());
                writer.write_all(line.as_bytes()).is_ok()
            }
            Err(RecvTimeoutError::Timeout) => writer.write_all(b": heartbeat\n\n").is_ok(),
            Err(RecvTimeoutError::Disconnected) => false,
        } && writer.flush().is_ok();
    }
    store.lock().unwrap().event_bus().unsubscribe(sub_id);
    crate::rlog!(
        DEBUG,
        "ralphus [http] SSE client disconnected sub_id={sub_id}"
    );
}

#[cfg(test)]
mod tests {
    // Test harness output (`SKIP:` notices) legitimately goes to stdout so
    // `cargo test --nocapture` shows it; no JSON contract exists here.
    #![allow(clippy::print_stdout)]

    use super::*;

    const GOOD: &str = "[[task]]\nname=\"t\"\n[[task.session]]\ncwd=\"/r\"\nprompt=\"p\"\n";

    fn daemon() -> Daemon {
        Daemon::new(Store::open_in_memory().unwrap(), 12)
    }

    fn submit_body(toml: &str) -> String {
        serde_json::to_string(&serde_json::json!({ "toml": toml })).unwrap()
    }

    // -----------------------------------------------------------------------
    // Structured check input substitution + cleanup chaining (RAL-164)
    // -----------------------------------------------------------------------

    fn check_input(name: &str, message: &str, default: &str) -> crate::guardian::CheckInput {
        crate::guardian::CheckInput {
            name: name.to_string(),
            message: message.to_string(),
            default: default.to_string(),
        }
    }

    #[test]
    fn substitute_check_inputs_prefers_submitted_over_stored_over_default() {
        let inputs = vec![check_input("port", "Port", "7890")];
        let submitted = std::collections::HashMap::from([("port".to_string(), "9001".to_string())]);
        let stored = std::collections::HashMap::from([("port".to_string(), "9002".to_string())]);
        assert_eq!(
            substitute_check_inputs("serve --port {port}", &inputs, &submitted, &stored),
            "serve --port 9001"
        );
        assert_eq!(
            substitute_check_inputs(
                "serve --port {port}",
                &inputs,
                &std::collections::HashMap::new(),
                &stored
            ),
            "serve --port 9002"
        );
        assert_eq!(
            substitute_check_inputs(
                "serve --port {port}",
                &inputs,
                &std::collections::HashMap::new(),
                &std::collections::HashMap::new()
            ),
            "serve --port 7890"
        );
    }

    #[test]
    fn build_check_command_line_without_cleanup_is_plain() {
        let check = crate::guardian::GuardianCheck {
            label: None,
            command: Some("cargo test".to_string()),
            prompt: None,
            cleanup_command: Some("echo would-clean".to_string()),
            inputs: vec![],
        };
        let empty = std::collections::HashMap::new();
        let line =
            build_check_command_line("C:/repo", &check, &empty, &empty, false).expect("command");
        assert_eq!(line, "cd /d \"C:/repo\" && cargo test");
    }

    #[test]
    fn build_check_command_line_with_cleanup_chains_with_ampersand_not_double() {
        let check = crate::guardian::GuardianCheck {
            label: None,
            command: Some("ralphus-daemon serve --port {port}".to_string()),
            prompt: None,
            cleanup_command: Some("ralphus-daemon stop --port {port}".to_string()),
            inputs: vec![check_input("port", "Port", "7890")],
        };
        let submitted = std::collections::HashMap::from([("port".to_string(), "9001".to_string())]);
        let stored = std::collections::HashMap::new();
        let line = build_check_command_line("C:/repo", &check, &submitted, &stored, true)
            .expect("command");
        assert_eq!(
            line,
            "cd /d \"C:/repo\" && (ralphus-daemon stop --port 9001) & ralphus-daemon serve --port 9001"
        );
    }

    #[test]
    fn build_check_command_line_prompt_only_check_is_none() {
        let check = crate::guardian::GuardianCheck {
            label: Some("Check UI".to_string()),
            command: None,
            prompt: Some("open localhost:3000".to_string()),
            cleanup_command: None,
            inputs: vec![],
        };
        let empty = std::collections::HashMap::new();
        assert!(build_check_command_line("C:/repo", &check, &empty, &empty, false).is_none());
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

    // ── machine provider registry (RAL-185) ───────────────────────────────

    fn machine_body(scheme: &str, program: &str) -> String {
        serde_json::json!({"scheme": scheme, "program": program, "description": "d"}).to_string()
    }

    #[test]
    fn machine_register_list_get_and_deregister_roundtrip() {
        let d = daemon();
        let r = route(
            &d,
            "POST",
            "/api/machines",
            &machine_body("incredibuild", "/opt/ib.sh"),
        );
        assert_eq!(r.status, 201, "{}", r.body);

        let r = route(&d, "GET", "/api/machines", "");
        assert_eq!(r.status, 200, "{}", r.body);
        assert!(r.body.contains("incredibuild"), "{}", r.body);
        // The built-in is advertised alongside registered rows so a client
        // doesn't think `local` is missing.
        assert!(r.body.contains("\"builtin\":[\"local\"]"), "{}", r.body);

        let r = route(&d, "GET", "/api/machines/incredibuild", "");
        assert_eq!(r.status, 200, "{}", r.body);
        assert!(r.body.contains("/opt/ib.sh"), "{}", r.body);

        let r = route(&d, "DELETE", "/api/machines/incredibuild", "");
        assert_eq!(r.status, 200, "{}", r.body);
        let r = route(&d, "GET", "/api/machines/incredibuild", "");
        assert_eq!(r.status, 404, "{}", r.body);
    }

    #[test]
    fn checking_an_unreachable_machine_records_the_failure_without_erroring() {
        // The endpoint reports *a result*, not an error: "this machine is down"
        // is a successful answer to the question the board is asking.
        let d = daemon();
        route(
            &d,
            "POST",
            "/api/machines",
            &machine_body("ib", "/definitely/not/a/real/provider"),
        );
        let r = route(&d, "POST", "/api/machines/ib/check", "");
        assert_eq!(r.status, 200, "{}", r.body);
        assert!(r.body.contains("\"ok\":false"), "{}", r.body);

        // And it is remembered, so the tab shows the last known answer.
        let m = d.lock().get_machine_provider("ib").unwrap().unwrap();
        assert_eq!(m.last_check_ok, Some(false));
        assert!(m.last_check_ms.is_some());
        assert!(m.last_check_note.is_some_and(|n| !n.is_empty()));
    }

    #[test]
    fn a_freshly_registered_machine_reports_not_checked_rather_than_healthy() {
        // "not checked" must be distinguishable from "reachable" -- claiming a
        // machine is fine on no evidence is the failure this guards against.
        let d = daemon();
        route(
            &d,
            "POST",
            "/api/machines",
            &machine_body("ib", "/opt/ib.sh"),
        );
        let m = d.lock().get_machine_provider("ib").unwrap().unwrap();
        assert_eq!(m.last_check_ok, None);
        assert_eq!(m.last_check_ms, None);
    }

    #[test]
    fn machine_register_rejects_overriding_a_builtin_scheme() {
        let d = daemon();
        let r = route(&d, "POST", "/api/machines", &machine_body("local", "/x.sh"));
        assert_eq!(r.status, 400, "{}", r.body);
        assert!(r.body.contains("built-in"), "{}", r.body);
    }

    #[test]
    fn machine_register_rejects_an_unusable_scheme() {
        let d = daemon();
        // A one-character scheme can never be referenced, because `parse_machine`
        // rejects it to keep Windows drive letters from parsing as schemes.
        let r = route(&d, "POST", "/api/machines", &machine_body("C", "/x.sh"));
        assert_eq!(r.status, 400, "{}", r.body);
        let r = route(&d, "POST", "/api/machines", &machine_body("ib", ""));
        assert_eq!(r.status, 400, "{}", r.body);
    }

    #[test]
    fn submit_rejects_an_unregistered_machine_scheme() {
        let d = daemon();
        let toml = "[[task]]\nname=\"t\"\nmachine=\"ghostfarm:A\"\n[[task.session]]\ncwd=\".\"\nprompt=\"p\"\n";
        let r = route(&d, "POST", "/api/runs", &submit_body(toml));
        assert_eq!(r.status, 400, "{}", r.body);
        assert!(r.body.contains("machine_validation_failed"), "{}", r.body);
        assert!(r.body.contains("ghostfarm"), "{}", r.body);
    }

    #[test]
    fn submit_accepts_a_registered_remote_machine_and_persists_it_on_the_session() {
        // Phase 2: a registered machine now submits successfully, and the
        // resolved value is stored on the session so the scheduler's router can
        // dispatch it without re-deriving inheritance.
        let d = daemon();
        route(
            &d,
            "POST",
            "/api/machines",
            &machine_body("incredibuild", "/opt/ib.sh"),
        );
        let toml = "[[task]]\nname=\"t\"\nmachine=\"incredibuild:A\"\n[[task.session]]\ncwd=\".\"\nprompt=\"p\"\n";
        let r = route(&d, "POST", "/api/runs", &submit_body(toml));
        assert_eq!(r.status, 201, "{}", r.body);
        let run_id = serde_json::from_str::<serde_json::Value>(&r.body).unwrap()["run_id"]
            .as_str()
            .unwrap()
            .to_string();
        let sessions = d.lock().sessions_of(&run_id).unwrap();
        assert_eq!(sessions[0].machine.as_deref(), Some("incredibuild:A"));
    }

    #[test]
    fn a_session_inherits_its_tasks_machine_when_it_declares_none() {
        let d = daemon();
        route(
            &d,
            "POST",
            "/api/machines",
            &machine_body("incredibuild", "/opt/ib.sh"),
        );
        // Both sessions resolve to the task's machine: the first by inheriting
        // it, the second by restating it. They may not diverge — see
        // `two_sessions_in_one_task_may_not_run_on_different_machines`.
        let toml = "[[task]]\nname=\"t\"\nmachine=\"incredibuild:A\"\n\
                    [[task.session]]\ncwd=\".\"\nprompt=\"p\"\n\
                    [[task.session]]\ncwd=\".\"\nprompt=\"q\"\nmachine=\"incredibuild:A\"\n";
        let r = route(&d, "POST", "/api/runs", &submit_body(toml));
        assert_eq!(r.status, 201, "{}", r.body);
        let run_id = serde_json::from_str::<serde_json::Value>(&r.body).unwrap()["run_id"]
            .as_str()
            .unwrap()
            .to_string();
        let sessions = d.lock().sessions_of(&run_id).unwrap();
        assert_eq!(sessions[0].machine.as_deref(), Some("incredibuild:A"));
        assert_eq!(sessions[1].machine.as_deref(), Some("incredibuild:A"));
    }

    #[test]
    fn submit_accepts_a_remote_session_with_a_worktree_placeholder() {
        // RAL-185 Phase 2: the placeholder is now materialized by the
        // provider's `provision` verb on its own machine (see
        // `crate::worktrees::provision_remote`), so this combination is valid.
        // It is deliberately NOT resolved at submit -- provisioning happens at
        // schedule time, alongside local worktree materialization.
        let d = daemon();
        let repo = tmp_git_repo("remote-placeholder");
        route(
            &d,
            "POST",
            "/api/projects",
            &register_body("proj", &repo.to_string_lossy(), ""),
        );
        route(
            &d,
            "POST",
            "/api/machines",
            &machine_body("incredibuild", "/opt/ib.sh"),
        );
        let toml = "[[task]]\nname=\"t\"\nproject=\"proj\"\nmachine=\"incredibuild:A\"\n\
                    [[task.session]]\nid=\"work\"\ncwd=\"ralphus:new-worktree/feat\"\nprompt=\"p\"\n";
        let r = route(&d, "POST", "/api/runs", &submit_body(toml));
        assert_eq!(r.status, 201, "{}", r.body);
    }

    #[test]
    fn a_local_session_may_still_use_a_worktree_placeholder() {
        // Regression guard: the check above must not affect ordinary local runs.
        let d = daemon();
        let repo = tmp_git_repo("local-placeholder-ok");
        route(
            &d,
            "POST",
            "/api/projects",
            &register_body("proj", &repo.to_string_lossy(), ""),
        );
        let toml = "[[task]]\nname=\"t\"\nproject=\"proj\"\nmachine=\"local\"\n\
                    [[task.session]]\ncwd=\"ralphus:new-worktree/feat\"\nprompt=\"p\"\n";
        let r = route(&d, "POST", "/api/runs", &submit_body(toml));
        assert_eq!(r.status, 201, "{}", r.body);
    }

    // ── Phase 3a: within-task machine affinity ────────────────────────────

    /// Submit `toml` against a daemon with `incredibuild` registered, returning
    /// the reply so a test can assert on accept/reject.
    fn submit_with_machines(toml: &str) -> Reply {
        let d = daemon();
        route(
            &d,
            "POST",
            "/api/machines",
            &machine_body("incredibuild", "/opt/ib.sh"),
        );
        route(&d, "POST", "/api/runs", &submit_body(toml))
    }

    #[test]
    fn two_sessions_in_one_task_may_not_run_on_different_machines() {
        // They share one workspace and hand off through files on disk, so
        // splitting them would leave the second reading a directory the first
        // never wrote to.
        let r = submit_with_machines(
            "[[task]]
name=\"t\"
             [[task.session]]
id=\"a\"
cwd=\".\"
prompt=\"p\"
machine=\"incredibuild:A\"
             [[task.session]]
id=\"b\"
cwd=\".\"
prompt=\"q\"
machine=\"incredibuild:B\"
",
        );
        assert_eq!(r.status, 400, "{}", r.body);
        assert!(r.body.contains("spans two machines"), "{}", r.body);
        // The message must name both sides, or it is not actionable.
        assert!(r.body.contains("incredibuild:A"), "{}", r.body);
        assert!(r.body.contains("incredibuild:B"), "{}", r.body);
        assert!(r.body.contains("session \\\"b\\\""), "{}", r.body);
    }

    #[test]
    fn a_session_may_not_diverge_from_its_own_tasks_machine() {
        let r = submit_with_machines(
            "[[task]]
name=\"t\"
machine=\"incredibuild:A\"
             [[task.session]]
id=\"a\"
cwd=\".\"
prompt=\"p\"
machine=\"incredibuild:B\"
",
        );
        assert_eq!(r.status, 400, "{}", r.body);
        assert!(r.body.contains("spans two machines"), "{}", r.body);
        assert!(r.body.contains("task \\\"t\\\""), "{}", r.body);
    }

    #[test]
    fn a_verify_step_may_not_diverge_from_its_owning_session() {
        let r = submit_with_machines(
            "[[task]]
name=\"t\"
machine=\"incredibuild:A\"
             [[task.session]]
id=\"a\"
cwd=\".\"
prompt=\"p\"
             [[task.session.verify]]
command=\"cargo test\"
machine=\"incredibuild:B\"
",
        );
        assert_eq!(r.status, 400, "{}", r.body);
        assert!(r.body.contains("verify step"), "{}", r.body);
    }

    #[test]
    fn a_task_scope_verify_may_not_diverge_from_its_task() {
        let r = submit_with_machines(
            "[[task]]
name=\"t\"
machine=\"incredibuild:A\"
             [[task.session]]
cwd=\".\"
prompt=\"p\"
             [[task.verify]]
command=\"cargo fmt\"
machine=\"incredibuild:B\"
",
        );
        assert_eq!(r.status, 400, "{}", r.body);
        assert!(r.body.contains("task verify step"), "{}", r.body);
    }

    #[test]
    fn restating_the_same_machine_throughout_a_task_is_allowed() {
        // An explicit override that agrees with the task is redundant, not wrong.
        let r = submit_with_machines(
            "[[task]]
name=\"t\"
machine=\"incredibuild:A\"
             [[task.session]]
cwd=\".\"
prompt=\"p\"
machine=\"incredibuild:A\"
             [[task.session.verify]]
command=\"cargo test\"
machine=\"incredibuild:A\"
             [[task.verify]]
command=\"cargo fmt\"
machine=\"incredibuild:A\"
",
        );
        assert_eq!(r.status, 201, "{}", r.body);
    }

    #[test]
    fn different_tasks_may_run_on_different_machines() {
        // The whole point of the feature -- fanning work across a build farm.
        let r = submit_with_machines(
            "[[task]]
name=\"a\"
machine=\"incredibuild:A\"
             [[task.session]]
cwd=\".\"
prompt=\"p\"
             [[task]]
name=\"b\"
machine=\"incredibuild:B\"
             [[task.session]]
cwd=\".\"
prompt=\"q\"
",
        );
        assert_eq!(r.status, 201, "{}", r.body);
    }

    #[test]
    fn a_task_whose_sessions_are_all_unset_inherits_without_complaint() {
        // Regression guard: the rule must be inert for every pre-RAL-185 file.
        let r = submit_with_machines(
            "[[task]]
name=\"t\"
             [[task.session]]
cwd=\".\"
prompt=\"p\"
             [[task.session]]
cwd=\".\"
prompt=\"q\"
",
        );
        assert_eq!(r.status, 201, "{}", r.body);
    }

    #[test]
    fn one_session_naming_a_machine_sets_the_expectation_for_its_siblings() {
        // The task itself is unset, so the first session that names a machine
        // establishes it -- a sibling that disagrees is still a split task.
        let r = submit_with_machines(
            "[[task]]
name=\"t\"
             [[task.session]]
id=\"a\"
cwd=\".\"
prompt=\"p\"
machine=\"incredibuild:A\"
             [[task.session]]
id=\"b\"
cwd=\".\"
prompt=\"q\"
machine=\"incredibuild:B\"
",
        );
        assert_eq!(r.status, 400, "{}", r.body);
        assert!(r.body.contains("spans two machines"), "{}", r.body);
    }

    #[test]
    fn a_remote_session_feeding_a_review_must_declare_the_reviews_base() {
        // A remote session's worktree lives on another machine, so its base
        // cannot be read from that worktree's git upstream. Declaring it is the
        // only honest option -- guessing the project default would silently
        // review against the wrong base.
        let d = daemon();
        let repo = tmp_git_repo("remote-review-base");
        route(
            &d,
            "POST",
            "/api/projects",
            &register_body("proj", &repo.to_string_lossy(), ""),
        );
        route(
            &d,
            "POST",
            "/api/machines",
            &machine_body("incredibuild", "/opt/ib.sh"),
        );
        let toml = "[[task]]\nname=\"t\"\nproject=\"proj\"\nmachine=\"incredibuild:A\"\n\
                    [[task.session]]\nid=\"work\"\ncwd=\"ralphus:new-worktree/feat\"\nprompt=\"p\"\nreview=\"r\"\n\
                    [[review]]\nid=\"r\"\n";
        let r = route(&d, "POST", "/api/runs", &submit_body(toml));
        assert_eq!(r.status, 400, "{}", r.body);
        assert!(
            r.body.contains("base = ") || r.body.contains("base branch"),
            "the error must point at the missing [[review]] base: {}",
            r.body
        );
    }

    #[test]
    fn a_remote_session_feeding_a_review_needs_a_placeholder_cwd() {
        // A literal remote path gives the daemon no way to know the branch
        // without reading that machine's filesystem, so it must say so rather
        // than failing later inside git.
        let d = daemon();
        let repo = tmp_git_repo("remote-review-litpath");
        route(
            &d,
            "POST",
            "/api/projects",
            &register_body("proj", &repo.to_string_lossy(), ""),
        );
        route(
            &d,
            "POST",
            "/api/machines",
            &machine_body("incredibuild", "/opt/ib.sh"),
        );
        let toml = "[[task]]\nname=\"t\"\nproject=\"proj\"\nmachine=\"incredibuild:A\"\n\
                    [[task.session]]\nid=\"work\"\ncwd=\"/remote/wt\"\nprompt=\"p\"\nreview=\"r\"\n\
                    [[review]]\nid=\"r\"\nbase=\"main\"\n";
        let r = route(&d, "POST", "/api/runs", &submit_body(toml));
        assert_eq!(r.status, 400, "{}", r.body);
        assert!(r.body.contains("new-worktree"), "{}", r.body);
    }

    #[test]
    fn a_session_without_any_machine_stores_none_not_a_sentinel_string() {
        // The router keys off `None` to take the local path, so a sentinel
        // like "local" persisted here would send every legacy session through
        // machine resolution for no reason.
        let d = daemon();
        let toml = "[[task]]\nname=\"t\"\n[[task.session]]\ncwd=\".\"\nprompt=\"p\"\n";
        let r = route(&d, "POST", "/api/runs", &submit_body(toml));
        assert_eq!(r.status, 201, "{}", r.body);
        let run_id = serde_json::from_str::<serde_json::Value>(&r.body).unwrap()["run_id"]
            .as_str()
            .unwrap()
            .to_string();
        let sessions = d.lock().sessions_of(&run_id).unwrap();
        assert_eq!(sessions[0].machine, None);
    }

    #[test]
    fn submit_accepts_an_explicitly_local_machine() {
        let d = daemon();
        let toml =
            "[[task]]\nname=\"t\"\nmachine=\"local\"\n[[task.session]]\ncwd=\".\"\nprompt=\"p\"\n";
        let r = route(&d, "POST", "/api/runs", &submit_body(toml));
        assert_eq!(r.status, 201, "{}", r.body);
    }

    #[test]
    fn submit_is_unaffected_when_no_machine_is_declared_anywhere() {
        // Regression guard: every pre-RAL-185 task file must submit unchanged.
        let d = daemon();
        let toml = "[[task]]\nname=\"t\"\n[[task.session]]\ncwd=\".\"\nprompt=\"p\"\n";
        let r = route(&d, "POST", "/api/runs", &submit_body(toml));
        assert_eq!(r.status, 201, "{}", r.body);
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
        // "Open Agent" is only reachable for a session that actually ran
        // under an agent with a resume mechanism (a agent_session_id is
        // required to get here at all -- see open_agent_terminal), so the
        // resumed conversation should never immediately stall on a
        // permission prompt a human has to notice and click through.
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

    // ── RAL-153: read-only terminal-log viewer copy ─────────────────────────

    #[test]
    fn resolve_preferred_editor_prefers_visual_over_editor() {
        let resolved =
            resolve_preferred_editor_from(Some("code -w".to_string()), Some("vim".to_string()));
        assert_eq!(resolved, Some(("code".to_string(), vec!["-w".to_string()])));
    }

    #[test]
    fn resolve_preferred_editor_falls_back_to_editor_when_visual_unset() {
        let resolved = resolve_preferred_editor_from(None, Some("vim".to_string()));
        assert_eq!(resolved, Some(("vim".to_string(), vec![])));
    }

    #[test]
    fn resolve_preferred_editor_skips_a_blank_visual() {
        let resolved =
            resolve_preferred_editor_from(Some("   ".to_string()), Some("vim".to_string()));
        assert_eq!(resolved, Some(("vim".to_string(), vec![])));
    }

    #[test]
    fn resolve_preferred_editor_none_when_both_unset_or_blank() {
        assert_eq!(resolve_preferred_editor_from(None, None), None);
        assert_eq!(
            resolve_preferred_editor_from(Some(String::new()), Some("  ".to_string())),
            None
        );
    }

    #[test]
    fn resolve_preferred_editor_splits_multi_word_command() {
        let resolved = resolve_preferred_editor_from(None, Some("subl -w -n".to_string()));
        assert_eq!(
            resolved,
            Some(("subl".to_string(), vec!["-w".to_string(), "-n".to_string()]))
        );
    }

    #[test]
    fn readonly_viewer_copy_path_is_unique_per_call_and_scoped_to_dir() {
        let dir = std::path::Path::new("dummy-dir");
        let a = readonly_viewer_copy_path(dir, "my-session");
        let b = readonly_viewer_copy_path(dir, "my-session");
        assert_ne!(a, b, "each click's copy must get its own path");
        assert!(a.starts_with(dir));
        assert!(a.to_string_lossy().contains("my-session"));
    }

    #[test]
    fn is_stale_true_once_past_max_age() {
        let modified = std::time::SystemTime::UNIX_EPOCH;
        let now = modified + Duration::from_secs(3601);
        assert!(is_stale(modified, now, Duration::from_secs(3600)));
    }

    #[test]
    fn is_stale_false_within_max_age() {
        let modified = std::time::SystemTime::UNIX_EPOCH;
        let now = modified + Duration::from_secs(3599);
        assert!(!is_stale(modified, now, Duration::from_secs(3600)));
    }

    #[test]
    fn is_stale_false_when_modified_is_not_before_now() {
        // Clock skew (or a file written during this exact instant) must never
        // be treated as a reason to delete.
        let now = std::time::SystemTime::UNIX_EPOCH;
        let modified = now + Duration::from_secs(5);
        assert!(!is_stale(modified, now, Duration::from_secs(0)));
    }

    #[test]
    fn prune_stale_readonly_viewer_copies_leaves_a_freshly_written_file() {
        let dir = std::env::temp_dir().join(format!(
            "ralphus-test-readonly-viewer-{}-{}",
            std::process::id(),
            line!()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let fresh = dir.join("fresh.txt");
        std::fs::write(&fresh, "new").unwrap();

        prune_stale_readonly_viewer_copies(&dir);

        assert!(fresh.exists(), "a just-written copy must not be pruned");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn open_readonly_snapshot_terminal_reports_409_with_no_snapshot() {
        let r = open_readonly_snapshot_terminal("definitely-not-a-real-ralphus-session-xyz");
        assert_eq!(r.status, 409);
        assert!(r.body.contains("no_tmux_session"));
    }

    #[test]
    fn resume_codex_agent_command_always_bypasses_approvals() {
        let cmd = resume_codex_agent_command("codex", "thread-abc-123");
        assert!(cmd.contains("--dangerously-bypass-approvals-and-sandbox"));
        assert!(cmd.contains("exec resume 'thread-abc-123'"));
        assert!(cmd.contains("& 'codex'"));
    }

    #[test]
    fn resume_codex_agent_command_escapes_single_quotes() {
        let cmd = resume_codex_agent_command("my'codex", "sess'123");
        assert!(cmd.contains("my''codex"));
        assert!(cmd.contains("sess''123"));
    }

    #[test]
    fn is_codex_agent_matches_both_aliases() {
        assert!(is_codex_agent(Some("codex")));
        assert!(is_codex_agent(Some("codex-cli")));
    }

    #[test]
    fn is_codex_agent_false_for_claude_and_unset() {
        assert!(!is_codex_agent(Some("claude-code")));
        assert!(!is_codex_agent(Some("claude-cli")));
        assert!(!is_codex_agent(Some("ollama")));
        assert!(!is_codex_agent(None));
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

    #[test]
    fn cartographer_endpoint_filters_by_task() {
        let d = daemon();
        route(&d, "POST", "/api/runs", &submit_body(GOOD));
        d.lock()
            .set_task_state("run-000000000001", 0, NodeState::Done)
            .unwrap();
        let r = route(&d, "GET", "/api/cartographer?task=t", "");
        assert_eq!(r.status, 200);
        assert!(r.body.contains("task → done"));

        let r_missing = route(&d, "GET", "/api/cartographer?task=nope", "");
        assert_eq!(r_missing.status, 200);
        assert!(r_missing.body.contains("\"total\":0"));
    }

    #[test]
    fn cartographer_endpoint_entity_filter_resolves_task_uri_to_task_name() {
        let d = daemon();
        route(&d, "POST", "/api/runs", &submit_body(GOOD));
        d.lock()
            .set_task_state("run-000000000001", 0, NodeState::Done)
            .unwrap();
        let r = route(
            &d,
            "GET",
            "/api/cartographer?entity=task:run-000000000001:0",
            "",
        );
        assert_eq!(r.status, 200);
        assert!(r.body.contains("task → done"));
    }

    #[test]
    fn cartographer_endpoint_entity_filter_run_scopes_by_run_id_only() {
        let d = daemon();
        route(&d, "POST", "/api/runs", &submit_body(GOOD));
        route(&d, "POST", "/api/runs", &submit_body(GOOD));
        let r = route(
            &d,
            "GET",
            "/api/cartographer?entity=run:run-000000000001",
            "",
        );
        assert_eq!(r.status, 200);
        assert!(r.body.contains("run-000000000001"));
        assert!(!r.body.contains("run-000000000002"));
    }

    #[test]
    fn cartographer_endpoint_entity_filter_bad_uri_is_400() {
        let d = daemon();
        let r = route(&d, "GET", "/api/cartographer?entity=bogus", "");
        assert_eq!(r.status, 400);
    }

    #[test]
    fn cartographer_endpoint_explicit_run_id_wins_over_entity() {
        // An explicit `run_id=` should not be clobbered by a same-request
        // `entity=` naming a different run -- the more specific,
        // explicitly-given field always wins (see `apply_entity_filter`'s
        // doc comment).
        let d = daemon();
        route(&d, "POST", "/api/runs", &submit_body(GOOD)); // run-1
        route(&d, "POST", "/api/runs", &submit_body(GOOD)); // run-2
        let r = route(
            &d,
            "GET",
            "/api/cartographer?run_id=run-000000000002&entity=run:run-000000000001",
            "",
        );
        assert_eq!(r.status, 200);
        assert!(r.body.contains("run-000000000002"));
        assert!(!r.body.contains("run-000000000001"));
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
    fn run_timeline_route_returns_merged_view_with_metadata() {
        // See `timeline::TIMELINE_FILE_TEST_LOCK`: every fresh in-memory store's
        // first run is "run-000000000001", so this and `timeline.rs`'s own
        // tests would otherwise race on the same OS temp-file path.
        let _guard = crate::timeline::TIMELINE_FILE_TEST_LOCK.lock().unwrap();
        let d = daemon();
        route(&d, "POST", "/api/runs", &submit_body(GOOD));
        d.lock()
            .set_run_state("run-000000000001", RunState::Done)
            .unwrap();
        let r = route(&d, "GET", "/api/runs/run-000000000001/timeline", "");
        assert_eq!(r.status, 200);
        assert!(r.body.contains("\"run_id\":\"run-000000000001\""));
        assert!(r.body.contains("\"entries\":["));
        assert!(r.body.contains("\"text\":"));
        assert!(r.body.contains("\"file_path\":"));
        assert!(r.body.contains("run → done"));
    }

    #[test]
    fn run_timeline_missing_run_is_404() {
        let d = daemon();
        assert_eq!(
            route(&d, "GET", "/api/runs/run-999/timeline", "").status,
            404
        );
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
    fn guardian_branch_conflicts_missing_guardian_is_404() {
        let d = daemon();
        assert_eq!(
            route(
                &d,
                "GET",
                "/api/guardians/guardian-999/branches/branch-1/conflicts",
                ""
            )
            .status,
            404
        );
    }

    #[test]
    fn guardian_branch_conflicts_missing_branch_is_404() {
        let d = daemon();
        let id = d.lock().create_guardian("g", "main", "/r").unwrap();
        let r = route(
            &d,
            "GET",
            &format!("/api/guardians/{id}/branches/branch-999/conflicts"),
            "",
        );
        assert_eq!(r.status, 404);
    }

    #[test]
    fn guardian_branch_conflicts_no_worktree_yet_is_empty() {
        let d = daemon();
        let id = d.lock().create_guardian("g", "main", "/r").unwrap();
        d.lock().add_guardian_branch(&id, "feature/a").unwrap();
        let branch_id = d.lock().guardian_branches(&id).unwrap()[0].id.clone();
        let r = route(
            &d,
            "GET",
            &format!("/api/guardians/{id}/branches/{branch_id}/conflicts"),
            "",
        );
        assert_eq!(r.status, 200);
        assert!(r.body.contains("\"files\":[]"));
        assert!(r.body.contains("\"rebase_in_progress\":false"));
    }

    #[test]
    fn resolver_task_and_session_id_is_stable_across_reorder() {
        // RAL-192: the historical-record lookup must keep resolving to the
        // SAME session after branches are reordered, since it recomputes the
        // session id fresh on every call rather than caching what it saw when
        // the resolver actually ran (see `write_pane_snapshot`'s key).
        // Before the fix, the session id was keyed on the branch's mutable
        // stack `position`, so a reorder silently pointed this lookup at a
        // different branch's session -- guaranteed a miss for the branch that
        // moved.
        let d = daemon();
        let id = d.lock().create_guardian("g", "main", "/r").unwrap();
        d.lock().add_guardian_branch(&id, "a").unwrap();
        d.lock().add_guardian_branch(&id, "b").unwrap();
        d.lock().add_guardian_branch(&id, "c").unwrap();
        let branches = d.lock().guardian_branches(&id).unwrap();
        let branch_id_a = branches[0].id.clone();
        assert_eq!(branches[0].branch, "a");

        let before = resolver_task_and_session_id(&d, &id, &branch_id_a).unwrap();

        d.lock()
            .reorder_guardian_branches(&id, &["c".into(), "a".into(), "b".into()])
            .unwrap();
        let reordered = d.lock().guardian_branches(&id).unwrap();
        assert_eq!(
            reordered
                .iter()
                .find(|b| b.id == branch_id_a)
                .unwrap()
                .position,
            1,
            "branch a's position must actually have moved for this test to be meaningful"
        );

        let after = resolver_task_and_session_id(&d, &id, &branch_id_a).unwrap();
        assert_eq!(
            before, after,
            "session id for branch a must not change on reorder"
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
    fn shutdown_without_auto_cancel_leaves_run_state_alone_but_requests_shutdown() {
        // Real production code kills each known run/guardian's own
        // ralphus_{id}_-prefixed tmux.exe sessions (see
        // `kill_run_tmux_sessions`) — serialize against `tmux.rs`'s live
        // tests anyway, since a real `tmux list-sessions`/`kill-session`
        // round trip still happens. The run_id itself is also given a
        // per-invocation-unique suffix (RAL-177), not just serialized:
        // `kill_run_tmux_sessions` kills by `ralphus_{run_id}_` *prefix*, so
        // a deterministic literal id here would let this shutdown call reach
        // — and kill — a same-prefixed real session created by an identical
        // test running concurrently in a sibling worktree against the same
        // shared, machine-wide psmux server, even with a fully unique task
        // name on that other session (the prefix kill never looks at the
        // task portion at all). See `Store::insert_run_with_id`'s doc
        // comment.
        let _tmux_guard = crate::tmux::LIVE_TMUX_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        crate::tmux::sweep_dead_test_sessions_once();
        let d = daemon();
        let run_id = crate::tmux::unique_test_tag("shutdown-no-auto-cancel");
        d.lock()
            .insert_run_with_id(&run_id, &parse(GOOD), None, false)
            .unwrap();
        d.lock().set_run_state(&run_id, RunState::Running).unwrap();

        assert!(!d.shutdown_requested());
        let r = route(&d, "POST", "/api/daemon/shutdown", "");
        assert_eq!(r.status, 200, "body={}", r.body);
        assert!(r.body.contains("\"state\":\"stopping\""));
        assert!(r.body.contains("\"auto_cancel\":false"));
        assert!(d.shutdown_requested());

        // Left running for crash-recovery to resume on next `serve()` startup.
        assert!(
            route(&d, "GET", &format!("/api/runs/{run_id}"), "")
                .body
                .contains("\"state\":\"running\"")
        );
    }

    #[test]
    fn shutdown_with_auto_cancel_cancels_active_runs_and_guardians() {
        // See `shutdown_without_auto_cancel_leaves_run_state_alone_but_requests_shutdown`'s
        // comment for why each run_id here is per-invocation-unique
        // (RAL-177), not just the literal deterministic id.
        let _tmux_guard = crate::tmux::LIVE_TMUX_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        crate::tmux::sweep_dead_test_sessions_once();
        let d = daemon();
        let run_id_1 = crate::tmux::unique_test_tag("shutdown-auto-cancel-1");
        let run_id_2 = crate::tmux::unique_test_tag("shutdown-auto-cancel-2");
        d.lock()
            .insert_run_with_id(&run_id_1, &parse(GOOD), None, false)
            .unwrap(); // pending
        d.lock()
            .insert_run_with_id(&run_id_2, &parse(GOOD), None, false)
            .unwrap();
        d.lock().set_run_state(&run_id_2, RunState::Done).unwrap();

        let gbody =
            serde_json::json!({"name":"r","base_branch":"main","git_root":"/repo"}).to_string();
        route(&d, "POST", "/api/guardians", &gbody);
        let gid = "guardian-000000000001";

        let body = serde_json::to_string(&serde_json::json!({ "auto_cancel": true })).unwrap();
        let r = route(&d, "POST", "/api/daemon/shutdown", &body);
        assert_eq!(r.status, 200, "body={}", r.body);
        assert!(r.body.contains("\"auto_cancel\":true"));
        assert!(r.body.contains(&run_id_1));
        assert!(!r.body.contains(&run_id_2)); // already terminal — left alone
        assert!(r.body.contains(gid));
        assert!(d.shutdown_requested());

        assert!(
            route(&d, "GET", &format!("/api/runs/{run_id_1}"), "")
                .body
                .contains("\"state\":\"cancelled\"")
        );
        assert!(
            route(&d, "GET", &format!("/api/runs/{run_id_2}"), "")
                .body
                .contains("\"state\":\"done\"")
        );
        assert!(
            route(&d, "GET", &format!("/api/guardians/{gid}"), "")
                .body
                .contains("\"status\":\"cancelled\"")
        );
    }

    #[test]
    fn shutdown_body_defaults_auto_cancel_to_false() {
        let _tmux_guard = crate::tmux::LIVE_TMUX_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        crate::tmux::sweep_dead_test_sessions_once();
        let d = daemon();
        let r = route(&d, "POST", "/api/daemon/shutdown", "");
        assert_eq!(r.status, 200, "body={}", r.body);
        assert!(r.body.contains("\"auto_cancel\":false"));
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
    fn session_terminal_log_attempts_route_lists_and_reads_attempts() {
        let d = daemon();
        route(&d, "POST", "/api/runs", &submit_body(GOOD));
        let run_id = "run-000000000001";

        // Nothing persisted yet — an empty list, not an error.
        let list = route(
            &d,
            "GET",
            &format!("/api/runs/{run_id}/sessions/0/0/terminal-log-attempts"),
            "",
        );
        assert_eq!(list.status, 200);
        assert!(list.body.contains("\"attempts\":[]"), "{}", list.body);

        // A non-existent attempt on a session with no attempts at all is 404.
        let missing = route(
            &d,
            "GET",
            &format!("/api/runs/{run_id}/sessions/0/0/terminal-log-attempts/0"),
            "",
        );
        assert_eq!(missing.status, 404);

        // Seed a couple of attempts the same way `SubprocessRunner::run_via_tmux_attempt`
        // would persist them, keyed by the same deterministic session name.
        let task = d.lock().get_task_name(run_id, 0).unwrap();
        let session_id = d.lock().get_session_id(run_id, 0, 0).unwrap();
        let name = crate::tmux::session_name(run_id, &task, &session_id);
        crate::terminal_log::write_attempt(&name, 0, "first attempt output", 100);
        crate::terminal_log::write_attempt(&name, 1, "second attempt output", 100);

        let list = route(
            &d,
            "GET",
            &format!("/api/runs/{run_id}/sessions/0/0/terminal-log-attempts"),
            "",
        );
        assert_eq!(list.status, 200);
        assert!(list.body.contains("\"attempt\":0"), "{}", list.body);
        assert!(list.body.contains("\"attempt\":1"), "{}", list.body);

        let attempt0 = route(
            &d,
            "GET",
            &format!("/api/runs/{run_id}/sessions/0/0/terminal-log-attempts/0"),
            "",
        );
        assert_eq!(attempt0.status, 200);
        assert!(attempt0.body.contains("first attempt output"));

        let attempt1 = route(
            &d,
            "GET",
            &format!("/api/runs/{run_id}/sessions/0/0/terminal-log-attempts/1"),
            "",
        );
        assert_eq!(attempt1.status, 200);
        assert!(attempt1.body.contains("second attempt output"));

        let attempt_missing = route(
            &d,
            "GET",
            &format!("/api/runs/{run_id}/sessions/0/0/terminal-log-attempts/9"),
            "",
        );
        assert_eq!(attempt_missing.status, 404);

        crate::terminal_log::delete_for_session(&name);
    }

    #[test]
    fn deleting_a_run_deletes_its_terminal_logs() {
        let d = daemon();
        route(&d, "POST", "/api/runs", &submit_body(GOOD));
        let run_id = "run-000000000001";
        let task = d.lock().get_task_name(run_id, 0).unwrap();
        let session_id = d.lock().get_session_id(run_id, 0, 0).unwrap();
        let name = crate::tmux::session_name(run_id, &task, &session_id);
        crate::terminal_log::write_attempt(&name, 0, "will be deleted", 100);
        assert!(!crate::terminal_log::list_attempts(&name).is_empty());

        let del = route(&d, "DELETE", &format!("/api/runs/{run_id}"), "");
        assert_eq!(del.status, 200);

        assert!(crate::terminal_log::list_attempts(&name).is_empty());
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

    // RAL-157: solo/unsolo a task within a run.

    const TWO_TASKS: &str = "[[task]]\nname=\"a\"\n[[task.session]]\ncwd=\"/r\"\nprompt=\"p\"\n[[task]]\nname=\"b\"\n[[task.session]]\ncwd=\"/r\"\nprompt=\"p\"\n";
    const STOP_CASCADE_RUN: &str = r#"
[[task]]
name = "alpha"
[[task.session]]
id = "work"
cwd = "/r"
prompt = "p"
[[task.session.verify]]
id = "check-a"
command = "true"
[[task.session.verify]]
id = "check-b"
command = "true"
[[task.session]]
id = "after"
cwd = "/r"
prompt = "p"
depends_on = ["work"]
[[task.session.verify]]
id = "after-check"
command = "true"
[[task.verify]]
id = "task-check-a"
command = "true"
[[task.verify]]
id = "task-check-b"
command = "true"

[[task]]
name = "beta"
depends_on = ["alpha/after"]
[[task.session]]
id = "downstream"
cwd = "/r"
prompt = "p"
[[task.session.verify]]
id = "beta-check"
command = "true"
[[task.verify]]
id = "beta-task-check"
command = "true"

[[task]]
name = "side"
[[task.session]]
id = "unrelated"
cwd = "/r"
prompt = "p"
[[task.session.verify]]
id = "side-check"
command = "true"
[[task.verify]]
id = "side-task-check"
command = "true"
"#;
    const RUN_1: &str = "run-000000000001";

    fn submit_stop_cascade_run(d: &Daemon) {
        route(d, "POST", "/api/runs", &submit_body(STOP_CASCADE_RUN));
    }

    fn set_status_and_parse(d: &Daemon, body: serde_json::Value) -> serde_json::Value {
        let r = route(
            d,
            "POST",
            &format!("/api/runs/{RUN_1}/set-status"),
            &body.to_string(),
        );
        assert_eq!(r.status, 200, "body={}", r.body);
        serde_json::from_str(&r.body).unwrap()
    }

    fn seed_task_stop_states(d: &Daemon) {
        let guard = d.lock();
        guard.set_task_state(RUN_1, 0, NodeState::Running).unwrap();
        guard
            .set_session_state(RUN_1, 0, 0, NodeState::Running)
            .unwrap();
        guard.set_task_state(RUN_1, 2, NodeState::Running).unwrap();
        guard
            .set_session_state(RUN_1, 2, 0, NodeState::Running)
            .unwrap();
    }

    fn seed_session_verify_stop_states(d: &Daemon) {
        let guard = d.lock();
        guard.set_task_state(RUN_1, 0, NodeState::Running).unwrap();
        guard
            .set_session_state(RUN_1, 0, 0, NodeState::Done)
            .unwrap();
        guard
            .set_verify_state(RUN_1, 0, "session", 0, 0, NodeState::Running)
            .unwrap();
        guard.set_task_state(RUN_1, 2, NodeState::Running).unwrap();
        guard
            .set_session_state(RUN_1, 2, 0, NodeState::Running)
            .unwrap();
    }

    fn seed_task_verify_stop_states(d: &Daemon) {
        let guard = d.lock();
        guard
            .set_session_state(RUN_1, 0, 0, NodeState::Done)
            .unwrap();
        guard
            .set_verify_state(RUN_1, 0, "session", 0, 0, NodeState::Done)
            .unwrap();
        guard
            .set_verify_state(RUN_1, 0, "session", 0, 1, NodeState::Done)
            .unwrap();
        guard
            .set_session_state(RUN_1, 0, 1, NodeState::Done)
            .unwrap();
        guard
            .set_verify_state(RUN_1, 0, "session", 1, 0, NodeState::Done)
            .unwrap();
        guard.set_task_state(RUN_1, 0, NodeState::Running).unwrap();
        guard
            .set_verify_state(RUN_1, 0, "task", -1, 0, NodeState::Running)
            .unwrap();
        guard.set_task_state(RUN_1, 2, NodeState::Running).unwrap();
        guard
            .set_session_state(RUN_1, 2, 0, NodeState::Running)
            .unwrap();
    }

    #[test]
    fn solo_task_sets_soloed_and_returns_refreshed_run() {
        let d = daemon();
        route(&d, "POST", "/api/runs", &submit_body(TWO_TASKS));
        let r = route(&d, "POST", "/api/runs/run-000000000001/tasks/0/solo", "");
        assert_eq!(r.status, 200);
        let v: serde_json::Value = serde_json::from_str(&r.body).unwrap();
        assert_eq!(v["tasks"][0]["soloed"].as_bool(), Some(true));
        assert_eq!(
            v["tasks"][1]["soloed"].as_bool(),
            Some(false),
            "soloing one task must not solo its sibling"
        );
    }

    #[test]
    fn unsolo_task_clears_soloed() {
        let d = daemon();
        route(&d, "POST", "/api/runs", &submit_body(TWO_TASKS));
        route(&d, "POST", "/api/runs/run-000000000001/tasks/0/solo", "");
        let r = route(&d, "POST", "/api/runs/run-000000000001/tasks/0/unsolo", "");
        assert_eq!(r.status, 200);
        let v: serde_json::Value = serde_json::from_str(&r.body).unwrap();
        assert_eq!(v["tasks"][0]["soloed"].as_bool(), Some(false));
    }

    #[test]
    fn solo_task_multiple_at_once_has_no_auto_exclusivity() {
        let d = daemon();
        route(&d, "POST", "/api/runs", &submit_body(TWO_TASKS));
        route(&d, "POST", "/api/runs/run-000000000001/tasks/0/solo", "");
        let r = route(&d, "POST", "/api/runs/run-000000000001/tasks/1/solo", "");
        assert_eq!(r.status, 200);
        let v: serde_json::Value = serde_json::from_str(&r.body).unwrap();
        assert_eq!(v["tasks"][0]["soloed"].as_bool(), Some(true));
        assert_eq!(v["tasks"][1]["soloed"].as_bool(), Some(true));
    }

    #[test]
    fn solo_task_unknown_run_is_not_found() {
        let d = daemon();
        let r = route(&d, "POST", "/api/runs/run-nope/tasks/0/solo", "");
        assert_eq!(r.status, 404);
    }

    #[test]
    fn solo_task_unknown_task_index_is_not_found() {
        let d = daemon();
        route(&d, "POST", "/api/runs", &submit_body(GOOD));
        let r = route(&d, "POST", "/api/runs/run-000000000001/tasks/99/solo", "");
        assert_eq!(r.status, 404);
    }

    #[test]
    fn solo_task_non_integer_index_is_bad_request() {
        let d = daemon();
        route(&d, "POST", "/api/runs", &submit_body(GOOD));
        let r = route(&d, "POST", "/api/runs/run-000000000001/tasks/nope/solo", "");
        assert_eq!(r.status, 400);
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
    fn set_status_task_cancelled_cascades_only_to_downstream_branch() {
        let d = daemon();
        submit_stop_cascade_run(&d);
        seed_task_stop_states(&d);
        let v = set_status_and_parse(
            &d,
            serde_json::json!({"kind": "task", "task_idx": 0, "state": "cancelled"}),
        );
        assert_eq!(
            v["tasks"][0]["state"].as_str(),
            Some("cancelled"),
            "target task must be cancelled"
        );
        assert_eq!(
            v["tasks"][0]["sessions"][0]["state"].as_str(),
            Some("cancelled")
        );
        assert_eq!(
            v["tasks"][0]["sessions"][1]["state"].as_str(),
            Some("cancelled")
        );
        assert_eq!(
            v["tasks"][0]["sessions"][0]["verify"][0]["state"].as_str(),
            Some("cancelled")
        );
        assert_eq!(
            v["tasks"][0]["verify"][0]["state"].as_str(),
            Some("cancelled")
        );
        assert_eq!(v["tasks"][1]["state"].as_str(), Some("cancelled"));
        assert_eq!(
            v["tasks"][1]["sessions"][0]["state"].as_str(),
            Some("cancelled")
        );
        assert_eq!(
            v["tasks"][1]["sessions"][0]["verify"][0]["state"].as_str(),
            Some("cancelled")
        );
        assert_eq!(
            v["tasks"][1]["verify"][0]["state"].as_str(),
            Some("cancelled")
        );
        assert_ne!(v["tasks"][2]["state"].as_str(), Some("cancelled"));
        assert_eq!(
            v["tasks"][2]["sessions"][0]["state"].as_str(),
            Some("running")
        );
        assert_ne!(
            v["state"].as_str(),
            Some("cancelled"),
            "partial stop must not cancel the whole run"
        );
    }

    #[test]
    fn set_status_session_cancelled_cascades_only_to_downstream_branch() {
        let d = daemon();
        submit_stop_cascade_run(&d);
        seed_task_stop_states(&d);
        let v = set_status_and_parse(
            &d,
            serde_json::json!({
                "kind": "session", "task_idx": 0, "session_idx": 0, "state": "cancelled"
            }),
        );
        assert_eq!(v["tasks"][0]["state"].as_str(), Some("cancelled"));
        assert_eq!(
            v["tasks"][0]["sessions"][0]["state"].as_str(),
            Some("cancelled"),
            "target session must be cancelled"
        );
        assert_eq!(
            v["tasks"][0]["sessions"][1]["state"].as_str(),
            Some("cancelled")
        );
        assert_eq!(
            v["tasks"][0]["sessions"][0]["verify"][0]["state"].as_str(),
            Some("cancelled")
        );
        assert_eq!(
            v["tasks"][0]["verify"][0]["state"].as_str(),
            Some("cancelled")
        );
        assert_eq!(v["tasks"][1]["state"].as_str(), Some("cancelled"));
        assert_eq!(
            v["tasks"][1]["sessions"][0]["state"].as_str(),
            Some("cancelled")
        );
        assert_ne!(
            v["tasks"][2]["state"].as_str(),
            Some("cancelled"),
            "unrelated task must stay alive"
        );
        assert_eq!(
            v["tasks"][2]["sessions"][0]["state"].as_str(),
            Some("running")
        );
        assert_ne!(
            v["state"].as_str(),
            Some("cancelled"),
            "partial stop must not cancel the whole run"
        );
    }

    #[test]
    fn set_status_session_verify_cancelled_cascades_only_to_downstream_branch() {
        let d = daemon();
        submit_stop_cascade_run(&d);
        seed_session_verify_stop_states(&d);
        let v = set_status_and_parse(
            &d,
            serde_json::json!({
                "kind": "verify",
                "task_idx": 0,
                "session_idx": 0,
                "verify_idx": 0,
                "verify_scope": "session",
                "state": "cancelled"
            }),
        );
        assert_eq!(v["tasks"][0]["state"].as_str(), Some("cancelled"));
        assert_eq!(
            v["tasks"][0]["sessions"][0]["state"].as_str(),
            Some("done"),
            "owning session body is upstream of the stopped verify"
        );
        assert_eq!(
            v["tasks"][0]["sessions"][0]["verify"][0]["state"].as_str(),
            Some("cancelled")
        );
        assert_eq!(
            v["tasks"][0]["sessions"][0]["verify"][1]["state"].as_str(),
            Some("cancelled")
        );
        assert_eq!(
            v["tasks"][0]["sessions"][1]["state"].as_str(),
            Some("cancelled")
        );
        assert_eq!(
            v["tasks"][0]["sessions"][1]["verify"][0]["state"].as_str(),
            Some("cancelled")
        );
        assert_eq!(
            v["tasks"][0]["verify"][0]["state"].as_str(),
            Some("cancelled")
        );
        assert_eq!(v["tasks"][1]["state"].as_str(), Some("cancelled"));
        assert_eq!(
            v["tasks"][1]["sessions"][0]["state"].as_str(),
            Some("cancelled")
        );
        assert_ne!(v["tasks"][2]["state"].as_str(), Some("cancelled"));
        assert_eq!(
            v["tasks"][2]["sessions"][0]["state"].as_str(),
            Some("running")
        );
        assert_ne!(v["state"].as_str(), Some("cancelled"));
    }

    #[test]
    fn set_status_task_verify_cancelled_cascades_only_to_downstream_branch() {
        let d = daemon();
        submit_stop_cascade_run(&d);
        seed_task_verify_stop_states(&d);
        let v = set_status_and_parse(
            &d,
            serde_json::json!({
                "kind": "verify",
                "task_idx": 0,
                "session_idx": -1,
                "verify_idx": 0,
                "verify_scope": "task",
                "state": "cancelled"
            }),
        );
        assert_eq!(v["tasks"][0]["state"].as_str(), Some("cancelled"));
        assert_eq!(
            v["tasks"][0]["sessions"][0]["state"].as_str(),
            Some("done"),
            "task verify stop must not retroactively cancel upstream sessions"
        );
        assert_eq!(
            v["tasks"][0]["sessions"][0]["verify"][0]["state"].as_str(),
            Some("done")
        );
        assert_eq!(
            v["tasks"][0]["verify"][0]["state"].as_str(),
            Some("cancelled")
        );
        assert_eq!(
            v["tasks"][0]["verify"][1]["state"].as_str(),
            Some("cancelled")
        );
        assert_eq!(v["tasks"][1]["state"].as_str(), Some("cancelled"));
        assert_eq!(
            v["tasks"][1]["sessions"][0]["state"].as_str(),
            Some("cancelled")
        );
        assert_ne!(v["tasks"][2]["state"].as_str(), Some("cancelled"));
        assert_eq!(
            v["tasks"][2]["sessions"][0]["state"].as_str(),
            Some("running")
        );
        assert_ne!(v["state"].as_str(), Some("cancelled"));
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

    fn status_req(kind: &str, task_idx: i64, session_idx: i64, state: &str) -> SetStatusBody {
        SetStatusBody {
            kind: kind.to_string(),
            task_idx,
            session_idx,
            verify_idx: 0,
            verify_scope: String::new(),
            state: state.to_string(),
        }
    }

    #[test]
    fn stop_targets_for_session_kind_targets_that_exact_pane() {
        let d = daemon();
        route(&d, "POST", "/api/runs", &submit_body(GOOD));
        let run_id = "run-000000000001";
        let guard = d.lock();
        let req = status_req("session", 0, 0, "done");
        let targets = stop_targets_for_status_change(&guard, run_id, &req);
        let task = guard.get_task_name(run_id, 0).unwrap();
        let sid = guard.get_session_id(run_id, 0, 0).unwrap();
        assert_eq!(
            targets,
            vec![(crate::tmux::session_name(run_id, &task, &sid), 0)]
        );
    }

    #[test]
    fn stop_targets_for_task_kind_covers_every_session_in_the_task() {
        let d = daemon();
        let toml = "[[task]]\nname=\"t\"\n\
            [[task.session]]\nid=\"s1\"\ncwd=\"/r\"\nprompt=\"p\"\n\
            [[task.session]]\nid=\"s2\"\ncwd=\"/r\"\nprompt=\"p\"\n";
        route(&d, "POST", "/api/runs", &submit_body(toml));
        let run_id = "run-000000000001";
        let guard = d.lock();
        let req = status_req("task", 0, 0, "failed");
        let targets = stop_targets_for_status_change(&guard, run_id, &req);
        assert_eq!(
            targets,
            vec![
                (crate::tmux::session_name(run_id, "t", "s1"), 0),
                (crate::tmux::session_name(run_id, "t", "s2"), 1),
            ]
        );
    }

    #[test]
    fn stop_targets_for_session_scope_verify_targets_the_verify_pane_but_the_session_ghost() {
        let d = daemon();
        route(&d, "POST", "/api/runs", &submit_body(GOOD));
        let run_id = "run-000000000001";
        let guard = d.lock();
        let mut req = status_req("verify", 0, 0, "failed");
        req.verify_scope = "session".to_string();
        req.verify_idx = 3;
        let targets = stop_targets_for_status_change(&guard, run_id, &req);
        let task = guard.get_task_name(run_id, 0).unwrap();
        assert_eq!(
            targets,
            vec![(
                crate::tmux::session_name(run_id, &task, "verify-session-3"),
                0
            )]
        );
    }

    #[test]
    fn stop_targets_for_task_scope_verify_attributes_ghost_to_the_first_session() {
        let d = daemon();
        let toml = "[[task]]\nname=\"t\"\n\
            [[task.session]]\nid=\"s1\"\ncwd=\"/r\"\nprompt=\"p\"\n\
            [[task.session]]\nid=\"s2\"\ncwd=\"/r\"\nprompt=\"p\"\n";
        route(&d, "POST", "/api/runs", &submit_body(toml));
        let run_id = "run-000000000001";
        let guard = d.lock();
        let mut req = status_req("verify", 0, 0, "failed");
        req.verify_scope = "task".to_string();
        req.verify_idx = 1;
        let targets = stop_targets_for_status_change(&guard, run_id, &req);
        assert_eq!(
            targets,
            // Pane is keyed by the task-scope verify's own name, but the
            // ghost is attributed to session idx 0 (the task's first
            // session) -- mirroring `Store::get_task_first_session_cwd`.
            vec![(crate::tmux::session_name(run_id, "t", "verify-task-1"), 0)]
        );
    }

    #[test]
    fn stop_targets_empty_for_unknown_task() {
        let d = daemon();
        route(&d, "POST", "/api/runs", &submit_body(GOOD));
        let run_id = "run-000000000001";
        let guard = d.lock();
        let req = status_req("session", 99, 0, "done");
        assert_eq!(stop_targets_for_status_change(&guard, run_id, &req), vec![]);
    }

    #[test]
    fn stop_targets_empty_for_run_kind() {
        // "run" isn't handled by capture_and_stop_node at all -- the run-wide
        // cancel cascade (`kill_run_tmux_sessions`) already covers it.
        let d = daemon();
        route(&d, "POST", "/api/runs", &submit_body(GOOD));
        let run_id = "run-000000000001";
        let guard = d.lock();
        let req = status_req("run", 0, 0, "done");
        assert_eq!(stop_targets_for_status_change(&guard, run_id, &req), vec![]);
    }

    fn tmux_available() -> bool {
        crate::tmux::Tmux::resolve().is_ok()
    }

    /// A one-task-one-session TOML fixture like `GOOD`, but with a caller-
    /// chosen task name, so each live-tmux test below gets its own
    /// distinguishable task in the (already per-invocation-unique, via
    /// [`submit_unique_run`]) `crate::tmux::session_name` it derives,
    /// instead of every test sharing `GOOD`'s fixed task name.
    fn good_with_task(task: &str) -> String {
        format!("[[task]]\nname=\"{task}\"\n[[task.session]]\ncwd=\"/r\"\nprompt=\"p\"\n")
    }

    fn parse(src: &str) -> ralphus_core::schema::TaskFile {
        toml::from_str(src).expect("valid toml")
    }

    /// Submit `good_with_task(task_label)` directly through the store
    /// (bypassing the HTTP `/api/runs` route, which always assigns the
    /// store's deterministic sequential id) with a per-invocation-unique
    /// run_id instead (RAL-177) — see [`Store::insert_run_with_id`]'s doc
    /// comment. A fresh in-memory `Store`'s first run is always
    /// `run-000000000001`, identical across every worktree's identical
    /// test; a live-tmux test below both builds a real tmux session name
    /// from this run_id *and* can trigger a `ralphus_{run_id}_`-prefix-
    /// scoped kill (`kill_run_tmux_sessions`/`kill_guardian_tmux_sessions`)
    /// against the same shared, machine-wide psmux server, so a deterministic
    /// literal here is a collision risk on both counts, not just the exact
    /// session name. Returns the run_id actually used.
    fn submit_unique_run(d: &Daemon, task_label: &str) -> String {
        let run_id = crate::tmux::unique_test_tag(task_label);
        d.lock()
            .insert_run_with_id(&run_id, &parse(&good_with_task(task_label)), None, false)
            .unwrap();
        run_id
    }

    /// Create a real, detached tmux session named for `(run_id, task,
    /// session_id)` that echoes `marker` into its pane, then polls until the
    /// echo actually shows up in `capture-pane` — mirrors
    /// `tmux.rs::live_tmux_new_session_capture_and_kill_roundtrip`'s idiom, so
    /// the RAL-163 tests below have a real, non-racy pane to capture from.
    fn spawn_marker_session(name: &str, marker: &str) -> crate::tmux::Tmux {
        let tmux = crate::tmux::Tmux::resolve().unwrap();
        let _ = tmux.kill_session(name);
        let cwd = std::env::temp_dir().to_string_lossy().into_owned();
        let command = crate::tmux::build_command_line("echo", &[marker.to_string()]);
        tmux.new_detached_session_with_command(name, &cwd, &command)
            .unwrap();
        for _ in 0..50 {
            std::thread::sleep(std::time::Duration::from_millis(200));
            if tmux
                .capture_pane(name, 50)
                .unwrap_or_default()
                .contains(marker)
            {
                break;
            }
        }
        tmux
    }

    #[test]
    fn set_status_session_captures_pane_into_ghost_and_kills_it() {
        // RAL-163: setting a session to a terminal status while its agent is
        // still running must capture the pane's in-progress output into that
        // session's ghost, then stop (kill) the pane -- turning the manual
        // override into a recoverable checkpoint instead of an orphaned
        // background process.
        if !tmux_available() {
            println!("SKIP: tmux not found on PATH");
            return;
        }
        let _guard = crate::tmux::LIVE_TMUX_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        crate::tmux::sweep_dead_test_sessions_once();
        let d = daemon();
        let run_id = submit_unique_run(&d, "ral163-captures");

        let (task, sid) = {
            let guard = d.lock();
            (
                guard.get_task_name(&run_id, 0).unwrap(),
                guard.get_session_id(&run_id, 0, 0).unwrap(),
            )
        };
        let name = crate::tmux::session_name(&run_id, &task, &sid);
        let marker = "ral-163-in-progress-marker";
        spawn_marker_session(&name, marker);

        let body = serde_json::json!({
            "kind": "session", "task_idx": 0, "session_idx": 0, "state": "done"
        })
        .to_string();
        let r = route(&d, "POST", &format!("/api/runs/{run_id}/set-status"), &body);
        assert_eq!(r.status, 200, "body={}", r.body);
        let v: serde_json::Value = serde_json::from_str(&r.body).unwrap();
        assert_eq!(
            v["tasks"][0]["sessions"][0]["state"].as_str(),
            Some("done"),
            "the manually-set status must be applied: {}",
            r.body
        );

        let tmux = crate::tmux::Tmux::resolve().unwrap();
        assert!(
            !tmux.has_session(&name),
            "the agent's tmux pane must be stopped"
        );

        let uri = crate::ghost::session_uri(&run_id, 0, 0);
        let ghost = d.lock().get_ghost(&uri).unwrap();
        let ghost = ghost.expect("captured pane output must be saved to the session's ghost");
        assert!(
            ghost.content.contains(marker),
            "ghost must contain the captured in-progress output: {}",
            ghost.content
        );
    }

    #[test]
    fn set_status_pending_does_not_stop_a_running_agent() {
        // Per the ticket: "pending" is the only status that does not imply
        // "stop running" -- setting it must leave a live agent's pane alone.
        if !tmux_available() {
            println!("SKIP: tmux not found on PATH");
            return;
        }
        let _guard = crate::tmux::LIVE_TMUX_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        crate::tmux::sweep_dead_test_sessions_once();
        let d = daemon();
        let run_id = submit_unique_run(&d, "ral163-pending");

        let (task, sid) = {
            let guard = d.lock();
            (
                guard.get_task_name(&run_id, 0).unwrap(),
                guard.get_session_id(&run_id, 0, 0).unwrap(),
            )
        };
        let name = crate::tmux::session_name(&run_id, &task, &sid);
        let marker = "ral-163-pending-marker";
        let tmux = spawn_marker_session(&name, marker);
        let _cleanup = crate::tmux::KillSessionOnDrop(name.clone());

        let body = serde_json::json!({
            "kind": "session", "task_idx": 0, "session_idx": 0, "state": "pending"
        })
        .to_string();
        let r = route(&d, "POST", &format!("/api/runs/{run_id}/set-status"), &body);
        assert_eq!(r.status, 200, "body={}", r.body);

        assert!(
            tmux.has_session(&name),
            "setting status to pending must not stop a running agent"
        );
        let uri = crate::ghost::session_uri(&run_id, 0, 0);
        assert!(
            d.lock().get_ghost(&uri).unwrap().is_none(),
            "nothing should have been captured for a pending status change"
        );

        tmux.kill_session(&name).unwrap();
    }

    #[test]
    fn set_status_ignored_still_captures_and_stops() {
        // Q1 of the ticket's interview: `ignored` is deliberately non-terminal
        // in the state machine (reversible back to `pending`), but the trigger
        // condition for stop-and-capture is "any status but pending" -- a user
        // sets `ignored` specifically to skip past bad-looking output, and
        // losing that context would defeat the point.
        if !tmux_available() {
            println!("SKIP: tmux not found on PATH");
            return;
        }
        let _guard = crate::tmux::LIVE_TMUX_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        crate::tmux::sweep_dead_test_sessions_once();
        let d = daemon();
        let run_id = submit_unique_run(&d, "ral163-ignored");

        let (task, sid) = {
            let guard = d.lock();
            (
                guard.get_task_name(&run_id, 0).unwrap(),
                guard.get_session_id(&run_id, 0, 0).unwrap(),
            )
        };
        let name = crate::tmux::session_name(&run_id, &task, &sid);
        let marker = "ral-163-ignored-marker";
        spawn_marker_session(&name, marker);

        let body = serde_json::json!({
            "kind": "session", "task_idx": 0, "session_idx": 0, "state": "ignored"
        })
        .to_string();
        let r = route(&d, "POST", &format!("/api/runs/{run_id}/set-status"), &body);
        assert_eq!(r.status, 200, "body={}", r.body);

        let tmux = crate::tmux::Tmux::resolve().unwrap();
        assert!(
            !tmux.has_session(&name),
            "ignored must still stop the running agent"
        );
        let uri = crate::ghost::session_uri(&run_id, 0, 0);
        let ghost = d.lock().get_ghost(&uri).unwrap();
        assert!(
            ghost.is_some_and(|g| g.content.contains(marker)),
            "ignored must still capture the agent's in-progress output"
        );
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

    /// RAL-174: a restart note in the body must actually attach without
    /// hanging. Regression test for a `Mutex` self-deadlock: matching
    /// directly on `daemon.lock().restart_run(id)` keeps that `MutexGuard`
    /// alive for the whole arm (Rust extends a match scrutinee's temporaries
    /// to the arm body), so writing the note via a second `daemon.lock()`
    /// call inside that arm deadlocked whenever a note was actually present
    /// — every route test using an empty `""` body never exercised this path.
    #[test]
    fn restart_run_route_with_note_attaches_ghost_without_hanging() {
        let d = daemon();
        route(&d, "POST", "/api/runs", &submit_body(GOOD));
        let body = serde_json::json!({"note": "you were stopped midway through the migration"})
            .to_string();
        let r = route(&d, "POST", "/api/runs/run-000000000001/restart", &body);
        assert_eq!(r.status, 200);
        let ghost = d
            .lock()
            .get_ghost(&crate::ghost::session_uri("run-000000000001", 0, 0))
            .unwrap()
            .unwrap();
        assert_eq!(
            ghost.user_note.as_deref(),
            Some("you were stopped midway through the migration")
        );
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

    /// RAL-174 deadlock regression (see `restart_run_route_with_note_attaches_ghost_without_hanging`).
    #[test]
    fn restart_session_route_with_note_attaches_ghost_without_hanging() {
        let d = daemon();
        route(&d, "POST", "/api/runs", &submit_body(GOOD));
        let body = serde_json::json!({"note": "picking up where you left off"}).to_string();
        let r = route(
            &d,
            "POST",
            "/api/runs/run-000000000001/sessions/0/0/restart",
            &body,
        );
        assert_eq!(r.status, 200);
        let ghost = d
            .lock()
            .get_ghost(&crate::ghost::session_uri("run-000000000001", 0, 0))
            .unwrap()
            .unwrap();
        assert_eq!(
            ghost.user_note.as_deref(),
            Some("picking up where you left off")
        );
    }

    /// RAL-1xx regression: `restart_session`/`restart_session_verify`/
    /// `restart_task_verify` used to cancel a run's *entire* worker thread
    /// unconditionally, even when the restart target had already finished.
    /// A run's worker drives every one of that run's independent sessions
    /// concurrently over one shared `CancelToken` (see
    /// `scheduler::execute_run_inner`), so restarting one already-terminal
    /// (failed) session collaterally killed every other still-running,
    /// unrelated sibling session in the same run — observed in production as
    /// several healthy tasks in a batch run dying the instant a different,
    /// already-failed task was restarted, despite having no `depends_on`
    /// relationship to it. Task "a" fails immediately here; task "b" is still
    /// genuinely in flight (blocked in `run_cancellable`, standing in for a
    /// live claude-code session) when "a" is restarted. Before the fix, "b"
    /// observed `cancel.is_cancelled() == true` and aborted; after the fix,
    /// restarting "a" (already terminal) never touches the shared token at
    /// all, so "b" runs to completion undisturbed.
    #[test]
    fn restart_session_does_not_cancel_unrelated_sibling_session_in_same_run() {
        use crate::cancel::CancelToken;
        use crate::runner::{RunnerResult, RunnerSpec};
        use std::sync::atomic::{AtomicBool, Ordering};

        const TWO_INDEPENDENT_TASKS: &str = "[[task]]\nname=\"a\"\n[[task.session]]\ncwd=\".\"\ncommand=\"x\"\n[[task]]\nname=\"b\"\n[[task.session]]\ncwd=\".\"\ncommand=\"y\"\n";

        /// Task "a" fails immediately (so its session is already terminal by
        /// the time the test restarts it); task "b" blocks in
        /// `run_cancellable`, polling `cancel` like a real subprocess-backed
        /// session would, so the test can observe whether it ever gets
        /// collaterally cancelled.
        struct TwoTaskRunner {
            b_started: Arc<AtomicBool>,
            b_cancelled: Arc<AtomicBool>,
        }

        impl Runner for TwoTaskRunner {
            fn run(&self, _spec: &RunnerSpec) -> RunnerResult {
                RunnerResult::failure("unused")
            }
            fn run_cancellable(&self, spec: &RunnerSpec, cancel: &CancelToken) -> RunnerResult {
                if spec.task == "a" {
                    return RunnerResult::failure("task a fails immediately");
                }
                self.b_started.store(true, Ordering::SeqCst);
                for _ in 0..60 {
                    if cancel.is_cancelled() {
                        self.b_cancelled.store(true, Ordering::SeqCst);
                        return RunnerResult::failure("cancelled");
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                RunnerResult {
                    status: "done".to_string(),
                    tokens_in: 1,
                    tokens_out: 1,
                    cost_usd: 0.0,
                    summary: "b finished undisturbed".to_string(),
                    error: None,
                    verified: None,
                    agent_session_id: None,
                    ghost: None,
                }
            }
        }

        let d = daemon();
        let r = route(&d, "POST", "/api/runs", &submit_body(TWO_INDEPENDENT_TASKS));
        assert_eq!(r.status, 201);
        let run_id = "run-000000000001".to_string();

        let b_started = Arc::new(AtomicBool::new(false));
        let b_cancelled = Arc::new(AtomicBool::new(false));
        let runner: Arc<dyn Runner> = Arc::new(TwoTaskRunner {
            b_started: Arc::clone(&b_started),
            b_cancelled: Arc::clone(&b_cancelled),
        });

        let store = d.store_handle();
        let cancellations = d.cancellations_handle();
        let worker = {
            let (store, runner, cancellations, run_id) = (
                Arc::clone(&store),
                Arc::clone(&runner),
                cancellations.clone(),
                run_id.clone(),
            );
            std::thread::spawn(move || {
                // Mirrors what `scheduler::tick` does for a claimed run: register
                // a token in the shared registry before executing, remove it after.
                let token = cancellations.register(&run_id);
                crate::scheduler::execute_run_with(
                    &store,
                    runner.as_ref(),
                    &run_id,
                    &token,
                    &cancellations,
                );
                cancellations.remove(&run_id);
            })
        };

        // Wait until task "b" is genuinely in flight and task "a"'s failure
        // has actually been recorded in the store. A fixed sleep was flaky
        // under full-suite load: "a" had returned, but its `failed` state had
        // not always been persisted yet, so the restart still looked live and
        // cancelled the shared worker token.
        while !b_started.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(2));
        }
        let mut a_failed = false;
        for _ in 0..500 {
            if matches!(
                d.lock().session_state(&run_id, 0, 0),
                Ok(Some(crate::store::NodeState::Failed))
            ) {
                a_failed = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(
            a_failed,
            "task a should reach a recorded failed state before the restart"
        );

        // Restart task "a"'s already-failed session while "b" is still running.
        let rr = route(
            &d,
            "POST",
            &format!("/api/runs/{run_id}/sessions/0/0/restart"),
            "",
        );
        assert_eq!(rr.status, 200);

        worker.join().unwrap();

        assert!(
            !b_cancelled.load(Ordering::SeqCst),
            "restarting task a's already-terminal session must not cancel \
             task b's still-running, unrelated sibling session"
        );
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
    fn deleting_a_guardian_deletes_its_terminal_logs() {
        let d = daemon();
        let body =
            serde_json::json!({"name":"r","base_branch":"main","git_root":"/repo"}).to_string();
        route(&d, "POST", "/api/guardians", &body);
        let gid = "guardian-000000000001";
        let name = crate::tmux::session_name(
            &format!("guardian-{gid}"),
            crate::guardian_merge::RESOLVER_TASK,
            "resolver-0",
        );
        crate::terminal_log::write_attempt(&name, 0, "resolver output", 100);
        assert!(!crate::terminal_log::list_attempts(&name).is_empty());

        let del = route(&d, "DELETE", &format!("/api/guardians/{gid}"), "");
        assert_eq!(del.status, 200);

        assert!(crate::terminal_log::list_attempts(&name).is_empty());
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

    /// RAL-174 deadlock regression (see `restart_run_route_with_note_attaches_ghost_without_hanging`).
    #[test]
    fn restart_session_verify_route_with_note_attaches_ghost_without_hanging() {
        let d = daemon();
        route(&d, "POST", "/api/runs", &submit_body(GOOD));
        let body =
            serde_json::json!({"note": "verify was flaking on the last attempt"}).to_string();
        let r = route(
            &d,
            "POST",
            "/api/runs/run-000000000001/sessions/0/0/verify/0/restart",
            &body,
        );
        assert_eq!(r.status, 200);
        let ghost = d
            .lock()
            .get_ghost(&crate::ghost::session_uri("run-000000000001", 0, 0))
            .unwrap()
            .unwrap();
        assert_eq!(
            ghost.user_note.as_deref(),
            Some("verify was flaking on the last attempt")
        );
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

    /// RAL-174 deadlock regression (see `restart_run_route_with_note_attaches_ghost_without_hanging`).
    /// This is the endpoint that actually hung during development: unlike the
    /// other four, its handler unconditionally re-locks the store (via
    /// `sessions_of`) inside the match arm to compute the task's owned
    /// sessions, so it deadlocked even before reaching `apply_restart_note`.
    #[test]
    fn restart_task_verify_route_with_note_attaches_ghost_without_hanging() {
        let d = daemon();
        route(&d, "POST", "/api/runs", &submit_body(GOOD));
        let body = serde_json::json!({"note": "task verify needs a longer timeout"}).to_string();
        let r = route(
            &d,
            "POST",
            "/api/runs/run-000000000001/tasks/0/verify/0/restart",
            &body,
        );
        assert_eq!(r.status, 200);
        let ghost = d
            .lock()
            .get_ghost(&crate::ghost::session_uri("run-000000000001", 0, 0))
            .unwrap()
            .unwrap();
        assert_eq!(
            ghost.user_note.as_deref(),
            Some("task verify needs a longer timeout")
        );
    }

    #[test]
    fn restart_task_verify_bad_index_is_400() {
        let d = daemon();
        let r = route(&d, "POST", "/api/runs/run-1/tasks/x/verify/y/restart", "");
        assert_eq!(r.status, 400);
    }

    // ── RAL-150: task restart + env overrides ───────────────────────────────

    #[test]
    fn restart_task_preview_reports_impact_without_mutating() {
        let d = daemon();
        route(&d, "POST", "/api/runs", &submit_body(GOOD));
        let r = route(
            &d,
            "POST",
            "/api/runs/run-000000000001/tasks/0/restart/preview",
            "",
        );
        assert_eq!(r.status, 200);
        let body: serde_json::Value = serde_json::from_str(&r.body).unwrap();
        assert_eq!(body["sessions"].as_array().unwrap().len(), 1);
        assert_eq!(body["tasks"].as_array().unwrap().len(), 1);

        // Previewing did not mutate anything.
        let run = route(&d, "GET", "/api/runs/run-000000000001", "");
        let run: serde_json::Value = serde_json::from_str(&run.body).unwrap();
        assert_eq!(run["state"], "pending");
    }

    #[test]
    fn restart_task_preview_bad_index_is_400() {
        let d = daemon();
        let r = route(&d, "POST", "/api/runs/run-1/tasks/x/restart/preview", "");
        assert_eq!(r.status, 400);
    }

    #[test]
    fn restart_task_route_returns_pending() {
        let d = daemon();
        route(&d, "POST", "/api/runs", &submit_body(GOOD));
        let r = route(&d, "POST", "/api/runs/run-000000000001/tasks/0/restart", "");
        assert_eq!(r.status, 200);
        assert!(r.body.contains("\"state\":\"pending\""));
    }

    /// RAL-174 deadlock regression (see `restart_run_route_with_note_attaches_ghost_without_hanging`).
    #[test]
    fn restart_task_route_with_note_attaches_ghost_without_hanging() {
        let d = daemon();
        route(&d, "POST", "/api/runs", &submit_body(GOOD));
        let body = serde_json::json!({"note": "task was mid-refactor"}).to_string();
        let r = route(
            &d,
            "POST",
            "/api/runs/run-000000000001/tasks/0/restart",
            &body,
        );
        assert_eq!(r.status, 200);
        let ghost = d
            .lock()
            .get_ghost(&crate::ghost::session_uri("run-000000000001", 0, 0))
            .unwrap()
            .unwrap();
        assert_eq!(ghost.user_note.as_deref(), Some("task was mid-refactor"));
    }

    #[test]
    fn restart_task_bad_index_is_400() {
        let d = daemon();
        let r = route(&d, "POST", "/api/runs/run-1/tasks/x/restart", "");
        assert_eq!(r.status, 400);
    }

    #[test]
    fn restart_task_missing_task_is_404() {
        let d = daemon();
        route(&d, "POST", "/api/runs", &submit_body(GOOD));
        let r = route(&d, "POST", "/api/runs/run-000000000001/tasks/9/restart", "");
        assert_eq!(r.status, 404);
    }

    #[test]
    fn set_run_env_sets_and_is_reflected_on_get() {
        let d = daemon();
        route(&d, "POST", "/api/runs", &submit_body(GOOD));
        let body = serde_json::json!({"set": {"A": "1", "B": "2"}}).to_string();
        let r = route(&d, "POST", "/api/runs/run-000000000001/env", &body);
        assert_eq!(r.status, 200);
        let result: serde_json::Value = serde_json::from_str(&r.body).unwrap();
        assert_eq!(result["A"], "1");
        assert_eq!(result["B"], "2");

        let run = route(&d, "GET", "/api/runs/run-000000000001", "");
        let run: serde_json::Value = serde_json::from_str(&run.body).unwrap();
        assert_eq!(run["env_overrides"]["A"], "1");
        assert_eq!(run["env_overrides"]["B"], "2");
    }

    #[test]
    fn set_run_env_unsets_a_previously_set_key() {
        let d = daemon();
        route(&d, "POST", "/api/runs", &submit_body(GOOD));
        route(
            &d,
            "POST",
            "/api/runs/run-000000000001/env",
            &serde_json::json!({"set": {"A": "1"}}).to_string(),
        );
        let r = route(
            &d,
            "POST",
            "/api/runs/run-000000000001/env",
            &serde_json::json!({"unset": ["A"]}).to_string(),
        );
        assert_eq!(r.status, 200);
        let result: serde_json::Value = serde_json::from_str(&r.body).unwrap();
        assert!(result.as_object().unwrap().is_empty());
    }

    #[test]
    fn set_run_env_rejects_invalid_key() {
        let d = daemon();
        route(&d, "POST", "/api/runs", &submit_body(GOOD));
        let body = serde_json::json!({"set": {"NOT VALID": "1"}}).to_string();
        let r = route(&d, "POST", "/api/runs/run-000000000001/env", &body);
        assert_eq!(r.status, 400);
    }

    #[test]
    fn set_run_env_requires_set_or_unset() {
        let d = daemon();
        route(&d, "POST", "/api/runs", &submit_body(GOOD));
        let r = route(&d, "POST", "/api/runs/run-000000000001/env", "{}");
        assert_eq!(r.status, 400);
    }

    #[test]
    fn set_run_env_missing_run_is_404() {
        let d = daemon();
        let body = serde_json::json!({"set": {"A": "1"}}).to_string();
        let r = route(&d, "POST", "/api/runs/run-999/env", &body);
        assert_eq!(r.status, 404);
    }

    // ── hierarchical env overrides (RAL-150 extension) ─────────────────────

    #[test]
    fn set_task_env_sets_and_is_reflected_on_get() {
        let d = daemon();
        route(&d, "POST", "/api/runs", &submit_body(GOOD));
        let body = serde_json::json!({"set": {"A": "1"}}).to_string();
        let r = route(&d, "POST", "/api/runs/run-000000000001/tasks/0/env", &body);
        assert_eq!(r.status, 200);
        let result: serde_json::Value = serde_json::from_str(&r.body).unwrap();
        assert_eq!(result["A"], "1");

        let run = route(&d, "GET", "/api/runs/run-000000000001", "");
        let run: serde_json::Value = serde_json::from_str(&run.body).unwrap();
        assert_eq!(run["tasks"][0]["env_overrides"]["A"], "1");
    }

    #[test]
    fn set_task_env_missing_task_is_404() {
        let d = daemon();
        route(&d, "POST", "/api/runs", &submit_body(GOOD));
        let body = serde_json::json!({"set": {"A": "1"}}).to_string();
        let r = route(&d, "POST", "/api/runs/run-000000000001/tasks/9/env", &body);
        assert_eq!(r.status, 404);
    }

    #[test]
    fn set_task_env_bad_index_is_400() {
        let d = daemon();
        let body = serde_json::json!({"set": {"A": "1"}}).to_string();
        let r = route(&d, "POST", "/api/runs/run-1/tasks/x/env", &body);
        assert_eq!(r.status, 400);
    }

    #[test]
    fn set_task_env_rejects_invalid_key() {
        let d = daemon();
        route(&d, "POST", "/api/runs", &submit_body(GOOD));
        let body = serde_json::json!({"set": {"NOT VALID": "1"}}).to_string();
        let r = route(&d, "POST", "/api/runs/run-000000000001/tasks/0/env", &body);
        assert_eq!(r.status, 400);
    }

    #[test]
    fn set_task_verify_env_sets_and_is_reflected_on_get() {
        let d = daemon();
        route(&d, "POST", "/api/runs", &submit_body(GOOD));
        let body = serde_json::json!({"set": {"A": "1"}}).to_string();
        let r = route(
            &d,
            "POST",
            "/api/runs/run-000000000001/tasks/0/verify/env",
            &body,
        );
        assert_eq!(r.status, 200);
        let result: serde_json::Value = serde_json::from_str(&r.body).unwrap();
        assert_eq!(result["A"], "1");

        let run = route(&d, "GET", "/api/runs/run-000000000001", "");
        let run: serde_json::Value = serde_json::from_str(&run.body).unwrap();
        assert_eq!(run["tasks"][0]["verify_env_overrides"]["A"], "1");
        // The plain task-level map is untouched.
        assert!(run["tasks"][0].get("env_overrides").is_none());
    }

    #[test]
    fn set_task_verify_env_missing_task_is_404() {
        let d = daemon();
        route(&d, "POST", "/api/runs", &submit_body(GOOD));
        let body = serde_json::json!({"set": {"A": "1"}}).to_string();
        let r = route(
            &d,
            "POST",
            "/api/runs/run-000000000001/tasks/9/verify/env",
            &body,
        );
        assert_eq!(r.status, 404);
    }

    // ── RAL-191: per-verify-step and per-review-branch env endpoints ──────────

    /// A task with two verify steps plus a session verify step, so the
    /// per-step routes have distinct indices to address.
    const VERIFY_STEPS: &str = "[[task]]\nname=\"t\"\n\
        [[task.verify]]\ncommand=\"a\"\n\
        [[task.verify]]\ncommand=\"b\"\n\
        [[task.session]]\ncwd=\"/r\"\nprompt=\"p\"\n\
        [[task.session.verify]]\ncommand=\"c\"\n";

    #[test]
    fn set_task_verify_step_env_targets_one_step_not_the_whole_scope() {
        let d = daemon();
        route(&d, "POST", "/api/runs", &submit_body(VERIFY_STEPS));
        let body = serde_json::json!({"set": {"A": "step-1"}}).to_string();
        let r = route(
            &d,
            "POST",
            "/api/runs/run-000000000001/tasks/0/verify/1/env",
            &body,
        );
        assert_eq!(r.status, 200);

        let run = route(&d, "GET", "/api/runs/run-000000000001", "");
        let run: serde_json::Value = serde_json::from_str(&run.body).unwrap();
        assert_eq!(run["tasks"][0]["verify"][1]["env_overrides"]["A"], "step-1");
        // The sibling step and the scope-wide layer are both untouched.
        assert!(run["tasks"][0]["verify"][0].get("env_overrides").is_none());
        assert!(run["tasks"][0].get("verify_env_overrides").is_none());
    }

    #[test]
    fn set_session_verify_step_env_targets_one_step() {
        let d = daemon();
        route(&d, "POST", "/api/runs", &submit_body(VERIFY_STEPS));
        let body = serde_json::json!({"set": {"CI": "1"}}).to_string();
        let r = route(
            &d,
            "POST",
            "/api/runs/run-000000000001/sessions/0/0/verify/0/env",
            &body,
        );
        assert_eq!(r.status, 200);

        let run = route(&d, "GET", "/api/runs/run-000000000001", "");
        let run: serde_json::Value = serde_json::from_str(&run.body).unwrap();
        assert_eq!(
            run["tasks"][0]["sessions"][0]["verify"][0]["env_overrides"]["CI"],
            "1"
        );
    }

    #[test]
    fn the_scope_wide_verify_env_route_still_resolves_alongside_the_per_step_one() {
        // `/verify/env` and `/verify/{vi}/env` must not shadow each other.
        let d = daemon();
        route(&d, "POST", "/api/runs", &submit_body(VERIFY_STEPS));
        let body = serde_json::json!({"set": {"A": "scope"}}).to_string();
        let r = route(
            &d,
            "POST",
            "/api/runs/run-000000000001/tasks/0/verify/env",
            &body,
        );
        assert_eq!(r.status, 200);

        let run = route(&d, "GET", "/api/runs/run-000000000001", "");
        let run: serde_json::Value = serde_json::from_str(&run.body).unwrap();
        assert_eq!(run["tasks"][0]["verify_env_overrides"]["A"], "scope");
        assert!(run["tasks"][0]["verify"][0].get("env_overrides").is_none());
    }

    #[test]
    fn set_verify_step_env_missing_step_is_404() {
        let d = daemon();
        route(&d, "POST", "/api/runs", &submit_body(VERIFY_STEPS));
        let body = serde_json::json!({"set": {"A": "1"}}).to_string();
        let r = route(
            &d,
            "POST",
            "/api/runs/run-000000000001/tasks/0/verify/9/env",
            &body,
        );
        assert_eq!(r.status, 404);
    }

    #[test]
    fn set_verify_step_env_bad_index_is_400() {
        let d = daemon();
        let body = serde_json::json!({"set": {"A": "1"}}).to_string();
        let r = route(&d, "POST", "/api/runs/run-1/tasks/0/verify/x/env", &body);
        assert_eq!(r.status, 400);
    }

    #[test]
    fn set_branch_env_requires_set_unset_or_clear() {
        let d = daemon();
        let gid = {
            let store = d.lock();
            let id = store.create_guardian("r", "main", "/repo").unwrap();
            store.add_guardian_branch(&id, "feat").unwrap();
            id
        };
        let bid = {
            let store = d.lock();
            store.get_guardian(&gid).unwrap().branches[0].id.clone()
        };
        let r = route(
            &d,
            "POST",
            &format!("/api/guardians/{gid}/branches/{bid}/env"),
            "{}",
        );
        assert_eq!(r.status, 400);
    }

    #[test]
    fn set_branch_env_rejects_an_invalid_variable_name() {
        let d = daemon();
        let gid = {
            let store = d.lock();
            let id = store.create_guardian("r", "main", "/repo").unwrap();
            store.add_guardian_branch(&id, "feat").unwrap();
            id
        };
        let bid = {
            let store = d.lock();
            store.get_guardian(&gid).unwrap().branches[0].id.clone()
        };
        let body = serde_json::json!({"set": {"BAD-KEY": "x"}}).to_string();
        let r = route(
            &d,
            "POST",
            &format!("/api/guardians/{gid}/branches/{bid}/env"),
            &body,
        );
        assert_eq!(r.status, 400);
    }

    #[test]
    fn set_branch_env_missing_branch_is_404() {
        let d = daemon();
        let gid = {
            let store = d.lock();
            store.create_guardian("r", "main", "/repo").unwrap()
        };
        let body = serde_json::json!({"set": {"A": "1"}}).to_string();
        let r = route(
            &d,
            "POST",
            &format!("/api/guardians/{gid}/branches/branch-nope/env"),
            &body,
        );
        assert_eq!(r.status, 404);
    }

    #[test]
    fn set_branch_env_round_trips_override_and_tombstone_onto_the_view() {
        let d = daemon();
        let gid = {
            let store = d.lock();
            let id = store.create_guardian("r", "main", "/repo").unwrap();
            store.add_guardian_branch(&id, "feat").unwrap();
            id
        };
        let bid = {
            let store = d.lock();
            store.get_guardian(&gid).unwrap().branches[0].id.clone()
        };
        let body = serde_json::json!({"set": {"A": "x"}, "unset": ["B"]}).to_string();
        let r = route(
            &d,
            "POST",
            &format!("/api/guardians/{gid}/branches/{bid}/env"),
            &body,
        );
        assert_eq!(r.status, 200);
        let result: serde_json::Value = serde_json::from_str(&r.body).unwrap();
        assert_eq!(result["A"], "x");
        assert!(result["B"].is_null(), "a tombstone serializes as null");

        let g = route(&d, "GET", &format!("/api/guardians/{gid}"), "");
        let g: serde_json::Value = serde_json::from_str(&g.body).unwrap();
        assert_eq!(g["branches"][0]["env_overrides"]["A"], "x");
        assert!(g["branches"][0]["env_overrides"]["B"].is_null());
        // With no source session there is nothing to inherit, so the resolved
        // environment is just the override -- the tombstone drops out.
        assert_eq!(g["branches"][0]["resolved_env"]["A"], "x");
        assert!(g["branches"][0]["resolved_env"].get("B").is_none());
    }

    #[test]
    fn set_build_env_requires_set_unset_or_clear() {
        let d = daemon();
        let gid = {
            let store = d.lock();
            store.create_guardian("r", "main", "/repo").unwrap()
        };
        let r = route(&d, "POST", &format!("/api/guardians/{gid}/build-env"), "{}");
        assert_eq!(r.status, 400);
    }

    #[test]
    fn set_manual_checks_env_rejects_an_invalid_variable_name() {
        let d = daemon();
        let gid = {
            let store = d.lock();
            store.create_guardian("r", "main", "/repo").unwrap()
        };
        let body = serde_json::json!({"set": {"BAD-KEY": "x"}}).to_string();
        let r = route(
            &d,
            "POST",
            &format!("/api/guardians/{gid}/manual-checks-env"),
            &body,
        );
        assert_eq!(r.status, 400);
    }

    #[test]
    fn set_build_env_missing_guardian_is_404() {
        let d = daemon();
        let body = serde_json::json!({"set": {"A": "1"}}).to_string();
        let r = route(&d, "POST", "/api/guardians/guardian-nope/build-env", &body);
        assert_eq!(r.status, 404);
    }

    #[test]
    fn build_and_manual_checks_env_round_trip_independently_onto_the_view() {
        // RAL-203: the finalize-time build/check-gate step and the
        // manual-checks step each get their own override layer, exposed on
        // the guardian view as `build_env`/`manual_checks_env`
        // (`build_env_overrides`/`manual_checks_env_overrides` for the raw
        // layer) -- setting one must never affect the other.
        let d = daemon();
        let gid = {
            let store = d.lock();
            store.create_guardian("r", "main", "/repo").unwrap()
        };

        let build_body = serde_json::json!({"set": {"A": "build-value"}, "unset": ["B"]});
        let r = route(
            &d,
            "POST",
            &format!("/api/guardians/{gid}/build-env"),
            &build_body.to_string(),
        );
        assert_eq!(r.status, 200);
        let result: serde_json::Value = serde_json::from_str(&r.body).unwrap();
        assert_eq!(result["A"], "build-value");
        assert!(result["B"].is_null(), "a tombstone serializes as null");

        let manual_body = serde_json::json!({"set": {"A": "manual-value"}});
        let r = route(
            &d,
            "POST",
            &format!("/api/guardians/{gid}/manual-checks-env"),
            &manual_body.to_string(),
        );
        assert_eq!(r.status, 200);

        let g = route(&d, "GET", &format!("/api/guardians/{gid}"), "");
        let g: serde_json::Value = serde_json::from_str(&g.body).unwrap();
        assert_eq!(g["build_env_overrides"]["A"], "build-value");
        assert!(g["build_env_overrides"]["B"].is_null());
        assert_eq!(g["build_env"]["A"], "build-value");
        assert!(g["build_env"].get("B").is_none());
        assert_eq!(g["manual_checks_env_overrides"]["A"], "manual-value");
        assert_eq!(g["manual_checks_env"]["A"], "manual-value");
        // No branches at all here, so `combined_env` (the shared baseline) is
        // empty -- neither section's own override leaked into it.
        assert!(g["combined_env"].as_object().unwrap().is_empty());
    }

    #[test]
    fn set_session_env_sets_and_is_reflected_on_get() {
        let d = daemon();
        route(&d, "POST", "/api/runs", &submit_body(GOOD));
        let body = serde_json::json!({"set": {"A": "1"}}).to_string();
        let r = route(
            &d,
            "POST",
            "/api/runs/run-000000000001/sessions/0/0/env",
            &body,
        );
        assert_eq!(r.status, 200);
        let result: serde_json::Value = serde_json::from_str(&r.body).unwrap();
        assert_eq!(result["A"], "1");

        let run = route(&d, "GET", "/api/runs/run-000000000001", "");
        let run: serde_json::Value = serde_json::from_str(&run.body).unwrap();
        assert_eq!(run["tasks"][0]["sessions"][0]["env_overrides"]["A"], "1");
    }

    #[test]
    fn set_session_env_missing_session_is_404() {
        let d = daemon();
        route(&d, "POST", "/api/runs", &submit_body(GOOD));
        let body = serde_json::json!({"set": {"A": "1"}}).to_string();
        let r = route(
            &d,
            "POST",
            "/api/runs/run-000000000001/sessions/0/9/env",
            &body,
        );
        assert_eq!(r.status, 404);
    }

    #[test]
    fn set_session_env_bad_index_is_400() {
        let d = daemon();
        let body = serde_json::json!({"set": {"A": "1"}}).to_string();
        let r = route(&d, "POST", "/api/runs/run-1/sessions/x/0/env", &body);
        assert_eq!(r.status, 400);
    }

    #[test]
    fn set_session_verify_env_sets_and_is_reflected_on_get() {
        let d = daemon();
        route(&d, "POST", "/api/runs", &submit_body(GOOD));
        let body = serde_json::json!({"set": {"A": "1"}}).to_string();
        let r = route(
            &d,
            "POST",
            "/api/runs/run-000000000001/sessions/0/0/verify/env",
            &body,
        );
        assert_eq!(r.status, 200);
        let result: serde_json::Value = serde_json::from_str(&r.body).unwrap();
        assert_eq!(result["A"], "1");

        let run = route(&d, "GET", "/api/runs/run-000000000001", "");
        let run: serde_json::Value = serde_json::from_str(&run.body).unwrap();
        assert_eq!(
            run["tasks"][0]["sessions"][0]["verify_env_overrides"]["A"],
            "1"
        );
    }

    #[test]
    fn set_session_verify_env_missing_session_is_404() {
        let d = daemon();
        route(&d, "POST", "/api/runs", &submit_body(GOOD));
        let body = serde_json::json!({"set": {"A": "1"}}).to_string();
        let r = route(
            &d,
            "POST",
            "/api/runs/run-000000000001/sessions/0/9/verify/env",
            &body,
        );
        assert_eq!(r.status, 404);
    }

    #[test]
    fn set_env_overrides_requires_set_or_unset_for_every_scope() {
        let d = daemon();
        route(&d, "POST", "/api/runs", &submit_body(GOOD));
        for path in [
            "/api/runs/run-000000000001/tasks/0/env",
            "/api/runs/run-000000000001/tasks/0/verify/env",
            "/api/runs/run-000000000001/sessions/0/0/env",
            "/api/runs/run-000000000001/sessions/0/0/verify/env",
        ] {
            let r = route(&d, "POST", path, "{}");
            assert_eq!(r.status, 400, "{path}");
        }
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

    #[test]
    fn guardian_settings_sets_and_resets_verify_scope() {
        let d = daemon();
        let gid = make_guardian(&d);
        let r = route(&d, "GET", &format!("/api/guardians/{gid}"), "");
        assert!(r.body.contains("\"verify_scope\":null"));
        assert!(
            r.body
                .contains("\"effective_verify_scope\":\"each_branch\"")
        );

        let body = serde_json::json!({"verify_scope": "final_branch"}).to_string();
        let r = route(&d, "POST", &format!("/api/guardians/{gid}/settings"), &body);
        assert_eq!(r.status, 200);
        assert!(r.body.contains("\"verify_scope\":\"final_branch\""));
        assert!(
            r.body
                .contains("\"effective_verify_scope\":\"final_branch\"")
        );

        // Empty string resets it back to "inherit the project default".
        let body = serde_json::json!({"verify_scope": ""}).to_string();
        let r = route(&d, "POST", &format!("/api/guardians/{gid}/settings"), &body);
        assert_eq!(r.status, 200);
        assert!(r.body.contains("\"verify_scope\":null"));
        assert!(
            r.body
                .contains("\"effective_verify_scope\":\"each_branch\"")
        );
    }

    #[test]
    fn guardian_settings_sets_verify_skip_auto_clean() {
        let d = daemon();
        let gid = make_guardian(&d);
        let body = serde_json::json!({"verify_skip_auto_clean": true}).to_string();
        let r = route(&d, "POST", &format!("/api/guardians/{gid}/settings"), &body);
        assert_eq!(r.status, 200);
        assert!(r.body.contains("\"verify_skip_auto_clean\":true"));
        assert!(r.body.contains("\"effective_verify_skip_auto_clean\":true"));
    }

    // -----------------------------------------------------------------------
    // RAL-188: GET /api/resolve?uri=
    // -----------------------------------------------------------------------

    /// A run whose names exercise every addressing case the URI grammar has:
    /// a named session, a named task verify, and an *anonymous* session verify
    /// that is only reachable as `VERIFY[~0]`.
    const URI_RUN: &str = "\
[[task]]
name=\"ral-178\"
[[task.session]]
name=\"work\"
cwd=\"/r\"
prompt=\"p\"
[[task.session.verify]]
command=\"lint\"
[[task.verify]]
id=\"test\"
command=\"cargo test\"
";

    /// Submit `URI_RUN` and return its run id.
    fn uri_run(d: &Daemon) -> String {
        let r = route(d, "POST", "/api/runs", &submit_body(URI_RUN));
        assert_eq!(r.status, 201, "{}", r.body);
        let v: serde_json::Value = serde_json::from_str(&r.body).unwrap();
        v["run_id"].as_str().unwrap().to_string()
    }

    fn resolve(d: &Daemon, uri: &str) -> Reply {
        route(d, "GET", &format!("/api/resolve?uri={uri}"), "")
    }

    #[test]
    fn resolve_uri_maps_a_session_uri_to_positional_coordinates() {
        let d = daemon();
        let id = uri_run(&d);
        let r = resolve(
            &d,
            &format!("ralphus:/RUN[{id}]/TASK[ral-178]/SESSION[work]?id={id}"),
        );
        assert_eq!(r.status, 200, "{}", r.body);
        let v: serde_json::Value = serde_json::from_str(&r.body).unwrap();
        assert_eq!(v["kind"], "session");
        assert_eq!(v["run_id"], id);
        assert_eq!(v["task_idx"], 0);
        assert_eq!(v["session_idx"], 0);
    }

    #[test]
    fn resolve_uri_addresses_a_task_scope_verify_by_its_id() {
        let d = daemon();
        let id = uri_run(&d);
        let r = resolve(
            &d,
            &format!("ralphus:/RUN[{id}]/TASK[ral-178]/VERIFY[test]?id={id}"),
        );
        assert_eq!(r.status, 200, "{}", r.body);
        let v: serde_json::Value = serde_json::from_str(&r.body).unwrap();
        assert_eq!(v["kind"], "verify");
        assert_eq!(v["verify_scope"], "task");
        assert_eq!(v["verify_idx"], 0);
        assert_eq!(v["session_idx"], serde_json::Value::Null);
    }

    #[test]
    fn resolve_uri_addresses_an_anonymous_verify_only_by_its_positional_index() {
        let d = daemon();
        let id = uri_run(&d);
        let base = format!("ralphus:/RUN[{id}]/TASK[ral-178]/SESSION[work]");
        let r = resolve(&d, &format!("{base}/VERIFY[~0]?id={id}"));
        assert_eq!(r.status, 200, "{}", r.body);
        let v: serde_json::Value = serde_json::from_str(&r.body).unwrap();
        assert_eq!(v["verify_scope"], "session");
        assert_eq!(v["verify_idx"], 0);
        // The step has no author-supplied id, so it has no name to match --
        // a bare `VERIFY[0]` is a *name* lookup and must not silently hit it.
        assert_eq!(
            resolve(&d, &format!("{base}/VERIFY[0]?id={id}")).status,
            409
        );
    }

    #[test]
    fn resolve_uri_echoes_the_canonical_form_with_the_id_sidecar() {
        let d = daemon();
        let id = uri_run(&d);
        // Addressed by id (the run has no label), no `?id=` sidecar supplied.
        let r = resolve(&d, &format!("ralphus:/RUN[{id}]/TASK[ral-178]"));
        assert_eq!(r.status, 200, "{}", r.body);
        let v: serde_json::Value = serde_json::from_str(&r.body).unwrap();
        assert_eq!(
            v["uri"],
            format!("ralphus:/RUN[{id}]/TASK[ral-178]?id={id}")
        );
    }

    #[test]
    fn resolve_uri_rejects_a_malformed_uri_with_400_and_a_missing_run_with_404() {
        let d = daemon();
        assert_eq!(resolve(&d, "ralphus:/RUN[unterminated").status, 400);
        assert_eq!(resolve(&d, "ralphus:/BOGUS[x]").status, 400);
        assert_eq!(route(&d, "GET", "/api/resolve", "").status, 400);
        assert_eq!(resolve(&d, "ralphus:/RUN[nope]").status, 404);
    }

    #[test]
    fn resolve_uri_reports_an_unknown_name_rather_than_guessing() {
        let d = daemon();
        let id = uri_run(&d);
        let r = resolve(&d, &format!("ralphus:/RUN[{id}]/TASK[nope]?id={id}"));
        assert_eq!(r.status, 409, "{}", r.body);
        assert!(r.body.contains("ral-178"), "{}", r.body);
    }

    #[test]
    fn resolve_uri_accepts_a_percent_encoded_uri_query_value() {
        let d = daemon();
        let id = uri_run(&d);
        let raw = format!("ralphus:/RUN[{id}]/TASK[ral-178]?id={id}");
        let encoded = raw
            .replace('[', "%5B")
            .replace(']', "%5D")
            .replace('?', "%3F");
        assert_eq!(resolve(&d, &encoded).status, 200);
    }

    #[test]
    fn resolve_uri_resolves_a_review_and_its_combined_worktree() {
        let d = daemon();
        let gid = make_guardian(&d);
        let name = {
            let v: serde_json::Value =
                serde_json::from_str(&route(&d, "GET", &format!("/api/guardians/{gid}"), "").body)
                    .unwrap();
            v["name"].as_str().unwrap().to_string()
        };
        let r = resolve(&d, &format!("ralphus:/REVIEW[{name}]?id={gid}&combined"));
        assert_eq!(r.status, 200, "{}", r.body);
        let v: serde_json::Value = serde_json::from_str(&r.body).unwrap();
        assert_eq!(v["kind"], "review");
        assert_eq!(v["guardian_id"], gid);
        assert_eq!(v["combined"], true);
    }

    /// `?worktree=` reaches one stacked branch three ways: its **label** (the
    /// feature branch name the Reviews UI lists it under), its stable
    /// `branch-…` id (§C.2), or `~<position>`. All three land on the same
    /// branch, and the canonical form always echoes the label back.
    #[test]
    fn resolve_uri_addresses_a_review_worktree_by_label_id_or_position() {
        let d = daemon();
        let gid = make_guardian(&d);
        for branch in ["RAL-187-codex-token-cost-readout", "RAL-188-uri-scheme"] {
            let body = serde_json::json!({ "branch": branch }).to_string();
            let r = route(&d, "POST", &format!("/api/guardians/{gid}/branches"), &body);
            assert_eq!(r.status, 200, "{}", r.body);
        }
        let view: serde_json::Value =
            serde_json::from_str(&route(&d, "GET", &format!("/api/guardians/{gid}"), "").body)
                .unwrap();
        let branch_id = view["branches"][1]["id"].as_str().unwrap().to_string();

        let by = |worktree: &str| {
            let r = resolve(
                &d,
                &format!("ralphus:/REVIEW[r]?id={gid}&worktree={worktree}"),
            );
            assert_eq!(r.status, 200, "{worktree}: {}", r.body);
            serde_json::from_str::<serde_json::Value>(&r.body).unwrap()
        };
        for worktree in ["RAL-188-uri-scheme", branch_id.as_str(), "~1"] {
            let v = by(worktree);
            assert_eq!(v["branch_id"], branch_id, "{worktree}");
            assert_eq!(v["branch"], "RAL-188-uri-scheme", "{worktree}");
            // The canonical form is always the label, never the id or position.
            assert_eq!(
                v["uri"],
                format!("ralphus:/REVIEW[r]?id={gid}&worktree=RAL-188-uri-scheme"),
                "{worktree}"
            );
        }
        assert_eq!(
            resolve(&d, &format!("ralphus:/REVIEW[r]?id={gid}&worktree=nope")).status,
            409
        );
    }

    #[test]
    fn uri_query_value_takes_everything_after_uri_so_an_embedded_ampersand_survives() {
        assert_eq!(
            uri_query_value("uri=ralphus:/REVIEW[r]?id=g-1&combined"),
            Some("ralphus:/REVIEW[r]?id=g-1&combined")
        );
        assert_eq!(
            uri_query_value("mode=readonly&uri=ralphus:/RUN[r]"),
            Some("ralphus:/RUN[r]")
        );
        assert_eq!(uri_query_value("mode=readonly"), None);
    }
}
