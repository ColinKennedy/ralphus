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
use crate::store::{NodeState, SquadState, Store, StoreError};
use crate::summary_worker::SummaryQueue;
use crate::vcs;

/// The running daemon: its store handle plus configuration.
pub struct Daemon {
    store: Arc<Mutex<Store>>,
    max_concurrent: i64,
    /// Cancel tokens of in-flight squads, shared with the scheduler's workers so a
    /// `cancel` request can stop the running worker and its subprocess.
    cancellations: Cancellations,
    /// Per-cell detach tokens (RAL-288 Stage 6), shared with the scheduler's
    /// `SubprocessRunner` so the `open-terminal?mode=agent` handler can stop
    /// exactly one running cell -- without touching the rest of its squad --
    /// so a real interactive resume session can safely take over.
    detachments: crate::cancel::Detachments,
    /// Live registry of cell subprocess PIDs, shared with the runner so the
    /// resource-usage endpoint can attribute OS metrics to running tasks (RAL-11).
    procs: ProcRegistry,
    /// Global concurrency semaphore shared by the scheduler, task-level proofs,
    /// and guardian review merges so all three count against `max_concurrent`.
    /// `max_concurrent == 0` means no limit (see `Semaphore::new`).
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
    /// The bearer token every HTTP route requires (RAL-219), or `None` to
    /// leave the API open. `Daemon::new` leaves this unset so `route()`'s own
    /// ~100 in-process unit tests (and `serve_with`, used by over-the-wire
    /// tests that don't care about auth) keep working with no header
    /// plumbing; real entry points (`serve`, `serve_with_token`) always set
    /// one via [`Daemon::with_token`]. Checked in `run_http_loop`, not inside
    /// `route()` itself — auth is an HTTP-boundary concern, not part of the
    /// pure dispatcher (see the module doc comment).
    token: Option<String>,
    /// Short-lived, single-use tickets gating `/api/events` (RAL-222) — see
    /// `crate::token`'s module doc comment for why this exists alongside
    /// `token` above. Minted by the `POST /api/events/ticket` handler
    /// (through `route()`, so it's gated by the bearer token like every
    /// other route) and consumed by `run_http_loop` before it hands a
    /// connection off to `serve_events_stream`.
    events_tickets: crate::token::TicketStore,
    /// Short-lived, single-use tickets gating the terminal-relay WebSocket
    /// listener (RAL-355 Phase 10) -- same rationale as `events_tickets`
    /// (`?ticket=...` on a URL a browser `WebSocket` constructor cannot
    /// attach a bearer header to), kept in a separate namespace so a ticket
    /// minted for one purpose is never usable for the other.
    terminal_tickets: crate::token::TicketStore,
    /// The terminal-relay WebSocket listener's bound port (RAL-355 Phase
    /// 10), set once by `serve()` after a successful bind. `0` means "not
    /// started" -- `route()`'s own ~100 in-process unit tests and
    /// `serve_with`/`serve_with_token` never start this listener, so ticket
    /// minting there correctly reports it as unavailable rather than
    /// claiming a port nothing is actually listening on.
    terminal_relay_port: std::sync::atomic::AtomicU16,
    /// `(squad_id, task_idx, cell_idx)` of every remote terminal session
    /// currently attached (RAL-355 Phase 10) -- guards against two WebSocket
    /// clients independently minting tickets and connecting to the same
    /// idle cell at once, which would spawn two separate `claude --resume
    /// <same-session-id>` processes racing to write the same conversation
    /// transcript on the remote machine. Purely in-memory (a daemon restart
    /// drops it, same as the WS listener itself not surviving a restart) --
    /// there is nothing to reconcile it against on startup.
    active_terminal_sessions: Mutex<std::collections::HashSet<(String, i64, i64)>>,
    /// Decides which agents a user can see/select (`GET /api/agents`) --
    /// always [`crate::agent_access::DefaultAgentAccess`] today, since there
    /// is no real per-user auth to key a different implementation off of yet.
    /// A `dyn` field (rather than calling `DefaultAgentAccess` directly from
    /// the handler) so a real "User adapter" is a one-line swap here once
    /// RAL-252 lands.
    agent_access: Arc<dyn crate::agent_access::AgentAccess>,
    /// RAL-297: in-flight/completed "generation step" jobs backing the
    /// Simple task form's opt-in "Generate Proofs"/"Generate Manual Checks"
    /// buttons -- see `crate::generation`'s module doc comment for why this
    /// is fire-and-forget-plus-poll rather than a blocking HTTP call.
    generation_jobs: crate::generation::GenerationJobs,
}

impl Daemon {
    /// Build a daemon around an already-open store.
    #[must_use]
    pub fn new(store: Store, max_concurrent: i64) -> Self {
        Self {
            store: Arc::new(Mutex::new(store)),
            max_concurrent,
            cancellations: Cancellations::new(),
            detachments: crate::cancel::Detachments::new(),
            procs: ProcRegistry::new(),
            sem: Arc::new(Semaphore::new(max_concurrent)),
            summary_queue: SummaryQueue::new(),
            shutdown: Arc::new(AtomicBool::new(false)),
            token: None,
            events_tickets: crate::token::TicketStore::new(),
            terminal_tickets: crate::token::TicketStore::new(),
            terminal_relay_port: std::sync::atomic::AtomicU16::new(0),
            active_terminal_sessions: Mutex::new(std::collections::HashSet::new()),
            agent_access: Arc::new(crate::agent_access::DefaultAgentAccess),
            generation_jobs: crate::generation::GenerationJobs::new(),
        }
    }

    /// Require `token` on every HTTP route (via `Authorization: Bearer
    /// <token>`) except `/api/events` — see [`Daemon::authorized`] and the
    /// `token` field's doc comment.
    #[must_use]
    pub fn with_token(mut self, token: String) -> Self {
        self.token = Some(token);
        self
    }

    /// Whether `auth_header` (the raw `Authorization` header value, if any)
    /// satisfies this daemon's configured token. Always `true` when no token
    /// is configured (see the `token` field's doc comment).
    fn authorized(&self, auth_header: Option<&str>) -> bool {
        let Some(expected) = &self.token else {
            return true;
        };
        auth_header
            .and_then(|h| h.strip_prefix("Bearer "))
            .is_some_and(|presented| crate::token::constant_time_eq(presented, expected))
    }

    /// Mint a short-lived, single-use `/api/events` ticket (RAL-222). Called
    /// from the `POST /api/events/ticket` route handler, which is itself
    /// gated by [`Daemon::authorized`] like every other route.
    fn mint_events_ticket(&self) -> String {
        self.events_tickets.mint()
    }

    /// Validate and consume an `/api/events` ticket (RAL-222) — `true` if
    /// `ticket` was a valid, unexpired, not-yet-used ticket this daemon
    /// minted. Always `true` when no bearer token is configured, mirroring
    /// [`Daemon::authorized`] (see its doc comment) — an untokened daemon
    /// (`Daemon::new`/`serve_with`, used by `route()`'s own in-process tests)
    /// leaves the whole API open, `/api/events` included. Called by
    /// `run_http_loop` before it hands the connection off to
    /// `serve_events_stream`.
    fn consume_events_ticket(&self, ticket: Option<&str>) -> bool {
        if self.token.is_none() {
            return true;
        }
        ticket.is_some_and(|t| self.events_tickets.consume(t))
    }

    /// Mint a short-lived, single-use terminal-relay ticket (RAL-355
    /// Phase 10). Called from the `POST .../terminal-ticket` route handler,
    /// itself gated by [`Daemon::authorized`] like every other route.
    pub(crate) fn mint_terminal_ticket(&self) -> String {
        self.terminal_tickets.mint()
    }

    /// Validate and consume a terminal-relay ticket -- `true` if `ticket` was
    /// a valid, unexpired, not-yet-used ticket this daemon minted. Mirrors
    /// [`Daemon::consume_events_ticket`]'s "always true when no bearer token
    /// is configured" rule.
    pub(crate) fn consume_terminal_ticket(&self, ticket: Option<&str>) -> bool {
        if self.token.is_none() {
            return true;
        }
        ticket.is_some_and(|t| self.terminal_tickets.consume(t))
    }

    /// Record the terminal-relay listener's bound port after `serve()`
    /// starts it, so ticket-mint responses can tell a client where to
    /// connect.
    pub(crate) fn set_terminal_relay_port(&self, port: u16) {
        self.terminal_relay_port
            .store(port, std::sync::atomic::Ordering::Relaxed);
    }

    /// The terminal-relay listener's bound port, or `0` if it was never
    /// started (see the field's own doc comment).
    pub(crate) fn terminal_relay_port(&self) -> u16 {
        self.terminal_relay_port
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Claim the terminal-relay slot for one cell -- `true` if it was free
    /// and is now held by this call, `false` if another session already
    /// holds it. Pair with [`Daemon::release_terminal_session`] once the
    /// session ends (see `terminal_relay.rs`'s `SessionSlot` guard, which
    /// does this via `Drop` so every exit path releases it).
    pub(crate) fn try_acquire_terminal_session(
        &self,
        squad_id: &str,
        task_idx: i64,
        cell_idx: i64,
    ) -> bool {
        self.active_terminal_sessions
            .lock()
            .expect("active_terminal_sessions mutex poisoned")
            .insert((squad_id.to_string(), task_idx, cell_idx))
    }

    /// Release a slot claimed by [`Daemon::try_acquire_terminal_session`].
    /// A no-op if it was not held (defensive -- should never happen given
    /// the RAII guard, but a double-release must never panic).
    pub(crate) fn release_terminal_session(&self, squad_id: &str, task_idx: i64, cell_idx: i64) {
        self.active_terminal_sessions
            .lock()
            .expect("active_terminal_sessions mutex poisoned")
            .remove(&(squad_id.to_string(), task_idx, cell_idx));
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

    /// A cloned handle to the per-cell detach registry (RAL-288 Stage 6),
    /// for the scheduler's `SubprocessRunner`.
    #[must_use]
    pub fn detachments_handle(&self) -> crate::cancel::Detachments {
        self.detachments.clone()
    }

    /// A cloned handle to the subprocess PID registry (for the cell runner).
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

    pub(crate) fn lock(&self) -> MutexGuard<'_, Store> {
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
    /// Authoritative clone URL for provisioning this project on another
    /// machine. `url` is accepted as the user-facing API alias.
    #[serde(default, alias = "url")]
    clone_url: Option<String>,
    /// RAL-355: explicitly clear a previously registered clone URL. Distinct
    /// from omitting `clone_url` (which preserves whatever is already
    /// stored) and from sending an empty `clone_url` (which is rejected as
    /// invalid) -- clearing needs its own unambiguous signal rather than
    /// overloading either of those.
    #[serde(default)]
    clear_clone_url: bool,
    #[serde(default = "default_vcs")]
    vcs: String,
    /// RAL-307: explicit per-project default for whether a newly submitted PR
    /// defaults to the worktree/feature branch name. `None` (the field
    /// omitted) stamps the live global config's value instead, mirroring
    /// `skip_base_updates`'s auto-stamp -- see `Store::register_project`.
    #[serde(default)]
    match_pr_branch_name: Option<bool>,
}

#[derive(Serialize)]
struct ProjectsResponse {
    projects: Vec<crate::store::ProjectView>,
}

#[derive(Deserialize)]
struct CreateProjectForkBody {
    #[serde(default)]
    user: String,
    fork_url: String,
    #[serde(default)]
    remote_name: Option<String>,
    #[serde(default)]
    fork_owner: Option<String>,
}

#[derive(Deserialize, Default)]
struct PatchProjectForkBody {
    #[serde(default)]
    fork_url: Option<String>,
    #[serde(default)]
    remote_name: Option<String>,
    #[serde(default)]
    fork_owner: Option<String>,
}

#[derive(Serialize)]
struct ProjectForksResponse {
    forks: Vec<crate::project_forks::ForkRecord>,
}

#[derive(Serialize)]
struct AgentProfilesHealthResponse {
    profiles: Vec<crate::agent_profiles::ProfileHealthResult>,
}

#[derive(Serialize)]
struct AgentsResponse {
    agents: Vec<crate::agent_access::AvailableAgent>,
    /// The effective `[review].default_resolver_agent` (`.ralphus.toml`,
    /// global layered under `cwd`'s project config) -- whichever entry in
    /// `agents` this names is the one a review falls back to when it sets no
    /// resolver agent of its own. Not guaranteed to match an entry in
    /// `agents` if misconfigured -- see `ralphus check health`.
    default_agent: String,
}

#[derive(Serialize)]
struct UsersResponse {
    users: Vec<crate::users::UserView>,
}

/// `GET /api/whoami` response (RAL-332). `name` is `None` when no identity
/// resolves at all (no header, no `default_user`) -- same case
/// `current_user` returns `Ok(None)` for.
#[derive(Serialize)]
struct WhoAmIResponse {
    name: Option<String>,
    is_admin: bool,
}

/// `POST /api/users/{name}/admin` body (RAL-332).
#[derive(Deserialize)]
struct SetAdminBody {
    is_admin: bool,
}

#[derive(Serialize)]
struct HiddenResponse {
    hidden: Vec<crate::hidden::HiddenItem>,
}

#[derive(Serialize)]
struct HiddenStateResponse {
    hidden: bool,
}

/// `POST /api/hidden/squads/batch` body -- the board's multi-select
/// Hide/Unhide menu items send every selected squad id in one request
/// instead of one HTTP round trip per id.
#[derive(Deserialize)]
struct HiddenSquadsBatchBody {
    ids: Vec<String>,
    hidden: bool,
}

#[derive(Serialize)]
struct HiddenBatchFailure {
    id: String,
    error: String,
}

#[derive(Serialize)]
struct HiddenBatchResponse {
    hidden: bool,
    failed: Vec<HiddenBatchFailure>,
}

/// `POST /api/users` body -- see `crate::users`'s module doc comment for why
/// this is a placeholder identity registry, not authentication.
#[derive(Deserialize)]
struct CreateUserBody {
    name: String,
}

/// `POST /api/users/{name}/preferences` body (RAL-320) -- see
/// `crate::users::UserView`'s field docs for what `auto_watch` and
/// `default_notify_tiers` mean.
#[derive(Deserialize)]
struct UserPreferencesBody {
    #[serde(default)]
    auto_watch: bool,
    /// Tier names (`"urgent"`/`"high"`/`"normal"`), e.g.
    /// `["urgent", "high"]`. Omitted or empty means "everything" --
    /// see [`crate::mailbox::all_tiers`].
    #[serde(default)]
    default_notify_tiers: Option<Vec<String>>,
}

/// `GET /api/watches?user=` response body.
#[derive(Serialize)]
struct WatchesResponse {
    watches: Vec<crate::monitor::WatchView>,
}

/// `POST /api/watches?user=` body -- `notify_tiers` omitted or
/// empty defaults to the acting user's `default_notify_tiers` preference
/// (see `crate::users::UserView`), falling back to every tier if that user
/// isn't registered either.
#[derive(Deserialize)]
struct CreateWatchBody {
    entity_uri: String,
    #[serde(default)]
    notify_tiers: Option<Vec<String>>,
}

#[derive(Serialize)]
struct SecretEnvNamesResponse {
    names: Vec<crate::secret_env_names::SecretEnvNameView>,
}

/// `POST /api/secret-env-names` body (RAL-281) -- see
/// `crate::secret_env_names`'s module doc comment.
#[derive(Deserialize)]
struct AddSecretEnvNameBody {
    name: String,
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
    /// the board uses this to render `pending` squads as "waiting" rather than
    /// implying the scheduler is simply slow to pick them up.
    downtime_active: bool,
}

#[derive(Serialize)]
struct Board {
    daemon: DaemonStatus,
    squads: Vec<crate::store::SquadView>,
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
    squad_id: String,
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
    route_for_user(daemon, method, path, body, None)
}

/// Dispatch a request with the caller-claimed identity read at the HTTP
/// boundary. `route` uses no claimed identity, which keeps direct callers and
/// unit tests on the configured-default path.
fn route_for_user(
    daemon: &Daemon,
    method: &str,
    path: &str,
    body: &str,
    user_header: Option<&str>,
) -> Reply {
    let (path_only, query) = path.split_once('?').unwrap_or((path, ""));
    let segs: Vec<&str> = path_only.trim_matches('/').split('/').collect();
    match (method, segs.as_slice()) {
        ("GET", ["api", "daemon"]) => health(daemon),
        ("POST", ["api", "daemon", "shutdown"]) => shutdown(daemon, body),
        // RAL-222: mints a short-lived, single-use ticket for `/api/events`
        // (SSE), which can't carry the bearer token directly — see
        // `crate::token`'s module doc comment. Reached through the normal
        // `route()` dispatch, so it's gated by `Daemon::authorized` exactly
        // like every other route.
        ("POST", ["api", "events", "ticket"]) => mint_events_ticket(daemon),
        ("GET", ["api", "tasks"]) => board(daemon, query),
        // RAL-332: reads stay open to every caller -- `GET /api/projects` and
        // `.../branches` back the Simple task form's project/branch pickers
        // for every user, not just admins. Only the mutating registration
        // action is an admin action (matches `register_project`'s own doc
        // comment, "an explicit admin action, not something a task file can
        // declare" -- true before this ticket, just not enforced until now).
        ("GET", ["api", "projects"]) => list_projects(daemon),
        ("POST", ["api", "projects"]) => {
            admin_gated(daemon, user_header, || register_project(daemon, body))
        }
        ("GET", ["api", "projects", name]) => get_project(daemon, name),
        ("GET", ["api", "projects", name, "validate"]) => validate_project(daemon, name),
        ("GET", ["api", "projects", name, "branches"]) => project_branches(daemon, name),
        // RAL-338: fork registration. Reads open to every caller (matches the
        // `projects` pattern above); mutations admin-gated like
        // `register_project`. `GET /api/project-forks` is the unscoped list
        // across every project. A trailing user segment targets one specific
        // row (`.../forks/{user}`); PATCH/DELETE with *no* trailing segment
        // target the project-wide default (`user=""`) row instead -- the
        // literal empty segment form (`.../forks/`) is indistinguishable from
        // the collection path once `path.trim_matches('/')` strips a trailing
        // slash, so the no-segment form is this router's way of addressing
        // the default row.
        ("GET", ["api", "project-forks"]) => list_all_project_forks(daemon),
        // RAL-385: read-only worktree-retirement view -- every review
        // worktree classified by retirement state plus the durable history
        // of successful/failed retirement attempts. Open to every caller
        // (read-only, like the fork reads above).
        ("GET", ["api", "worktree-retirements"]) => worktree_retirements(daemon),
        ("GET", ["api", "projects", name, "forks"]) => {
            list_project_forks(daemon, &url_decode(name))
        }
        ("POST", ["api", "projects", name, "forks"]) => admin_gated(daemon, user_header, || {
            create_project_fork(daemon, &url_decode(name), body)
        }),
        ("PATCH", ["api", "projects", name, "forks"]) => admin_gated(daemon, user_header, || {
            patch_project_fork(daemon, &url_decode(name), "", body)
        }),
        ("DELETE", ["api", "projects", name, "forks"]) => admin_gated(daemon, user_header, || {
            delete_project_fork(daemon, &url_decode(name), "")
        }),
        ("PATCH", ["api", "projects", name, "forks", user]) => {
            admin_gated(daemon, user_header, || {
                patch_project_fork(daemon, &url_decode(name), &url_decode(user), body)
            })
        }
        ("DELETE", ["api", "projects", name, "forks", user]) => {
            admin_gated(daemon, user_header, || {
                delete_project_fork(daemon, &url_decode(name), &url_decode(user))
            })
        }
        // Machine provider registry (RAL-185) -- RAL-332: admin-only, client
        // and server side. Nothing outside the Machines tab reads this.
        ("GET", ["api", "machines"]) => admin_gated(daemon, user_header, || list_machines(daemon)),
        ("POST", ["api", "machines"]) => {
            admin_gated(daemon, user_header, || register_machine(daemon, body))
        }
        ("GET", ["api", "machines", scheme]) => {
            admin_gated(daemon, user_header, || get_machine(daemon, scheme))
        }
        ("DELETE", ["api", "machines", scheme]) => {
            admin_gated(daemon, user_header, || deregister_machine(daemon, scheme))
        }
        ("POST", ["api", "machines", scheme, "check"]) => {
            admin_gated(daemon, user_header, || check_machine(daemon, scheme))
        }
        ("POST", ["api", "machines", "cleanup"]) => {
            admin_gated(daemon, user_header, || cleanup_machine(daemon, body))
        }
        // Target inventory health (RAL-355 Phase 9).
        ("GET", ["api", "machines", "targets", "health"]) => {
            admin_gated(daemon, user_header, || health_all_targets(daemon))
        }
        // Triage type registry (RAL-318) -- RAL-332: admin-only, client and
        // server side. Nothing outside the Triage tab reads this.
        ("GET", ["api", "triage", "types"]) => {
            admin_gated(daemon, user_header, || list_triage_types(daemon))
        }
        ("POST", ["api", "triage", "types"]) => {
            admin_gated(daemon, user_header, || register_triage_type(daemon, body))
        }
        ("GET", ["api", "triage", "types", name]) => {
            admin_gated(daemon, user_header, || get_triage_type(daemon, name))
        }
        ("DELETE", ["api", "triage", "types", name]) => {
            admin_gated(daemon, user_header, || deregister_triage_type(daemon, name))
        }
        ("GET", ["api", "triage", "pools"]) => {
            admin_gated(daemon, user_header, || list_triage_pools(daemon))
        }
        ("POST", ["api", "triage", "pools", "threshold"]) => {
            admin_gated(daemon, user_header, || {
                set_triage_pool_threshold(daemon, body)
            })
        }
        ("GET", ["api", "triage", "schedules"]) => {
            admin_gated(daemon, user_header, || list_triage_schedules(daemon, query))
        }
        ("POST", ["api", "triage", "schedules"]) => {
            admin_gated(daemon, user_header, || add_triage_schedule(daemon, body))
        }
        ("DELETE", ["api", "triage", "schedules", id]) => {
            admin_gated(daemon, user_header, || remove_triage_schedule(daemon, id))
        }
        ("GET", ["api", "triage", "candidates"]) => {
            admin_gated(daemon, user_header, || list_triage_candidates(daemon))
        }
        ("GET", ["api", "resources"]) => resources(daemon),
        ("GET", ["api", "health", "agent-profiles"]) => agent_profiles_health(daemon, query),
        ("GET", ["api", "health", "project-forks"]) => project_forks_health(daemon),
        ("POST", ["api", "health", "arbiter"]) => health_arbiter(daemon),
        ("GET", ["api", "agents"]) => list_agents(daemon, query, user_header),
        // RAL-297: cwd-independent agent+model catalog for the Simple task
        // form's agent picker -- see `crate::agent_catalog`.
        ("GET", ["api", "agents", "catalog"]) => agent_catalog_reply(),
        // RAL-332: the current caller's resolved identity and admin flag --
        // lets the board decide whether to show its admin-only tabs without
        // it ever needing to know its own claimed name (it deliberately
        // never sends X-Ralphus-User outside of a "visit as" override).
        ("GET", ["api", "whoami"]) => whoami(daemon, user_header),
        // Minimal user registry (RAL-?) -- see `crate::users`'s module doc
        // comment: this is a placeholder identity layer, not authentication.
        // TODO: Replace with user auth once RAL-252 is done.
        // RAL-332: admin-only, client and server side -- nothing outside the
        // Users tab reads this.
        ("GET", ["api", "users"]) => admin_gated(daemon, user_header, || list_users(daemon)),
        ("POST", ["api", "users"]) => {
            admin_gated(daemon, user_header, || create_user(daemon, body))
        }
        ("DELETE", ["api", "users", name]) => admin_gated(daemon, user_header, || {
            delete_user(daemon, &url_decode(name))
        }),
        ("POST", ["api", "users", name, "rename"]) => admin_gated(daemon, user_header, || {
            user_rename(daemon, &url_decode(name), body)
        }),
        // RAL-320: per-user notification preferences layered on `users`.
        ("GET", ["api", "users", name, "preferences"]) => {
            get_user_preferences(daemon, &url_decode(name))
        }
        ("POST", ["api", "users", name, "preferences"]) => {
            set_user_preferences_endpoint(daemon, &url_decode(name), body)
        }
        // RAL-332: promotes/demotes another user's admin flag. Bootstrap
        // exception in `set_user_admin_endpoint` itself: if no admin is
        // registered yet, the very first promotion is allowed through so the
        // system isn't permanently stuck with zero admins.
        ("POST", ["api", "users", name, "admin"]) => {
            set_user_admin_endpoint(daemon, user_header, &url_decode(name), body)
        }
        // RAL-332: "Edit Profile" -- an admin viewing another user's
        // Preferences page. Audit-only: does not itself read or write
        // anything, just records that it happened.
        ("POST", ["api", "users", name, "visit"]) => {
            visit_user_profile(daemon, user_header, &url_decode(name))
        }
        ("GET", ["api", "hidden"]) => list_hidden(daemon, user_header),
        ("POST", ["api", "hidden", "squads", "batch"]) => {
            set_squads_hidden_batch(daemon, user_header, body)
        }
        ("POST", ["api", "hidden", "squads", id]) => {
            set_squad_hidden(daemon, user_header, id, true)
        }
        ("DELETE", ["api", "hidden", "squads", id]) => {
            set_squad_hidden(daemon, user_header, id, false)
        }
        ("POST", ["api", "hidden", "reviews", id]) => {
            set_review_hidden(daemon, user_header, id, true)
        }
        ("DELETE", ["api", "hidden", "reviews", id]) => {
            set_review_hidden(daemon, user_header, id, false)
        }
        // RAL-281: user-editable list of env-var names treated as secret --
        // see `crate::secret_env_names`'s module doc comment. RAL-332:
        // admin-only, client and server side.
        ("GET", ["api", "secret-env-names"]) => {
            admin_gated(daemon, user_header, || list_secret_env_names(daemon))
        }
        ("POST", ["api", "secret-env-names"]) => {
            admin_gated(daemon, user_header, || add_secret_env_name(daemon, body))
        }
        ("POST", ["api", "secret-env-names", name, "rename"]) => {
            admin_gated(daemon, user_header, || {
                rename_secret_env_name(daemon, name, body)
            })
        }
        ("DELETE", ["api", "secret-env-names", name]) => {
            admin_gated(daemon, user_header, || delete_secret_env_name(daemon, name))
        }
        ("GET", ["api", "config", "live-view"]) => live_view_config_reply(),
        ("GET", ["api", "config", "templates"]) => templates_config_reply(),
        ("GET", ["api", "cartographer"]) => cartographer_query(daemon, query, user_header),
        ("GET", ["api", "cartographer", id]) => cartographer_get(daemon, id, user_header),
        ("POST", ["api", "ghosts", "copy"]) => ghost_copy(daemon, body),
        ("GET", ["api", "ghosts", owner_uri]) => ghost_get(daemon, owner_uri),
        ("POST", ["api", "mailbox", "register"]) => mailbox_register(daemon),
        // Monitor watches + the per-user mailbox view they filter.
        // These literal "personal"/"watches" segments must stay ahead of the
        // client_id-parameterized mailbox routes just below, or the
        // client_id arm would swallow "personal" as if it were a client id.
        ("GET", ["api", "mailbox", "personal", "messages"]) => {
            personal_mailbox_messages(daemon, query)
        }
        ("POST", ["api", "mailbox", "personal", "drain"]) => {
            personal_mailbox_drain(daemon, query, body)
        }
        ("GET", ["api", "watches"]) => list_watches_endpoint(daemon, query, user_header),
        ("GET", ["api", "watches", entity_uri]) => watchers_endpoint(daemon, entity_uri),
        ("POST", ["api", "watches"]) => create_watch_endpoint(daemon, query, user_header, body),
        ("DELETE", ["api", "watches", entity_uri]) => {
            delete_watch_endpoint(daemon, query, user_header, entity_uri)
        }
        ("GET", ["api", "mailbox", client_id, "messages"]) => {
            mailbox_messages(daemon, client_id, query)
        }
        ("POST", ["api", "mailbox", client_id, "drain"]) => mailbox_drain(daemon, client_id, body),
        ("POST", ["api", "squads", "validate"]) => validate_endpoint(daemon, body),
        ("POST", ["api", "squads"]) => submit(daemon, body, query),
        // RAL-297: Simple task form's opt-in "generation step" primitive.
        ("POST", ["api", "generate"]) => generate_start(daemon, body),
        ("GET", ["api", "generate", id]) => generate_status(daemon, id),
        ("POST", ["api", "clear"]) => clear_all(daemon, body),
        ("GET", ["api", "queue"]) => queue(daemon),
        ("POST", ["api", "queue", "reorder"]) => queue_reorder(daemon, body),
        ("POST", ["api", "queue", "set-position"]) => queue_set_position(daemon, body),
        ("GET", ["api", "graph"]) => global_graph(daemon, query),
        ("GET", ["api", "resolve"]) => resolve_uri_endpoint(daemon, query),
        ("GET", ["api", "squads", id]) => get_squad(daemon, id),
        ("GET", ["api", "squads", id, "worktrees"]) => squad_worktrees(daemon, id),
        ("GET", ["api", "squads", id, "logs"]) => squad_logs(daemon, id),
        ("GET", ["api", "squads", id, "timeline"]) => squad_timeline(daemon, id),
        ("GET", ["api", "squads", id, "graph"]) => squad_graph(daemon, id),
        ("POST", ["api", "squads", id, "activate"]) => activate(daemon, id),
        ("POST", ["api", "squads", id, "cancel", "preview"]) => cancel_squad_preview(daemon, id),
        ("POST", ["api", "squads", id, "cancel"]) => cancel(daemon, id),
        ("POST", ["api", "squads", id, "set-status"]) => set_status(daemon, id, body),
        ("POST", ["api", "squads", id, "edit"]) => edit_squad(daemon, id, body),
        ("POST", ["api", "squads", id, "retry"]) => retry_squad(daemon, id),
        ("POST", ["api", "squads", id, "restart", "preview"]) => restart_squad_preview(daemon, id),
        ("POST", ["api", "squads", id, "restart"]) => restart_squad(daemon, id, body),
        ("POST", ["api", "squads", id, "add-dependency"]) => add_dependency(daemon, id, body),
        ("POST", ["api", "squads", id, "cells", ti, si, "restart", "preview"]) => {
            restart_cell_preview(daemon, id, ti, si)
        }
        ("POST", ["api", "squads", id, "cells", ti, si, "restart"]) => {
            restart_cell(daemon, id, ti, si, body)
        }
        ("POST", ["api", "squads", id, "cells", ti, si, "proof", vi, "restart"]) => {
            restart_cell_proof(daemon, id, ti, si, vi, body)
        }
        ("POST", ["api", "squads", id, "tasks", ti, "proof", vi, "restart"]) => {
            restart_task_proof(daemon, id, ti, vi, body)
        }
        ("POST", ["api", "squads", id, "tasks", ti, "restart", "preview"]) => {
            restart_task_preview(daemon, id, ti)
        }
        ("POST", ["api", "squads", id, "tasks", ti, "restart"]) => {
            restart_task(daemon, id, ti, body)
        }
        ("POST", ["api", "squads", id, "env"]) => set_squad_env(daemon, id, body),
        // RAL-324: each `POST .../env` route below has a read-only `GET` twin
        // on the same path serving that surface's resolved environment with
        // registered secret values masked -- see `crate::env_view`.
        ("GET", ["api", "squads", id, "env"]) => squad_env_view(daemon, id),
        ("GET", ["api", "squads", id, "tasks", ti, "proof", vi, "env"]) => {
            task_proof_step_env_view(daemon, id, ti, vi)
        }
        ("GET", ["api", "squads", id, "tasks", ti, "proof", "env"]) => {
            task_proof_env_view(daemon, id, ti)
        }
        ("GET", ["api", "squads", id, "tasks", ti, "env"]) => task_env_view(daemon, id, ti),
        ("GET", ["api", "squads", id, "cells", ti, si, "proof", vi, "env"]) => {
            cell_proof_step_env_view(daemon, id, ti, si, vi)
        }
        ("GET", ["api", "squads", id, "cells", ti, si, "proof", "env"]) => {
            cell_proof_env_view(daemon, id, ti, si)
        }
        ("GET", ["api", "squads", id, "cells", ti, si, "env"]) => cell_env_view(daemon, id, ti, si),
        // RAL-191: the per-step routes must precede the scope-wide ones, since
        // `["proof", "env"]` and `["proof", vi, "env"]` are otherwise
        // ambiguous to a reader (they are not to the matcher, which is
        // length-sensitive -- but keeping them adjacent and ordered narrow-first
        // makes the layering obvious).
        ("POST", ["api", "squads", id, "tasks", ti, "proof", vi, "env"]) => {
            set_task_proof_step_env(daemon, id, ti, vi, body)
        }
        ("POST", ["api", "squads", id, "tasks", ti, "proof", "env"]) => {
            set_task_proof_env(daemon, id, ti, body)
        }
        ("POST", ["api", "squads", id, "tasks", ti, "env"]) => set_task_env(daemon, id, ti, body),
        ("POST", ["api", "squads", id, "cells", ti, si, "proof", vi, "env"]) => {
            set_cell_proof_step_env(daemon, id, ti, si, vi, body)
        }
        ("POST", ["api", "squads", id, "cells", ti, si, "proof", "env"]) => {
            set_cell_proof_env(daemon, id, ti, si, body)
        }
        ("POST", ["api", "squads", id, "cells", ti, si, "env"]) => {
            set_cell_env(daemon, id, ti, si, body)
        }
        ("POST", ["api", "squads", id, "tasks", ti, "solo"]) => solo_task(daemon, id, ti),
        ("POST", ["api", "squads", id, "tasks", ti, "unsolo"]) => unsolo_task(daemon, id, ti),
        ("POST", ["api", "squads", id, "cells", ti, si, "open-terminal"]) => {
            open_terminal(daemon, id, ti, si, query)
        }
        ("POST", ["api", "squads", id, "cells", ti, si, "terminal-ticket"]) => {
            mint_terminal_ticket_route(daemon, id, ti, si)
        }
        ("POST", ["api", "squads", id, "cells", ti, si, "resume-automation"]) => {
            resume_automation(daemon, id, ti, si)
        }
        ("GET", ["api", "squads", id, "cells", ti, si, "pane"]) => {
            cell_pane(daemon, id, ti, si, query)
        }
        ("GET", ["api", "squads", id, "cells", ti, si, "debug-events"]) => {
            cell_debug_events(daemon, id, ti, si)
        }
        (
            "GET",
            [
                "api",
                "squads",
                id,
                "cells",
                ti,
                si,
                "terminal-log-attempts",
            ],
        ) => cell_terminal_log_attempts(daemon, id, ti, si),
        (
            "GET",
            [
                "api",
                "squads",
                id,
                "cells",
                ti,
                si,
                "terminal-log-attempts",
                attempt,
            ],
        ) => cell_terminal_log_attempt(daemon, id, ti, si, attempt),
        (
            "POST",
            [
                "api",
                "squads",
                id,
                "proofs",
                task_idx,
                scope,
                cell_idx,
                proof_idx,
                "open-terminal",
            ],
        ) => open_proof_terminal(daemon, id, task_idx, scope, cell_idx, proof_idx, query),
        (
            "GET",
            [
                "api",
                "squads",
                id,
                "proofs",
                task_idx,
                scope,
                cell_idx,
                proof_idx,
                "pane",
            ],
        ) => proof_pane(daemon, id, task_idx, scope, cell_idx, proof_idx, query),
        (
            "GET",
            [
                "api",
                "squads",
                id,
                "proofs",
                task_idx,
                scope,
                cell_idx,
                proof_idx,
                "debug-events",
            ],
        ) => proof_debug_events(daemon, id, task_idx, scope, cell_idx, proof_idx),
        (
            "GET",
            [
                "api",
                "squads",
                id,
                "proofs",
                task_idx,
                scope,
                cell_idx,
                proof_idx,
                "terminal-log-attempts",
            ],
        ) => proof_terminal_log_attempts(daemon, id, task_idx, scope, cell_idx, proof_idx),
        (
            "GET",
            [
                "api",
                "squads",
                id,
                "proofs",
                task_idx,
                scope,
                cell_idx,
                proof_idx,
                "terminal-log-attempts",
                attempt,
            ],
        ) => proof_terminal_log_attempt(daemon, id, task_idx, scope, cell_idx, proof_idx, attempt),
        ("DELETE", ["api", "squads", id]) => delete_squad(daemon, id),
        ("GET", ["api", "guardians"]) => guardian_list(daemon),
        ("POST", ["api", "guardians"]) => guardian_create(daemon, user_header, body),
        ("GET", ["api", "guardians", id]) => guardian_get(daemon, id),
        ("GET", ["api", "guardians", id, "logs"]) => guardian_logs(daemon, id),
        ("POST", ["api", "guardians", id, "rename"]) => guardian_rename(daemon, id, body),
        ("POST", ["api", "guardians", id, "settings"]) => guardian_settings(daemon, id, body),
        ("POST", ["api", "guardians", id, "details"]) => guardian_details(daemon, id, body),
        ("POST", ["api", "guardians", id, "squash"]) => guardian_squash(daemon, id, body),
        ("DELETE", ["api", "guardians", id]) => guardian_delete(daemon, id),
        ("POST", ["api", "guardians", id, "branches"]) => guardian_add_branch(daemon, id, body),
        ("POST", ["api", "guardians", id, "branches", "reorder"]) => {
            guardian_reorder(daemon, id, body)
        }
        ("POST", ["api", "guardians", id, "branches", "arrange"]) => {
            guardian_arrange(daemon, id, body)
        }
        ("POST", ["api", "guardians", id, "sync-pr"]) => guardian_sync_pr(daemon, id),
        ("POST", ["api", "guardians", id, "branches", branch_id, "feedback"]) => {
            guardian_feedback(daemon, user_header, id, branch_id, body)
        }
        ("GET", ["api", "guardians", id, "branches", branch_id, "messages"]) => {
            guardian_branch_messages(daemon, id, branch_id)
        }
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
        ("GET", ["api", "guardians", id, "branches", branch_id, "env"]) => {
            review_worktree_env_view(daemon, id, branch_id)
        }
        ("GET", ["api", "guardians", id, "build-env"]) => {
            review_step_env_view(daemon, id, crate::env_view::ReviewStep::Build)
        }
        // RAL-324: the check gates ("Tests") get their own viewer entry point
        // even though they share the build step's stored override layer --
        // there is deliberately no `POST .../tests-env` twin to edit.
        ("GET", ["api", "guardians", id, "tests-env"]) => {
            review_step_env_view(daemon, id, crate::env_view::ReviewStep::Tests)
        }
        ("GET", ["api", "guardians", id, "manual-checks-env"]) => {
            review_step_env_view(daemon, id, crate::env_view::ReviewStep::ManualChecks)
        }
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
                "debug-events",
            ],
        ) => guardian_branch_debug_events(daemon, id, branch_id),
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
        ("GET", ["api", "guardians", id, "manual-checks", "debug-events"]) => {
            guardian_manual_checks_debug_events(daemon, id)
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
        ("POST", ["api", "guardians", id, "stop"]) => guardian_stop(daemon, id),
        ("POST", ["api", "guardians", id, "cancel_and_merge"]) => {
            guardian_cancel_and_merge(daemon, id)
        }
        ("POST", ["api", "guardians", id, "approve"]) => guardian_approve(daemon, id),
        ("POST", ["api", "guardians", id, "cancel"]) => guardian_cancel(daemon, id),
        ("POST", ["api", "guardians", id, "reopen"]) => guardian_reopen(daemon, id),
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
            guardian_submit_prs(daemon, id, body, user_header)
        }
        ("GET", ["api", "guardians", id, "pull-requests"]) => guardian_list_prs(daemon, id),
        ("POST", ["api", "guardians", id, "pull-requests", "unlink"]) => {
            guardian_unlink_prs(daemon, id)
        }
        ("GET", ["api", "guardians", id, "pull-request-stacks"]) => {
            guardian_list_pr_stacks(daemon, id)
        }
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
/// /api/squads` that creates a squad persists this request's trace context onto
/// the new squad (via [`Store::set_squad_trace_context`]) so the scheduler's
/// later, asynchronous execution of that squad continues the *same* trace
/// instead of starting a disconnected one.
pub fn route_with_trace(
    daemon: &Daemon,
    method: &str,
    path: &str,
    body: &str,
    traceparent: Option<&str>,
) -> Reply {
    route_with_trace_for_user(daemon, method, path, body, traceparent, None)
}

fn route_with_trace_for_user(
    daemon: &Daemon,
    method: &str,
    path: &str,
    body: &str,
    traceparent: Option<&str>,
    user_header: Option<&str>,
) -> Reply {
    let cx = crate::otel::context_from_traceparent(traceparent);
    let span = crate::otel::start_span("daemon.http", &cx, SpanKind::Server);
    span.set_attribute("http.method", method.to_string());
    span.set_attribute("http.target", path.to_string());

    let reply = route_for_user(daemon, method, path, body, user_header);

    span.set_attribute("http.status_code", i64::from(reply.status));
    if reply.status >= 400 {
        span.set_status(Status::error(format!("http {}", reply.status)));
    } else {
        span.set_status(Status::Ok);
    }

    let path_only = path.split('?').next().unwrap_or(path);
    if method == "POST" && path_only.trim_matches('/') == "api/squads" && reply.status == 201 {
        if let (Some(squad_id), Some(tp)) = (
            extract_squad_id(&reply.body),
            crate::otel::traceparent_from_context(&span.cx),
        ) {
            let _ = daemon.lock().set_squad_trace_context(&squad_id, &tp);
        }
    }

    reply
}

/// Pull `"squad_id"` out of a [`SubmitResponse`] JSON body without a full typed
/// deserialize — the caller only wants the one field.
fn extract_squad_id(body: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()?
        .get("squad_id")?
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

#[derive(Serialize)]
struct EventsTicketResponse {
    ticket: String,
}

/// `POST /api/events/ticket` handler (RAL-222) — see the route dispatch
/// comment and `crate::token`'s module doc comment for the full picture.
fn mint_events_ticket(daemon: &Daemon) -> Reply {
    json(
        200,
        &EventsTicketResponse {
            ticket: daemon.mint_events_ticket(),
        },
    )
}

/// Filter and sort a squad list for `GET /api/tasks` (CLI_PARITY_PLAN.local.md Q1:
/// server-side so the board and CLI share one filter/sort implementation).
///
/// - `status`: comma-separated, case-insensitive match against `SquadView::state`.
/// - `name`: comma-separated list of case-insensitive substrings matched
///   against `SquadView::label`; a squad matches if ANY needle is a substring
///   of its label (union), and a squad with no label never matches a
///   non-empty filter. Whitespace around each comma-separated needle is
///   trimmed.
/// - `sort`: `"name"` sorts by label (falling back to id) case-insensitively,
///   ascending; anything else (including absent) keeps `list_squads`'s existing
///   newest-first order.
fn filter_and_sort_squads(
    mut squads: Vec<crate::store::SquadView>,
    status: Option<&str>,
    name: Option<&str>,
    sort: Option<&str>,
) -> Vec<crate::store::SquadView> {
    if let Some(status) = status {
        let wanted: Vec<String> = status
            .split(',')
            .map(|s| s.trim().to_lowercase())
            .filter(|s| !s.is_empty())
            .collect();
        if !wanted.is_empty() {
            squads.retain(|r| wanted.iter().any(|w| w == &r.state.to_lowercase()));
        }
    }
    if let Some(name) = name {
        let needles: Vec<String> = name
            .split(',')
            .map(|s| s.trim().to_lowercase())
            .filter(|s| !s.is_empty())
            .collect();
        if !needles.is_empty() {
            squads.retain(|r| {
                let label = r.label.as_deref().unwrap_or("").to_lowercase();
                needles.iter().any(|n| label.contains(n.as_str()))
            });
        }
    }
    if sort == Some("name") {
        squads.sort_by(|a, b| {
            let ak = a.label.as_deref().unwrap_or(&a.id).to_lowercase();
            let bk = b.label.as_deref().unwrap_or(&b.id).to_lowercase();
            ak.cmp(&bk)
        });
    }
    squads
}

/// `?status=queued,running&name=foo&sort=name` — see [`filter_and_sort_squads`].
fn board(daemon: &Daemon, query: &str) -> Reply {
    let store = daemon.lock();
    // Ground truth is the shared concurrency semaphore, not a DB row count:
    // a permit is held for a cell/proof/review-merge's entire time in
    // flight, which outlasts the windows where any single row actually reads
    // `running` (see `Semaphore::in_use`) — counting DB rows undercounts.
    let running = daemon.sem.in_use();
    let running_reviews: Vec<RunningReviewItem> = store
        .merging_guardians()
        .unwrap_or_default()
        .into_iter()
        .map(|(id, name)| RunningReviewItem { id, name })
        .collect();
    match store.list_squads() {
        Ok(squads) => {
            let status = query_filter(query, "status");
            let name = query_filter(query, "name");
            let sort = query_filter(query, "sort");
            let squads =
                filter_and_sort_squads(squads, status.as_deref(), name.as_deref(), sort.as_deref());
            json(
                200,
                &Board {
                    daemon: DaemonStatus {
                        running,
                        max_concurrent: daemon.max_concurrent,
                        running_reviews,
                        downtime_active: crate::config::scheduler_in_downtime(),
                    },
                    squads,
                },
            )
        }
        Err(e) => store_error(&e),
    }
}

/// Per-task resource usage for every running cell with a live subprocess
/// (RAL-11). Snapshots the running set + PIDs under the lock, then samples
/// CPU/RAM/GPU with the lock released (sampling briefly sleeps to measure CPU).
fn resources(daemon: &Daemon) -> Reply {
    let squads = {
        let store = daemon.lock();
        match store.list_squads() {
            Ok(squads) => squads,
            Err(e) => return store_error(&e),
        }
    };
    let procs = daemon.procs_handle();
    let rows = crate::resources::build(&squads, &procs);
    json(200, &ResourcesResponse { resources: rows })
}

fn validate_endpoint(daemon: &Daemon, body: &str) -> Reply {
    let Ok(req) = serde_json::from_str::<ValidateBody>(body) else {
        return error(
            400,
            "bad_request",
            "body must be {\"toml\": \"...\"}",
            vec![],
        );
    };
    let mut report = validate_toml(&req.toml);
    if report.is_ok() {
        if let Ok(file) = toml::from_str::<ralphus_core::schema::TaskFile>(&req.toml) {
            report
                .errors
                .extend(crate::agent_profiles::validate_task_file_profiles(
                    &daemon.lock(),
                    &req.toml,
                    &file,
                ));
        }
    }
    json(
        200,
        &ValidateResponse {
            valid: report.is_ok(),
            errors: &report.errors,
            warnings: &report.warnings,
        },
    )
}

/// For every task with at least one placeholder-`cwd` cell, check that its
/// `project` field resolves against the project registry (RAL-100). Returns
/// the first failure's message; core's structural `validate_toml` already
/// guarantees `project` is set whenever a placeholder is used, so this only
/// needs to check registry membership (the one piece core can't see).
fn validate_projects_registered(
    store: &Store,
    file: &ralphus_core::schema::TaskFile,
) -> std::result::Result<(), String> {
    for task in &file.task {
        let needs_project = task.cell.iter().any(|s| {
            s.cwd
                .as_deref()
                .and_then(ralphus_core::schema::first_worktree_placeholder_in_text)
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
/// A review normally *infers* each contributing branch and its upstream by
/// running the VCS against the cell's worktree. That read only works on the
/// machine holding it, so a review fed by a remote cell must instead be told:
///
/// - the branch, via a `ralphus:new-worktree/<branch>` cwd, and
/// - the upstream, via `[[review]] upstream`.
///
/// Checked here rather than inside `crate::reviews::derive_reviews` so it fails
/// before the provider is asked to provision a workspace — a missing
/// `upstream` is a static authoring mistake, and there is no reason to spend a
/// remote checkout discovering it. `derive_reviews` keeps its own equivalent
/// errors as a backstop for paths that don't come through submit.
fn validate_remote_reviews_are_declarative(
    store: &Store,
    file: &ralphus_core::schema::TaskFile,
) -> std::result::Result<(), String> {
    for task in &file.task {
        for (idx, cell) in task.cell.iter().enumerate() {
            let Some(review_id) = cell
                .review
                .as_deref()
                .and_then(ralphus_core::schema::parse_cell_review_sentinel)
            else {
                continue;
            };
            let machine = ralphus_core::schema::resolve_cell_machine(task, cell);
            if store
                .resolve_machine(machine.as_deref())
                .is_ok_and(|m| m.is_local())
            {
                continue;
            }
            let sid = cell.id.clone().unwrap_or_else(|| format!("cell-{idx}"));
            let machine_label = machine.as_deref().unwrap_or("local");
            if cell
                .cwd
                .as_deref()
                .and_then(ralphus_core::schema::first_worktree_placeholder_in_text)
                .is_none()
            {
                return Err(format!(
                    "cell \"{sid}\" runs on machine \"{machine_label}\" and opts into review                      \"{review_id}\", so its branch must be knowable without reading that                      machine's filesystem. Give it a cwd of the form                      \"ralphus:new-worktree/<branch>\" instead of a literal path."
                ));
            }
            let declared = file
                .review
                .iter()
                .find(|r| r.id.as_deref() == Some(review_id))
                .and_then(|r| r.upstream.as_deref())
                .map(str::trim)
                .filter(|b| !b.is_empty());
            if declared.is_none() {
                return Err(format!(
                    "review \"{review_id}\" is fed by cell \"{sid}\" running on machine                      \"{machine_label}\", so its upstream branch cannot be read from that worktree's                      git upstream — this daemon cannot see another machine's filesystem. Declare it                      explicitly: [[review]] upstream = \"main\"."
                ));
            }
        }
    }
    Ok(())
}

/// Enforce the within-task machine affinity rule (RAL-185 Phase 3a).
///
/// Every cell under one task — and every proof step under those cells,
/// plus the task's own proof steps — must resolve to the **same** machine.
/// A task is one unit of work sharing one workspace: cells hand off through
/// files on disk, and a task-scope proof runs against whatever its cells
/// produced. Splitting that across machines would silently verify an empty or
/// stale directory.
///
/// Different *tasks* may freely use different machines, and a review's machine
/// is independent of all of them — that is what makes a fan-out across a build
/// farm possible in the first place.
///
/// Checked at submit rather than discovered at run time: by the point the
/// scheduler noticed, the first cell would already have run somewhere.
fn validate_machine_affinity(
    file: &ralphus_core::schema::TaskFile,
) -> std::result::Result<(), String> {
    for task in &file.task {
        // The task's own value is the baseline every member must agree with;
        // when it is unset, the first cell that names one sets the
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
                     runs on \"{m}\". Every cell and proof step under one task must resolve to \
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

        for (idx, cell) in task.cell.iter().enumerate() {
            let sid = cell.id.clone().unwrap_or_else(|| format!("cell-{idx}"));
            check(
                ralphus_core::schema::resolve_cell_machine(task, cell),
                format!("cell \"{sid}\""),
            )?;
            for (vi, proof) in cell.proof.iter().enumerate() {
                check(
                    ralphus_core::schema::resolve_cell_proof_machine(task, cell, proof),
                    format!("cell \"{sid}\" proof step #{vi}"),
                )?;
            }
        }
        for (vi, proof) in task.proof.iter().enumerate() {
            check(
                ralphus_core::schema::resolve_task_proof_machine(task, proof),
                format!("task proof step #{vi}"),
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
        for proof in &task.proof {
            check(
                proof.machine.as_deref(),
                &format!("task \"{}\" proof step", task.name),
            )?;
        }
        for cell in &task.cell {
            let sid = cell.id.as_deref().unwrap_or("<unnamed>");
            check(
                cell.machine.as_deref(),
                &format!("task \"{}\" cell \"{sid}\"", task.name),
            )?;
            for proof in &cell.proof {
                check(
                    proof.machine.as_deref(),
                    &format!("task \"{}\" cell \"{sid}\" proof step", task.name),
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
    let clone_url = req
        .clone_url
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    if req.clone_url.is_some() && clone_url.is_none() {
        return error(
            400,
            "invalid_value",
            "'clone_url'/'url' must not be empty when provided",
            vec![],
        );
    }
    if clone_url.is_some_and(|value| value.contains(['\r', '\n', '\0'])) {
        return error(
            400,
            "invalid_value",
            "'clone_url'/'url' must not contain control characters",
            vec![],
        );
    }
    if req.clear_clone_url && req.clone_url.is_some() {
        return error(
            400,
            "invalid_value",
            "'clear_clone_url' cannot be combined with a 'clone_url'/'url' value",
            vec![],
        );
    }
    // RAL-355: a password embedded in an `http(s)://` clone URL is not
    // rejected -- some legacy remotes genuinely need it -- but it is stored
    // exactly as given, so warn the caller once, at registration time, so
    // they can switch to an SSH key or a credential helper instead. Only the
    // redacted form of the URL is ever named, both here and in the daemon's
    // own log, so registering the warning never itself becomes a leak path.
    let credential_warning = clone_url.filter(|url| ralphus_core::redact::https_url_has_embedded_password(url)).map(|url| {
        let redacted = ralphus_core::redact::redact_url_credentials(url);
        crate::rlog!(
            WARNING,
            "ralphus [store] project {:?} registered with a clone URL that embeds inline credentials ({redacted}); consider an SSH key or credential helper instead",
            req.name
        );
        format!(
            "clone URL embeds inline credentials ({redacted}); consider an SSH key or credential helper instead"
        )
    });
    let guard = daemon.lock();
    match guard.register_project_with_clone_url_ex(
        &req.name,
        &req.description,
        &req.path,
        &req.vcs,
        clone_url,
        req.match_pr_branch_name,
    ) {
        Ok(()) => {
            if req.clear_clone_url {
                if let Err(e) = guard.clear_project_clone_url(&req.name) {
                    return store_error(&e);
                }
            }
            let mut body = serde_json::json!({"name": req.name});
            if let Some(warning) = credential_warning {
                body["warnings"] = serde_json::json!([warning]);
            }
            json(201, &body)
        }
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
    // A probe belongs to no squad; the spec exists only to satisfy the invocation
    // shape (env overrides, ids for the provider's own logging).
    let spec = crate::runner::RunnerSpec::for_command_proof(
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
    /// Full `<scheme>:<uri>` value naming the machine to clean up on.
    machine: String,
    /// Registered project name. A machine can hold many projects' worth of
    /// durable clones (RAL-355 Phase 4), so `cleanup` needs to know which
    /// one -- there is no longer a single "the" workspace per machine.
    project: String,
    /// Worktree branch to remove. Omitted: remove the whole project
    /// directory (repository plus every worktree).
    #[serde(default)]
    branch: Option<String>,
}

/// `POST /api/machines/cleanup`: tear down one provisioned workspace
/// (RAL-201, reshaped by RAL-355 Phase 2 remainder). Body
/// `{"machine": "<scheme>:<uri>", "project": "<name>", "branch": "<optional>"}`.
///
/// **Never invoked automatically** — see [`crate::remote_runner::ProviderRunner::cleanup`]'s
/// doc for the retention-on-failure policy this mirrors. This is the explicit,
/// operator-initiated action an admin takes once they are actually done with a
/// workspace, the same way nobody automatically deletes a local
/// `.git/.ralphus_worktrees/<branch>` directory either.
///
/// `project`/`branch` exist because Phase 4 made one machine hold many
/// projects' worth of durable clones, each with many worktrees — the
/// original `{"machine": "..."}`-only body could only ever have meant "the
/// one workspace this machine has", which stopped being a coherent request
/// the moment that became untrue.
fn cleanup_machine(daemon: &Daemon, body: &str) -> Reply {
    let Ok(req) = serde_json::from_str::<CleanupMachineBody>(body) else {
        return error(
            400,
            "bad_request",
            "body must be {\"machine\": \"...\", \"project\": \"...\", \"branch\"?: \"...\"}",
            vec![],
        );
    };
    let machine = req.machine.trim();
    let project_name = req.project.trim();
    if project_name.is_empty() {
        return error(400, "bad_request", "\"project\" must not be empty", vec![]);
    }
    let (provider, clone_url, remote_root) = {
        let store = daemon.lock();
        let provider = match crate::remote_runner::provider_from_store(&store, machine) {
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
        };
        let project = match store.get_project(project_name) {
            Ok(Some(p)) => p,
            Ok(None) => {
                return error(
                    404,
                    "not_found",
                    &format!("no registered project named {project_name:?}"),
                    vec![],
                );
            }
            Err(e) => return error(500, "internal_error", &e.to_string(), vec![]),
        };
        let Some(clone_url) = project.clone_url.filter(|u| !u.trim().is_empty()) else {
            return error(
                400,
                "invalid_value",
                &format!(
                    "project {project_name:?} has no registered clone URL -- nothing could \
                     have been provisioned remotely for it"
                ),
                vec![],
            );
        };
        let remote_root = match crate::machine_targets::load_machine_targets() {
            Ok(targets) => crate::machine_targets::find_by_machine(&targets, machine)
                .map(|t| t.remote_root.clone()),
            Err(e) => return error(500, "internal_error", &e, vec![]),
        };
        (provider, clone_url, remote_root)
    };
    let cleanup_req = crate::remote_runner::CleanupRequest {
        project: project_name.to_string(),
        clone_url,
        branch: req.branch.clone().filter(|b| !b.trim().is_empty()),
        remote_root,
    };
    let spec = crate::runner::RunnerSpec::for_command_proof(
        "machine-cleanup",
        "machine-cleanup",
        machine,
        ".",
        "",
        "claude",
        Some(60),
    );
    let result = provider.cleanup(&cleanup_req, &spec);
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
        squad_id: None,
        guardian_id: None,
        cell_id: None,
        task: None,
        log_path: None,
        payload: serde_json::json!({
            "machine": machine,
            "project": project_name,
            "branch": req.branch,
            "ok": result.is_ok(),
            "removed": result.as_ref().ok().and_then(|r| r.clone()),
            "error": result.as_ref().err(),
        }),
        admin_only: false,
    });
    match result {
        Ok(removed) => json(200, &serde_json::json!({"ok": true, "removed": removed})),
        // RAL-201: the workspace is left exactly as it was on failure -- there
        // is no daemon-side record of it to roll back or discard (`provision`
        // re-derives the same workspace deterministically every time), so the
        // only responsibility here is surfacing the real reason rather than
        // swallowing it.
        Err(e) => error(502, "provider_error", &e, vec![]),
    }
}

/// `GET /api/machines/targets/health`: check every configured
/// `[machine.targets.*]` entry (RAL-355 Phase 9). Always live -- computed on
/// demand, never cached/polled, matching the plan's "checks are
/// user-triggered, not polled every board refresh" requirement by simply
/// never storing a result to poll in the first place.
fn health_all_targets(daemon: &Daemon) -> Reply {
    match crate::health_targets::check_all_targets(&daemon.store_handle()) {
        Ok(reports) => {
            let any_fail = reports
                .iter()
                .any(crate::health_targets::TargetHealthReport::any_fail);
            json(
                200,
                &serde_json::json!({"ok": true, "any_fail": any_fail, "targets": reports}),
            )
        }
        Err(e) => error(500, "internal_error", &e, vec![]),
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

/// `POST /api/triage/types` body (RAL-318). Mirrors [`RegisterMachineBody`]'s
/// shape/rationale: registering a Triage type is an explicit administrative
/// action, not declarable inside a submitted task file.
#[derive(Deserialize)]
struct RegisterTriageTypeBody {
    name: String,
    #[serde(default)]
    label: String,
    #[serde(default)]
    description: String,
}

#[derive(Serialize)]
struct TriageTypesResponse {
    types: Vec<crate::triage::TriageTypeView>,
}

/// `POST /api/triage/types`: register (or update) a Triage type (RAL-318).
fn register_triage_type(daemon: &Daemon, body: &str) -> Reply {
    let Ok(req) = serde_json::from_str::<RegisterTriageTypeBody>(body) else {
        return error(
            400,
            "bad_request",
            "body must include a \"name\" string",
            vec![],
        );
    };
    let name = req.name.trim();
    if name.is_empty() {
        return error(400, "invalid_value", "'name' must not be empty", vec![]);
    }
    match daemon
        .lock()
        .register_triage_type(name, &req.label, &req.description)
    {
        Ok(()) => json(201, &serde_json::json!({"name": name})),
        Err(e) => store_error(&e),
    }
}

/// `GET /api/triage/types`: every registered Triage type (including the
/// built-in `unclassified`).
fn list_triage_types(daemon: &Daemon) -> Reply {
    match daemon.lock().list_triage_types() {
        Ok(types) => json(200, &TriageTypesResponse { types }),
        Err(e) => store_error(&e),
    }
}

/// `GET /api/triage/types/{name}`.
fn get_triage_type(daemon: &Daemon, name: &str) -> Reply {
    match daemon.lock().get_triage_type(name) {
        Ok(Some(t)) => json(200, &t),
        Ok(None) => error(
            404,
            "not_found",
            &format!("triage type \"{name}\" is not registered"),
            vec![],
        ),
        Err(e) => store_error(&e),
    }
}

/// `DELETE /api/triage/types/{name}`. The built-in `unclassified` type can
/// never be deregistered (RAL-318).
fn deregister_triage_type(daemon: &Daemon, name: &str) -> Reply {
    match daemon.lock().deregister_triage_type(name) {
        Ok(crate::triage::DeregisterOutcome::Removed) => {
            json(200, &serde_json::json!({"deleted": true}))
        }
        Ok(crate::triage::DeregisterOutcome::NotFound) => error(
            404,
            "not_found",
            &format!("triage type \"{name}\" is not registered"),
            vec![],
        ),
        Ok(crate::triage::DeregisterOutcome::BuiltIn) => error(
            400,
            "invalid_value",
            &format!(
                "\"{}\" is the built-in fallback Triage type and can never be deregistered",
                crate::triage::UNCLASSIFIED_TYPE
            ),
            vec![],
        ),
        Err(e) => store_error(&e),
    }
}

/// `POST /api/health/arbiter`: a live completion round-trip against the
/// configured Arbiter agent/model (RAL-318), backing `ralphus check health`.
/// User-triggered only -- never polled -- so no caching/rate-limiting of its
/// own cost is needed; it still respects the Arbiter's `maximum_budget_usd`
/// cap like any other Arbiter call.
fn health_arbiter(daemon: &Daemon) -> Reply {
    let arbiter = crate::arbiter::Arbiter::current();
    match crate::arbiter::health_check(&daemon.lock(), &arbiter) {
        Ok(reply) => json(
            200,
            &serde_json::json!({"status": "pass", "agent": arbiter.agent, "model": arbiter.model, "reply": reply}),
        ),
        Err(e) => json(
            200,
            &serde_json::json!({"status": "fail", "agent": arbiter.agent, "model": arbiter.model, "detail": e}),
        ),
    }
}

/// One `(project, triage_type)` pool's current state -- the board's Triage
/// tab "current pool state" view (RAL-318).
#[derive(Serialize)]
struct TriagePoolView {
    project: String,
    triage_type: String,
    count: i64,
    threshold: Option<i64>,
}

#[derive(Serialize)]
struct TriagePoolsResponse {
    pools: Vec<TriagePoolView>,
}

/// `GET /api/triage/pools`: every `(project, triage_type)` key that either
/// has at least one pooled cell or a configured count threshold (RAL-318) --
/// the union lets a threshold set ahead of the first pooled cell (e.g. "fire
/// every 4 bug fixes" configured before any bug fix has landed) show up and
/// stay editable immediately, the same way a cron schedule already does.
fn list_triage_pools(daemon: &Daemon) -> Reply {
    let store = daemon.lock();
    let mut keys = match store.triage_pool_keys() {
        Ok(k) => k,
        Err(e) => return store_error(&e),
    };
    let threshold_keys = match store.triage_threshold_keys() {
        Ok(k) => k,
        Err(e) => return store_error(&e),
    };
    for key in threshold_keys {
        if !keys.contains(&key) {
            keys.push(key);
        }
    }
    keys.sort();
    let pools = keys
        .into_iter()
        .map(|(project, triage_type)| {
            let count = store.triage_pool_count(&project, &triage_type).unwrap_or(0);
            let threshold = store
                .get_triage_pool_threshold(&project, &triage_type)
                .unwrap_or(None);
            TriagePoolView {
                project,
                triage_type,
                count,
                threshold,
            }
        })
        .collect();
    json(200, &TriagePoolsResponse { pools })
}

/// `POST /api/triage/pools/threshold` body (RAL-318). `threshold: null`
/// clears a previously configured threshold.
#[derive(Deserialize)]
struct SetTriagePoolThresholdBody {
    project: String,
    triage_type: String,
    #[serde(default)]
    threshold: Option<i64>,
}

/// `POST /api/triage/pools/threshold`: set (or clear) a pool's count
/// threshold.
fn set_triage_pool_threshold(daemon: &Daemon, body: &str) -> Reply {
    let Ok(req) = serde_json::from_str::<SetTriagePoolThresholdBody>(body) else {
        return error(
            400,
            "bad_request",
            "body must include \"project\" and \"triage_type\" strings",
            vec![],
        );
    };
    if let Some(t) = req.threshold {
        if t < 1 {
            return error(
                400,
                "invalid_value",
                "'threshold' must be at least 1",
                vec![],
            );
        }
    }
    let store = daemon.lock();
    let project = crate::triage::resolve_pool_key_input(&store, &req.project);
    match store.set_triage_pool_threshold(&project, &req.triage_type, req.threshold) {
        Ok(()) => json(200, &serde_json::json!({"ok": true})),
        Err(e) => store_error(&e),
    }
}

/// `GET /api/triage/schedules[?project=...&triage_type=...]`: every
/// configured cron schedule entry, optionally filtered to one pool key
/// (RAL-318).
#[derive(Serialize)]
struct TriageSchedulesResponse {
    schedules: Vec<crate::triage::TriageScheduleRow>,
}

fn list_triage_schedules(daemon: &Daemon, query: &str) -> Reply {
    let store = daemon.lock();
    let resolved_project =
        query_param(query, "project").map(|p| crate::triage::resolve_pool_key_input(&store, p));
    let filter = match (&resolved_project, query_param(query, "triage_type")) {
        (Some(p), Some(t)) => Some((p.as_str(), t)),
        _ => None,
    };
    match store.list_triage_schedules(filter) {
        Ok(schedules) => json(200, &TriageSchedulesResponse { schedules }),
        Err(e) => store_error(&e),
    }
}

/// `POST /api/triage/schedules` body (RAL-318). `anchor_date_ms` establishes
/// interval parity together with `every_n` -- see `crate::triage`'s module
/// doc comment.
#[derive(Deserialize)]
struct AddTriageScheduleBody {
    project: String,
    triage_type: String,
    cron_expr: String,
    anchor_date_ms: i64,
    #[serde(default = "default_every_n")]
    every_n: i64,
}

fn default_every_n() -> i64 {
    1
}

/// `POST /api/triage/schedules`: register a new cron schedule entry for a
/// `(project, triage_type)` pool.
fn add_triage_schedule(daemon: &Daemon, body: &str) -> Reply {
    let Ok(req) = serde_json::from_str::<AddTriageScheduleBody>(body) else {
        return error(
            400,
            "bad_request",
            "body must include \"project\", \"triage_type\", \"cron_expr\", and \"anchor_date_ms\"",
            vec![],
        );
    };
    let store = daemon.lock();
    let project = crate::triage::resolve_pool_key_input(&store, &req.project);
    match store.add_triage_schedule(
        &project,
        &req.triage_type,
        &req.cron_expr,
        req.anchor_date_ms,
        req.every_n,
    ) {
        Ok(id) => json(201, &serde_json::json!({"id": id})),
        Err(e) => error(400, "invalid_value", &e, vec![]),
    }
}

/// `DELETE /api/triage/schedules/{id}`.
fn remove_triage_schedule(daemon: &Daemon, id: &str) -> Reply {
    let Ok(id) = id.parse::<i64>() else {
        return error(
            400,
            "invalid_value",
            "schedule id must be an integer",
            vec![],
        );
    };
    match daemon.lock().remove_triage_schedule(id) {
        Ok(true) => json(200, &serde_json::json!({"deleted": true})),
        Ok(false) => error(404, "not_found", "no such schedule", vec![]),
        Err(e) => store_error(&e),
    }
}

#[derive(Serialize)]
struct TriageCandidatesResponse {
    candidates: Vec<crate::triage::TriageCandidateView>,
}

/// `GET /api/triage/candidates`: every cell across every squad that has
/// opted into Triage and has not yet been linked to an actual review -- the
/// board's Triage tab candidate list. A cell appears here from the moment
/// its Triage type(s) resolve at submit time, whether or not it has run
/// yet, and drops off once its pool drains into a review.
fn list_triage_candidates(daemon: &Daemon) -> Reply {
    match daemon.lock().triage_candidates() {
        Ok(candidates) => json(200, &TriageCandidatesResponse { candidates }),
        Err(e) => store_error(&e),
    }
}

fn list_projects(daemon: &Daemon) -> Reply {
    match daemon.lock().list_projects() {
        Ok(projects) => json(200, &ProjectsResponse { projects }),
        Err(e) => store_error(&e),
    }
}

/// Health-checks agent profiles' `from_env` vars and `executable` resolution
/// against the daemon's *own* process environment/PATH -- the check `ralphus
/// check health` used to run client-side in the CLI process, which can
/// silently diverge from whatever environment `ralphus-daemon serve` was
/// actually started in.
fn agent_profiles_health(daemon: &Daemon, query: &str) -> Reply {
    let Some(cwd) = query_param(query, "cwd").map(url_decode) else {
        return error(
            400,
            "bad_request",
            "cwd query parameter is required",
            vec![],
        );
    };
    let store = daemon.lock();
    let profiles = crate::agent_profiles::check_profiles_health(&store, Path::new(&cwd));
    json(200, &AgentProfilesHealthResponse { profiles })
}

/// Health-checks every registered fork row (RAL-338): missing local git
/// remotes, unreachable forks, unregistered users, and forge relationship/
/// instance problems -- daemon-side for the same reason
/// [`agent_profiles_health`] is: this needs the daemon process's own git/
/// forge-token environment, which can diverge from the CLI's. Advisory only
/// (see [`crate::project_forks::check_fork_health`]'s doc comment);
/// submission's own pre-flight remains authoritative.
fn project_forks_health(daemon: &Daemon) -> Reply {
    let store = daemon.lock();
    let forks = match store.list_project_forks() {
        Ok(forks) => forks,
        Err(e) => return store_error(&e),
    };
    let checks: Vec<crate::project_forks::ForkHealthCheck> = forks
        .iter()
        .flat_map(|fork| crate::project_forks::check_fork_health(&store, fork))
        .collect();
    json(200, &serde_json::json!({ "checks": checks }))
}

/// Resolve the current placeholder identity from the request header, falling
/// back to `[daemon].default_user`. Unknown names are rejected because they
/// cannot own user-scoped rows in the store.
fn current_user(daemon: &Daemon, user_header: Option<&str>) -> Result<Option<String>, Reply> {
    // TODO(RAL-252): replace with verified user identity once login exists
    let user_name = user_header
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .or_else(|| crate::config::load_daemon_config().default_user);
    let Some(user_name) = user_name else {
        return Ok(None);
    };
    match daemon.lock().get_user(&user_name) {
        Ok(Some(_)) => Ok(Some(user_name)),
        Ok(None) => Err(error(
            400,
            "unknown_user",
            &format!("user {user_name:?} is not registered"),
            vec![],
        )),
        Err(e) => Err(store_error(&e)),
    }
}

fn require_current_user(daemon: &Daemon, user_header: Option<&str>) -> Result<String, Reply> {
    current_user(daemon, user_header)?.ok_or_else(|| {
        error(
            400,
            "current_user_required",
            "set X-Ralphus-User or configure [daemon].default_user",
            vec![],
        )
    })
}

/// Requires the resolved current user to be a registered admin (RAL-332).
///
/// This is a UI-level convenience gate, not a real security boundary -- see
/// `crate::users`'s module doc comment. There is no verified login yet
/// (RAL-252), so anyone holding the daemon's shared bearer token can already
/// reach every endpoint this gates by calling it directly; the point is only
/// to keep the board's admin-only tabs and actions consistent between the
/// client-side hide and what the server actually accepts.
///
/// Bootstrap exception: before any user anywhere has ever been promoted,
/// every admin-gated endpoint behaves as if the caller already is one --
/// otherwise a fresh instance could never register its first user (itself
/// admin-gated), let alone promote one, since every path to doing so would
/// be locked behind an admin who cannot yet exist. This window closes
/// permanently the instant any user is promoted, from any caller.
fn require_admin(daemon: &Daemon, user_header: Option<&str>) -> Result<String, Reply> {
    let any_admin_exists = match daemon.lock().list_users() {
        Ok(users) => users.iter().any(|u| u.is_admin),
        Err(e) => return Err(store_error(&e)),
    };
    if !any_admin_exists {
        // Bootstrap exception: allow unregistered users in a fresh instance.
        let user_name = current_user(daemon, user_header)
            .ok()
            .flatten()
            .unwrap_or_default();
        return Ok(user_name);
    }
    eprintln!("DEBUG: Admin exists, requiring current user");
    let user_name = require_current_user(daemon, user_header)?;
    match daemon.lock().is_admin(&user_name) {
        Ok(true) => Ok(user_name),
        Ok(false) => Err(error(
            403,
            "admin_required",
            &format!("user {user_name:?} is not an admin"),
            vec![],
        )),
        Err(e) => Err(store_error(&e)),
    }
}

/// Runs `reply` only if the current caller is a registered admin, else
/// returns [`require_admin`]'s error `Reply` unchanged. Keeps every
/// admin-gated route entry in the dispatch table a one-liner instead of
/// repeating the same `match require_admin(...) { ... }` at each handler.
fn admin_gated(daemon: &Daemon, user_header: Option<&str>, reply: impl FnOnce() -> Reply) -> Reply {
    match require_admin(daemon, user_header) {
        Ok(_) => reply(),
        Err(r) => r,
    }
}

/// `GET /api/whoami` (RAL-332): the caller's own resolved identity and admin
/// flag, so the board can decide whether to show its admin-only tabs
/// without ever needing to already know its own claimed name. Unlike
/// [`require_current_user`], an unresolved identity is not an error here --
/// it just reports `is_admin: false`, the same as any other non-admin.
fn whoami(daemon: &Daemon, user_header: Option<&str>) -> Reply {
    let name = match current_user(daemon, user_header) {
        Ok(name) => name,
        Err(reply) => return reply,
    };
    let is_admin = match &name {
        Some(n) => match daemon.lock().is_admin(n) {
            Ok(v) => v,
            Err(e) => return store_error(&e),
        },
        None => false,
    };
    json(200, &WhoAmIResponse { name, is_admin })
}

/// Sets or clears another user's admin flag (RAL-332). Gated by
/// [`require_admin`], including its bootstrap exception -- a fresh instance
/// with zero admins can promote its first one.
fn set_user_admin_endpoint(
    daemon: &Daemon,
    user_header: Option<&str>,
    name: &str,
    body: &str,
) -> Reply {
    if let Err(reply) = require_admin(daemon, user_header) {
        return reply;
    }
    let Ok(req) = serde_json::from_str::<SetAdminBody>(body) else {
        return error(
            400,
            "bad_request",
            "body must include an \"is_admin\" boolean",
            vec![],
        );
    };
    match daemon.lock().set_user_admin(name, req.is_admin) {
        Ok(()) => json(
            200,
            &serde_json::json!({"name": name, "is_admin": req.is_admin}),
        ),
        Err(e) => store_error(&e),
    }
}

/// Loads the effective config and applies `[daemon].default_user_is_admin`
/// -- see [`apply_default_user_admin`] for the actual logic, split out so
/// it's testable against an explicit [`crate::config::DaemonConfig`]
/// instead of the real env/filesystem `load_daemon_config` reads from.
fn bootstrap_default_user_admin(store: &Store) {
    apply_default_user_admin(store, &crate::config::load_daemon_config());
}

/// Applies `[daemon].default_user_is_admin` (RAL-332) once at daemon
/// startup: config is the source of truth, so this both promotes and
/// demotes to match it. A no-op when the flag is unset, or when the
/// store's current state already matches -- so a normal restart with no
/// config change touches neither the DB nor the log/Cartographer.
///
/// Exists because the board's Users tab is itself admin-gated: without
/// this, the very first admin can only be granted through a raw
/// `POST /api/users/{name}/admin` call (`require_admin`'s bootstrap
/// exception for "zero admins registered").
fn apply_default_user_admin(store: &Store, cfg: &crate::config::DaemonConfig) {
    let Some(want_admin) = cfg.default_user_is_admin else {
        return;
    };
    let Some(name) = cfg.default_user.clone() else {
        crate::cartographer::Note::new("startup")
            .level(crate::logging::LogLevel::WARNING)
            .emit(
                store,
                "[daemon].default_user_is_admin is set but no default_user is configured -- nothing to promote/demote",
                serde_json::json!({}),
            );
        return;
    };
    if want_admin {
        match store.get_user(&name) {
            Ok(Some(_)) => {}
            Ok(None) => {
                if let Err(e) = store.create_user(&name) {
                    crate::cartographer::Note::new("startup")
                        .level(crate::logging::LogLevel::ERROR)
                        .emit(
                            store,
                            format!("could not register default_user {name:?}: {e}"),
                            serde_json::json!({"user_name": name, "error": e.to_string()}),
                        );
                    return;
                }
            }
            Err(e) => {
                crate::cartographer::Note::new("startup")
                    .level(crate::logging::LogLevel::ERROR)
                    .emit(
                        store,
                        format!("could not look up default_user {name:?}: {e}"),
                        serde_json::json!({"user_name": name, "error": e.to_string()}),
                    );
                return;
            }
        }
    }
    match store.is_admin(&name) {
        Ok(is_admin) if is_admin == want_admin => {}
        Ok(_) => match store.set_user_admin(&name, want_admin) {
            Ok(()) => crate::cartographer::Note::new("startup").emit(
                store,
                format!(
                    "set default_user {name:?} admin={want_admin} via [daemon].default_user_is_admin"
                ),
                serde_json::json!({"user_name": name, "is_admin": want_admin}),
            ),
            Err(e) => crate::cartographer::Note::new("startup")
                .level(crate::logging::LogLevel::ERROR)
                .emit(
                    store,
                    format!("could not set default_user {name:?} admin={want_admin}: {e}"),
                    serde_json::json!({"user_name": name, "error": e.to_string()}),
                ),
        },
        Err(e) => crate::cartographer::Note::new("startup")
            .level(crate::logging::LogLevel::ERROR)
            .emit(
                store,
                format!("could not check admin status for default_user {name:?}: {e}"),
                serde_json::json!({"user_name": name, "error": e.to_string()}),
            ),
    }
}

/// `POST /api/users/{name}/visit` (RAL-332): records that an admin opened
/// "Edit Profile" for another user -- i.e. viewed RAL-329's Preferences page
/// scoped to `name` instead of their own. Audit-only: this endpoint does not
/// itself read or change anything for either user. Requires the caller to
/// already be an admin (no bootstrap exception -- unlike promotion, there is
/// no reason this needs to work before any admin exists).
fn visit_user_profile(daemon: &Daemon, user_header: Option<&str>, name: &str) -> Reply {
    let admin_name = match require_admin(daemon, user_header) {
        Ok(name) => name,
        Err(reply) => return reply,
    };
    match daemon.lock().get_user(name) {
        Ok(Some(_)) => {}
        Ok(None) => {
            return error(
                404,
                "not_found",
                &format!("user {name:?} is not registered"),
                vec![],
            );
        }
        Err(e) => return store_error(&e),
    }
    let store = daemon.lock();
    // RAL-332: admin-only Cartographer visibility -- see `Note::admin_only`'s
    // doc comment. Closes the loop RAL-328 left open at `set_squad_hidden`/
    // `set_review_hidden`'s own `TODO(RAL-332)` comments.
    crate::cartographer::Note::new("users")
        .scope("user")
        .admin_only()
        .emit(
            &store,
            "admin viewed user profile",
            serde_json::json!({ "admin": admin_name, "target": name }),
        );
    json(
        200,
        &serde_json::json!({"admin": admin_name, "target": name}),
    )
}

/// Lists the agents a user may select for `cwd` -- built-in backends plus
/// whatever `.ralphus.toml` custom profiles apply there. The caller-presented
/// identity is currently inert in `AgentAccess`, but shares the same header
/// and configured-default resolution as other user-scoped endpoints.
fn list_agents(daemon: &Daemon, query: &str, user_header: Option<&str>) -> Reply {
    let Some(cwd) = query_param(query, "cwd").map(url_decode) else {
        return error(
            400,
            "bad_request",
            "cwd query parameter is required",
            vec![],
        );
    };
    let user = match current_user(daemon, user_header) {
        Ok(id) => crate::agent_access::UserContext { id },
        Err(reply) => return reply,
    };
    match daemon.agent_access.available_agents(&user, Path::new(&cwd)) {
        Ok(agents) => {
            let default_agent = crate::config::resolve(Path::new(&cwd))
                .default_resolver_agent()
                .to_string();
            json(
                200,
                &AgentsResponse {
                    agents,
                    default_agent,
                },
            )
        }
        Err(e) => error(500, "internal", &e, vec![]),
    }
}

fn list_hidden(daemon: &Daemon, user_header: Option<&str>) -> Reply {
    let user_name = match require_current_user(daemon, user_header) {
        Ok(name) => name,
        Err(reply) => return reply,
    };
    match daemon.lock().list_hidden(&user_name) {
        Ok(hidden) => json(200, &HiddenResponse { hidden }),
        Err(e) => store_error(&e),
    }
}

fn set_squad_hidden(
    daemon: &Daemon,
    user_header: Option<&str>,
    squad_id: &str,
    hidden: bool,
) -> Reply {
    let user_name = match require_current_user(daemon, user_header) {
        Ok(name) => name,
        Err(reply) => return reply,
    };
    let store = daemon.lock();
    let result = if hidden {
        store.hide_squad(&user_name, squad_id)
    } else {
        store.unhide_squad(&user_name, squad_id)
    };
    if let Err(e) = result {
        return store_error(&e);
    }
    // RAL-332: admin-only Cartographer visibility -- a hide/unhide row
    // reveals one user's personal preference, which every other viewer of
    // the Logs tab should not see.
    crate::cartographer::Note::new("hidden")
        .squad(squad_id)
        .scope("squad")
        .admin_only()
        .emit(
            &store,
            if hidden {
                "squad hidden"
            } else {
                "squad unhidden"
            },
            serde_json::json!({ "user_name": user_name }),
        );
    json(200, &HiddenStateResponse { hidden })
}

/// Batch form of [`set_squad_hidden`] -- one request for the board's
/// multi-select Hide/Unhide menu items, applying every id under a single
/// held `Store` lock instead of one HTTP round trip per squad. Best-effort:
/// an id that fails (e.g. already deleted) is reported in `failed` rather
/// than aborting the rest of the batch, and the response is still `200`.
fn set_squads_hidden_batch(daemon: &Daemon, user_header: Option<&str>, body: &str) -> Reply {
    let user_name = match require_current_user(daemon, user_header) {
        Ok(name) => name,
        Err(reply) => return reply,
    };
    let Ok(req) = serde_json::from_str::<HiddenSquadsBatchBody>(body) else {
        return error(400, "bad_request", "invalid body", vec![]);
    };
    if req.ids.is_empty() {
        return error(400, "bad_request", "ids must not be empty", vec![]);
    }
    let store = daemon.lock();
    let failed = store.set_squads_hidden(&user_name, &req.ids, req.hidden);
    let failed_ids: std::collections::HashSet<&str> =
        failed.iter().map(|(id, _)| id.as_str()).collect();
    // RAL-332: admin-only Cartographer visibility -- see `set_squad_hidden`'s
    // matching comment. One note per squad that actually changed, mirroring
    // the granularity of the single-item endpoint.
    for id in &req.ids {
        if failed_ids.contains(id.as_str()) {
            continue;
        }
        crate::cartographer::Note::new("hidden")
            .squad(id)
            .scope("squad")
            .admin_only()
            .emit(
                &store,
                if req.hidden {
                    "squad hidden"
                } else {
                    "squad unhidden"
                },
                serde_json::json!({ "user_name": user_name }),
            );
    }
    json(
        200,
        &HiddenBatchResponse {
            hidden: req.hidden,
            failed: failed
                .into_iter()
                .map(|(id, e)| HiddenBatchFailure {
                    id,
                    error: e.to_string(),
                })
                .collect(),
        },
    )
}

fn set_review_hidden(
    daemon: &Daemon,
    user_header: Option<&str>,
    guardian_id: &str,
    hidden: bool,
) -> Reply {
    let user_name = match require_current_user(daemon, user_header) {
        Ok(name) => name,
        Err(reply) => return reply,
    };
    let store = daemon.lock();
    let result = if hidden {
        store.hide_review(&user_name, guardian_id)
    } else {
        store.unhide_review(&user_name, guardian_id)
    };
    if let Err(e) = result {
        return store_error(&e);
    }
    // RAL-332: admin-only Cartographer visibility -- see `set_squad_hidden`'s
    // matching comment above.
    crate::cartographer::Note::new("hidden")
        .guardian(guardian_id)
        .scope("guardian")
        .admin_only()
        .emit(
            &store,
            if hidden {
                "review hidden"
            } else {
                "review unhidden"
            },
            serde_json::json!({ "user_name": user_name }),
        );
    json(200, &HiddenStateResponse { hidden })
}

/// All registered users (RAL-?) -- see `crate::users`'s module doc comment.
fn list_users(daemon: &Daemon) -> Reply {
    match daemon.lock().list_users() {
        Ok(users) => json(200, &UsersResponse { users }),
        Err(e) => store_error(&e),
    }
}

/// Register a placeholder user by name (RAL-?) -- see `crate::users`'s
/// module doc comment; this grants no permissions.
fn create_user(daemon: &Daemon, body: &str) -> Reply {
    let Ok(req) = serde_json::from_str::<CreateUserBody>(body) else {
        return error(
            400,
            "bad_request",
            "body must include a \"name\" string",
            vec![],
        );
    };
    let name = req.name.trim();
    if name.is_empty() {
        return error(400, "bad_request", "\"name\" must not be empty", vec![]);
    }
    match daemon.lock().create_user(name) {
        Ok(()) => json(200, &serde_json::json!({"name": name})),
        Err(e) => store_error(&e),
    }
}

/// Rename a registered user (RAL-?). Reuses [`RenameBody`] (`{name}`), the
/// same shape `guardian_rename` uses.
fn user_rename(daemon: &Daemon, name: &str, body: &str) -> Reply {
    let Ok(req) = serde_json::from_str::<RenameBody>(body) else {
        return error(400, "bad_request", "body must be {name}", vec![]);
    };
    let new_name = req.name.trim();
    if new_name.is_empty() {
        return error(400, "bad_request", "name must not be empty", vec![]);
    }
    match daemon.lock().rename_user(name, new_name) {
        Ok(()) => json(200, &serde_json::json!({"name": new_name})),
        Err(e) => store_error(&e),
    }
}

/// Remove a registered user by exact name (RAL-?).
fn delete_user(daemon: &Daemon, name: &str) -> Reply {
    match daemon.lock().delete_user(name) {
        Ok(true) => json(200, &serde_json::json!({"deleted": true})),
        Ok(false) => error(
            404,
            "not_found",
            &format!("user \"{name}\" is not registered"),
            vec![],
        ),
        Err(e) => store_error(&e),
    }
}

/// Resolve the acting user for a request (RAL-320): an explicit `?user=`
/// query parameter, falling back to `.ralphus.toml`'s `default_user`
/// (`crate::config::DaemonConfig::default_user`) when the caller doesn't
/// name one. `None` when neither is set -- callers that require a user
/// should go through [`require_acting_user`] instead.
fn resolve_acting_user(query: &str) -> Option<String> {
    if let Some(user) = query_param(query, "user") {
        // Query parameter was explicitly provided; use it as-is (don't fall back to config)
        let decoded = url_decode(user);
        (!decoded.is_empty()).then_some(decoded)
    } else {
        // No query parameter; fall back to config default
        crate::config::load_daemon_config().default_user
    }
}

/// Like [`resolve_acting_user`], but a `400` reply when no user can be
/// resolved -- the shared guard for every watches/personal-mailbox endpoint,
/// none of which make sense for an anonymous caller.
fn require_acting_user(query: &str) -> Result<String, Reply> {
    query_param(query, "user")
        .map(url_decode)
        .filter(|u| !u.is_empty())
        .ok_or_else(|| error(400, "bad_request", "no user: pass ?user=<name>", vec![]))
}

/// Resolve a watch request's user from the CLI-compatible query parameter or
/// the board's current-user header/default fallback.
fn require_watch_user(
    daemon: &Daemon,
    query: &str,
    user_header: Option<&str>,
) -> Result<String, Reply> {
    if let Some(user) = query_param(query, "user").map(url_decode) {
        return Ok(user);
    }
    require_current_user(daemon, user_header)
}

/// Parse a list of tier-name strings, `400`-erroring on the first one that
/// isn't `urgent`/`high`/`normal`.
fn parse_notify_tiers(tiers: &[String]) -> Result<Vec<crate::mailbox::MailboxPriority>, Reply> {
    let mut parsed = Vec::with_capacity(tiers.len());
    for t in tiers {
        match crate::mailbox::MailboxPriority::parse(t) {
            Some(p) => parsed.push(p),
            None => {
                return Err(error(
                    400,
                    "bad_request",
                    "notify tiers must be one of urgent/high/normal",
                    vec![],
                ));
            }
        }
    }
    Ok(parsed)
}

/// `GET /api/users/{name}/preferences` (RAL-320).
fn get_user_preferences(daemon: &Daemon, name: &str) -> Reply {
    match daemon.lock().get_user(name) {
        Ok(Some(u)) => json(200, &u),
        Ok(None) => error(
            404,
            "not_found",
            &format!("user \"{name}\" is not registered"),
            vec![],
        ),
        Err(e) => store_error(&e),
    }
}

/// `POST /api/users/{name}/preferences` (RAL-320) -- registers `name` first
/// if needed, same as [`create_watch_endpoint`].
fn set_user_preferences_endpoint(daemon: &Daemon, name: &str, body: &str) -> Reply {
    let Ok(req) = serde_json::from_str::<UserPreferencesBody>(body) else {
        return error(
            400,
            "bad_request",
            "body must be {auto_watch, default_notify_tiers?}",
            vec![],
        );
    };
    let tiers = match req.default_notify_tiers {
        Some(tiers) if !tiers.is_empty() => match parse_notify_tiers(&tiers) {
            Ok(t) => t,
            Err(r) => return r,
        },
        _ => crate::mailbox::all_tiers(),
    };
    match daemon
        .lock()
        .set_user_preferences(name, req.auto_watch, &tiers)
    {
        Ok(u) => json(200, &u),
        Err(e) => store_error(&e),
    }
}

/// `GET /api/watches?user=` -- every watch the acting user owns.
fn list_watches_endpoint(daemon: &Daemon, query: &str, user_header: Option<&str>) -> Reply {
    let user = match require_watch_user(daemon, query, user_header) {
        Ok(u) => u,
        Err(r) => return r,
    };
    match daemon.lock().list_watches(&user) {
        Ok(watches) => json(200, &WatchesResponse { watches }),
        Err(e) => store_error(&e),
    }
}

fn watchers_endpoint(daemon: &Daemon, entity_uri: &str) -> Reply {
    if !is_watchable_entity(entity_uri) {
        return error(
            400,
            "invalid_entity_uri",
            "only squads, tasks, cells, and reviews can be watched",
            vec![],
        );
    }
    match daemon.lock().watchers_for_entity(entity_uri) {
        Ok(watches) => json(200, &WatchesResponse { watches }),
        Err(e) => error(500, "store_error", &e.to_string(), vec![]),
    }
}

/// `POST /api/watches?user=` -- watch (or update the watch, changing
/// tiers in place) an [`crate::entity_uri::EntityUri`] on the acting user's
/// behalf.
fn create_watch_endpoint(
    daemon: &Daemon,
    query: &str,
    user_header: Option<&str>,
    body: &str,
) -> Reply {
    let user = match require_watch_user(daemon, query, user_header) {
        Ok(u) => u,
        Err(r) => return r,
    };
    let Ok(req) = serde_json::from_str::<CreateWatchBody>(body) else {
        return error(
            400,
            "bad_request",
            "body must include an \"entity_uri\" string",
            vec![],
        );
    };
    if !is_watchable_entity(&req.entity_uri) {
        return error(
            400,
            "bad_request",
            "only squads, tasks, cells, and reviews can be watched",
            vec![],
        );
    }
    let store = daemon.lock();
    let tiers = match req.notify_tiers {
        Some(tiers) if !tiers.is_empty() => match parse_notify_tiers(&tiers) {
            Ok(t) => t,
            Err(r) => return r,
        },
        _ => store
            .get_user(&user)
            .ok()
            .flatten()
            .map(|u| u.default_notify_tiers)
            .filter(|t| !t.is_empty())
            .unwrap_or_else(crate::mailbox::all_tiers),
    };
    match store.create_watch(&user, &req.entity_uri, &tiers) {
        Ok(watch) => json(201, &watch),
        Err(e) => store_error(&e),
    }
}

fn is_watchable_entity(entity_uri: &str) -> bool {
    matches!(
        crate::entity_uri::parse(entity_uri),
        Some(
            crate::entity_uri::EntityUri::Squad { .. }
                | crate::entity_uri::EntityUri::Task { .. }
                | crate::entity_uri::EntityUri::Cell { .. }
                | crate::entity_uri::EntityUri::Guardian { .. }
        )
    )
}

/// `DELETE /api/watches/{entity_uri}?user=`.
fn delete_watch_endpoint(
    daemon: &Daemon,
    query: &str,
    user_header: Option<&str>,
    entity_uri: &str,
) -> Reply {
    let user = match require_watch_user(daemon, query, user_header) {
        Ok(u) => u,
        Err(r) => return r,
    };
    match daemon.lock().delete_watch(&user, entity_uri) {
        Ok(true) => json(200, &serde_json::json!({"deleted": true})),
        Ok(false) => error(404, "not_found", "no such watch", vec![]),
        Err(e) => store_error(&e),
    }
}

/// `GET /api/mailbox/personal/messages?user=` (RAL-320) -- the acting
/// user's personal mailbox view, filtered through their watches. Mirrors
/// [`mailbox_messages`]'s query handling.
fn personal_mailbox_messages(daemon: &Daemon, query: &str) -> Reply {
    let user = match require_acting_user(query) {
        Ok(u) => u,
        Err(r) => return r,
    };
    let unread_only = query_param(query, "unread").is_some_and(|v| v == "1" || v == "true");
    let priority = match query_param(query, "priority") {
        None => None,
        Some(p) => match crate::mailbox::MailboxPriority::parse(p) {
            Some(p) => Some(p),
            None => {
                return error(
                    400,
                    "bad_request",
                    "priority must be one of urgent/high/normal",
                    vec![],
                );
            }
        },
    };
    match daemon
        .lock()
        .personal_mailbox_messages_for_user(&user, unread_only, priority)
    {
        Ok(messages) => json(200, &messages),
        Err(e) => store_error(&e),
    }
}

/// `POST /api/mailbox/personal/drain?user=` (RAL-320). Mirrors
/// [`mailbox_drain`]'s body handling.
fn personal_mailbox_drain(daemon: &Daemon, query: &str, body: &str) -> Reply {
    let user = match require_acting_user(query) {
        Ok(u) => u,
        Err(r) => return r,
    };
    let req: MailboxDrainBody = if body.trim().is_empty() {
        MailboxDrainBody::default()
    } else {
        match serde_json::from_str(body) {
            Ok(b) => b,
            Err(_) => {
                return error(
                    400,
                    "bad_request",
                    "body must be JSON with an optional \"message_ids\" array of strings",
                    vec![],
                );
            }
        }
    };
    match daemon
        .lock()
        .drain_personal_mailbox_messages(&user, req.message_ids.as_deref())
    {
        Ok(drained) => json(200, &serde_json::json!({"drained": drained})),
        Err(e) => store_error(&e),
    }
}

/// `GET /api/secret-env-names` (RAL-281) -- see `crate::secret_env_names`'s
/// module doc comment.
fn list_secret_env_names(daemon: &Daemon) -> Reply {
    match daemon.lock().list_secret_env_names() {
        Ok(names) => json(200, &SecretEnvNamesResponse { names }),
        Err(e) => store_error(&e),
    }
}

/// Register a new secret env-var name (RAL-281). `409` if already
/// registered -- adding a duplicate is rejected, not silently merged.
fn add_secret_env_name(daemon: &Daemon, body: &str) -> Reply {
    let Ok(req) = serde_json::from_str::<AddSecretEnvNameBody>(body) else {
        return error(
            400,
            "bad_request",
            "body must include a \"name\" string",
            vec![],
        );
    };
    let name = req.name.trim();
    if !crate::config::is_valid_env_key(name) {
        return error(
            400,
            "invalid_value",
            "\"name\" must be a valid environment-variable identifier ([A-Za-z_][A-Za-z0-9_]*)",
            vec![],
        );
    }
    match daemon.lock().add_secret_env_name(name) {
        Ok(()) => json(201, &serde_json::json!({"name": name})),
        Err(e) => store_error(&e),
    }
}

/// Rename a registered secret env-var name (RAL-281). `404` if `name` isn't
/// registered, `409` if the new name is already registered by a different
/// entry.
fn rename_secret_env_name(daemon: &Daemon, name: &str, body: &str) -> Reply {
    let Ok(req) = serde_json::from_str::<RenameBody>(body) else {
        return error(400, "bad_request", "body must be {name}", vec![]);
    };
    let new_name = req.name.trim();
    if !crate::config::is_valid_env_key(new_name) {
        return error(
            400,
            "invalid_value",
            "\"name\" must be a valid environment-variable identifier ([A-Za-z_][A-Za-z0-9_]*)",
            vec![],
        );
    }
    match daemon.lock().rename_secret_env_name(name, new_name) {
        Ok(()) => json(200, &serde_json::json!({"name": new_name})),
        Err(e) => store_error(&e),
    }
}

/// Remove a registered secret env-var name (RAL-281).
fn delete_secret_env_name(daemon: &Daemon, name: &str) -> Reply {
    match daemon.lock().delete_secret_env_name(name) {
        Ok(true) => json(200, &serde_json::json!({"deleted": true})),
        Ok(false) => error(
            404,
            "not_found",
            &format!("secret env-var name {name:?} is not registered"),
            vec![],
        ),
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

/// `GET /api/project-forks` (RAL-338): every registered fork row, across
/// every project and user.
fn list_all_project_forks(daemon: &Daemon) -> Reply {
    match daemon.lock().list_project_forks() {
        Ok(forks) => json(200, &ProjectForksResponse { forks }),
        Err(e) => store_error(&e),
    }
}

/// `GET /api/worktree-retirements` (RAL-385): every review worktree
/// classified by retirement state (`scheduled`/`eligible`/`claimed`/
/// `failed`/`deferred`/`opted_out`/`retired`, see
/// [`crate::guardian_merge::WorktreeRetirementEntry`]), plus durable history
/// for already-removed worktrees.
fn worktree_retirements(daemon: &Daemon) -> Reply {
    match crate::guardian_merge::worktree_retirement_view(&daemon.lock()) {
        Ok(view) => json(200, &view),
        Err(e) => store_error(&e),
    }
}

/// `GET /api/projects/{name}/forks` (RAL-338): every fork row registered for
/// one project, including its `user=""` default row if present.
fn list_project_forks(daemon: &Daemon, project: &str) -> Reply {
    match daemon.lock().list_project_forks_for_project(project) {
        Ok(forks) => json(200, &ProjectForksResponse { forks }),
        Err(e) => store_error(&e),
    }
}

/// `POST /api/projects/{name}/forks` (RAL-338): register or replace a fork
/// row. `user` defaults to `""` (the project-wide row) when omitted.
fn create_project_fork(daemon: &Daemon, project: &str, body: &str) -> Reply {
    let Ok(req) = serde_json::from_str::<CreateProjectForkBody>(body) else {
        return error(
            400,
            "bad_request",
            "body must include a non-empty \"fork_url\" string",
            vec![],
        );
    };
    let fork_url = req.fork_url.trim();
    if fork_url.is_empty() {
        return error(400, "invalid_value", "'fork_url' must not be empty", vec![]);
    }
    let user = req.user.trim();
    let remote_name = req
        .remote_name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| crate::project_forks::default_remote_name(user));
    let explicit_owner = req
        .fork_owner
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    // RAL-338: auto-derive `fork_owner` from the URL when the caller didn't
    // give one explicitly and the URL looks like a GitHub host -- GitLab
    // addresses cross-project MRs by numeric project id instead, so it's
    // left empty there (`derive_owner_from_url` doesn't know the forge kind
    // itself, only URL shape).
    let derived_owner = explicit_owner.is_none().then(|| {
        crate::forge::parse_remote_url(fork_url)
            .and_then(|(host, _)| crate::forge::ForgeKind::from_host(&host))
            .filter(|kind| *kind == crate::forge::ForgeKind::GitHub)
            .and_then(|_| crate::forge::derive_owner_from_url(fork_url))
    });
    let fork_owner = explicit_owner
        .map(str::to_string)
        .or(derived_owner.flatten())
        .unwrap_or_default();
    match daemon
        .lock()
        .upsert_project_fork(project, user, fork_url, &remote_name, &fork_owner)
    {
        Ok(record) => json(201, &record),
        Err(e) => store_error(&e),
    }
}

/// `PATCH /api/projects/{name}/forks[/{user}]` (RAL-338): field-selective
/// update of an existing fork row.
fn patch_project_fork(daemon: &Daemon, project: &str, user: &str, body: &str) -> Reply {
    let req: PatchProjectForkBody = serde_json::from_str(body).unwrap_or_default();
    if req.fork_url.as_deref().is_some_and(str::is_empty) {
        return error(400, "invalid_value", "'fork_url' must not be empty", vec![]);
    }
    match daemon.lock().patch_project_fork(
        project,
        user,
        req.fork_url.as_deref(),
        req.remote_name.as_deref(),
        req.fork_owner.as_deref(),
    ) {
        Ok(record) => json(200, &record),
        Err(crate::store::StoreError::NotFound) => error(
            404,
            "not_found",
            &format!("no fork registered for project {project:?} user {user:?}"),
            vec![],
        ),
        Err(e) => store_error(&e),
    }
}

/// `DELETE /api/projects/{name}/forks[/{user}]` (RAL-338).
fn delete_project_fork(daemon: &Daemon, project: &str, user: &str) -> Reply {
    match daemon.lock().delete_project_fork(project, user) {
        Ok(true) => json(200, &serde_json::json!({"removed": true})),
        Ok(false) => error(
            404,
            "not_found",
            &format!("no fork registered for project {project:?} user {user:?}"),
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

/// `GET /api/projects/{name}/branches` (RAL-297): local and `origin`
/// remote-tracking branch names for a registered git project, so the Simple
/// task form can validate a user-typed upstream branch at submit time
/// before generating a `ralphus:new-worktree/...?upstream=...` placeholder
/// that would otherwise only fail much later, at worktree-materialization
/// time (`daemon/src/worktrees.rs::resolve_upstream`). Reuses
/// `guardian_merge::list_base_branches`, the same git-shelling-out helper
/// the Reviews tab's base-branch picker already relies on. Non-git projects,
/// or ones whose path no longer resolves, get an empty list rather than an
/// error -- the branch field is only ever shown for a git project in the
/// first place.
#[derive(Serialize)]
struct ProjectBranchesResponse {
    branches: Vec<String>,
}

fn project_branches(daemon: &Daemon, name: &str) -> Reply {
    match daemon.lock().get_project(name) {
        Ok(Some(p)) if p.vcs == "git" => {
            let mut branches = crate::guardian_merge::list_base_branches(&p.path, "main");
            for b in crate::guardian_merge::list_base_branches(&p.path, "origin/HEAD") {
                if !branches.contains(&b) {
                    branches.push(b);
                }
            }
            json(200, &ProjectBranchesResponse { branches })
        }
        Ok(Some(_)) => json(200, &ProjectBranchesResponse { branches: vec![] }),
        Ok(None) => error(
            404,
            "not_found",
            &format!("project \"{name}\" is not registered"),
            vec![],
        ),
        Err(e) => store_error(&e),
    }
}

#[derive(Serialize)]
struct GenerateStartResponse {
    id: String,
}

/// `POST /api/generate` (RAL-297): kicks off one "generation step" (Simple
/// form's opt-in "Generate Proofs"/"Generate Manual Checks") on a background
/// thread and returns `202` immediately with a job id -- see
/// `crate::generation`'s module doc comment for why this can't block the
/// accept loop. Poll `GET /api/generate/{id}` for the result.
fn generate_start(daemon: &Daemon, body: &str) -> Reply {
    let req: crate::generation::GenerateRequest = match serde_json::from_str(body) {
        Ok(r) => r,
        Err(e) => {
            return error(
                400,
                "bad_request",
                &format!("invalid generation request body: {e}"),
                vec![],
            );
        }
    };
    if crate::generation::GenerationKind::parse(&req.kind).is_none() {
        return error(
            400,
            "bad_request",
            "kind must be \"proof_steps\" or \"manual_checks\"",
            vec![],
        );
    }
    let id = daemon.generation_jobs.start();
    let jobs = daemon.generation_jobs.clone();
    let job_id = id.clone();
    std::thread::spawn(move || {
        let result = crate::generation::run_generation(&req);
        jobs.finish(&job_id, result);
    });
    json(202, &GenerateStartResponse { id })
}

/// `GET /api/generate/{id}` (RAL-297): poll a generation job started by
/// [`generate_start`].
fn generate_status(daemon: &Daemon, id: &str) -> Reply {
    match daemon.generation_jobs.get(id) {
        Some(job) => json(200, &job),
        None => error(404, "not_found", "no such generation job", vec![]),
    }
}

fn submit(daemon: &Daemon, body: &str, query: &str) -> Reply {
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
    let profile_errors =
        crate::agent_profiles::validate_task_file_profiles(&daemon.lock(), &req.toml, &file);
    if !profile_errors.is_empty() {
        return error(
            400,
            "validation_failed",
            "the submitted TOML is invalid",
            profile_errors,
        );
    }

    // RAL-318: an inline `triage_type` must name a registered Triage type --
    // `core::validate` only checked its structure (non-empty, requires
    // `triage = true`), since `core` has no store access.
    let triage_type_errors =
        crate::triage::validate_task_file_triage_types(&daemon.lock(), &req.toml, &file);
    if !triage_type_errors.is_empty() {
        return error(
            400,
            "validation_failed",
            "the submitted TOML is invalid",
            triage_type_errors,
        );
    }

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
    // Phase 3a: every cell/proof under one task must share a machine.
    if let Err(msg) = validate_machine_affinity(&file) {
        return error(400, "machine_validation_failed", &msg, vec![]);
    }
    // Phase 3b: a remote-fed review must declare what cannot be discovered.
    if let Err(msg) = validate_remote_reviews_are_declarative(&daemon.lock(), &file) {
        return error(400, "machine_validation_failed", &msg, vec![]);
    }
    if let Some(label) = req.label.as_deref() {
        if let Some(reply) = reject_label_with_comma(label) {
            return reply;
        }
    }

    let mut store = daemon.lock();
    let squad_id = match store.insert_squad(&file, req.label.as_deref(), req.hold) {
        Ok(id) => id,
        Err(e) => return store_error(&e),
    };
    // RAL-318: classify every Triage-opted-in cell exactly once, before it
    // reaches the pool -- either its own inline `triage_type` (already
    // validated as registered above), or a fresh Arbiter classification.
    // Single-attempt, no retry: `arbiter::classify` itself falls back to
    // `unclassified` on any failure, so this never blocks or slows down
    // submission beyond one headless LLM call per un-typed Triage cell.
    let arbiter = crate::arbiter::Arbiter::current();
    for (task_idx, task) in file.task.iter().enumerate() {
        for (idx, cell) in task.cell.iter().enumerate() {
            if !cell.triage {
                continue;
            }
            let (task_idx, idx) = (task_idx as i64, idx as i64);
            let inline_types: Vec<String> = cell
                .triage_type
                .as_deref()
                .unwrap_or_default()
                .iter()
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty())
                .collect();
            let resolved_types = if inline_types.is_empty() {
                let cell_id = cell.id.clone().unwrap_or_else(|| format!("cell-{idx}"));
                let context = cell
                    .prompt
                    .clone()
                    .or_else(|| cell.command.clone())
                    .unwrap_or_default();
                crate::arbiter::classify(&store, &arbiter, &squad_id, &cell_id, &context)
            } else {
                inline_types
            };
            let _ = store.set_cell_triage_types(&squad_id, task_idx, idx, &resolved_types);
        }
    }

    // Derive per-project review guardians. A preflight failure (bad worktree, no
    // upstream for a `<<upstream>>` base) rolls the squad back and rejects the submit.
    if let Err(e) = crate::reviews::derive_reviews(&store, &squad_id, &file) {
        let _ = store.delete_squad(&squad_id);
        return error(400, "review_preflight_failed", &e.message, vec![]);
    }
    // RAL-318: pool every Triage-opted-in cell and fire any pool whose count
    // threshold this submission just reached. Failures here mirror
    // `derive_reviews`'s rollback -- Triage pooling shares the same worktree/
    // upstream preconditions.
    if let Err(e) = crate::reviews::derive_triage_pools(&store, &squad_id, &file) {
        let _ = store.delete_squad(&squad_id);
        return error(400, "review_preflight_failed", &e.message, vec![]);
    }
    let state = if req.hold {
        SquadState::Queued
    } else {
        SquadState::Pending
    };
    // RAL-320: auto-watch-on-submit -- a user who has opted in via
    // `auto_watch` (`crate::users::set_user_preferences`) is watched to
    // their own squad automatically, using their `default_notify_tiers`, so
    // they don't have to separately `watch` every squad they submit.
    if let Some(user) = resolve_acting_user(query) {
        if let Ok(Some(u)) = store.get_user(&user) {
            if u.auto_watch {
                let entity_uri = crate::entity_uri::EntityUri::Squad {
                    squad_id: squad_id.clone(),
                }
                .to_string();
                let _ = store.create_watch(&user, &entity_uri, &u.default_notify_tiers);
                if let Ok(guardian_ids) = store.guardians_for_squad(&squad_id) {
                    for guardian_id in guardian_ids {
                        let entity_uri =
                            crate::entity_uri::EntityUri::Guardian { guardian_id }.to_string();
                        let _ = store.create_watch(&user, &entity_uri, &u.default_notify_tiers);
                    }
                }
            }
        }
    }
    json(
        201,
        &SubmitResponse {
            squad_id,
            state: state.as_str(),
        },
    )
}

fn get_squad(daemon: &Daemon, id: &str) -> Reply {
    match daemon.lock().get_squad(id) {
        Ok(squad) => json(200, &squad),
        Err(e) => store_error(&e),
    }
}

// ── RAL-188: ralphus URI resolution ──────────────────────────────────────────

/// The positional coordinates a ralphus URI resolves to — the addressing the
/// rest of this API is built on (`/api/squads/{id}/cells/{ti}/{si}/pane` and
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
    /// `squad` | `task` | `cell` | `proof` | `review`.
    kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    squad_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    task_idx: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cell_idx: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    proof_idx: Option<usize>,
    /// `task` or `cell` — which scope the addressed proof step lives in.
    #[serde(skip_serializing_if = "Option::is_none")]
    proof_scope: Option<String>,
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
            squad_id: None,
            task_idx: None,
            cell_idx: None,
            proof_idx: None,
            proof_scope: None,
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

/// The addressable name for each proof step: its author-supplied `id`, or
/// empty for an anonymous step — addressable only as `~N` (RAL-188 §C.4).
fn proof_candidates(steps: &[crate::store::ProofView]) -> Vec<String> {
    steps
        .iter()
        .map(|s| s.id.clone().unwrap_or_default())
        .collect()
}

/// Find the squad a `SQUAD[label]` segment names when the URI carried no `?id=`.
///
/// Matches a squad's label *or* its id, since a squad with no label renders as its
/// id (§C.2). Ambiguity lists the candidates and points at `?id=`; it is never
/// silently resolved to the most recent (§C.3).
fn squad_id_for_label(store: &MutexGuard<'_, Store>, label: &str) -> Result<String, ResolveError> {
    let squads = store.list_squads().map_err(|e| ResolveError {
        status: 500,
        code: "internal",
        message: e.to_string(),
    })?;
    let matches: Vec<&crate::store::SquadView> = squads
        .iter()
        .filter(|r| r.id == label || r.label.as_deref() == Some(label))
        .collect();
    match matches.as_slice() {
        [] => Err(not_found(format!("no squad labelled '{label}'"))),
        [only] => Ok(only.id.clone()),
        many => {
            let ids: Vec<&str> = many.iter().map(|r| r.id.as_str()).collect();
            Err(ambiguous(format!(
                "'{label}' matches {} squads ({}) -- add '?id=<squad id>' to say which one",
                many.len(),
                ids.join(", ")
            )))
        }
    }
}

/// Rebuild the canonical URI for a resolved squad-family entity from the squad's
/// current names, falling back to `~N` at any level that has no name of its own.
fn canonical_squad_uri(
    squad: &crate::store::SquadView,
    task_idx: Option<usize>,
    cell_idx: Option<usize>,
    proof_idx: Option<usize>,
) -> String {
    let label = squad
        .label
        .clone()
        .filter(|l| !l.is_empty())
        .unwrap_or_else(|| squad.id.clone());
    let mut segments = vec![Segment::named("SQUAD", label)];
    if let Some(ti) = task_idx {
        segments.push(match squad.tasks.get(ti) {
            Some(t) if !t.name.is_empty() => Segment::named("TASK", t.name.clone()),
            _ => Segment::positional("TASK", ti),
        });
        let steps = if let Some(si) = cell_idx {
            let cell = squad.tasks.get(ti).and_then(|t| t.cells.get(si));
            segments.push(match cell {
                Some(s) => {
                    let name = s
                        .name
                        .clone()
                        .filter(|n| !n.is_empty())
                        .unwrap_or_else(|| s.id.clone());
                    if name.is_empty() {
                        Segment::positional("CELL", si)
                    } else {
                        Segment::named("CELL", name)
                    }
                }
                None => Segment::positional("CELL", si),
            });
            cell.map(|s| s.proof.as_slice()).unwrap_or_default()
        } else {
            squad
                .tasks
                .get(ti)
                .map(|t| t.proof.as_slice())
                .unwrap_or_default()
        };
        if let Some(vi) = proof_idx {
            segments.push(match steps.get(vi).and_then(|s| s.id.as_deref()) {
                Some(id) if !id.is_empty() => Segment::named("PROOF", id),
                _ => Segment::positional("PROOF", vi),
            });
        }
    }
    RalphusUri {
        segments,
        query: vec![("id".to_string(), Some(squad.id.clone()))],
    }
    .to_string()
}

fn resolve_squad_uri(
    store: &MutexGuard<'_, Store>,
    uri: &RalphusUri,
) -> Result<ResolvedUri, ResolveError> {
    let squad_seg = uri
        .segment("SQUAD")
        .ok_or_else(|| not_found("no SQUAD[...] segment".to_string()))?;
    if squad_seg.index.is_some() {
        return Err(ResolveError {
            status: 400,
            code: "bad_uri",
            message: "SQUAD[~N] is not addressable -- a squad has no stable position. \
                      Use its label or id (and ideally '?id=<squad id>')."
                .to_string(),
        });
    }
    let squad_id = match uri.id() {
        Some(id) => id.to_string(),
        None => squad_id_for_label(store, squad_seg.name.as_deref().unwrap_or(""))?,
    };
    let squad = store
        .get_squad(&squad_id)
        .map_err(|_| not_found(format!("no squad '{squad_id}'")))?;

    let Some(task_seg) = uri.segment("TASK") else {
        let mut out = ResolvedUri::new("squad", canonical_squad_uri(&squad, None, None, None));
        out.squad_id = Some(squad.id.clone());
        return Ok(out);
    };
    let task_names: Vec<String> = squad.tasks.iter().map(|t| t.name.clone()).collect();
    let task_idx = task_seg
        .token()
        .resolve(&task_names, "task")
        .map_err(|e| ambiguous(e.to_string()))?;
    let task = &squad.tasks[task_idx];

    let proof_seg = uri.segment("PROOF");
    let Some(cell_seg) = uri.segment("CELL") else {
        // No CELL segment: either the task itself, or one of its own
        // task-scope proof steps.
        let Some(proof_seg) = proof_seg else {
            let mut out = ResolvedUri::new(
                "task",
                canonical_squad_uri(&squad, Some(task_idx), None, None),
            );
            out.squad_id = Some(squad.id.clone());
            out.task_idx = Some(task_idx);
            return Ok(out);
        };
        let proof_idx = proof_seg
            .token()
            .resolve(&proof_candidates(&task.proof), "task proof")
            .map_err(|e| ambiguous(e.to_string()))?;
        let mut out = ResolvedUri::new(
            "proof",
            canonical_squad_uri(&squad, Some(task_idx), None, Some(proof_idx)),
        );
        out.squad_id = Some(squad.id.clone());
        out.task_idx = Some(task_idx);
        out.proof_idx = Some(proof_idx);
        out.proof_scope = Some("task".to_string());
        return Ok(out);
    };

    let cell_names: Vec<String> = task
        .cells
        .iter()
        .map(|s| {
            s.name
                .clone()
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| s.id.clone())
        })
        .collect();
    let cell_idx = cell_seg
        .token()
        .resolve(&cell_names, "cell")
        .map_err(|e| ambiguous(e.to_string()))?;

    let Some(proof_seg) = proof_seg else {
        let mut out = ResolvedUri::new(
            "cell",
            canonical_squad_uri(&squad, Some(task_idx), Some(cell_idx), None),
        );
        out.squad_id = Some(squad.id.clone());
        out.task_idx = Some(task_idx);
        out.cell_idx = Some(cell_idx);
        return Ok(out);
    };
    let cell = &task.cells[cell_idx];
    let proof_idx = proof_seg
        .token()
        .resolve(&proof_candidates(&cell.proof), "cell proof")
        .map_err(|e| ambiguous(e.to_string()))?;
    let mut out = ResolvedUri::new(
        "proof",
        canonical_squad_uri(&squad, Some(task_idx), Some(cell_idx), Some(proof_idx)),
    );
    out.squad_id = Some(squad.id.clone());
    out.task_idx = Some(task_idx);
    out.cell_idx = Some(cell_idx);
    out.proof_idx = Some(proof_idx);
    out.proof_scope = Some("cell".to_string());
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
        resolve_squad_uri(&store, &uri)
    };
    match result {
        Ok(resolved) => json(200, &resolved),
        Err(e) => error(e.status, e.code, &e.message, vec![]),
    }
}

#[derive(Serialize)]
struct CellPaths {
    task_idx: usize,
    cell_idx: usize,
    worktree: Option<String>,
    project: Option<String>,
    /// The read-only "upstream" to show for this cell's git worktree —
    /// either the branch of a chained dependency (`upstream = "<<task:...>>"`,
    /// RAL-50) or the worktree's own git tracking branch (typically a
    /// non-worktree base branch, e.g. `main`). `null` when the cwd isn't a
    /// git worktree or no upstream can be resolved. See
    /// `crate::reviews::cell_upstream_display`.
    upstream: Option<String>,
}

/// For each cell in a squad, its worktree (`cwd`), the derived project root
/// (the shared git dir), and its display upstream, so the detail pane can show
/// them as distinct read-only fields (CCTL-148; upstream row added later). The
/// project/upstream are `null` when the cwd is not a git worktree. Computed on
/// demand (runs git per cell) rather than on the hot board path.
fn squad_worktrees(daemon: &Daemon, id: &str) -> Reply {
    let squad = match daemon.lock().get_squad(id) {
        Ok(r) => r,
        Err(e) => return store_error(&e),
    };
    let rows = match daemon.lock().cells_of(id) {
        Ok(r) => r,
        Err(e) => return store_error(&e),
    };
    let mut paths = Vec::new();
    for (ti, task) in squad.tasks.iter().enumerate() {
        for (si, s) in task.cells.iter().enumerate() {
            let project = s.cwd.as_deref().and_then(crate::reviews::project_root_of);
            let upstream = crate::reviews::cell_upstream_display(
                s.cwd.as_deref(),
                &rows,
                i64::try_from(ti).unwrap_or(0),
                i64::try_from(si).unwrap_or(0),
            );
            paths.push(CellPaths {
                task_idx: ti,
                cell_idx: si,
                worktree: s.cwd.clone(),
                project,
                upstream,
            });
        }
    }
    json(200, &paths)
}

/// The execution/transition log for a squad (CCTL-99). Latest 500 entries,
/// oldest-first. 404 if the squad does not exist.
fn squad_logs(daemon: &Daemon, id: &str) -> Reply {
    let store = daemon.lock();
    if let Err(e) = store.squad_state(id) {
        return store_error(&e);
    }
    match store.events_for_squad(id, 500) {
        Ok(events) => json(200, &events),
        Err(e) => store_error(&e),
    }
}

/// A squad's internal cell dependency graph (CLI_PARITY_PLAN.local.md
/// Phase 6). 404 if the squad does not exist; 500 on a dependency cycle (should
/// not happen for an already-submitted squad -- submission itself rejects
/// cycles -- but `plan::graph` is fallible so this stays honest).
fn squad_graph(daemon: &Daemon, id: &str) -> Reply {
    let store = daemon.lock();
    if let Err(e) = store.squad_state(id) {
        return store_error(&e);
    }
    let cells = match store.cells_of(id) {
        Ok(s) => s,
        Err(e) => return store_error(&e),
    };
    let tasks = match store.tasks_of(id) {
        Ok(t) => t,
        Err(e) => return store_error(&e),
    };
    match crate::plan::graph(&cells, &tasks) {
        Ok(g) => json(200, &g),
        Err(e) => error(500, "cycle", &e, vec![]),
    }
}

/// The cross-squad `[[default]] depends_on` gating graph (CLI_PARITY_PLAN.local.md
/// Phase 6, `ralphus graph --global`). `?all=1` includes terminal
/// (done/failed/cancelled) squads; otherwise only active (queued/pending/running)
/// squads are included, per plan Q5.
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
/// "show me everything" (no filters) and "show me this one squad/cell/
/// guardian's history" (`squad_id`/`cell_id`/`guardian_id` filters) — the
/// per-squad/per-guardian Logs sub-tab in the board is just this view with a
/// filter applied, not a separate implementation.
///
/// Query params: `source`, `scope`, `level`, `squad_id`, `guardian_id`,
/// `cell_id`, `task`, `q` (substring match on message), `since_ms`,
/// `until_ms`, `limit` (default 100, max 1000), `offset`, `sort` (`asc`/`desc`,
/// default `desc`) — plus `entity`, a single-string URI (RAL-155, see
/// `crate::entity_uri`) that addresses a squad/task/cell/proof/guardian
/// uniformly. `entity` is resolved into the equivalent `squad_id`/`task`/
/// `cell_id`/`guardian_id` filter fields (via `Store::task_name_at`/
/// `cell_sid_at`, since Cartographer's `task`/`cell_id` columns store
/// the task's *name*/cell's *sid*, not the index an entity URI addresses
/// by) and composes with any of those fields also given explicitly — an
/// explicit field always wins if both are present and disagree, since it's
/// the more specific ask.
/// Whether the caller currently resolves to a registered admin (RAL-332).
/// Unlike [`require_admin`], an unresolved or unknown identity is not an
/// error here -- it just means "hide admin-only rows", the same outcome a
/// resolved non-admin identity gets.
fn caller_is_admin(daemon: &Daemon, user_header: Option<&str>) -> bool {
    match current_user(daemon, user_header) {
        Ok(Some(name)) => daemon.lock().is_admin(&name).unwrap_or(false),
        _ => false,
    }
}

fn cartographer_query(daemon: &Daemon, query: &str, user_header: Option<&str>) -> Reply {
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
        squad_id: query_filter(query, "squad_id"),
        guardian_id: query_filter(query, "guardian_id"),
        cell_id: query_filter(query, "cell_id"),
        task: query_filter(query, "task"),
        q: query_filter(query, "q"),
        since_ms: query_param(query, "since_ms").and_then(|s| s.parse::<i64>().ok()),
        until_ms: query_param(query, "until_ms").and_then(|s| s.parse::<i64>().ok()),
        limit,
        offset,
        ascending: query_param(query, "sort") == Some("asc"),
        include_admin_only: caller_is_admin(daemon, user_header),
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
/// index-addressed task/cell down to the name/sid Cartographer's columns
/// actually store. Only overwrites a field the caller didn't already set
/// explicitly (see `cartographer_query`'s doc comment).
fn apply_entity_filter(
    store: &Store,
    filter: &mut crate::cartographer::CartographerFilter,
    entity: &crate::entity_uri::EntityUri,
) -> crate::store::Result<()> {
    use crate::entity_uri::EntityUri;
    if let Some(squad_id) = entity.squad_id() {
        filter.squad_id.get_or_insert_with(|| squad_id.to_string());
    }
    match entity {
        EntityUri::Squad { .. } => {}
        EntityUri::Task { squad_id, task_idx }
        | EntityUri::Proof {
            squad_id, task_idx, ..
        } => {
            if filter.task.is_none() {
                filter.task = store.task_name_at(squad_id, *task_idx)?;
            }
            if let EntityUri::Proof {
                squad_id,
                task_idx,
                cell_idx,
                ..
            } = entity
            {
                if filter.cell_id.is_none() && *cell_idx >= 0 {
                    filter.cell_id = store.cell_sid_at(squad_id, *task_idx, *cell_idx)?;
                }
            }
        }
        EntityUri::Cell {
            squad_id,
            task_idx,
            cell_idx,
        } => {
            if filter.task.is_none() {
                filter.task = store.task_name_at(squad_id, *task_idx)?;
            }
            if filter.cell_id.is_none() {
                filter.cell_id = store.cell_sid_at(squad_id, *task_idx, *cell_idx)?;
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
/// for a whole squad (RAL-155). A side effect of every call: the rendered text
/// is (best-effort) written to a temp file — see `crate::timeline::SquadTimeline::file_path`
/// — so the board's "generate to file, then display" button and this JSON
/// response come from the exact same generation, not two separate code paths.
fn squad_timeline(daemon: &Daemon, id: &str) -> Reply {
    match crate::timeline::build_squad_timeline(&daemon.lock(), id) {
        Ok(timeline) => json(200, &timeline),
        Err(e) => store_error(&e),
    }
}

/// Fetch one Cartographer row's full detail (used when the table's payload
/// column is truncated and the UI needs the whole JSON body).
fn cartographer_get(daemon: &Daemon, id: &str, user_header: Option<&str>) -> Reply {
    let Ok(row_id) = id.parse::<i64>() else {
        return error(400, "bad_request", "id must be an integer", vec![]);
    };
    // Resolved to an owned `Result` (not matched directly on `daemon.lock()`)
    // so the `MutexGuard` is dropped before `caller_is_admin` below takes the
    // same lock again -- a match guard's condition is evaluated while the
    // scrutinee's temporaries (including a `daemon.lock()` guard) are still
    // alive, so matching directly on `daemon.lock().cartographer_get(...)`
    // and calling `caller_is_admin` from a guard clause self-deadlocks on
    // this non-reentrant mutex.
    let result = daemon.lock().cartographer_get(row_id);
    match result {
        // RAL-332: an admin-only row is indistinguishable from a missing one
        // to a non-admin caller -- same 404, no separate "forbidden" leak of
        // its existence.
        Ok(Some(row)) if row.admin_only && !caller_is_admin(daemon, user_header) => {
            error(404, "not_found", "no such cartographer event", vec![])
        }
        Ok(Some(row)) => json(200, &row),
        Ok(None) => error(404, "not_found", "no such cartographer event", vec![]),
        Err(e) => store_error(&e),
    }
}

/// RAL-136: fetch one ghost (a cell's or review's handoff note) by its
/// owner URI. Lets any consumer explicitly query a ghost beyond the
/// automatic one-level-up lookup the scheduler already does at cell start.
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
/// independent of the dependency graph. `target_uri`'s `cell:`/`review:`
/// prefix determines the copy's `kind`/`squad_id`/`guardian_id`
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
    let Some((kind, squad_id, guardian_id)) = crate::ghost::parse_owner_uri(&req.target_uri) else {
        return error(
            400,
            "bad_request",
            "target_uri must be a cell:... or review:... URI",
            vec![],
        );
    };
    match daemon.lock().copy_ghost(
        &req.source_uri,
        &req.target_uri,
        kind,
        squad_id,
        guardian_id,
    ) {
        Ok(g) => json(200, &g),
        Err(e) => store_error(&e),
    }
}

#[derive(Serialize)]
struct MailboxRegisterResponse {
    client_id: String,
}

/// RAL-241: register a new mailbox client, returning a fresh `client_id`.
/// Called once by `ralphus quick-start watcher ...`/`ralphus mailbox check`,
/// which then persist the id locally so it's stable across restarts.
fn mailbox_register(daemon: &Daemon) -> Reply {
    match daemon.lock().register_mailbox_client() {
        Ok(client_id) => json(201, &MailboxRegisterResponse { client_id }),
        Err(e) => store_error(&e),
    }
}

/// RAL-241: list mailbox messages visible to `client_id`. Query params:
/// `unread` (`1`/`true` to restrict to messages this client has not yet
/// drained), `priority` (`urgent`/`high`/`normal`), and `category` (RAL-375,
/// e.g. `review` -- restricts to messages tagged with that category; omitted
/// means no category filtering).
fn mailbox_messages(daemon: &Daemon, client_id: &str, query: &str) -> Reply {
    let store = daemon.lock();
    match store.mailbox_client_exists(client_id) {
        Ok(true) => {}
        Ok(false) => return error(404, "not_found", "no such mailbox client", vec![]),
        Err(e) => return store_error(&e),
    }
    let unread_only = query_param(query, "unread").is_some_and(|v| v == "1" || v == "true");
    let priority = match query_param(query, "priority") {
        None => None,
        Some(p) => match crate::mailbox::MailboxPriority::parse(p) {
            Some(p) => Some(p),
            None => {
                return error(
                    400,
                    "bad_request",
                    "priority must be one of urgent/high/normal",
                    vec![],
                );
            }
        },
    };
    let category = query_param(query, "category");
    match store.mailbox_messages_for_client_filtered(client_id, unread_only, priority, category) {
        Ok(messages) => json(200, &messages),
        Err(e) => store_error(&e),
    }
}

#[derive(Deserialize, Default)]
struct MailboxDrainBody {
    #[serde(default)]
    message_ids: Option<Vec<String>>,
}

/// RAL-241: drain (mark read) mailbox messages for `client_id`. An empty/
/// omitted body drains every currently unread message; `{"message_ids": [...]}`
/// drains exactly those ids.
fn mailbox_drain(daemon: &Daemon, client_id: &str, body: &str) -> Reply {
    let req: MailboxDrainBody = if body.trim().is_empty() {
        MailboxDrainBody::default()
    } else {
        match serde_json::from_str(body) {
            Ok(b) => b,
            Err(_) => {
                return error(
                    400,
                    "bad_request",
                    "body must be JSON with an optional \"message_ids\" array of strings",
                    vec![],
                );
            }
        }
    };
    let store = daemon.lock();
    match store.mailbox_client_exists(client_id) {
        Ok(true) => {}
        Ok(false) => return error(404, "not_found", "no such mailbox client", vec![]),
        Err(e) => return store_error(&e),
    }
    match store.drain_mailbox_messages(client_id, req.message_ids.as_deref()) {
        Ok(drained) => json(200, &serde_json::json!({"drained": drained})),
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

/// Solo a task within a squad (RAL-157): pauses every other task in the squad
/// (their not-yet-started cells won't be dispatched) until un-soloed.
/// Returns the refreshed [`SquadView`] so the board reflects the new `soloed`
/// flag in the same round trip.
fn solo_task(daemon: &Daemon, id: &str, ti: &str) -> Reply {
    let Ok(task_idx) = ti.parse::<i64>() else {
        return error(400, "bad_request", "task index must be an integer", vec![]);
    };
    let guard = daemon.lock();
    match guard.solo_task(id, task_idx) {
        Ok(()) => match guard.get_squad(id) {
            Ok(squad) => json(200, &squad),
            Err(e) => store_error(&e),
        },
        Err(e) => store_error(&e),
    }
}

/// Un-solo a task (RAL-157) — the reverse of [`solo_task`]. Returns the
/// refreshed [`SquadView`].
fn unsolo_task(daemon: &Daemon, id: &str, ti: &str) -> Reply {
    let Ok(task_idx) = ti.parse::<i64>() else {
        return error(400, "bad_request", "task index must be an integer", vec![]);
    };
    let guard = daemon.lock();
    match guard.unsolo_task(id, task_idx) {
        Ok(()) => match guard.get_squad(id) {
            Ok(squad) => json(200, &squad),
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
    cell_idx: i64,
    #[serde(default)]
    proof_idx: i64,
    #[serde(default)]
    proof_scope: String,
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
    #[serde(default)]
    auto_compact_threshold: Option<String>,
    #[serde(default)]
    maximum_tool_output_tokens: Option<String>,
    #[serde(default)]
    system_prompt: Option<String>,
}

fn non_empty(v: Option<&String>) -> Option<&str> {
    v.map(String::as_str).filter(|s| !s.is_empty())
}

/// A squad label is split on `,` for multi-name filtering (RAL-323, see
/// `filter_and_sort_squads`), so a comma inside a label would make that
/// label impossible to filter for on its own. Returns an error `Reply` if
/// `label` contains a comma, naming the offending value.
fn reject_label_with_comma(label: &str) -> Option<Reply> {
    if label.contains(',') {
        Some(error(
            400,
            "invalid_label",
            &format!("squad label must not contain a comma: \"{label}\""),
            vec![],
        ))
    } else {
        None
    }
}

/// Decodes a `CellEdit` nullable field from the request body: `None` -- the
/// key was absent, the caller didn't mean to touch this field; `Some(None)`
/// -- present but empty, clear it; `Some(Some(v))` -- present and
/// non-empty, set it to `v`.
fn nullable_field_edit(v: Option<&String>) -> Option<Option<&str>> {
    v.map(|s| non_empty(Some(s)))
}

/// Like [`nullable_field_edit`] but for an integer-valued nullable field
/// (`auto_compact_threshold`, `maximum_tool_output_tokens`): `None` --
/// untouched; `Some(None)` -- present but empty, clear it; `Some(Some(n))`
/// -- present and non-empty, parsed to `n`. `Err` means the caller supplied
/// a non-empty value that isn't a valid integer, or isn't positive.
///
/// `field` names the field in the error message, since more than one field
/// decodes through here.
///
/// Positivity mirrors `core::validate`'s `check_positive_number`, which
/// rejects `0` and negatives for both fields at submit time -- without it an
/// edit could store a value no task file would have been accepted with.
fn nullable_i64_field_edit(
    field: &str,
    v: Option<&String>,
) -> std::result::Result<Option<Option<i64>>, String> {
    match v.map(|s| non_empty(Some(s))) {
        None => Ok(None),
        Some(None) => Ok(Some(None)),
        Some(Some(s)) => match s.parse::<i64>() {
            Ok(n) if n > 0 => Ok(Some(Some(n))),
            Ok(n) => Err(format!("{field}: must be a positive integer, got {n}")),
            Err(_) => Err(format!("{field}: invalid integer '{s}'")),
        },
    }
}

/// Reject a `maximum_tool_output_tokens` edit up front when the agent that
/// would run the edited node has no delivery mechanism for the cap (RAL-333),
/// mirroring `core::validate`'s submit-time rule rather than storing a value
/// the backend would silently never apply -- the same shape as the
/// `system_prompt` guard in `edit_squad`'s `"cell"` arm.
///
/// A custom agent profile name (not in `RESERVED_AGENT_NAMES`) is deferred the
/// way `core` defers it: resolving a profile's backend needs the cwd/config
/// this edit path doesn't have on hand.
fn reject_unsupported_maximum_tool_output_tokens(agent: &str) -> Option<Reply> {
    if ralphus_core::schema::RESERVED_AGENT_NAMES.contains(&agent)
        && !ralphus_core::schema::agent_supports_maximum_tool_output_tokens(agent)
    {
        return Some(error(
            400,
            "bad_request",
            &format!(
                "'maximum_tool_output_tokens' is only supported for the 'claude-code'/'codex'/'pi' agents right now, not '{agent}'"
            ),
            vec![],
        ));
    }
    None
}

/// Edit a squad's label, a task's name/project/model, a cell's fields, or a
/// proof step's model.
///
/// A `squad` edit (the label only) is purely cosmetic -- it isn't tied to any
/// node in the dependency graph or to the content executed, so it does not
/// touch execution state at all. A `task` edit resets the *whole* squad back
/// to Pending: its fields aren't tied to one node in the dependency graph
/// either, but (unlike the label) they can affect what actually runs, so
/// there's no narrower scope to preserve. A `cell` edit instead reuses
/// [`Store::restart_cell`]'s downstream-only reset -- only the edited
/// cell and whatever is downstream of it in the plan graph goes back to
/// Pending; upstream/sibling cells that already finished stay Done.
/// (Previously every edit kind, including `cell`, called the whole-squad
/// reset -- so editing one cell's prompt silently re-ran every
/// already-`done` upstream task in the squad too. See
/// [`wait_for_worker_stop`]'s doc comment for why any in-flight worker this
/// touches must be cancelled *and waited out* first.) A `proof` edit mirrors
/// the `cell` case's narrow-scope principle: it reuses
/// [`Store::restart_cell_proof`]/[`Store::restart_task_proof`] so only that
/// proof step (and any later step in its scope) goes back to Pending, rather
/// than resetting the whole squad or task.
fn edit_squad(daemon: &Daemon, id: &str, body: &str) -> Reply {
    let Ok(req) = serde_json::from_str::<EditBody>(body) else {
        return error(400, "bad_request", "invalid edit body", vec![]);
    };
    match req.kind.as_str() {
        "squad" => {
            if let Some(label) = non_empty(req.label.as_ref()) {
                if let Some(reply) = reject_label_with_comma(label) {
                    return reply;
                }
            }
            let store = daemon.lock();
            if let Err(e) = store.edit_squad_label(id, non_empty(req.label.as_ref())) {
                return store_error(&e);
            }
        }
        "task" => {
            let store = daemon.lock();
            let edit = crate::store::TaskEdit {
                name: non_empty(req.name.as_ref()),
                project: nullable_field_edit(req.project.as_ref()),
                model: nullable_field_edit(req.model.as_ref()),
            };
            if let Err(e) = store.edit_task_fields(id, req.task_idx, &edit) {
                return store_error(&e);
            }
            if let Err(e) = store.reset_squad_to_pending(id) {
                return store_error(&e);
            }
        }
        "cell" => {
            // Keep prompt XOR command: whichever of the two the caller
            // actually supplies wins and clears the other. If neither is
            // given, both are left as they already were.
            let (prompt, command) = if req.command.is_some() {
                (Some(None), nullable_field_edit(req.command.as_ref()))
            } else if req.prompt.is_some() {
                (nullable_field_edit(req.prompt.as_ref()), Some(None))
            } else {
                (None, None)
            };
            let auto_compact_threshold = match nullable_i64_field_edit(
                "auto_compact_threshold",
                req.auto_compact_threshold.as_ref(),
            ) {
                Ok(v) => v,
                Err(msg) => return error(400, "bad_request", &msg, vec![]),
            };
            let maximum_tool_output_tokens = match nullable_i64_field_edit(
                "maximum_tool_output_tokens",
                req.maximum_tool_output_tokens.as_ref(),
            ) {
                Ok(v) => v,
                Err(msg) => return error(400, "bad_request", &msg, vec![]),
            };
            let system_prompt = nullable_field_edit(req.system_prompt.as_ref());
            let new_agent = non_empty(req.agent.as_ref());
            // Reject up front (mirroring `core::validate::check_system_prompt`'s
            // submit-time rule) rather than silently storing a system_prompt the
            // cell's agent has no way to deliver -- RAL-341. Clearing the field
            // back out (`Some(None)`) needs no such check. A custom agent
            // profile name (not in `RESERVED_AGENT_NAMES`) is deferred the same
            // way `core` defers it, since resolving a profile's backend needs
            // the cell's cwd/config, which this edit path doesn't have on hand.
            if let Some(Some(_)) = system_prompt {
                let effective_agent = match new_agent {
                    Some(a) => a.to_string(),
                    None => match daemon.lock().get_cell_agent(id, req.task_idx, req.cell_idx) {
                        Ok(a) => a,
                        Err(e) => return store_error(&e),
                    },
                };
                if ralphus_core::schema::RESERVED_AGENT_NAMES.contains(&effective_agent.as_str())
                    && !ralphus_core::schema::agent_supports_system_prompt(&effective_agent)
                {
                    return error(
                        400,
                        "bad_request",
                        &format!(
                            "'system_prompt' is only supported for the 'claude-code'/'codex'/'pi' agents right now, not '{effective_agent}'"
                        ),
                        vec![],
                    );
                }
            }
            // Same up-front rejection as `system_prompt` above, for the same
            // reason: a cap the cell's agent can't deliver is silently inert.
            // Clearing the field back out (`Some(None)`) needs no check.
            if let Some(Some(_)) = maximum_tool_output_tokens {
                let effective_agent = match new_agent {
                    Some(a) => a.to_string(),
                    None => match daemon.lock().get_cell_agent(id, req.task_idx, req.cell_idx) {
                        Ok(a) => a,
                        Err(e) => return store_error(&e),
                    },
                };
                if let Some(reply) = reject_unsupported_maximum_tool_output_tokens(&effective_agent)
                {
                    return reply;
                }
            }
            let edit = crate::store::CellEdit {
                cwd: nullable_field_edit(req.cwd.as_ref()),
                agent: new_agent,
                model: nullable_field_edit(req.model.as_ref()),
                prompt,
                command,
                auto_compact_threshold,
                maximum_tool_output_tokens,
                system_prompt,
            };
            if let Err(e) = daemon
                .lock()
                .edit_cell_fields(id, req.task_idx, req.cell_idx, &edit)
            {
                return store_error(&e);
            }
            // Same double-dispatch guard the `restart_cell` HTTP handler
            // uses: stop every worker this is about to reset *before*
            // resetting it, and never hold the daemon lock across the
            // (up to 5s) wait.
            if let Ok(impact) =
                daemon
                    .lock()
                    .compute_cell_restart_impact(id, req.task_idx, req.cell_idx)
            {
                for dep in &impact.dirtied_squads {
                    daemon.cancellations.cancel(&dep.id);
                }
                for dep in &impact.dirtied_squads {
                    wait_for_worker_stop(daemon, &dep.id);
                }
            }
            daemon.cancellations.cancel(id);
            wait_for_worker_stop(daemon, id);
            if let Err(e) = daemon.lock().restart_cell(id, req.task_idx, req.cell_idx) {
                return store_error(&e);
            }
        }
        "proof" => {
            let maximum_tool_output_tokens = match nullable_i64_field_edit(
                "maximum_tool_output_tokens",
                req.maximum_tool_output_tokens.as_ref(),
            ) {
                Ok(v) => v,
                Err(msg) => return error(400, "bad_request", &msg, vec![]),
            };
            // A proof step carries its own agent, so it is gated on that
            // rather than on the owning cell's -- see the `"cell"` arm.
            if let Some(Some(_)) = maximum_tool_output_tokens {
                let agent = match daemon.lock().get_proof_agent(
                    id,
                    req.task_idx,
                    &req.proof_scope,
                    req.cell_idx,
                    req.proof_idx,
                ) {
                    Ok(a) => a,
                    Err(e) => return store_error(&e),
                };
                if let Some(reply) = reject_unsupported_maximum_tool_output_tokens(&agent) {
                    return reply;
                }
            }
            let edit = crate::store::ProofEdit {
                model: nullable_field_edit(req.model.as_ref()),
                maximum_tool_output_tokens,
            };
            if let Err(e) = daemon.lock().edit_proof_fields(
                id,
                req.task_idx,
                &req.proof_scope,
                req.cell_idx,
                req.proof_idx,
                &edit,
            ) {
                return store_error(&e);
            }
            daemon.cancellations.cancel(id);
            wait_for_worker_stop(daemon, id);
            let restarted = if req.proof_scope == "cell" {
                daemon
                    .lock()
                    .restart_cell_proof(id, req.task_idx, req.cell_idx, req.proof_idx)
            } else {
                daemon
                    .lock()
                    .restart_task_proof(id, req.task_idx, req.proof_idx)
            };
            match restarted {
                Ok(dirtied) => {
                    for dep_id in &dirtied {
                        daemon.cancellations.cancel(dep_id);
                        wait_for_worker_stop(daemon, dep_id);
                    }
                }
                Err(e) => return store_error(&e),
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
    match daemon.lock().get_squad(id) {
        Ok(squad) => json(200, &squad),
        Err(e) => store_error(&e),
    }
}

/// Re-squad a squad with its existing parameters by resetting it (and its tasks,
/// cells, and proofs) back to Pending so the scheduler picks it up again.
fn retry_squad(daemon: &Daemon, id: &str) -> Reply {
    let store = daemon.lock();
    if let Err(e) = store.squad_state(id) {
        return store_error(&e);
    }
    if let Err(e) = store.reset_squad_to_pending(id) {
        return store_error(&e);
    }
    match store.get_squad(id) {
        Ok(squad) => json(200, &squad),
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

/// Add/replace (`set`) and remove (`unset`) a squad's persistent
/// environment-variable overrides (RAL-150, ticket Q1/Q4). Does *not* itself
/// retry anything — the overrides just sit on the squad until the scheduler
/// next executes one of its cells/proof steps (see
/// `RunnerSpec::env_overrides` and `run_command_proof_capture`'s `env`
/// param); pair this with `POST .../retry` or `POST .../restart` to actually
/// re-squad something under the new values, exactly as the CLI's `ralphus
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
fn set_squad_env(daemon: &Daemon, id: &str, body: &str) -> Reply {
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
    for (key, value) in &req.set {
        if !crate::config::is_valid_env_value(value) {
            return error(
                400,
                "bad_request",
                &format!(
                    "invalid environment variable value for {key:?}: contains control characters"
                ),
                vec![],
            );
        }
    }
    let store = daemon.lock();
    if let Err(e) = store.squad_state(id) {
        return store_error(&e);
    }
    let result = match store.set_squad_env_overrides(id, &req.set, &req.unset) {
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
        message: "squad env overrides changed",
        scope: Some("squad"),
        squad_id: Some(id),
        guardian_id: None,
        cell_id: None,
        task: None,
        log_path: None,
        payload: serde_json::json!({"set": redacted_set, "unset": req.unset}),
        admin_only: false,
    });
    json(200, &result)
}

/// Shared body for the hierarchical-env-overrides extension's four
/// `POST .../env` endpoints (task / task-proof / cell / cell-proof):
/// parses `{set, unset}`, validates every key is a syntactically valid
/// env-var name, applies via `apply` (which does its own existence check —
/// every `set_*_env_overrides` store method resolves to `StoreError::NotFound`
/// when the owning task/cell doesn't exist, exactly like
/// `set_squad_env_overrides` does for a missing squad), and logs an
/// allowlist-redacted Cartographer record, mirroring [`set_squad_env`] in every
/// respect except which store method `apply` calls and which entity refs the
/// Cartographer entry carries.
fn set_env_overrides(
    daemon: &Daemon,
    body: &str,
    scope: &'static str,
    squad_id: Option<&str>,
    task: Option<&str>,
    cell_id: Option<&str>,
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
    for (key, value) in &req.set {
        if !crate::config::is_valid_env_value(value) {
            return error(
                400,
                "bad_request",
                &format!(
                    "invalid environment variable value for {key:?}: contains control characters"
                ),
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
        squad_id,
        guardian_id: None,
        cell_id,
        task,
        log_path: None,
        payload: serde_json::json!({"set": redacted_set, "unset": req.unset}),
        admin_only: false,
    });
    json(200, &result)
}

/// Add/replace/remove a task's own persistent environment-variable overrides
/// (hierarchical env overrides, extending RAL-150) — see
/// [`Store::resolve_cell_env_overrides`] for how this merges under the
/// squad's and over into every cell under this task.
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
/// task's own (task-scoped) proof steps — see
/// [`Store::resolve_task_proof_env_overrides`].
fn set_task_proof_env(daemon: &Daemon, id: &str, ti: &str, body: &str) -> Reply {
    let Ok(task_idx) = ti.parse::<i64>() else {
        return error(400, "bad_request", "task index must be an integer", vec![]);
    };
    let label = format!("t{task_idx}");
    set_env_overrides(
        daemon,
        body,
        "task-proof",
        Some(id),
        Some(&label),
        None,
        |store, set, unset| store.set_task_proof_env_overrides(id, task_idx, set, unset),
    )
}

/// Add/replace/remove a cell's own persistent environment-variable
/// overrides — see [`Store::resolve_cell_env_overrides`].
fn set_cell_env(daemon: &Daemon, id: &str, ti: &str, si: &str, body: &str) -> Reply {
    let (Ok(task_idx), Ok(cell_idx)) = (ti.parse::<i64>(), si.parse::<i64>()) else {
        return error(
            400,
            "bad_request",
            "task/cell index must be integers",
            vec![],
        );
    };
    let task_label = format!("t{task_idx}");
    let cell_label = format!("s{cell_idx}");
    set_env_overrides(
        daemon,
        body,
        "cell",
        Some(id),
        Some(&task_label),
        Some(&cell_label),
        |store, set, unset| store.set_cell_env_overrides(id, task_idx, cell_idx, set, unset),
    )
}

/// Add/replace/remove the environment-variable overrides applied only to a
/// cell's own (cell-scoped) proof steps — see
/// [`Store::resolve_cell_proof_env_overrides`].
fn set_cell_proof_env(daemon: &Daemon, id: &str, ti: &str, si: &str, body: &str) -> Reply {
    let (Ok(task_idx), Ok(cell_idx)) = (ti.parse::<i64>(), si.parse::<i64>()) else {
        return error(
            400,
            "bad_request",
            "task/cell index must be integers",
            vec![],
        );
    };
    let task_label = format!("t{task_idx}");
    let cell_label = format!("s{cell_idx}");
    set_env_overrides(
        daemon,
        body,
        "cell-proof",
        Some(id),
        Some(&task_label),
        Some(&cell_label),
        |store, set, unset| store.set_cell_proof_env_overrides(id, task_idx, cell_idx, set, unset),
    )
}

/// Add/replace/remove the environment-variable overrides applied to **one
/// individual task-scoped proof step** (RAL-191) — the narrowest layer, see
/// [`Store::resolve_task_proof_step_env_overrides`].
fn set_task_proof_step_env(daemon: &Daemon, id: &str, ti: &str, vi: &str, body: &str) -> Reply {
    let (Ok(task_idx), Ok(proof_idx)) = (ti.parse::<i64>(), vi.parse::<i64>()) else {
        return error(
            400,
            "bad_request",
            "task/proof index must be integers",
            vec![],
        );
    };
    let label = format!("t{task_idx}/proof#{proof_idx}");
    set_env_overrides(
        daemon,
        body,
        "task-proof-step",
        Some(id),
        Some(&label),
        None,
        |store, set, unset| {
            // Task-scoped proof rows are stored with `cell_idx = -1`.
            store.set_proof_step_env_overrides(id, task_idx, "task", -1, proof_idx, set, unset)
        },
    )
}

/// Add/replace/remove the environment-variable overrides applied to **one
/// individual cell-scoped proof step** (RAL-191) — see
/// [`Store::resolve_cell_proof_step_env_overrides`].
fn set_cell_proof_step_env(
    daemon: &Daemon,
    id: &str,
    ti: &str,
    si: &str,
    vi: &str,
    body: &str,
) -> Reply {
    let (Ok(task_idx), Ok(cell_idx), Ok(proof_idx)) =
        (ti.parse::<i64>(), si.parse::<i64>(), vi.parse::<i64>())
    else {
        return error(
            400,
            "bad_request",
            "task/cell/proof index must be integers",
            vec![],
        );
    };
    let task_label = format!("t{task_idx}/proof#{proof_idx}");
    let cell_label = format!("s{cell_idx}");
    set_env_overrides(
        daemon,
        body,
        "cell-proof-step",
        Some(id),
        Some(&task_label),
        Some(&cell_label),
        |store, set, unset| {
            store
                .set_proof_step_env_overrides(id, task_idx, "cell", cell_idx, proof_idx, set, unset)
        },
    )
}

/// Body for `POST /api/guardians/{id}/branches/{bid}/env` (RAL-191) and the
/// guardian-level `POST /api/guardians/{id}/build-env` /
/// `.../manual-checks-env` endpoints (RAL-203). Unlike the squad/task/cell
/// layers this one has three operations, because these values are *inherited*
/// (from a source cell, or from the combined worktree's branch union)
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
/// A review worktree is built from a cell's work and inherits that
/// cell's resolved environment; this endpoint layers per-branch changes on
/// top, which then apply to that branch's conflict resolver, final-proof
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
    for (key, value) in &req.set {
        if !crate::config::is_valid_env_value(value) {
            return error(
                400,
                "bad_request",
                &format!(
                    "invalid environment variable value for {key:?}: contains control characters"
                ),
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
        squad_id: None,
        guardian_id: Some(id),
        cell_id: None,
        task: Some(branch_id),
        log_path: None,
        payload: serde_json::json!({
            "set": redacted_set,
            "unset": req.unset,
            "clear": req.clear,
        }),
        admin_only: false,
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
/// which the combined worktree borrows in place of a source cell of its
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
    for (key, value) in &req.set {
        if !crate::config::is_valid_env_value(value) {
            return error(
                400,
                "bad_request",
                &format!(
                    "invalid environment variable value for {key:?}: contains control characters"
                ),
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
        squad_id: None,
        guardian_id: Some(id),
        cell_id: None,
        task: Some(section.label()),
        log_path: None,
        payload: serde_json::json!({
            "set": redacted_set,
            "unset": req.unset,
            "clear": req.clear,
        }),
        admin_only: false,
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

// -- RAL-324: read-only resolved-environment views --------------------------
//
// Every `POST .../env` route above gains a `GET` on the *same* path returning
// that surface's fully resolved environment, secret values masked. The board's
// env-viewer popup and `ralphus env` both read these, so neither client can
// become a redaction bypass for the other -- the daemon never emits a
// registered secret's value in the first place. See `crate::env_view`.

/// Serve one resolved-environment view, mapping any store failure the same way
/// every other read route does.
fn env_view_reply(
    daemon: &Daemon,
    build: impl FnOnce(&Store) -> std::result::Result<crate::env_view::EnvView, StoreError>,
) -> Reply {
    let store = daemon.lock();
    match build(&store) {
        Ok(view) => json(200, &view),
        Err(e) => store_error(&e),
    }
}

/// Parse a path index segment, or the 400 every `.../env` route returns for a
/// non-numeric one.
fn parse_index(raw: &str, what: &str) -> std::result::Result<i64, Reply> {
    raw.parse::<i64>().map_err(|_| {
        error(
            400,
            "bad_request",
            &format!("{what} must be an integer"),
            vec![],
        )
    })
}

/// `GET /api/squads/{id}/env` (RAL-324).
fn squad_env_view(daemon: &Daemon, id: &str) -> Reply {
    env_view_reply(daemon, |store| crate::env_view::squad_env(store, id))
}

/// `GET /api/squads/{id}/tasks/{ti}/env` (RAL-324).
fn task_env_view(daemon: &Daemon, id: &str, ti: &str) -> Reply {
    let task_idx = match parse_index(ti, "task index") {
        Ok(v) => v,
        Err(reply) => return reply,
    };
    env_view_reply(daemon, |store| {
        crate::env_view::task_env(store, id, task_idx)
    })
}

/// `GET /api/squads/{id}/tasks/{ti}/proof/env` (RAL-324).
fn task_proof_env_view(daemon: &Daemon, id: &str, ti: &str) -> Reply {
    let task_idx = match parse_index(ti, "task index") {
        Ok(v) => v,
        Err(reply) => return reply,
    };
    env_view_reply(daemon, |store| {
        crate::env_view::task_proof_env(store, id, task_idx)
    })
}

/// `GET /api/squads/{id}/tasks/{ti}/proof/{vi}/env` (RAL-324).
fn task_proof_step_env_view(daemon: &Daemon, id: &str, ti: &str, vi: &str) -> Reply {
    let task_idx = match parse_index(ti, "task index") {
        Ok(v) => v,
        Err(reply) => return reply,
    };
    let idx = match parse_index(vi, "proof index") {
        Ok(v) => v,
        Err(reply) => return reply,
    };
    env_view_reply(daemon, |store| {
        crate::env_view::proof_step_env(store, id, task_idx, "task", -1, idx)
    })
}

/// `GET /api/squads/{id}/cells/{ti}/{si}/env` (RAL-324).
fn cell_env_view(daemon: &Daemon, id: &str, ti: &str, si: &str) -> Reply {
    let (task_idx, cell_idx) = match (parse_index(ti, "task index"), parse_index(si, "cell index"))
    {
        (Ok(t), Ok(c)) => (t, c),
        (Err(reply), _) | (_, Err(reply)) => return reply,
    };
    env_view_reply(daemon, |store| {
        crate::env_view::cell_env(store, id, task_idx, cell_idx)
    })
}

/// `GET /api/squads/{id}/cells/{ti}/{si}/proof/env` (RAL-324).
fn cell_proof_env_view(daemon: &Daemon, id: &str, ti: &str, si: &str) -> Reply {
    let (task_idx, cell_idx) = match (parse_index(ti, "task index"), parse_index(si, "cell index"))
    {
        (Ok(t), Ok(c)) => (t, c),
        (Err(reply), _) | (_, Err(reply)) => return reply,
    };
    env_view_reply(daemon, |store| {
        crate::env_view::cell_proof_env(store, id, task_idx, cell_idx)
    })
}

/// `GET /api/squads/{id}/cells/{ti}/{si}/proof/{vi}/env` (RAL-324).
fn cell_proof_step_env_view(daemon: &Daemon, id: &str, ti: &str, si: &str, vi: &str) -> Reply {
    let (task_idx, cell_idx) = match (parse_index(ti, "task index"), parse_index(si, "cell index"))
    {
        (Ok(t), Ok(c)) => (t, c),
        (Err(reply), _) | (_, Err(reply)) => return reply,
    };
    let idx = match parse_index(vi, "proof index") {
        Ok(v) => v,
        Err(reply) => return reply,
    };
    env_view_reply(daemon, |store| {
        crate::env_view::proof_step_env(store, id, task_idx, "cell", cell_idx, idx)
    })
}

/// `GET /api/guardians/{id}/branches/{branch_id}/env` (RAL-324).
fn review_worktree_env_view(daemon: &Daemon, id: &str, branch_id: &str) -> Reply {
    env_view_reply(daemon, |store| {
        crate::env_view::review_worktree_env(store, id, branch_id)
    })
}

/// `GET /api/guardians/{id}/build-env`, `.../tests-env`, and
/// `.../manual-checks-env` (RAL-324). `tests-env` is the check gates' own
/// entry point; it deliberately resolves the build step's stored override
/// layer, because that is the environment the gates actually run under.
fn review_step_env_view(daemon: &Daemon, id: &str, step: crate::env_view::ReviewStep) -> Reply {
    env_view_reply(daemon, |store| {
        crate::env_view::review_step_env(store, id, step)
    })
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
    /// every cell downstream of the restart's target(s) within the squad,
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

/// Attach a restart note (if any) to the cell(s) a restart request
/// targets. Best-effort: by the time this runs the restart's own state
/// transition has already succeeded, so a failure to write the (purely
/// advisory) ghost note must never fail the restart response itself.
fn apply_restart_note(
    daemon: &Daemon,
    squad_id: &str,
    roots: &[(i64, i64)],
    req: &RestartNoteBody,
) {
    if let Some(note) = req.trimmed_note() {
        let _ = daemon
            .lock()
            .apply_restart_user_note(squad_id, roots, req.apply_to_all, &note);
    }
}

/// Block until `squad_id`'s in-flight worker (if any) has actually exited,
/// bounded so a wedged worker can never hang the HTTP thread forever.
///
/// `Cancellations::cancel` only flips a flag the worker polls at the top of
/// its dispatcher loop (~25ms) and its tmux capture-pane loop (`runner.rs`'s
/// `TMUX_POLL_INTERVAL`, 500ms) — cancelling alone does not make the worker
/// stop *before this function returns*. Without waiting here, a restart that
/// only calls `cancel()` and immediately resets the squad to `Pending` still
/// leaves a window (up to ~500ms) where the scheduler's next tick claims the
/// squad and spawns a *second* worker while the first is still mid-poll,
/// holding its own tmux session open — reproducing the exact double-dispatch
/// race `cancel()` was meant to close (two workers racing on the same
/// deterministic tmux/spec-file names; one's "kill stale cell before
/// resuming" cleanup tears down the other's still-live cell out from under
/// it, which then reports "runner produced no result file" since its python
/// process never got to finish). `Cancellations::remove` — called
/// unconditionally once `execute_squad_inner` returns, cancelled or not — is
/// the reliable "actually stopped" signal this polls for.
fn wait_for_worker_stop(daemon: &Daemon, squad_id: &str) {
    const TIMEOUT: Duration = Duration::from_secs(5);
    const POLL_INTERVAL: Duration = Duration::from_millis(50);
    let started = Instant::now();
    while daemon.cancellations.is_active(squad_id) {
        if started.elapsed() >= TIMEOUT {
            break;
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// Restart a whole squad and dirty every squad that depends on it (RAL-19).
///
/// `Store::restart_squad` forces the squad back to `Pending` unconditionally, even
/// if a worker thread from a still-in-flight previous attempt hasn't exited
/// yet — that thread has no idea it's been restarted underneath it. See
/// [`wait_for_worker_stop`] for why cancelling alone isn't enough: this stops
/// the old worker *and* waits for it to actually exit before the restart's
/// fresh claim can begin.
fn restart_squad(daemon: &Daemon, id: &str, body: &str) -> Reply {
    let note_req = RestartNoteBody::parse(body);
    // Stop every worker this restart is about to touch — the target squad and
    // everything it will dirty — *before* any of them are reset to Pending,
    // so no old worker can still be mid-poll when the fresh claim lands.
    let impact = daemon.lock().compute_squad_restart_impact(id);
    if let Ok(impact) = &impact {
        for dep in &impact.dirtied_squads {
            daemon.cancellations.cancel(&dep.id);
        }
        for dep in &impact.dirtied_squads {
            wait_for_worker_stop(daemon, &dep.id);
        }
    }
    daemon.cancellations.cancel(id);
    wait_for_worker_stop(daemon, id);
    // Bound to a `let` (not matched directly) so the `MutexGuard` `.lock()`
    // returns is dropped at the end of *this* statement -- matching on
    // `daemon.lock().restart_squad(id)` directly would keep that guard alive
    // for the whole arm body below (Rust extends a match scrutinee's
    // temporaries to the arm), and `apply_restart_note` below takes its own
    // lock, which would then deadlock against the still-held one.
    let restarted = daemon.lock().restart_squad(id);
    match restarted {
        Ok(dirtied) => {
            // A whole-squad restart's target IS every cell in the squad — there
            // is no narrower "exact target" to restrict to, so the note always
            // reaches all of them regardless of "Apply To All Children".
            if let Ok(impact) = &impact {
                let roots: Vec<(i64, i64)> =
                    impact.cells.iter().map(|s| (s.task_idx, s.idx)).collect();
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

/// Add a cross-squad dependency (RAL-105): `id` will not be scheduled until
/// `target_id` reaches a state that satisfies dependents (normally Done).
/// Appends to the same `[[default]] depends_on` list that `ralphus submit`
/// populates from TOML, so it is picked up by the existing whole-squad gating
/// in `Store::list_ready` — no new scheduling path. Rejects a self-reference
/// or a reference that would create a cycle in the cross-squad dependency
/// graph (409); an unknown `id`/`target_id` is 404.
fn add_dependency(daemon: &Daemon, id: &str, body: &str) -> Reply {
    let Ok(req) = serde_json::from_str::<AddDependencyBody>(body) else {
        return error(400, "bad_request", "invalid add-dependency body", vec![]);
    };
    if req.target_id.trim().is_empty() {
        return error(400, "bad_request", "target_id is required", vec![]);
    }
    let store = daemon.lock();
    if let Err(e) = store.add_squad_dependency(id, &req.target_id) {
        return store_error(&e);
    }
    match store.get_squad(id) {
        Ok(squad) => json(200, &squad),
        Err(e) => store_error(&e),
    }
}

/// Dry-run preview of [`restart_squad`]: computes the same downstream-impact
/// set the real restart would dirty, without mutating anything (RAL-104).
fn restart_squad_preview(daemon: &Daemon, id: &str) -> Reply {
    match daemon.lock().compute_squad_restart_impact(id) {
        Ok(impact) => json(200, &impact),
        Err(e) => store_error(&e),
    }
}

/// Restart a single cell (and its downstream), dirtying dependent squads.
///
/// See [`restart_squad`]/[`wait_for_worker_stop`]'s doc comments for why the
/// owning squad's worker sometimes must be cancelled *and waited out* first:
/// `Store::restart_cell` resets the squad to `Pending` too, and without
/// stopping a still-in-flight worker for the *restarted cell itself*
/// first, a restart mid-squad causes a double-dispatch race on the
/// deterministic per-cell tmux/spec-file names.
///
/// That cancellation is scoped to whether the restart target is actually
/// still running, though — not unconditional. A squad's worker thread drives
/// every one of that squad's cells concurrently over one shared
/// [`crate::cancel::CancelToken`] (see `scheduler::execute_squad_inner`), so
/// cancelling it to restart one *already-terminal* (done/failed) cell
/// used to also kill every other still-running, unrelated sibling cell in
/// the same squad as collateral damage (RAL-1xx) — restarting a batch squad's
/// one failed task could take down several of its perfectly healthy
/// siblings. Skipping the cancel when the target isn't `Running` avoids
/// that; `claim_ready` (`scheduler.rs`) now defers re-claiming a `Pending`
/// squad until any worker still registered for it exits naturally, so this
/// stays double-dispatch-safe even without the cancel.
fn restart_cell(daemon: &Daemon, id: &str, ti: &str, si: &str, body: &str) -> Reply {
    let note_req = RestartNoteBody::parse(body);
    let (Ok(task_idx), Ok(cell_idx)) = (ti.parse::<i64>(), si.parse::<i64>()) else {
        return error(
            400,
            "bad_request",
            "task/cell index must be integers",
            vec![],
        );
    };
    let impact = daemon
        .lock()
        .compute_cell_restart_impact(id, task_idx, cell_idx);
    if let Ok(impact) = &impact {
        for dep in &impact.dirtied_squads {
            daemon.cancellations.cancel(&dep.id);
        }
        for dep in &impact.dirtied_squads {
            wait_for_worker_stop(daemon, &dep.id);
        }
    }
    // Cancel only when something this restart will actually touch (the
    // target cell itself, or one of its own downstream cells within this
    // squad, per `impact.cells`) is genuinely still live -- checked at both
    // the cell-body and cell-scoped-proof granularity, not just the cell's
    // own coarse `NodeState`. RAL-288 bug this replaced: the old check
    // inferred "already stopped" purely from the *target cell's own*
    // `NodeState` (`!= Running` -> skip), which missed the target cell's
    // body reaching `Done` while its own cell-scoped proof step was still
    // being actively driven by the same worker -- that combination read as
    // "already stopped", skipped the cancel+wait entirely, and left the
    // still-running worker's squad-level cancellation-registry slot
    // orphaned once `Store::restart_cell` reset the cell underneath it,
    // permanently blocking every future re-claim of the squad (caught
    // live: a manual "Restart cell" while a proof step was mid-run left the
    // cell `pending` forever). The naive opposite fix -- cancel whenever
    // `cancellations.is_active(id)` at all, i.e. the whole squad's shared
    // token -- collaterally cancels a genuinely unrelated, still-running
    // sibling cell elsewhere in the same squad (regression caught by
    // `restart_cell_does_not_cancel_unrelated_sibling_cell_in_same_squad`
    // right above this), since that token has only squad-wide granularity.
    // Scoping the liveness check to `impact.cells` is what avoids both
    // mistakes at once.
    let target_still_active = match &impact {
        Ok(impact) => impact.cells.iter().any(|c| {
            let guard = daemon.lock();
            matches!(
                guard.cell_state(id, c.task_idx, c.idx),
                Ok(Some(NodeState::Running))
            ) || guard
                .cell_proof_running_from(id, c.task_idx, c.idx, 0)
                .unwrap_or(true)
        }),
        // Unknown/error stays on the safe (cancel) side, same as the
        // original behavior's intent.
        Err(_) => true,
    };
    if target_still_active {
        daemon.cancellations.cancel(id);
        wait_for_worker_stop(daemon, id);
    }
    // See `restart_squad`'s comment on why this is a `let` and not matched
    // directly -- `apply_restart_note` below takes its own lock, which would
    // deadlock against a guard still held by the match scrutinee.
    let restarted = daemon.lock().restart_cell(id, task_idx, cell_idx);
    match restarted {
        Ok(dirtied) => {
            apply_restart_note(daemon, id, &[(task_idx, cell_idx)], &note_req);
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

/// Dry-run preview of [`restart_cell`]: computes the same downstream-impact
/// set the real restart would dirty, without mutating anything (RAL-104).
fn restart_cell_preview(daemon: &Daemon, id: &str, ti: &str, si: &str) -> Reply {
    let (Ok(task_idx), Ok(cell_idx)) = (ti.parse::<i64>(), si.parse::<i64>()) else {
        return error(
            400,
            "bad_request",
            "task/cell index must be integers",
            vec![],
        );
    };
    match daemon
        .lock()
        .compute_cell_restart_impact(id, task_idx, cell_idx)
    {
        Ok(impact) => json(200, &impact),
        Err(e) => store_error(&e),
    }
}

/// Restart a whole task within a squad (and its downstream cells), dirtying
/// dependent squads (RAL-150) — the task-granularity counterpart of
/// [`restart_squad`]/[`restart_cell`], backing the CLI's generic `ralphus
/// retry <selector>` when the selector resolves to a task. See
/// [`wait_for_worker_stop`]'s doc comment for why the owning squad's worker
/// (and every squad this restart dirties) must be cancelled *and waited out*
/// first.
fn restart_task(daemon: &Daemon, id: &str, ti: &str, body: &str) -> Reply {
    let note_req = RestartNoteBody::parse(body);
    let Ok(task_idx) = ti.parse::<i64>() else {
        return error(400, "bad_request", "task index must be an integer", vec![]);
    };
    let impact = daemon.lock().compute_task_restart_impact(id, task_idx);
    if let Ok(impact) = &impact {
        for dep in &impact.dirtied_squads {
            daemon.cancellations.cancel(&dep.id);
        }
        for dep in &impact.dirtied_squads {
            wait_for_worker_stop(daemon, &dep.id);
        }
    }
    daemon.cancellations.cancel(id);
    wait_for_worker_stop(daemon, id);
    // See `restart_squad`'s comment on why this is a `let` and not matched
    // directly -- `apply_restart_note` below takes its own lock, which would
    // deadlock against a guard still held by the match scrutinee.
    let restarted = daemon.lock().restart_task(id, task_idx);
    match restarted {
        Ok(dirtied) => {
            // The task's own (directly-owned) cells are exactly the ones
            // in `impact.cells` with this task_idx — every cell
            // downstream of them (possibly in other tasks) is a "child" only
            // reached when "Apply To All Children" is set.
            if let Ok(impact) = &impact {
                let roots: Vec<(i64, i64)> = impact
                    .cells
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

/// Restart a single cell's proof steps from `vi` onwards: resets
/// cell-level proofs at index >= vi to Pending while leaving the cell
/// body Done so the scheduler re-runs only the affected proof steps.
///
/// Like [`restart_cell`], only cancels the squad's worker when one of the
/// targeted proof steps is actually still `Running` — see that function's
/// doc comment for why an unconditional cancel collaterally kills unrelated
/// sibling cells in the same squad (RAL-1xx).
fn restart_cell_proof(
    daemon: &Daemon,
    id: &str,
    ti: &str,
    si: &str,
    vi: &str,
    body: &str,
) -> Reply {
    let note_req = RestartNoteBody::parse(body);
    let (Ok(task_idx), Ok(cell_idx), Ok(proof_from)) =
        (ti.parse::<i64>(), si.parse::<i64>(), vi.parse::<i64>())
    else {
        return error(
            400,
            "bad_request",
            "task/cell/proof index must be integers",
            vec![],
        );
    };
    let target_running = daemon
        .lock()
        .cell_proof_running_from(id, task_idx, cell_idx, proof_from)
        .unwrap_or(true);
    if target_running {
        daemon.cancellations.cancel(id);
        wait_for_worker_stop(daemon, id);
    }
    // See `restart_squad`'s comment on why this is a `let` and not matched
    // directly -- `apply_restart_note` below takes its own lock, which would
    // deadlock against a guard still held by the match scrutinee.
    let restarted = daemon
        .lock()
        .restart_cell_proof(id, task_idx, cell_idx, proof_from);
    match restarted {
        Ok(dirtied) => {
            // No precomputed-impact endpoint exists for proof restarts, so
            // (unlike restart_squad/restart_cell above) dependents can only
            // be cancelled after the fact — a smaller, second-order version
            // of the same race remains for *their* worker, but the primary
            // squad this handler targets is fully protected.
            for dep_id in &dirtied {
                daemon.cancellations.cancel(dep_id);
                wait_for_worker_stop(daemon, dep_id);
            }
            apply_restart_note(daemon, id, &[(task_idx, cell_idx)], &note_req);
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

/// Restart a task's proof steps from `vi` onwards: resets task-scope proofs
/// at index >= vi to Pending while leaving all cells Done so the scheduler
/// re-runs only the affected task-level proofs.
///
/// Like [`restart_cell`], only cancels the squad's worker when one of the
/// targeted proof steps is actually still `Running` — see that function's
/// doc comment for why an unconditional cancel collaterally kills unrelated
/// sibling cells in the same squad (RAL-1xx).
fn restart_task_proof(daemon: &Daemon, id: &str, ti: &str, vi: &str, body: &str) -> Reply {
    let note_req = RestartNoteBody::parse(body);
    let (Ok(task_idx), Ok(proof_from)) = (ti.parse::<i64>(), vi.parse::<i64>()) else {
        return error(
            400,
            "bad_request",
            "task/proof index must be integers",
            vec![],
        );
    };
    let target_running = daemon
        .lock()
        .task_proof_running_from(id, task_idx, proof_from)
        .unwrap_or(true);
    if target_running {
        daemon.cancellations.cancel(id);
        wait_for_worker_stop(daemon, id);
    }
    // See `restart_squad`'s comment on why this is a `let` and not matched
    // directly -- both `cells_of` and `apply_restart_note` below take
    // their own lock, which would deadlock against a guard still held by the
    // match scrutinee.
    let restarted = daemon.lock().restart_task_proof(id, task_idx, proof_from);
    match restarted {
        Ok(dirtied) => {
            for dep_id in &dirtied {
                daemon.cancellations.cancel(dep_id);
                wait_for_worker_stop(daemon, dep_id);
            }
            // The task's own directly-owned cells -- mirrors restart_task
            // above (no separate impact-preview endpoint exists here, so
            // filter cells_of directly instead of an already-computed
            // impact set).
            // Same `let`-before-`if let` reasoning as above: `cells_of`'s
            // guard must be dropped before `apply_restart_note` takes its own.
            let cells = daemon.lock().cells_of(id);
            if let Ok(cells) = cells {
                let roots: Vec<(i64, i64)> = cells
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
pub(crate) fn query_param<'a>(query: &'a str, key: &str) -> Option<&'a str> {
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

/// The Live View "Show Debug Messages" checkbox's config-driven default
/// (RAL-232): the effective (global-under-project) `[live_view]` setting.
#[derive(Serialize)]
struct LiveViewConfigResponse {
    show_debug_messages_default: bool,
}

/// `GET /api/config/live-view` (RAL-232): lets the board initialize its
/// per-pane "Show Debug Messages" checkbox from the operator's configured
/// default rather than a hardcoded one. Purely a rendering preference --
/// unrelated to `capture_pane_reply`/Cartographer, which always capture and
/// persist both agent and ralphus-diagnostic lines regardless of this value.
fn live_view_config_reply() -> Reply {
    json(
        200,
        &LiveViewConfigResponse {
            show_debug_messages_default: crate::config::load_live_view_config()
                .show_debug_messages_default(),
        },
    )
}

/// The Simple task form's template picker (RAL-297): the effective
/// `[[templates]]` list (falling back to the built-in
/// [`crate::config::default_template`] when nothing valid is configured) and
/// the `[ui] new_task_default_tab` default. Cwd-independent, following
/// [`live_view_config_reply`]'s precedent -- there is no per-project layer
/// today since the daemon's HTTP handlers have no per-request project root
/// (see `crate::config::load_templates_config`'s doc comment).
#[derive(Serialize)]
struct TemplatesResponse {
    templates: Vec<crate::config::TemplateDef>,
    default_new_task_tab: String,
    /// `true` when zero valid `[[templates]]` are configured and `templates`
    /// is therefore just the built-in fallback -- the board uses this to
    /// disable the template picker and show an explanatory tooltip.
    using_fallback: bool,
}

fn templates_config_reply() -> Reply {
    let (templates, using_fallback) = crate::config::effective_templates();
    let default_new_task_tab = crate::config::load_ui_config()
        .new_task_default_tab()
        .to_string();
    json(
        200,
        &TemplatesResponse {
            templates,
            default_new_task_tab,
            using_fallback,
        },
    )
}

/// `GET /api/agents/catalog` (RAL-297): the Simple task form's agent picker
/// -- unlike `GET /api/agents`, this is deliberately cwd-independent (see
/// `crate::agent_catalog`'s module doc comment) and carries each agent's
/// known model list so the board can scope its model dropdown.
#[derive(Serialize)]
struct AgentCatalogResponse {
    agents: Vec<crate::agent_catalog::CatalogAgent>,
    default_agent: String,
}

fn agent_catalog_reply() -> Reply {
    json(
        200,
        &AgentCatalogResponse {
            agents: crate::agent_catalog::agent_catalog(),
            default_agent: ralphus_core::schema::DEFAULT_AGENT.to_string(),
        },
    )
}

/// A pane's current content for the live "read-only terminal" peek view
/// (RAL-102). `active` is `false` once the underlying tmux session has ended
/// (the cell/proof/resolver finished and the daemon tore it down, or it was
/// never started) — the board is expected to degrade gracefully in that case
/// rather than treat it as an error. When inactive, `content` is the
/// persisted last-pane-content snapshot (`crate::tmux::read_pane_snapshot`,
/// written by `SubprocessRunner::run_via_tmux_attempt` as each attempt ends)
/// if one exists — a read-only historical record of what the pane last
/// showed, not fresh output — or empty if the cell never ran under tmux
/// at all. The board distinguishes the two by whether `content` is non-empty.
///
/// `last_activity_ms` (RAL-170) is the Unix-epoch-milliseconds time the
/// daemon last observed *fresh* pane output (more lines than the previous
/// poll) for this cell — the liveness signal that lets the Live View
/// distinguish "still working, just quiet" from "hasn't produced a line in
/// a suspiciously long time". Tracked continuously and in-memory by
/// `SubprocessRunner::run_via_tmux_attempt` for every running tmux-wrapped
/// cell regardless of whether a Live View is open (see
/// `Store::note_live_activity`'s doc comment for the scale tradeoff behind
/// that decision) — so this is always fresh at the moment it's read, never
/// backfilled or stale from before the view opened. `None` when the cell
/// has produced no output yet, or has already ended (`active: false` here
/// too, in that case).
#[derive(Serialize)]
struct PaneResponse {
    active: bool,
    content: String,
    last_activity_ms: Option<i64>,
}

/// An inactive [`PaneResponse`] falling back to the persisted snapshot for
/// `session_name`, if one exists — shared by every "no live cell" branch
/// in [`capture_pane_reply`] so the read-only historical record degrades the
/// same way regardless of *why* the cell isn't live right now. Liveness
/// tracking (RAL-170) only covers currently-running cells (see
/// `Store::clear_live_activity`), so `last_activity_ms` is always `None`
/// here.
fn inactive_pane_reply(session_name: &str) -> Reply {
    json(
        200,
        &PaneResponse {
            active: false,
            content: strip_ralphus_pane_markers(
                &crate::tmux::read_pane_snapshot(session_name).unwrap_or_default(),
            ),
            last_activity_ms: None,
        },
    )
}

/// Strips ralphus's own `RALPHUS_EVENT:`/`RALPHUS_TMUX_DONE` marker lines out
/// of raw Live View pane text (RAL-288 Stage 5), so the board's peek pane is
/// pure agent output unconditionally, by construction — rather than the
/// previous design of serving the raw (marker-included) text and relying on
/// fragile client-side JS regex matching (deleted:
/// `librarian/assets/board.html`'s old `stripDebugLines`/
/// `isRalphusDebugLine`) to hide them, which both ate genuine agent output
/// starting with `ralphus [` and leaked anything not matching its three
/// hardcoded prefixes. `ralphus [...]`-prefixed diagnostic lines are not
/// filtered here at all, because Stage 5 also relocated every one the
/// runner used to print — `execute.rs::log_llm_start`/`log_llm_done`,
/// `main.rs`'s invocation/result-file-write-failure lines,
/// `agent_backend.rs`'s `llm-invoke` lines — to Cartographer-only, so
/// nothing should legitimately emit that prefix into a pane anymore.
///
/// Classifies line by line, matching the same trimmed-line-start rule the
/// deleted client-side version used, so a marker emitted mid-tool-call is
/// dropped without disturbing the agent lines immediately around it. The
/// live-agent tool/result-labeled lines (`[tool] ...`, `[result] ...`,
/// `[you] ...`, etc.) are runner-rendered translations of genuine agent
/// activity, not ralphus diagnostics, and are deliberately left alone.
#[must_use]
fn strip_ralphus_pane_markers(text: &str) -> String {
    text.lines()
        .filter(|line| {
            let trimmed = line.trim_start();
            !(trimmed.starts_with("RALPHUS_EVENT: ") || trimmed.starts_with("RALPHUS_TMUX_DONE"))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Capture-pane content for the tmux session keyed by `(squad_id, task,
/// cell_id)` (see `crate::tmux::session_name`), or an inactive
/// [`PaneResponse`] if no such cell currently exists. A tmux resolution
/// failure (no binary available at all) is the only case reported as a real
/// error, since that reflects a daemon configuration problem rather than
/// "this squad finished".
fn capture_pane_reply(
    daemon: &Daemon,
    squad_id: &str,
    task: &str,
    cell_id: &str,
    query: &str,
) -> Reply {
    let lines: u32 = query_param(query, "lines")
        .and_then(|s| s.parse().ok())
        .unwrap_or(2000);
    let tmux = match crate::tmux::Tmux::resolve() {
        Ok(t) => t,
        Err(e) => return error(500, "tmux_error", &e.to_string(), vec![]),
    };
    let name = crate::tmux::session_name(squad_id, task, cell_id);
    if !tmux.has_session(&name) {
        return inactive_pane_reply(&name);
    }
    match tmux.capture_pane(&name, lines) {
        Ok(content) => json(
            200,
            &PaneResponse {
                active: true,
                // RAL-247: scrub credential env-var values before serving the
                // live pane to the board/CLI. (A live pane is served before
                // it's ever persisted, so this is the one read path the
                // snapshot/terminal-log write-time redaction can't cover.)
                // RAL-288 Stage 5: also strip ralphus's own event/done marker
                // lines -- see `strip_ralphus_pane_markers`'s doc comment.
                content: strip_ralphus_pane_markers(&ralphus_core::redact::redact_secrets(
                    &content,
                )),
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
/// keyed by `(squad_id, task, cell_id)` (see `crate::tmux::session_name`),
/// ascending by attempt number. Empty (never an error) when the cell never
/// ran under tmux, or no attempt has finished writing its log yet. Shared by
/// every "Open Terminal Log" context (task cell, proof step, guardian
/// branch resolver, guardian manual-checks) the same way [`capture_pane_reply`]
/// already is.
fn terminal_log_attempts_reply(squad_id: &str, task: &str, cell_id: &str) -> Reply {
    let name = crate::tmux::session_name(squad_id, task, cell_id);
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
/// by `(squad_id, task, cell_id)`. `404` when that attempt was never
/// written, or has since been pruned/deleted (`crate::terminal_log::prune`,
/// or the owning squad/guardian's deletion).
fn terminal_log_attempt_content_reply(
    squad_id: &str,
    task: &str,
    cell_id: &str,
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
    let name = crate::tmux::session_name(squad_id, task, cell_id);
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
/// `(squad_id, task, cell_id)` — the "open terminal" button (RAL-102). When
/// the cell is currently live, attaches to it directly. Otherwise
/// degrades to [`open_readonly_snapshot_terminal`] — a finished
/// cell/proof/resolver has already had its tmux session torn down (see
/// `crate::runner::SubprocessRunner::run_via_tmux`), but its last pane
/// content may still be available as a persisted, read-only historical
/// record.
fn attach_tmux_terminal(squad_id: &str, task: &str, cell_id: &str) -> Reply {
    let tmux = match crate::tmux::Tmux::resolve() {
        Ok(t) => t,
        Err(e) => return error(500, "tmux_error", &e.to_string(), vec![]),
    };
    let name = crate::tmux::session_name(squad_id, task, cell_id);
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
/// genuinely nothing to show: the cell never ran under tmux at all, or
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
/// `pane_snapshots/<cell>.txt` under `state_dir()` — so a stray edit in
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

/// Open a terminal for a task cell — the "Open Terminal Log" / "Open
/// Agent" actions (RAL-102 follow-up).
///
/// `mode` (query param):
/// - `"open"` (default) — attach to the runner's tmux-wrapped cell, which
///   shows its log/event stream (this is the original RAL-102 behavior,
///   itself a replacement for the old `claude --resume` spawn).
/// - `"agent"` — spawn a real, interactive `claude --resume <id>` cell,
///   for actually continuing the conversation rather than watching its log.
fn open_terminal(daemon: &Daemon, id: &str, ti: &str, si: &str, query: &str) -> Reply {
    let (Ok(task_idx), Ok(cell_idx)) = (ti.parse::<i64>(), si.parse::<i64>()) else {
        return error(
            400,
            "bad_request",
            "task/cell index must be integers",
            vec![],
        );
    };
    if query_param(query, "mode") == Some("agent") {
        let (cwd, agent, agent_session_id) =
            match daemon.lock().get_cell_agent_resume(id, task_idx, cell_idx) {
                Ok(v) => v,
                Err(e) => return store_error(&e),
            };
        // RAL-288 Stage 6: while the cell is still genuinely running (local
        // host, a real prompt cell, a session id already pre-assigned -- see
        // `scheduler::assign_agent_session_id`), detach it cleanly first,
        // then open the *real* resume session -- safe now, since the
        // detach-and-wait below guarantees the original process is
        // genuinely gone before a second one touches the same conversation.
        // Backend-agnostic: the detach mechanism (killing the cell's tmux
        // session) and the resume command both already exist for
        // claude/codex/pi alike.
        let (machine, is_command_cell, state) =
            match daemon.lock().get_cell_input_gate(id, task_idx, cell_idx) {
                Ok(v) => v,
                Err(e) => return store_error(&e),
            };
        if state == crate::store::NodeState::Running && machine.is_none() && !is_command_cell {
            let Some(session_id) = agent_session_id.clone() else {
                return error(
                    409,
                    "no_claude_session",
                    "no resumable agent session recorded yet for this still-running cell",
                    vec![],
                );
            };
            let task = match daemon.lock().get_task_name(id, task_idx) {
                Ok(v) => v,
                Err(e) => return store_error(&e),
            };
            let cell_id = match daemon.lock().get_cell_id(id, task_idx, cell_idx) {
                Ok(v) => v,
                Err(e) => return store_error(&e),
            };
            return detach_and_open_agent(daemon, id, &cwd, &task, &cell_id, &agent, &session_id);
        }
        return open_agent_terminal(&cwd, Some(agent.as_str()), agent_session_id.as_deref());
    }
    let cell_id = match daemon.lock().get_cell_id(id, task_idx, cell_idx) {
        Ok(v) => v,
        Err(e) => return store_error(&e),
    };
    let task = match daemon.lock().get_task_name(id, task_idx) {
        Ok(v) => v,
        Err(e) => return store_error(&e),
    };
    attach_tmux_terminal(id, &task, &cell_id)
}

/// RAL-288 Stage 6: "resume automation" -- called once a human is done with
/// the real interactive session a detach opened, to hand the cell back to
/// unattended execution. Unlike a generic `restart_cell` (which always
/// starts fresh), this specifically continues the *same* conversation: it
/// marks the cell to resume its own recorded `agent_session_id` on its next
/// dispatch (`Store::set_force_resume_own_session`, consumed by
/// `scheduler::run_cell_worker`) before resetting it to `pending`, so the
/// human's work in the real session isn't silently discarded. A cell with
/// no recorded session has nothing to resume, and is rejected rather than
/// silently falling through to a fresh run.
fn resume_automation(daemon: &Daemon, id: &str, ti: &str, si: &str) -> Reply {
    let (Ok(task_idx), Ok(cell_idx)) = (ti.parse::<i64>(), si.parse::<i64>()) else {
        return error(
            400,
            "bad_request",
            "task/cell index must be integers",
            vec![],
        );
    };
    let agent_session_id = match daemon.lock().get_cell_agent_resume(id, task_idx, cell_idx) {
        Ok((_, _, sid)) => sid,
        Err(e) => return store_error(&e),
    };
    if agent_session_id.is_none() {
        return error(
            409,
            "no_claude_session",
            "this cell has no recorded agent session to resume",
            vec![],
        );
    }
    // Safety guard: this must only ever fire on a genuinely detached cell,
    // never one that's still actively running headlessly -- calling
    // `restart_cell` on a live cell would start a second process racing the
    // one already touching this exact conversation/worktree, the same
    // corruption risk `detach_and_open_agent`'s wait loop exists to avoid.
    let task = match daemon.lock().get_task_name(id, task_idx) {
        Ok(v) => v,
        Err(e) => return store_error(&e),
    };
    let cell_id = match daemon.lock().get_cell_id(id, task_idx, cell_idx) {
        Ok(v) => v,
        Err(e) => return store_error(&e),
    };
    let tmux = match crate::tmux::Tmux::resolve() {
        Ok(t) => t,
        Err(e) => return error(500, "tmux_error", &e.to_string(), vec![]),
    };
    let session_name = crate::tmux::session_name(id, &task, &cell_id);
    if tmux.has_session(&session_name) {
        return error(
            409,
            "still_running",
            "this cell is still actively running -- detach it first",
            vec![],
        );
    }
    // RAL-288: the human may not have closed the interactive resume session
    // (they clicked "Resume Automation" instead of exiting the CLI, or just
    // forgot it's open) -- kill it before handing the conversation back to a
    // fresh headless dispatch, or both processes would touch the exact same
    // `agent_session_id` at once, the same corruption risk `detach_and_open_
    // agent`'s wait loop exists to avoid on the way in. Best-effort: a
    // missing/already-gone session is not an error here.
    let resume_session_name = crate::tmux::session_name(id, &task, &format!("{cell_id}-resume"));
    let _ = tmux.kill_session(&resume_session_name);
    if let Err(e) = daemon
        .lock()
        .set_force_resume_own_session(id, task_idx, cell_idx)
    {
        return store_error(&e);
    }
    // Deliberately `resume_detached_cell`, not `restart_cell`: this is
    // handing one still-in-progress cell back to automation, not restarting
    // a squad. It must never touch the squad's own row, sibling tasks, or
    // cross-squad dependents -- none of that is relevant to "let this exact
    // conversation continue."
    if let Err(e) = daemon.lock().resume_detached_cell(id, task_idx, cell_idx) {
        return store_error(&e);
    }
    // The common case: this squad's own dispatcher worker is still alive
    // (driving this cell's siblings, or just polling) -- its own loop
    // reclaims a cell that goes back to `pending` under it without any
    // squad-level signal (see `scheduler::execute_squad_inner`'s Detached
    // revival). Only when that worker has already exited entirely (e.g. this
    // was the squad's last live cell) does nothing remain to notice the
    // reset cell at all, so the squad needs to be handed back to
    // `scheduler::tick` the same way `restart_cell` would -- but only that,
    // never the cell/task-level resets `restart_cell` also does.
    if !daemon.cancellations.is_active(id) {
        if let Err(e) = daemon.lock().set_squad_state(id, SquadState::Pending) {
            return store_error(&e);
        }
    }
    json(200, &OpenTerminalResponse { ok: true })
}

/// The live pane content of a task cell's tmux session, for the
/// auto-refreshing "read-only terminal" peek view (RAL-102).
fn cell_pane(daemon: &Daemon, id: &str, ti: &str, si: &str, query: &str) -> Reply {
    let (Ok(task_idx), Ok(cell_idx)) = (ti.parse::<i64>(), si.parse::<i64>()) else {
        return error(
            400,
            "bad_request",
            "task/cell index must be integers",
            vec![],
        );
    };
    let cell_id = match daemon.lock().get_cell_id(id, task_idx, cell_idx) {
        Ok(v) => v,
        Err(e) => return store_error(&e),
    };
    let task = match daemon.lock().get_task_name(id, task_idx) {
        Ok(v) => v,
        Err(e) => return store_error(&e),
    };
    capture_pane_reply(daemon, id, &task, &cell_id, query)
}

/// This entity's own chronologically-merged, current-attempt-only debug
/// stream (RAL-296) -- backs both the Live View pane's "Show Debug Messages"
/// checkbox and the "Open Terminal Log" attempt-history popup, for whichever
/// of the four terminal-log contexts `terminal_log_attempts_reply`'s doc
/// comment already lists (task cell, proof step, guardian branch resolver,
/// guardian manual-checks). Bare JSON array, ascending by time, matching
/// every other Cartographer-backed list endpoint's response shape. See
/// `crate::timeline::entity_debug_timeline`'s doc comment for the merge and
/// current-attempt-trim rules.
fn debug_events_reply(daemon: &Daemon, squad_id: &str, task: &str, cell_id: &str) -> Reply {
    match crate::timeline::entity_debug_timeline(&daemon.lock(), squad_id, task, cell_id) {
        Ok(entries) => json(200, &entries),
        Err(e) => store_error(&e),
    }
}

/// This task cell's own debug stream (RAL-288 Stage 5 / RAL-296).
fn cell_debug_events(daemon: &Daemon, id: &str, ti: &str, si: &str) -> Reply {
    let (Ok(task_idx), Ok(cell_idx)) = (ti.parse::<i64>(), si.parse::<i64>()) else {
        return error(
            400,
            "bad_request",
            "task/cell index must be integers",
            vec![],
        );
    };
    let cell_id = match daemon.lock().get_cell_id(id, task_idx, cell_idx) {
        Ok(v) => v,
        Err(e) => return store_error(&e),
    };
    let task = match daemon.lock().get_task_name(id, task_idx) {
        Ok(v) => v,
        Err(e) => return store_error(&e),
    };
    debug_events_reply(daemon, id, &task, &cell_id)
}

/// List a task cell's persisted historical terminal-log attempts (RAL-154)
/// — the board's "Open Terminal Log" button uses this to list attempts beyond
/// just the current live one.
fn cell_terminal_log_attempts(daemon: &Daemon, id: &str, ti: &str, si: &str) -> Reply {
    let (Ok(task_idx), Ok(cell_idx)) = (ti.parse::<i64>(), si.parse::<i64>()) else {
        return error(
            400,
            "bad_request",
            "task/cell index must be integers",
            vec![],
        );
    };
    let cell_id = match daemon.lock().get_cell_id(id, task_idx, cell_idx) {
        Ok(v) => v,
        Err(e) => return store_error(&e),
    };
    let task = match daemon.lock().get_task_name(id, task_idx) {
        Ok(v) => v,
        Err(e) => return store_error(&e),
    };
    terminal_log_attempts_reply(id, &task, &cell_id)
}

/// Content of one of a task cell's historical terminal-log attempts (RAL-154).
fn cell_terminal_log_attempt(
    daemon: &Daemon,
    id: &str,
    ti: &str,
    si: &str,
    attempt: &str,
) -> Reply {
    let (Ok(task_idx), Ok(cell_idx)) = (ti.parse::<i64>(), si.parse::<i64>()) else {
        return error(
            400,
            "bad_request",
            "task/cell index must be integers",
            vec![],
        );
    };
    let cell_id = match daemon.lock().get_cell_id(id, task_idx, cell_idx) {
        Ok(v) => v,
        Err(e) => return store_error(&e),
    };
    let task = match daemon.lock().get_task_name(id, task_idx) {
        Ok(v) => v,
        Err(e) => return store_error(&e),
    };
    terminal_log_attempt_content_reply(id, &task, &cell_id, attempt)
}

/// The `(squad_id, cell_id)` pair a `prompt`-kind proof step's tmux session
/// is keyed by — mirrors exactly how `scheduler.rs::squad_proofs` builds the
/// `RunnerSpec` for the same proof step (`squad_id` is the owning squad;
/// `cell_id` is `proof-{scope}-{proof_idx}`, matching the proof step's
/// own position within its `(task_idx, scope, cell_idx)` group — see the
/// doc comment on `Store::proof_specs`). The third leg of the key, `task`,
/// is fetched separately by each caller via `Store::get_task_name` since
/// `proof_idx` alone repeats across sibling tasks (RAL-102 collision bug —
/// see `crate::tmux::session_name`'s doc comment).
fn proof_tmux_keys(squad_id: &str, scope: &str, proof_idx: &str) -> (String, String) {
    (squad_id.to_string(), format!("proof-{scope}-{proof_idx}"))
}

/// Open a terminal for a `prompt`-kind proof step — the "Open Terminal Log"
/// / "Open Agent" actions (RAL-102 follow-up). `mode` query param semantics
/// mirror `open_terminal`'s doc comment.
#[allow(clippy::too_many_arguments)]
fn open_proof_terminal(
    daemon: &Daemon,
    id: &str,
    task_idx: &str,
    scope: &str,
    cell_idx: &str,
    proof_idx: &str,
    query: &str,
) -> Reply {
    let (Ok(task_idx_n), Ok(cell_idx_n), Ok(proof_idx_n)) = (
        task_idx.parse::<i64>(),
        cell_idx.parse::<i64>(),
        proof_idx.parse::<i64>(),
    ) else {
        return error(
            400,
            "bad_request",
            "task/cell/proof index must be integers",
            vec![],
        );
    };
    if let Err(e) = daemon.lock().proof_specs(id, task_idx_n, scope, cell_idx_n) {
        return store_error(&e);
    }
    if query_param(query, "mode") == Some("agent") {
        let (agent, agent_session_id) = match daemon.lock().get_proof_agent_session_id(
            id,
            task_idx_n,
            scope,
            cell_idx_n,
            proof_idx_n,
        ) {
            Ok(v) => v,
            Err(e) => return store_error(&e),
        };
        // A task-scope step's cwd is its task's first cell's (see
        // `Store::get_task_first_cell_cwd`'s doc comment); a cell-scope
        // step's cwd is that exact cell's.
        let cwd = if scope == "task" {
            daemon.lock().get_task_first_cell_cwd(id, task_idx_n)
        } else {
            daemon
                .lock()
                .get_cell_agent_resume(id, task_idx_n, cell_idx_n)
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
    let (squad_id, cell_id) = proof_tmux_keys(id, scope, proof_idx);
    attach_tmux_terminal(&squad_id, &task, &cell_id)
}

/// The live pane content of a `prompt`-kind proof step's tmux session, for
/// the auto-refreshing "read-only terminal" peek view (RAL-102).
#[allow(clippy::too_many_arguments)]
fn proof_pane(
    daemon: &Daemon,
    id: &str,
    task_idx: &str,
    scope: &str,
    cell_idx: &str,
    proof_idx: &str,
    query: &str,
) -> Reply {
    let (Ok(task_idx_n), Ok(cell_idx_n), Ok(_proof_idx_n)) = (
        task_idx.parse::<i64>(),
        cell_idx.parse::<i64>(),
        proof_idx.parse::<i64>(),
    ) else {
        return error(
            400,
            "bad_request",
            "task/cell/proof index must be integers",
            vec![],
        );
    };
    if let Err(e) = daemon.lock().proof_specs(id, task_idx_n, scope, cell_idx_n) {
        return store_error(&e);
    }
    let task = match daemon.lock().get_task_name(id, task_idx_n) {
        Ok(v) => v,
        Err(e) => return store_error(&e),
    };
    let (squad_id, cell_id) = proof_tmux_keys(id, scope, proof_idx);
    capture_pane_reply(daemon, &squad_id, &task, &cell_id, query)
}

/// List a `prompt`-kind proof step's persisted historical terminal-log
/// attempts (RAL-154).
fn proof_terminal_log_attempts(
    daemon: &Daemon,
    id: &str,
    task_idx: &str,
    scope: &str,
    cell_idx: &str,
    proof_idx: &str,
) -> Reply {
    let (Ok(task_idx_n), Ok(cell_idx_n), Ok(_proof_idx_n)) = (
        task_idx.parse::<i64>(),
        cell_idx.parse::<i64>(),
        proof_idx.parse::<i64>(),
    ) else {
        return error(
            400,
            "bad_request",
            "task/cell/proof index must be integers",
            vec![],
        );
    };
    if let Err(e) = daemon.lock().proof_specs(id, task_idx_n, scope, cell_idx_n) {
        return store_error(&e);
    }
    let task = match daemon.lock().get_task_name(id, task_idx_n) {
        Ok(v) => v,
        Err(e) => return store_error(&e),
    };
    let (squad_id, cell_id) = proof_tmux_keys(id, scope, proof_idx);
    terminal_log_attempts_reply(&squad_id, &task, &cell_id)
}

/// This `prompt`-kind proof step's own debug stream (RAL-296).
#[allow(clippy::too_many_arguments)]
fn proof_debug_events(
    daemon: &Daemon,
    id: &str,
    task_idx: &str,
    scope: &str,
    cell_idx: &str,
    proof_idx: &str,
) -> Reply {
    let (Ok(task_idx_n), Ok(cell_idx_n), Ok(_proof_idx_n)) = (
        task_idx.parse::<i64>(),
        cell_idx.parse::<i64>(),
        proof_idx.parse::<i64>(),
    ) else {
        return error(
            400,
            "bad_request",
            "task/cell/proof index must be integers",
            vec![],
        );
    };
    if let Err(e) = daemon.lock().proof_specs(id, task_idx_n, scope, cell_idx_n) {
        return store_error(&e);
    }
    let task = match daemon.lock().get_task_name(id, task_idx_n) {
        Ok(v) => v,
        Err(e) => return store_error(&e),
    };
    let (squad_id, cell_id) = proof_tmux_keys(id, scope, proof_idx);
    debug_events_reply(daemon, &squad_id, &task, &cell_id)
}

/// Content of one of a `prompt`-kind proof step's historical terminal-log
/// attempts (RAL-154).
#[allow(clippy::too_many_arguments)]
fn proof_terminal_log_attempt(
    daemon: &Daemon,
    id: &str,
    task_idx: &str,
    scope: &str,
    cell_idx: &str,
    proof_idx: &str,
    attempt: &str,
) -> Reply {
    let (Ok(task_idx_n), Ok(cell_idx_n), Ok(_proof_idx_n)) = (
        task_idx.parse::<i64>(),
        cell_idx.parse::<i64>(),
        proof_idx.parse::<i64>(),
    ) else {
        return error(
            400,
            "bad_request",
            "task/cell/proof index must be integers",
            vec![],
        );
    };
    if let Err(e) = daemon.lock().proof_specs(id, task_idx_n, scope, cell_idx_n) {
        return store_error(&e);
    }
    let task = match daemon.lock().get_task_name(id, task_idx_n) {
        Ok(v) => v,
        Err(e) => return store_error(&e),
    };
    let (squad_id, cell_id) = proof_tmux_keys(id, scope, proof_idx);
    terminal_log_attempt_content_reply(&squad_id, &task, &cell_id, attempt)
}

/// Open a terminal for a review branch.
///
/// `mode` (query param):
/// - `"open"` (default) — attach to the conflict-resolver's live tmux
///   cell (RAL-102 — replaces the old `claude --resume` spawn; `409` if
///   the resolver isn't currently running)
/// - `"worktree"` — open a plain shell in the branch's review worktree
///   directory; available as soon as the worktree exists, even before the
///   first resolver pass
/// - `"agent"` — spawn a real, interactive `claude --resume <id>` cell
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
    // This is available as soon as the worktree exists, before any cell ID.
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
    // RAL-149: while the branch is `proof_pending`, the live cell is the
    // dedicated final-proof call, not the fix pass -- attach to that one
    // instead.
    let (task, cell_id) = match resolver_task_and_cell_id(daemon, id, branch_id) {
        Ok(v) => v,
        Err(e) => return e,
    };
    attach_tmux_terminal(&format!("guardian-{id}"), task, &cell_id)
}

/// The `(task, cell_id)` pair addressing a review branch's conflict-resolver
/// tmux session -- live or historical (RAL-102, extended for RAL-149's
/// fix/proof split, RAL-192's stable-id keying, and RAL-298's
/// feedback-actioning session) -- shared by [`open_guardian_branch_terminal`]
/// (attach) and [`guardian_branch_pane`] (read-only peek) so the two never
/// drift on which call's cell they resolve to.
fn resolver_task_and_cell_id(
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
    // RAL-298: while reviewer feedback is being actioned, that resolver
    // agent's own session is the live one to show -- distinct from (and not
    // reachable through) the merge/rebase resolver below.
    if b.merge_status == "actioning" {
        return Ok((
            crate::guardian_merge::FEEDBACK_TASK,
            crate::guardian_merge::feedback_cell_id(&b.id),
        ));
    }
    if b.merge_status == "proof_pending" {
        return Ok((
            crate::guardian_merge::RESOLVER_PROOF_TASK,
            format!("resolver-proof-{}", b.id),
        ));
    }
    // RAL-149: `in_progress` means the merge/rebase resolver is live right
    // now -- always the right answer regardless of any earlier feedback
    // pass's snapshot recency.
    if b.merge_status == "in_progress" {
        return Ok((
            crate::guardian_merge::RESOLVER_TASK,
            format!("resolver-{}", b.id),
        ));
    }
    // Settled (done/conflict_resolved/failed/ready/pending): neither session
    // is live, so fall back to whichever of the merge/rebase resolver or a
    // feedback-actioning pass most recently actually ran (RAL-298) -- so a
    // completed feedback revision doesn't get shadowed by a stale, earlier
    // merge/rebase log the way it did before this resolved feedback's own
    // session at all.
    Ok(freshest_resolver_or_feedback(id, &b.id))
}

/// Picks whichever of the merge/rebase resolver's or a feedback-actioning
/// pass's persisted pane snapshot was written more recently for `branch_id`,
/// falling back to the merge/rebase resolver when neither has ever run (the
/// pre-RAL-298 default, and the common case for a branch feedback was never
/// given on). Both candidates are settled (never live) whenever this is
/// called -- see [`resolver_task_and_cell_id`]'s callers.
fn freshest_resolver_or_feedback(id: &str, branch_id: &str) -> (&'static str, String) {
    let resolver = (
        crate::guardian_merge::RESOLVER_TASK,
        format!("resolver-{branch_id}"),
    );
    let feedback = (
        crate::guardian_merge::FEEDBACK_TASK,
        crate::guardian_merge::feedback_cell_id(branch_id),
    );
    let squad = format!("guardian-{id}");
    let resolver_mtime =
        pane_snapshot_mtime(&crate::tmux::session_name(&squad, resolver.0, &resolver.1));
    let feedback_mtime =
        pane_snapshot_mtime(&crate::tmux::session_name(&squad, feedback.0, &feedback.1));
    pick_freshest(resolver, feedback, resolver_mtime, feedback_mtime)
}

/// Pure decision behind [`freshest_resolver_or_feedback`], split out so the
/// "which of the two ran more recently" logic is unit-testable without
/// touching the filesystem: `resolver`/`feedback` are the two candidate
/// values, `resolver_mtime`/`feedback_mtime` their snapshots' last-write
/// times (`None` when that candidate never wrote one). Ties and "neither has
/// a snapshot" both default to `resolver`, matching the pre-RAL-298 behavior
/// for a branch that has never had feedback actioned on it.
fn pick_freshest<T>(
    resolver: T,
    feedback: T,
    resolver_mtime: Option<std::time::SystemTime>,
    feedback_mtime: Option<std::time::SystemTime>,
) -> T {
    match (resolver_mtime, feedback_mtime) {
        (Some(r), Some(f)) if f > r => feedback,
        (None, Some(_)) => feedback,
        _ => resolver,
    }
}

/// Last-modified time of a tmux session's persisted pane snapshot (see
/// `crate::tmux::write_pane_snapshot`), or `None` when that session never
/// wrote one (never ran under tmux, or every attempt produced no output).
fn pane_snapshot_mtime(session_name: &str) -> Option<std::time::SystemTime> {
    std::fs::metadata(crate::tmux::pane_snapshot_path(session_name))
        .and_then(|m| m.modified())
        .ok()
}

/// Spawn a real, interactive resumed CLI cell (`claude --resume <id>
/// --dangerously-skip-permissions` or `codex resume <id>
/// --dangerously-bypass-approvals-and-sandbox`, depending on which agent the
/// cell actually ran under) in a new terminal window, rooted at `cwd` —
/// the "Open Agent" terminal action (RAL-102 follow-up). Unlike
/// `attach_tmux_terminal` (which re-attaches to the runner's own
/// tmux-wrapped subprocess and only shows its log/event stream — see
/// `claude_code_backend.py`/`codex_backend.py`'s streaming-JSON parsing),
/// this launches the actual CLI so a human can keep the conversation going
/// interactively, exactly as the pre-RAL-102 "resume" flow did. Permission
/// prompts are bypassed unconditionally, matching each backend's own
/// headless invocation, so the resumed conversation doesn't immediately
/// stall on a prompt the human has to notice and click through.
// Shared with `cli`'s cell/review-terminal commands, so both sides
// build the exact same resume command line -- see `ralphus_core::agent_resume`
// for the logic and its tests.
use ralphus_core::agent_resume::{
    is_codex_agent, is_pi_agent, resume_agent_command, resume_codex_agent_command,
    resume_pi_agent_command,
};

/// Builds the `pwsh -Command <resume line>` invocation shared by
/// [`open_agent_terminal`] (bare window, finished cell) and
/// [`open_agent_terminal_via_tmux`] (RAL-288 Stage 6: tmux-wrapped, just
/// detached from a live cell) -- both ultimately run the exact same real
/// CLI resume command, just handed to a different spawn mechanism.
fn resume_shell_invocation(agent: Option<&str>, agent_session_id: &str) -> (String, Vec<String>) {
    let shell_cmd = std::env::var("RALPHUS_SHELL_CMD").unwrap_or_else(|_| "pwsh".to_string());
    let command = if is_codex_agent(agent) {
        // Mirrors `RALPHUS_CODEX_COMMAND` in `codex_backend.py` — the same
        // override point resolves both the headless squad and this resumed
        // one to the same binary.
        let program =
            std::env::var("RALPHUS_CODEX_COMMAND").unwrap_or_else(|_| "codex".to_string());
        resume_codex_agent_command(&program, agent_session_id)
    } else if is_pi_agent(agent) {
        let program = std::env::var("RALPHUS_PI_COMMAND").unwrap_or_else(|_| "pi".to_string());
        resume_pi_agent_command(&program, agent_session_id)
    } else {
        // Mirrors `RALPHUS_CLAUDE_COMMAND` in `claude_code_backend.py` — the
        // same override point resolves both the headless squad and this
        // resumed one to the same binary.
        let program =
            std::env::var("RALPHUS_CLAUDE_COMMAND").unwrap_or_else(|_| "claude".to_string());
        resume_agent_command(&program, agent_session_id)
    };
    let shell_args = vec![
        "-NoExit".to_string(),
        "-NoProfile".to_string(),
        "-Command".to_string(),
        command,
    ];
    (shell_cmd, shell_args)
}

/// `POST /api/squads/{id}/cells/{ti}/{si}/terminal-ticket` (RAL-355 Phase
/// 10): mint a short-lived ticket for the terminal-relay WebSocket listener,
/// after checking eligibility up front so a client gets an actionable error
/// immediately rather than only after opening (and being refused by) the WS
/// connection itself.
///
/// Eligibility, checked in this order: the cell must be resolved (a real
/// squad/task/cell), its `machine` must be a *remote* one (this relay never
/// handles local cells -- "Open Agent" already works for those, and a
/// browser client has no way to see a window popped open on the daemon's own
/// desktop for a local cell anyway), its agent must be Claude Code (this
/// pass's only supported harness), and it must carry a resumable
/// `agent_session_id`.
fn mint_terminal_ticket_route(daemon: &Daemon, id: &str, ti: &str, si: &str) -> Reply {
    let (Ok(task_idx), Ok(cell_idx)) = (ti.parse::<i64>(), si.parse::<i64>()) else {
        return error(
            400,
            "bad_request",
            "task/cell index must be integers",
            vec![],
        );
    };
    let guard = daemon.lock();
    let (cwd, agent, agent_session_id) = match guard.get_cell_agent_resume(id, task_idx, cell_idx) {
        Ok(v) => v,
        Err(e) => return store_error(&e),
    };
    let (machine, _is_command_cell, state) = match guard.get_cell_input_gate(id, task_idx, cell_idx)
    {
        Ok(v) => v,
        Err(e) => return store_error(&e),
    };
    drop(guard);
    let _ = cwd;
    let Some(machine) = machine else {
        return error(
            400,
            "not_remote",
            "this cell runs locally -- use the existing Open Agent action instead of the \
             remote terminal relay",
            vec![],
        );
    };
    // Remote cells have no detach mechanism yet (unlike local `open-agent`'s
    // RAL-288 Stage 6 dance) -- a `Running` remote cell may still have a
    // live headless process writing to this exact `agent_session_id`
    // conversation, so opening a second interactive resume against it now
    // would race the same way a local cell's still-running branch exists to
    // prevent. Reject rather than risk corrupting the session; retry once
    // the cell finishes.
    if state == crate::store::NodeState::Running {
        return error(
            409,
            "still_running",
            "this cell is still running remotely -- wait for it to finish before opening a \
             terminal (the remote terminal relay has no detach mechanism yet)",
            vec![],
        );
    }
    if !ralphus_core::agent_resume::is_claude_agent(Some(agent.as_str())) {
        return error(
            400,
            "unsupported_agent",
            &format!(
                "the remote terminal relay only supports Claude Code cells this release; \
                 this cell's agent is {agent:?}"
            ),
            vec![],
        );
    }
    let Some(session_id) = agent_session_id else {
        return error(
            409,
            "no_claude_session",
            "no resumable agent session recorded yet for this cell",
            vec![],
        );
    };
    let port = daemon.terminal_relay_port();
    if port == 0 {
        return error(
            503,
            "relay_unavailable",
            "the terminal-relay listener is not running",
            vec![],
        );
    }
    let ticket = daemon.mint_terminal_ticket();
    let _ = session_id; // the WS connection itself re-derives it from the cell, not the ticket
    let _ = machine;
    json(
        200,
        &serde_json::json!({"ticket": ticket, "port": port, "path": "/terminal"}),
    )
}

fn open_agent_terminal(cwd: &str, agent: Option<&str>, agent_session_id: Option<&str>) -> Reply {
    let Some(cell_id) = agent_session_id else {
        return error(
            409,
            "no_claude_session",
            "no CLI-agent cell id recorded yet — it may not have started, \
             or didn't run under an agent that supports resume",
            vec![],
        );
    };
    // No leading `Set-Location ...;` here -- `cwd` is passed as its own
    // argument below so it goes through wt's `-d` flag / `current_dir`
    // instead of a semicolon `wt.exe` can't pass through to the agent (see
    // `spawn_in_terminal`'s doc comment).
    let (shell_cmd, shell_args) = resume_shell_invocation(agent, cell_id);
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

/// RAL-288 Stage 6: the real interactive resume session, launched *inside a
/// named tmux session* rather than a bare terminal window, once a
/// just-detached cell has been confirmed genuinely stopped (see
/// `detach_and_open_agent`, this function's only caller). tmux, not this
/// code, is what makes "close the client terminal, everything the human did
/// is still there on reattach" work -- it's tmux's own core behavior, not
/// something built for this. The session name is the cell's own
/// deterministic name with a `-resume` suffix, so it never collides with
/// the (now-dead) original cell session and `attach_tmux_terminal`'s
/// existing "attach a local terminal to a named session" mechanism can
/// reattach to it later exactly the way it already does for a plain cell.
/// Idempotent: if the resume session is already alive (a second "Open Agent"
/// click, or the human closed only their terminal window and not the tmux
/// session itself), this skips straight to spawning another local terminal
/// attached to it instead of trying to create a duplicate.
fn open_agent_terminal_via_tmux(
    cwd: &str,
    squad_id: &str,
    task: &str,
    cell_id: &str,
    agent: Option<&str>,
    agent_session_id: &str,
) -> Reply {
    let tmux = match crate::tmux::Tmux::resolve() {
        Ok(t) => t,
        Err(e) => return error(500, "tmux_error", &e.to_string(), vec![]),
    };
    let resume_session_name =
        crate::tmux::session_name(squad_id, task, &format!("{cell_id}-resume"));
    // RAL-288: idempotent by design -- a second "Open Agent" click on an
    // already-detached cell (the human never closed the resume session, or
    // just wants another terminal attached to it) must reattach to the
    // existing live session rather than trying to create a duplicate, which
    // psmux/tmux both reject outright.
    if !tmux.has_session(&resume_session_name) {
        let (shell_cmd, shell_args) = resume_shell_invocation(agent, agent_session_id);
        if let Err(e) = tmux.new_detached_session_with_command(
            &resume_session_name,
            cwd,
            &std::collections::BTreeMap::new(),
            &shell_cmd,
            &shell_args,
        ) {
            return error(500, "tmux_error", &e.to_string(), vec![]);
        }
    }
    let tmux_program = tmux.program().to_string();
    let mut attach_args: Vec<String> = tmux.prefix_args().to_vec();
    attach_args.extend([
        "attach-session".to_string(),
        "-t".to_string(),
        resume_session_name,
    ]);
    match spawn_in_terminal(
        None,
        &tmux_program,
        &attach_args,
        &std::collections::BTreeMap::new(),
    ) {
        Ok(()) => json(200, &OpenTerminalResponse { ok: true }),
        Err(msg) => error(500, "terminal_error", &msg, vec![]),
    }
}

/// How long [`detach_and_open_agent`] waits for a just-requested detach to
/// actually finish (the runner's poll loop notices within
/// `TMUX_POLL_INTERVAL` and kills the tmux session almost immediately, so
/// this is a generous upper bound, not an expected wait) before giving up
/// and telling the caller to retry, rather than opening a second session
/// against a conversation that might still be live.
const DETACH_WAIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
const DETACH_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(150);
/// Live-observed (RAL-288): creating a new tmux session immediately after
/// `has-session` first reports the old one gone can still transiently fail
/// ("psmux: failed to create session") -- psmux needs a beat to finish
/// releasing whatever it was still holding. A short unconditional settle
/// delay before creating the resume session clears this reliably; a bare
/// retry a few seconds later also worked when this was hit live, which is
/// roughly this order of magnitude.
const DETACH_SETTLE_DELAY: std::time::Duration = std::time::Duration::from_millis(400);

/// RAL-288 Stage 6: the "Open Agent" action for a cell that's still
/// genuinely running. Requests a clean detach via the shared
/// [`crate::cancel::Detachments`] registry, then blocks -- bounded by
/// [`DETACH_WAIT_TIMEOUT`], since this daemon's HTTP loop is otherwise
/// synchronous -- until the original tmux session is confirmed gone, before
/// opening the real resume session. "Detach requested" is not the same
/// guarantee as "detach happened"; two processes on one conversation
/// transcript is exactly what corrupts it, so the wait is not optional.
fn detach_and_open_agent(
    daemon: &Daemon,
    squad_id: &str,
    cwd: &str,
    task: &str,
    cell_id: &str,
    agent: &str,
    agent_session_id: &str,
) -> Reply {
    let tmux = match crate::tmux::Tmux::resolve() {
        Ok(t) => t,
        Err(e) => return error(500, "tmux_error", &e.to_string(), vec![]),
    };
    let session_name = crate::tmux::session_name(squad_id, task, cell_id);
    daemon.detachments_handle().cancel(&session_name);
    let deadline = std::time::Instant::now() + DETACH_WAIT_TIMEOUT;
    while tmux.has_session(&session_name) {
        if std::time::Instant::now() >= deadline {
            return error(
                503,
                "detach_in_progress",
                "still shutting down the live session -- try again in a moment",
                vec![],
            );
        }
        std::thread::sleep(DETACH_POLL_INTERVAL);
    }
    std::thread::sleep(DETACH_SETTLE_DELAY);
    open_agent_terminal_via_tmux(cwd, squad_id, task, cell_id, Some(agent), agent_session_id)
}

/// The live pane content of a review branch's conflict-resolver tmux
/// cell, for the auto-refreshing "read-only terminal" peek view in the
/// Review tab (RAL-102).
fn guardian_branch_pane(daemon: &Daemon, id: &str, branch_id: &str, query: &str) -> Reply {
    // RAL-192: the tmux session name is keyed on the branch's stable id (see
    // `resolver_task_and_cell_id`), so it resolves correctly here even if
    // the branch's stack position has since changed -- including falling
    // back to the persisted historical snapshot (`inactive_pane_reply`, via
    // `capture_pane_reply`) once the live cell is gone. RAL-149:
    // `resolver_task_and_cell_id` also picks the dedicated final-proof
    // cell while the branch is `proof_pending`, so this peek view tracks
    // whichever call is actually live.
    let (task, cell_id) = match resolver_task_and_cell_id(daemon, id, branch_id) {
        Ok(v) => v,
        Err(e) => return e,
    };
    capture_pane_reply(daemon, &format!("guardian-{id}"), task, &cell_id, query)
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
/// rather than persisted -- mirrors `squad_worktrees`'s "computed on demand,
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

/// List a review branch's currently-relevant resolver session's persisted
/// historical terminal-log attempts (RAL-154; RAL-298: shares
/// [`resolver_task_and_cell_id`] so this always lists attempts for the same
/// session the pane/attach endpoints show -- the merge/rebase resolver, the
/// final-proof pass, or a feedback-actioning pass, whichever last actually
/// ran on this branch -- instead of the merge/rebase resolver
/// unconditionally, which used to leave a completed feedback pass's own
/// attempts unreachable here).
fn guardian_branch_terminal_log_attempts(daemon: &Daemon, id: &str, branch_id: &str) -> Reply {
    let (task, cell_id) = match resolver_task_and_cell_id(daemon, id, branch_id) {
        Ok(v) => v,
        Err(e) => return e,
    };
    terminal_log_attempts_reply(&format!("guardian-{id}"), task, &cell_id)
}

/// Content of one of [`guardian_branch_terminal_log_attempts`]'s persisted
/// terminal-log attempts (RAL-154, RAL-298).
fn guardian_branch_terminal_log_attempt(
    daemon: &Daemon,
    id: &str,
    branch_id: &str,
    attempt: &str,
) -> Reply {
    let (task, cell_id) = match resolver_task_and_cell_id(daemon, id, branch_id) {
        Ok(v) => v,
        Err(e) => return e,
    };
    terminal_log_attempt_content_reply(&format!("guardian-{id}"), task, &cell_id, attempt)
}

/// This review branch's conflict-resolver's own debug stream (RAL-296).
fn guardian_branch_debug_events(daemon: &Daemon, id: &str, branch_id: &str) -> Reply {
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
    debug_events_reply(
        daemon,
        &format!("guardian-{id}"),
        crate::guardian_merge::RESOLVER_TASK,
        &format!("resolver-{position}"),
    )
}

/// Open a terminal for a review's manual-checks generation pass (RAL-88
/// follow-up). `mode` (query param) mirrors `open_guardian_branch_terminal`'s:
/// - `"open"` (default) — attach to the generation run's live tmux session;
///   `409` if it isn't currently running.
/// - `"agent"` — spawn a real, interactive resumed cell resuming that
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

/// This review's manual-checks generation pass's own debug stream (RAL-296).
fn guardian_manual_checks_debug_events(daemon: &Daemon, id: &str) -> Reply {
    if daemon.lock().get_guardian(id).is_err() {
        return error(404, "not_found", "no such guardian", vec![]);
    }
    debug_events_reply(
        daemon,
        &format!("guardian-{id}"),
        crate::guardian_merge::MANUAL_COMMANDS_TASK,
        crate::guardian_merge::MANUAL_COMMANDS_SESSION,
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
/// cell, whose environment was fixed at spawn time).
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
        wt_cmd.args(["--startingDirectory", dir]);
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
/// `squad_id`, as a robust backstop alongside `Cancellations::cancel`.
///
/// `Cancellations::cancel` only trips a flag a worker's polling loop checks
/// on its own schedule (up to `TMUX_POLL_INTERVAL`, currently 500ms) — and
/// is a silent no-op if that squad's worker thread isn't currently alive at
/// all (already finished, or never started). Either way, a tmux pane that's
/// actually still running would be left orphaned, alive, and unresponsive to
/// "cancelled" until something else notices. This call doesn't wait on any
/// worker at all: it asks psmux directly what's running under this squad's
/// deterministic `ralphus_<squad_id>_...` cell-name prefix and kills it
/// immediately, regardless of whether the token mechanism is working.
fn kill_squad_tmux_sessions(squad_id: &str) -> usize {
    let Ok(tmux) = crate::tmux::Tmux::resolve() else {
        return 0;
    };
    tmux.kill_sessions_with_prefix(&format!("ralphus_{squad_id}_"))
}

/// The guardian-cell counterpart to [`kill_squad_tmux_sessions`]: kills
/// every live tmux session belonging to guardian `guardian_id`. Guardian
/// resolver/PR-description/summary cells are named via
/// `tmux::session_name(&format!("guardian-{guardian_id}"), ...)` (see the
/// `RunnerSpec` construction sites in `guardian_merge.rs`), so the same
/// `ralphus_{squad_id}_` scoping `kill_squad_tmux_sessions` uses applies here
/// with that formatted id in place of a plain squad id.
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

/// Cancel a squad and cascade the cancellation to every squad transitively
/// dependent on it (RAL-116). Always available and idempotent regardless of
/// the squad's current state — even an already-terminal squad is (re-)cancelled,
/// so it can never be picked up again by another trigger (a restart,
/// cross-squad gating, etc).
fn cancel(daemon: &Daemon, id: &str) -> Reply {
    match daemon.lock().cancel_squad(id, false) {
        Ok(impact) => {
            // The store has flipped every affected squad (and their non-terminal
            // nodes) to cancelled; now stop each one's worker thread and kill
            // its subprocess, if it has one in flight.
            for r in &impact.squads {
                daemon.cancellations.cancel(&r.id);
                kill_squad_tmux_sessions(&r.id);
            }
            json(
                200,
                &CancelResponse {
                    state: "cancelled",
                    cancelled: impact.squads.into_iter().map(|r| r.id).collect(),
                },
            )
        }
        Err(e) => store_error(&e),
    }
}

/// Dry-run preview of [`cancel`]: computes the same cascade-cancel impact set
/// the real cancel would affect, without mutating anything (RAL-116).
fn cancel_squad_preview(daemon: &Daemon, id: &str) -> Reply {
    match daemon.lock().cancel_squad(id, true) {
        Ok(impact) => json(200, &impact),
        Err(e) => store_error(&e),
    }
}

#[derive(Deserialize, Default)]
struct ShutdownBody {
    /// When true, every non-terminal squad and cancellable guardian is also
    /// marked `cancelled` in the store before the daemon exits, so history
    /// records an intentional stop and nothing auto-resumes on next
    /// `ralphus-daemon serve`. When false (the default), DB state is left
    /// alone — killed processes' rows stay `running`/`merging`/etc, and the
    /// existing crash-recovery path (`Store::recover_orphaned_squads` at
    /// `serve()` startup, `scheduler::recover_interrupted_reviews` at
    /// scheduler startup) resumes them on the next start.
    #[serde(default)]
    auto_cancel: bool,
}

#[derive(Serialize)]
struct ShutdownResponse {
    state: &'static str,
    auto_cancel: bool,
    cancelled_squads: Vec<String>,
    cancelled_guardians: Vec<String>,
}

/// Kill every process this daemon has spawned — cell/proof/review/summary
/// subprocesses, tmux panes, everything — and request that the daemon exit
/// once this response is sent.
///
/// Unconditionally (regardless of `auto_cancel`): trips every registered
/// cancellation token (stops scheduler-squad cells' subprocesses via their
/// existing polling loop) and kills every tmux session belonging to a
/// currently-known squad or guardian, one entity at a time, via
/// [`kill_squad_tmux_sessions`] / [`kill_guardian_tmux_sessions`] — each
/// scoped to that single entity's own deterministic cell-name prefix
/// (see `tmux::session_name`). With `auto_cancel: true`, also
/// cascade-cancels every non-terminal squad and cancellable guardian so their
/// DB state reflects an intentional stop rather than being left for
/// crash-recovery to resume.
///
/// Previously called `tmux::reap_orphaned_sessions_at_startup` here instead
/// — a machine-wide, unscoped kill of *every* `ralphus_`-prefixed tmux.exe
/// process, sound only at actual daemon startup (its own doc comment says
/// so). Called from this live endpoint, it matches — and kills — the very
/// pane this handler happens to be running in whenever that pane's own
/// cell name starts with `ralphus_` (which every real daemon-spawned
/// cell's name does, by construction). See `PSMUX_CRASH_NOTES.local.md`'s
/// "SOLVED" section for the full incident.
///
/// Never terminates the process itself — see the `shutdown` field's doc
/// comment on [`Daemon`] for why that has to happen outside `route()`.
fn shutdown(daemon: &Daemon, body: &str) -> Reply {
    let req: ShutdownBody = serde_json::from_str(body).unwrap_or_default();

    daemon.cancellations.cancel_all();

    let squads = daemon.lock().list_squads().unwrap_or_default();
    let guardians = daemon.lock().list_guardians().unwrap_or_default();
    let tmux_killed: usize = squads
        .iter()
        .map(|r| kill_squad_tmux_sessions(&r.id))
        .sum::<usize>()
        + guardians
            .iter()
            .map(|g| kill_guardian_tmux_sessions(&g.id))
            .sum::<usize>();

    let mut cancelled_squads = Vec::new();
    let mut cancelled_guardians = Vec::new();
    if req.auto_cancel {
        let active_squad_ids: Vec<String> = squads
            .into_iter()
            .filter(|r| !SquadState::parse(&r.state).is_some_and(SquadState::is_terminal))
            .map(|r| r.id)
            .collect();
        for id in active_squad_ids {
            if let Ok(impact) = daemon.lock().cancel_squad(&id, false) {
                for r in &impact.squads {
                    daemon.cancellations.cancel(&r.id);
                }
                cancelled_squads.extend(impact.squads.into_iter().map(|r| r.id));
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
            squad_id: None,
            guardian_id: None,
            cell_id: None,
            task: None,
            log_path: None,
            payload: serde_json::json!({
                "auto_cancel": req.auto_cancel,
                "tmux_killed": tmux_killed,
                "cancelled_squads": cancelled_squads.len(),
                "cancelled_guardians": cancelled_guardians.len(),
            }),
            admin_only: false,
        });
    crate::rlog!(
        WARNING,
        "ralphus [daemon] shutdown requested: auto_cancel={} tmux_killed={tmux_killed} \
         cancelled_squads={} cancelled_guardians={}",
        req.auto_cancel,
        cancelled_squads.len(),
        cancelled_guardians.len()
    );

    daemon.request_shutdown();
    json(
        200,
        &ShutdownResponse {
            state: "stopping",
            auto_cancel: req.auto_cancel,
            cancelled_squads,
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
    cell_idx: i64,
    #[serde(default)]
    proof_idx: i64,
    #[serde(default)]
    proof_scope: String,
    state: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct StopTarget {
    pane_name: String,
    ghost_task_idx: i64,
    ghost_cell_idx: i64,
}

#[derive(Default)]
struct StopCascadePlan {
    state_tasks: BTreeSet<i64>,
    state_cells: BTreeSet<(i64, i64)>,
    state_proofs: BTreeSet<(i64, String, i64, i64)>,
    stop_cells: BTreeSet<(i64, i64)>,
    stop_proofs: BTreeSet<(i64, String, i64, i64)>,
}

impl StopCascadePlan {
    fn add_task_state(&mut self, task_idx: i64) {
        self.state_tasks.insert(task_idx);
    }

    fn add_cell(&mut self, task_idx: i64, cell_idx: i64) {
        self.state_cells.insert((task_idx, cell_idx));
        self.stop_cells.insert((task_idx, cell_idx));
    }

    fn add_proof(&mut self, task_idx: i64, scope: &str, cell_idx: i64, proof_idx: i64) {
        let key = (task_idx, scope.to_string(), cell_idx, proof_idx);
        self.state_proofs.insert(key.clone());
        self.stop_proofs.insert(key);
    }

    fn state_requests(&self) -> Vec<SetStatusBody> {
        let mut reqs = Vec::new();
        for &task_idx in &self.state_tasks {
            reqs.push(SetStatusBody {
                kind: "task".to_string(),
                task_idx,
                cell_idx: 0,
                proof_idx: 0,
                proof_scope: String::new(),
                state: "cancelled".to_string(),
            });
        }
        for &(task_idx, cell_idx) in &self.state_cells {
            reqs.push(SetStatusBody {
                kind: "cell".to_string(),
                task_idx,
                cell_idx,
                proof_idx: 0,
                proof_scope: String::new(),
                state: "cancelled".to_string(),
            });
        }
        for (task_idx, scope, cell_idx, proof_idx) in &self.state_proofs {
            reqs.push(SetStatusBody {
                kind: "proof".to_string(),
                task_idx: *task_idx,
                cell_idx: *cell_idx,
                proof_idx: *proof_idx,
                proof_scope: scope.clone(),
                state: "cancelled".to_string(),
            });
        }
        reqs
    }

    fn stop_requests(&self) -> Vec<SetStatusBody> {
        let mut reqs = Vec::new();
        for &(task_idx, cell_idx) in &self.stop_cells {
            reqs.push(SetStatusBody {
                kind: "cell".to_string(),
                task_idx,
                cell_idx,
                proof_idx: 0,
                proof_scope: String::new(),
                state: "cancelled".to_string(),
            });
        }
        for (task_idx, scope, cell_idx, proof_idx) in &self.stop_proofs {
            reqs.push(SetStatusBody {
                kind: "proof".to_string(),
                task_idx: *task_idx,
                cell_idx: *cell_idx,
                proof_idx: *proof_idx,
                proof_scope: scope.clone(),
                state: "cancelled".to_string(),
            });
        }
        reqs
    }
}

fn add_proof_range(
    store: &Store,
    squad_id: &str,
    plan: &mut StopCascadePlan,
    task_idx: i64,
    scope: &str,
    cell_idx: i64,
    from_idx: i64,
) -> Result<(), StoreError> {
    let start = usize::try_from(from_idx).unwrap_or(usize::MAX);
    for (proof_idx, _) in store
        .proofs_for(squad_id, task_idx, scope, cell_idx)?
        .iter()
        .enumerate()
        .skip(start)
    {
        plan.add_proof(
            task_idx,
            scope,
            cell_idx,
            i64::try_from(proof_idx).unwrap_or(i64::MAX),
        );
    }
    Ok(())
}

fn all_task_cells(
    store: &Store,
    squad_id: &str,
    task_idx: i64,
) -> Result<Vec<(i64, i64)>, StoreError> {
    Ok(store
        .get_task_cell_ids(squad_id, task_idx)?
        .into_iter()
        .map(|(cell_idx, _)| (task_idx, cell_idx))
        .collect())
}

fn stop_cascade_plan(
    store: &Store,
    squad_id: &str,
    req: &SetStatusBody,
) -> Result<StopCascadePlan, StoreError> {
    let mut plan = StopCascadePlan::default();
    match req.kind.as_str() {
        "task" => {
            let impact = store.compute_task_restart_impact(squad_id, req.task_idx)?;
            for task in impact.tasks {
                plan.add_task_state(task.idx);
                add_proof_range(store, squad_id, &mut plan, task.idx, "task", -1, 0)?;
            }
            for cell in impact.cells {
                plan.add_cell(cell.task_idx, cell.idx);
                add_proof_range(
                    store,
                    squad_id,
                    &mut plan,
                    cell.task_idx,
                    "cell",
                    cell.idx,
                    0,
                )?;
            }
        }
        "cell" => {
            let impact = store.compute_cell_restart_impact(squad_id, req.task_idx, req.cell_idx)?;
            for task in impact.tasks {
                plan.add_task_state(task.idx);
                add_proof_range(store, squad_id, &mut plan, task.idx, "task", -1, 0)?;
            }
            for cell in impact.cells {
                plan.add_cell(cell.task_idx, cell.idx);
                add_proof_range(
                    store,
                    squad_id,
                    &mut plan,
                    cell.task_idx,
                    "cell",
                    cell.idx,
                    0,
                )?;
            }
        }
        "proof" if req.proof_scope == "cell" => {
            let impact = store.compute_cell_restart_impact(squad_id, req.task_idx, req.cell_idx)?;
            for task in &impact.tasks {
                plan.add_task_state(task.idx);
                add_proof_range(store, squad_id, &mut plan, task.idx, "task", -1, 0)?;
            }
            add_proof_range(
                store,
                squad_id,
                &mut plan,
                req.task_idx,
                "cell",
                req.cell_idx,
                req.proof_idx,
            )?;
            for cell in impact.cells {
                if cell.task_idx == req.task_idx && cell.idx == req.cell_idx {
                    continue;
                }
                plan.add_cell(cell.task_idx, cell.idx);
                add_proof_range(
                    store,
                    squad_id,
                    &mut plan,
                    cell.task_idx,
                    "cell",
                    cell.idx,
                    0,
                )?;
            }
        }
        "proof" if req.proof_scope == "task" => {
            let impact = store.compute_task_restart_impact(squad_id, req.task_idx)?;
            for task in &impact.tasks {
                plan.add_task_state(task.idx);
            }
            add_proof_range(
                store,
                squad_id,
                &mut plan,
                req.task_idx,
                "task",
                -1,
                req.proof_idx,
            )?;
            for task in impact.tasks {
                if task.idx == req.task_idx {
                    continue;
                }
                add_proof_range(store, squad_id, &mut plan, task.idx, "task", -1, 0)?;
            }
            let root_cells: HashSet<(i64, i64)> = all_task_cells(store, squad_id, req.task_idx)?
                .into_iter()
                .collect();
            for cell in impact.cells {
                let key = (cell.task_idx, cell.idx);
                if root_cells.contains(&key) {
                    continue;
                }
                plan.add_cell(cell.task_idx, cell.idx);
                add_proof_range(
                    store,
                    squad_id,
                    &mut plan,
                    cell.task_idx,
                    "cell",
                    cell.idx,
                    0,
                )?;
            }
        }
        "proof" => {
            add_proof_range(
                store,
                squad_id,
                &mut plan,
                req.task_idx,
                &req.proof_scope,
                req.cell_idx,
                req.proof_idx,
            )?;
        }
        _ => {}
    }
    Ok(plan)
}

fn apply_stop_cascade(
    store: &Store,
    squad_id: &str,
    reqs: &[SetStatusBody],
) -> Result<(), StoreError> {
    for req in reqs {
        match req.kind.as_str() {
            "task" => store.set_task_state(squad_id, req.task_idx, NodeState::Cancelled)?,
            "cell" => {
                store.set_cell_state(squad_id, req.task_idx, req.cell_idx, NodeState::Cancelled)?
            }
            "proof" => store.set_proof_state(
                squad_id,
                req.task_idx,
                &req.proof_scope,
                req.cell_idx,
                req.proof_idx,
                NodeState::Cancelled,
            )?,
            _ => {}
        }
    }
    Ok(())
}

/// The tmux pane name(s) and owning cell that a manual status-set
/// (RAL-163) on a task/cell/proof step must capture and stop, or `None`
/// if the target doesn't resolve to a real node.
fn stop_targets_for_status_change_full(
    store: &Store,
    squad_id: &str,
    req: &SetStatusBody,
) -> Vec<StopTarget> {
    let Ok(task) = store.get_task_name(squad_id, req.task_idx) else {
        return Vec::new();
    };
    match req.kind.as_str() {
        "cell" => {
            let Ok(sid) = store.get_cell_id(squad_id, req.task_idx, req.cell_idx) else {
                return Vec::new();
            };
            vec![StopTarget {
                pane_name: crate::tmux::session_name(squad_id, &task, &sid),
                ghost_task_idx: req.task_idx,
                ghost_cell_idx: req.cell_idx,
            }]
        }
        "task" => store
            .get_task_cell_ids(squad_id, req.task_idx)
            .unwrap_or_default()
            .into_iter()
            .map(|(idx, sid)| StopTarget {
                pane_name: crate::tmux::session_name(squad_id, &task, &sid),
                ghost_task_idx: req.task_idx,
                ghost_cell_idx: idx,
            })
            .collect(),
        "proof" => {
            let ghost_cell_idx = if req.proof_scope == "task" {
                store
                    .get_task_cell_ids(squad_id, req.task_idx)
                    .unwrap_or_default()
                    .into_iter()
                    .next()
                    .map(|(idx, _)| idx)
            } else {
                Some(req.cell_idx)
            };
            let Some(ghost_cell_idx) = ghost_cell_idx else {
                return Vec::new();
            };
            let (vsquad_id, vcell_id) =
                proof_tmux_keys(squad_id, &req.proof_scope, &req.proof_idx.to_string());
            vec![StopTarget {
                pane_name: crate::tmux::session_name(&vsquad_id, &task, &vcell_id),
                ghost_task_idx: req.task_idx,
                ghost_cell_idx,
            }]
        }
        _ => Vec::new(),
    }
}

#[cfg(test)]
/// The tmux pane name(s) and owning-cell index that a manual status-set
/// (RAL-163) on a task/cell/proof step must capture and stop. Kept as the
/// compact `(pane_name, ghost_cell_idx)` shape for the existing unit tests.
fn stop_targets_for_status_change(
    store: &Store,
    squad_id: &str,
    req: &SetStatusBody,
) -> Vec<(String, i64)> {
    stop_targets_for_status_change_full(store, squad_id, req)
        .into_iter()
        .map(|target| (target.pane_name, target.ghost_cell_idx))
        .collect()
}

/// RAL-163: before a manual status change away from `pending` takes effect on
/// a task/cell/proof step, capture whatever the agent running under it
/// has produced so far and stop it — turning a hard cutoff into a
/// recoverable checkpoint instead of leaving the agent running in the
/// background with no relationship to the new status. Capture goes into the
/// owning cell's ghost (`ghost::upsert_ghost`, merged onto whatever that
/// ghost already held), which `ghost::format_context_block` automatically
/// prepends to that cell's prompt on its next attempt — so no separate
/// "pass it to the next worker" plumbing is needed.
///
/// Best-effort throughout, and never blocks the status change itself: most
/// of the time there's no live agent to capture (the common case is
/// overriding an already-finished node), and even a tmux resolution failure
/// shouldn't stop a user from being able to force a status.
fn capture_and_stop_nodes(store: &Store, squad_id: &str, reqs: &[SetStatusBody]) {
    let Ok(tmux) = crate::tmux::Tmux::resolve() else {
        return;
    };
    let mut seen = HashSet::new();
    let mut targets = Vec::new();
    for req in reqs {
        for target in stop_targets_for_status_change_full(store, squad_id, req) {
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
            // RAL-247: scrub credential env-var values before folding the pane
            // capture into the cell's ghost (which is later served back and
            // prepended to the next attempt's prompt).
            let masked = ralphus_core::redact::redact_secrets(&content);
            let trimmed = masked.trim();
            if !trimmed.is_empty() {
                let uri =
                    crate::ghost::cell_uri(squad_id, target.ghost_task_idx, target.ghost_cell_idx);
                let cwd = store
                    .get_cell_agent_resume(squad_id, target.ghost_task_idx, target.ghost_cell_idx)
                    .map(|(cwd, _, _)| cwd)
                    .unwrap_or_default();
                let revision = crate::ghost::current_revision(&cwd);
                if store
                    .upsert_ghost(
                        &uri,
                        crate::ghost::KIND_CELL,
                        Some(squad_id),
                        None,
                        trimmed,
                        revision.as_deref(),
                    )
                    .is_ok()
                {
                    crate::cartographer::Note::new("set_status")
                        .squad(squad_id)
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

fn capture_and_stop_node(store: &Store, squad_id: &str, req: &SetStatusBody) {
    capture_and_stop_nodes(store, squad_id, std::slice::from_ref(req));
}

/// Manually override the state of a squad, task, cell, or proof step (RAL-74).
///
/// Routes through the same store setters used by natural transitions so that
/// audit log entries are written and the scheduler can observe the new state on
/// its next tick (e.g. a squad moved to Pending will be claimed and re-executed).
/// Targeting the *squad itself* at `cancelled` goes through the same cascading
/// cancel path as the "Cancel Squad" button (RAL-116) — not a separate DB-only
/// flip — so both entry points stop every in-flight agent in the squad and
/// cascade to dependent squads identically. Targeting a task/cell/proof at
/// `cancelled` is the board's scoped "Stop" path (RAL-181): cancel that node
/// plus everything downstream of it *within the same squad*, while leaving the
/// rest of the squad alive. This is intentionally narrower than the squad-wide
/// `Cancel Squad` action and intentionally broader than an arbitrary one-row DB
/// flip — it follows the squad's existing dependency graph rather than treating
/// sibling branches as collateral damage.
fn set_status(daemon: &Daemon, id: &str, body: &str) -> Reply {
    let Ok(req) = serde_json::from_str::<SetStatusBody>(body) else {
        return error(400, "bad_request", "invalid set-status body", vec![]);
    };
    let store = daemon.lock();
    let stop_plan =
        if matches!(req.kind.as_str(), "task" | "cell" | "proof") && req.state == "cancelled" {
            match stop_cascade_plan(&store, id, &req) {
                Ok(plan) => Some(plan),
                Err(StoreError::NotFound) => {
                    return error(404, "not_found", "squad or node not found", vec![]);
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

    // RAL-163: any status other than "pending" on a task/cell/proof step
    // means "stop the agent running there" (including `ignored`, which is
    // deliberately non-terminal in the state machine but still means "stop
    // and save what happened" per the ticket, and `cancelled`). Capture +
    // kill happens before the state change itself is applied below. For a
    // cascading RAL-181 stop, stop every impacted pane in that branch; for any
    // other manual status change, keep the old single-node behavior.
    if matches!(req.kind.as_str(), "task" | "cell" | "proof") {
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
        "squad" => {
            let Some(state) = SquadState::parse(&req.state) else {
                return error(
                    400,
                    "bad_request",
                    &format!("unknown squad state '{}'", req.state),
                    vec![],
                );
            };
            if state == SquadState::Cancelled {
                store.cancel_squad(id, false).map(|impact| {
                    for r in &impact.squads {
                        daemon.cancellations.cancel(&r.id);
                        kill_squad_tmux_sessions(&r.id);
                    }
                })
            } else {
                store.set_squad_state(id, state)
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
        "cell" => {
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
                    store.set_cell_state(id, req.task_idx, req.cell_idx, state)
                }
            } else {
                store.set_cell_state(id, req.task_idx, req.cell_idx, state)
            }
        }
        "proof" => {
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
                    store.set_proof_state(
                        id,
                        req.task_idx,
                        &req.proof_scope,
                        req.cell_idx,
                        req.proof_idx,
                        state,
                    )
                }
            } else {
                store.set_proof_state(
                    id,
                    req.task_idx,
                    &req.proof_scope,
                    req.cell_idx,
                    req.proof_idx,
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
    // RAL-315: a task cancelled here (directly, or via `apply_stop_cascade`
    // cancelling its owning task from a cell/proof-level stop) is invisible
    // to the scheduler's own end-of-dispatch aggregation, since this request
    // happens outside that loop. Re-run the same precedence check so a squad
    // cancelled one task at a time still reaches `cancelled` once every task
    // has landed.
    if matches!(req.kind.as_str(), "task" | "cell" | "proof") && req.state == "cancelled" {
        let _ = store.reconcile_squad_cancellation(id);
    }
    match store.get_squad(id) {
        Ok(squad) => json(200, &squad),
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

/// The classified, ordered list of runnable work across all schedulable squads.
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

/// Delete a squad and all of its child rows.
fn delete_squad(daemon: &Daemon, id: &str) -> Reply {
    match daemon.lock().delete_squad(id) {
        Ok(()) => {
            // RAL-154: a deleted squad's durable terminal logs must not outlive
            // the squad itself — same `ralphus_<squad_id>_` prefix scoping
            // `kill_squad_tmux_sessions` already uses to find its live tmux
            // cells.
            crate::terminal_log::delete_with_prefix(&format!("ralphus_{id}_"));
            json(200, &StateResponse { state: "deleted" })
        }
        Err(e) => store_error(&e),
    }
}

#[derive(Deserialize)]
struct ClearBody {
    /// Optional squad-state filter; when empty, everything is cleared.
    #[serde(default)]
    states: Vec<String>,
    /// Keep on-disk review worktrees instead of purging them.
    #[serde(default)]
    keep_temporary: bool,
}

#[derive(Serialize)]
struct ClearResponse {
    squads_deleted: usize,
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
        match SquadState::parse(s) {
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
    // RAL-154: same per-entity cleanup `delete_squad`/`guardian_delete` do,
    // applied to every squad/guardian this bulk clear actually removed.
    for squad_id in &outcome.squad_ids {
        crate::terminal_log::delete_with_prefix(&format!("ralphus_{squad_id}_"));
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
            squads_deleted: outcome.squads_deleted,
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
    skip_worktrees: bool,
    #[serde(default)]
    review_type: Option<String>,
}

#[derive(Deserialize)]
struct GuardianSettingsBody {
    #[serde(default)]
    skip_auto_build: Option<bool>,
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
    /// RAL-168: this review's own Proof-scope override -- `"each_branch"`,
    /// `"final_branch"`, or `"nothing"`. An empty string resets it to
    /// "inherit the project default" (same empty-string-means-unset
    /// convention `resolver_agent`/`resolver_model` above use).
    #[serde(default)]
    proof_scope: Option<String>,
    /// RAL-168: this review's own override for `"each_branch"` scope's
    /// auto-clean-skip sub-option. Always an explicit override when present
    /// -- a boolean has no natural "reset to inherit" sentinel the way an
    /// empty string works for `proof_scope`.
    #[serde(default)]
    proof_skip_auto_clean: Option<bool>,
    /// RAL-250: this review's own override for whether the automatic
    /// base-branch auto-update rebuild is skipped. `None` (or the field being
    /// absent) means "inherit the project/global default".
    #[serde(default)]
    skip_base_updates: Option<bool>,
    /// RAL-307: this review's own override for whether a newly submitted
    /// PR's branch defaults to the exact worktree/feature branch name
    /// instead of the convention-derived alias. `None` (or the field being
    /// absent) means "inherit the project/global default".
    #[serde(default)]
    match_pr_branch_name: Option<bool>,
    /// RAL-317: this review's own override for whether the PR stack is
    /// auto-submitted/grown as each branch reaches a terminal merge state.
    /// `None` (or the field being absent) means "inherit the project/global
    /// default".
    #[serde(default)]
    auto_submit_pr_stack: Option<bool>,
    /// RAL-378: this review's own override for whether its pull request is
    /// pushed to a branch separate from its review branch. `None` (or the
    /// field being absent) means "inherit the project/global default".
    #[serde(default)]
    separate_pr_branch: Option<bool>,
}

/// Body for `POST /api/guardians/{id}/details` -- the board's single
/// "Edit Details" modal (RAL-410). A strict superset of
/// [`GuardianSettingsBody`] plus the fields that otherwise require
/// `/rename`, `/base`, `/squash`, `/build-env`, `/manual-checks-env`, and
/// `/branches/{id}/env` as separate calls. Every field is `Option<T>`; only
/// `Some(..)` fields are applied. This endpoint is purely additive -- all of
/// those individual endpoints remain unchanged and in place for the
/// CLI/MCP, which call them directly.
#[derive(Deserialize)]
struct GuardianDetailsBody {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    base_branch: Option<String>,
    #[serde(default)]
    resolver_agent: Option<String>,
    #[serde(default)]
    resolver_model: Option<String>,
    #[serde(default)]
    proof_scope: Option<String>,
    #[serde(default)]
    proof_skip_auto_clean: Option<bool>,
    #[serde(default)]
    skip_auto_build: Option<bool>,
    #[serde(default)]
    skip_worktrees: Option<bool>,
    #[serde(default)]
    separate_pr_branch: Option<bool>,
    #[serde(default)]
    match_pr_branch_name: Option<bool>,
    #[serde(default)]
    auto_submit_pr_stack: Option<bool>,
    /// Full desired squash membership: every project in this list gets
    /// squash turned ON, every other project in the review's
    /// [`crate::guardian::GuardianView::projects`] gets it turned OFF.
    /// `None` leaves every project's squash setting untouched.
    #[serde(default)]
    squash_projects: Option<Vec<String>>,
    /// Reuses [`InheritedEnvOverridesBody`]'s `set`/`unset`/`clear` shape so
    /// the inherited-vs-tombstoned-vs-reverted distinction the per-scope
    /// endpoints already support isn't lost in this batched path.
    #[serde(default)]
    build_env: Option<InheritedEnvOverridesBody>,
    #[serde(default)]
    manual_checks_env: Option<InheritedEnvOverridesBody>,
    /// Keyed by branch id, matching `/branches/{branch_id}/env`'s addressing.
    #[serde(default)]
    branch_env: Option<std::collections::BTreeMap<String, InheritedEnvOverridesBody>>,
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

fn guardian_create(daemon: &Daemon, user_header: Option<&str>, body: &str) -> Reply {
    let Ok(req) = serde_json::from_str::<CreateGuardianBody>(body) else {
        return error(
            400,
            "bad_request",
            "body must be {name, base_branch, git_root}",
            vec![],
        );
    };
    let auto_watch_user = current_user(daemon, user_header).ok().flatten();
    let store = daemon.lock();
    match store.create_guardian(&req.name, &req.base_branch, &req.git_root) {
        Ok(id) => {
            if !req.checks.is_empty() {
                let _ = store.set_guardian_checks(&id, &req.checks);
            }
            if req.skip_auto_build {
                let _ = store.set_guardian_skip_auto_build(&id, true);
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
            if let Some(user) = auto_watch_user {
                if let Ok(Some(preferences)) = store.get_user(&user) {
                    if preferences.auto_watch {
                        let entity_uri = format!("guardian:{id}");
                        let _ = store.create_watch(
                            &user,
                            &entity_uri,
                            &preferences.default_notify_tiers,
                        );
                    }
                }
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
            //
            // RAL-303: gated on `collecting`, matching `guardian_list`. The
            // preliminary summary only describes branches whose stacked rebase
            // hasn't run yet, so a settled review has nothing for it to add --
            // and since the review page polls this endpoint, an ungated
            // promote spent a `git log` per branch on every poll for the rest
            // of the review's life. `repair_missing_final_summary` covers the
            // settled review that is genuinely missing a summary.
            if g.status == "collecting" {
                daemon.summary_queue_handle().promote(id);
            }
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
            "body must be {skip_auto_build?, proof_scope?}",
            vec![],
        );
    };
    let store = daemon.lock();
    if let Some(skip) = req.skip_auto_build {
        if let Err(e) = store.set_guardian_skip_auto_build(id, skip) {
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
    if let Some(scope) = req.proof_scope.as_deref() {
        let scope = if scope.is_empty() { None } else { Some(scope) };
        if let Err(e) = store.set_guardian_proof_scope(id, scope) {
            return store_error(&e);
        }
    }
    if let Some(skip) = req.proof_skip_auto_clean {
        if let Err(e) = store.set_guardian_proof_skip_auto_clean(id, Some(skip)) {
            return store_error(&e);
        }
    }
    if let Some(skip) = req.skip_base_updates {
        if let Err(e) = store.set_guardian_skip_base_updates(id, Some(skip)) {
            return store_error(&e);
        }
    }
    if let Some(enabled) = req.match_pr_branch_name {
        if let Err(e) = store.set_guardian_match_pr_branch_name(id, Some(enabled)) {
            return store_error(&e);
        }
    }
    if let Some(enabled) = req.auto_submit_pr_stack {
        if let Err(e) = store.set_guardian_auto_submit_pr_stack(id, Some(enabled)) {
            return store_error(&e);
        }
    }
    if let Some(enabled) = req.separate_pr_branch {
        if let Err(e) = store.set_guardian_separate_pr_branch(id, Some(enabled)) {
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
    let base_changed = req.base_branch.as_deref().is_some_and(|s| !s.is_empty());
    drop(store);
    // RAL-277: a local base edit is not complete until every open PR/MR base
    // has been verified on the forge. Keep this inside the request so callers
    // never observe a successful response while the forge still targets the
    // old chain.
    if base_changed {
        if let Err(e) = crate::pr::resync_pr_bases_synchronously(&daemon.store_handle(), id) {
            return error(
                502,
                "forge_error",
                &format!("base saved locally but PR/MR base sync failed: {e}"),
                vec![],
            );
        }
    }
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
            // ralphus[ignore-rlog-pair]: this transport-only diagnostic has no event entity; handlers emit the structured request or state record
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

/// Shared env-key/value validation for one `GuardianDetailsBody` env-override
/// section, matching `set_guardian_branch_env`/`set_guardian_scoped_env`'s
/// inline checks so the batched path enforces the identical rules.
fn validate_env_overrides_body(req: &InheritedEnvOverridesBody) -> Option<Reply> {
    for key in req
        .set
        .keys()
        .chain(req.unset.iter())
        .chain(req.clear.iter())
    {
        if !crate::config::is_valid_env_key(key) {
            return Some(error(
                400,
                "bad_request",
                &format!("invalid environment variable name: {key:?}"),
                vec![],
            ));
        }
    }
    for (key, value) in &req.set {
        if !crate::config::is_valid_env_value(value) {
            return Some(error(
                400,
                "bad_request",
                &format!(
                    "invalid environment variable value for {key:?}: contains control characters"
                ),
                vec![],
            ));
        }
    }
    None
}

/// Cartographer entry for one env-override section changed by
/// [`guardian_details`], matching [`set_guardian_scoped_env`]'s
/// allowlist-redacted logging shape.
fn cartographer_log_guardian_env(
    store: &Store,
    id: &str,
    scope: &str,
    task: Option<&str>,
    req: &InheritedEnvOverridesBody,
) {
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
        scope: Some(scope),
        squad_id: None,
        guardian_id: Some(id),
        cell_id: None,
        task,
        log_path: None,
        payload: serde_json::json!({
            "set": redacted_set,
            "unset": req.unset,
            "clear": req.clear,
        }),
        admin_only: false,
    });
}

#[derive(Serialize)]
struct GuardianDetailsReply {
    guardian: crate::guardian::GuardianView,
    #[serde(skip_serializing_if = "Option::is_none")]
    base_change: Option<ChangeBaseStatus>,
}

/// Batched update for the board's "Edit Details" modal (RAL-410). A single
/// request that supersets `guardian_rename`/[`guardian_settings`]/
/// `guardian_change_base`/`guardian_squash`/`set_guardian_build_env`/
/// `set_guardian_manual_checks_env`/`set_guardian_branch_env` -- every
/// present field is applied under one held `Store` lock, then **at most
/// one** rebase-relevant side effect (PR base resync, then a merge
/// restart/kickoff) runs at the end, instead of each field independently
/// deciding to trigger one the way the individual endpoints above do. This
/// is purely additive: every one of those endpoints remains in place
/// unchanged for the CLI/MCP, which call them directly.
///
/// Deliberate refinement over [`guardian_settings`]'s policy: that handler
/// restarts an in-flight merge on *any* field write, even a purely cosmetic
/// one. This handler only restarts when something in the batch was actually
/// rebase-relevant, so a rename bundled with cosmetic-only fields never
/// forces a restart.
fn guardian_details(daemon: &Daemon, id: &str, body: &str) -> Reply {
    let Ok(req) = serde_json::from_str::<GuardianDetailsBody>(body) else {
        return error(400, "bad_request", "invalid body", vec![]);
    };

    let store = daemon.lock();
    let guardian = match store.get_guardian(id) {
        Ok(g) => g,
        Err(e) => return store_error(&e),
    };

    // Guard: base/resolver are frozen once approved/deployed, same as
    // guardian_change_base's existing check.
    let frozen = matches!(guardian.status.as_str(), "approved" | "deployed");
    if frozen
        && (req
            .base_branch
            .as_deref()
            .is_some_and(|s| !s.trim().is_empty())
            || req.resolver_agent.is_some()
            || req.resolver_model.is_some())
    {
        return error(
            409,
            "invalid_transition",
            "cannot change base branch or resolver on an approved or deployed review",
            vec![],
        );
    }

    // Validate every env-override section up front so a mid-way failure
    // never leaves some fields applied and others rejected.
    if let Some(patch) = &req.build_env {
        if let Some(e) = validate_env_overrides_body(patch) {
            return e;
        }
    }
    if let Some(patch) = &req.manual_checks_env {
        if let Some(e) = validate_env_overrides_body(patch) {
            return e;
        }
    }
    if let Some(branches) = &req.branch_env {
        for patch in branches.values() {
            if let Some(e) = validate_env_overrides_body(patch) {
                return e;
            }
        }
    }

    if let Some(name) = req.name.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        if let Err(e) = store.rename_guardian(id, name) {
            return store_error(&e);
        }
    }
    let base_changed = req
        .base_branch
        .as_deref()
        .is_some_and(|s| !s.trim().is_empty());
    if base_changed {
        if let Err(e) =
            store.set_guardian_base_branch(id, req.base_branch.as_deref().unwrap().trim())
        {
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
    if let Some(scope) = req.proof_scope.as_deref() {
        let scope = if scope.is_empty() { None } else { Some(scope) };
        if let Err(e) = store.set_guardian_proof_scope(id, scope) {
            return store_error(&e);
        }
    }
    if let Some(skip) = req.proof_skip_auto_clean {
        if let Err(e) = store.set_guardian_proof_skip_auto_clean(id, Some(skip)) {
            return store_error(&e);
        }
    }
    if let Some(skip) = req.skip_auto_build {
        if let Err(e) = store.set_guardian_skip_auto_build(id, skip) {
            return store_error(&e);
        }
    }
    if let Some(skip) = req.skip_worktrees {
        if let Err(e) = store.set_guardian_skip_worktrees(id, skip) {
            return store_error(&e);
        }
    }
    if let Some(enabled) = req.separate_pr_branch {
        if let Err(e) = store.set_guardian_separate_pr_branch(id, Some(enabled)) {
            return store_error(&e);
        }
    }
    if let Some(enabled) = req.match_pr_branch_name {
        if let Err(e) = store.set_guardian_match_pr_branch_name(id, Some(enabled)) {
            return store_error(&e);
        }
    }
    if let Some(enabled) = req.auto_submit_pr_stack {
        if let Err(e) = store.set_guardian_auto_submit_pr_stack(id, Some(enabled)) {
            return store_error(&e);
        }
    }
    if let Some(desired) = &req.squash_projects {
        for project in &guardian.projects {
            let want_on = desired.contains(project);
            if let Err(e) = store.set_guardian_project_squash(id, project, want_on) {
                return store_error(&e);
            }
        }
    }
    if let Some(patch) = &req.build_env {
        if let Err(e) =
            store.set_guardian_build_env_overrides(id, &patch.set, &patch.unset, &patch.clear)
        {
            return store_error(&e);
        }
        cartographer_log_guardian_env(
            &store,
            id,
            GuardianEnvSection::Build.cartographer_scope(),
            Some(GuardianEnvSection::Build.label()),
            patch,
        );
    }
    if let Some(patch) = &req.manual_checks_env {
        if let Err(e) = store.set_guardian_manual_checks_env_overrides(
            id,
            &patch.set,
            &patch.unset,
            &patch.clear,
        ) {
            return store_error(&e);
        }
        cartographer_log_guardian_env(
            &store,
            id,
            GuardianEnvSection::ManualChecks.cartographer_scope(),
            Some(GuardianEnvSection::ManualChecks.label()),
            patch,
        );
    }
    if let Some(branches) = &req.branch_env {
        for (branch_id, patch) in branches {
            if let Err(e) = store.set_guardian_branch_env_overrides(
                id,
                branch_id,
                &patch.set,
                &patch.unset,
                &patch.clear,
            ) {
                return store_error(&e);
            }
            cartographer_log_guardian_env(&store, id, "guardian-branch", Some(branch_id), patch);
        }
    }

    // -- single side-effect decision point --
    let rebase_relevant = base_changed
        || req.resolver_agent.is_some()
        || req.resolver_model.is_some()
        || req.skip_auto_build.is_some()
        || req.skip_worktrees.is_some()
        || req.squash_projects.is_some()
        || req.build_env.is_some()
        || req.manual_checks_env.is_some()
        || req.branch_env.is_some();
    let status = guardian.status.clone();
    let has_branches = !guardian.branches.is_empty();
    drop(store);

    if base_changed {
        if let Err(e) = crate::pr::resync_pr_bases_synchronously(&daemon.store_handle(), id) {
            return error(
                502,
                "forge_error",
                &format!("details saved locally but PR/MR base sync failed: {e}"),
                vec![],
            );
        }
    }

    let mut base_change: Option<ChangeBaseStatus> = None;
    if status == "merging" {
        if rebase_relevant {
            let runner = guardian_agent_runner(daemon);
            let restarted = crate::guardian_merge::restart_guardian_merge(
                daemon.store_handle(),
                daemon.cancellations_handle(),
                runner,
                id,
                daemon.semaphore_handle(),
            );
            if restarted.status >= 400 {
                // ralphus[ignore-rlog-pair]: this transport-only diagnostic has no event entity; handlers emit the structured request or state record
                crate::rlog!(
                    WARNING,
                    "ralphus [guardian] review {id} details-triggered merge restart failed: {}",
                    restarted.body
                );
            } else {
                base_change = Some(ChangeBaseStatus {
                    status: "merging".to_string(),
                    message: "Details saved and the rebase restarted.".to_string(),
                    action: None,
                });
            }
        } else if base_changed {
            base_change = Some(ChangeBaseStatus {
                status: "rebase_in_progress".to_string(),
                message: "Details saved; a rebase is already in progress.".to_string(),
                action: None,
            });
        }
    } else if rebase_relevant && has_branches && status != "approved" && status != "deployed" {
        let runner = guardian_agent_runner(daemon);
        if let Ok(crate::guardian_merge::StartMergeOutcome::Merging) =
            crate::guardian_merge::kickoff_merge(
                daemon.store_handle(),
                runner,
                id,
                daemon.semaphore_handle(),
                daemon.cancellations_handle(),
            )
        {
            base_change = Some(ChangeBaseStatus {
                status: "merging".to_string(),
                message: "Details saved and the rebase started.".to_string(),
                action: None,
            });
        }
    }

    match daemon.lock().get_guardian(id) {
        Ok(g) => json(
            200,
            &GuardianDetailsReply {
                guardian: g,
                base_change,
            },
        ),
        Err(e) => store_error(&e),
    }
}

// ── PR submission + feedback loop (RAL-117) ─────────────────────────────────

#[derive(Deserialize)]
struct SubmitPrsBody {
    prs: Vec<crate::pr::PrRequest>,
    /// RAL-338: downgrades a definite "no forge relationship" fork pre-flight
    /// result from a hard error to a logged warning. Ignored for a project
    /// with no registered fork.
    #[serde(default)]
    allow_unlinked_fork: bool,
}

/// Submit one or more PRs/MRs for a guardian's stacked and/or combined
/// worktree(s). Kicks off in the background (git push + forge API calls);
/// poll `GET .../pull-requests` for the resulting rows.
///
/// RAL-338: resolves the acting user once from the request context (falling
/// back to `[daemon].default_user`, same as every other placeholder-identity
/// call site -- see `current_user`'s doc comment) to decide which fork (if
/// any) this submission routes through.
fn guardian_submit_prs(daemon: &Daemon, id: &str, body: &str, user_header: Option<&str>) -> Reply {
    let Ok(req) = serde_json::from_str::<SubmitPrsBody>(body) else {
        return error(400, "bad_request", "body must be {prs: [...]}", vec![]);
    };
    let user = match current_user(daemon, user_header) {
        Ok(name) => name.unwrap_or_default(),
        Err(_) => String::new(),
    };
    let runner: Arc<dyn Runner> =
        Arc::new(SubprocessRunner::from_env().with_cartographer(daemon.store_handle()));
    crate::pr::start_submit_pull_requests(
        daemon.store_handle(),
        runner,
        id,
        req.prs,
        user,
        req.allow_unlinked_fork,
    )
}

/// List every PR/MR submitted for a guardian, oldest first. Bare JSON array
/// (see the `GET /api/guardians` convention this mirrors).
fn guardian_list_prs(daemon: &Daemon, id: &str) -> Reply {
    match daemon.lock().list_pull_requests_for_guardian(id) {
        Ok(prs) => json(200, &prs),
        Err(e) => store_error(&e),
    }
}

/// RAL-317: bulk-drop every currently open PR row for a guardian and clear
/// its registered GitHub-native PR stack number (`review pr unlink`) -- the
/// guardian-wide "start this review's PR stack over" action, e.g. after an
/// auto-submitted stack went wrong. Dropped rows remain visible as history
/// via `GET .../pull-request-stacks` (`PrStackView`/`group_into_stacks`);
/// this never hard-deletes.
fn guardian_unlink_prs(daemon: &Daemon, id: &str) -> Reply {
    let store = daemon.lock();
    if let Err(e) = store.get_guardian(id) {
        return store_error(&e);
    }
    let dropped = match store.bulk_drop_open_pull_requests(id, "unlinked") {
        Ok(n) => n,
        Err(e) => return store_error(&e),
    };
    // Best-effort: an already-unregistered stack number's `NotFound` here
    // isn't worth failing an otherwise-successful unlink over.
    let _ = store.clear_guardian_forge_stack_number(id);
    json(200, &serde_json::json!({"dropped": dropped}))
}

/// List every past PR stack submitted for a guardian (RAL-302), most recent
/// first -- every PR row ralphus has ever created for this review, in any
/// state (open/merged/closed/dropped), grouped by the single "submit a
/// stack" call that created it. Bare JSON array of [`crate::pr::PrStackView`].
fn guardian_list_pr_stacks(daemon: &Daemon, id: &str) -> Reply {
    match daemon.lock().list_pull_requests_for_guardian(id) {
        Ok(prs) => json(200, &crate::pr::group_into_stacks(prs)),
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
    let client = match crate::forge::resolve_remote(root, &guardian.base_branch, &forge_cfg) {
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
            // cells) must not outlive it.
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

/// Fire a [`crate::pr::check_and_apply_forge_reorder`] check in the
/// background for `id` (RAL-273) -- used by the explicit `sync-pr`
/// endpoint ([`guardian_sync_pr`]) only. The 5-minute background sweep is
/// [`crate::pr::poll_forge_reorders`], wired into `scheduler::run_loop`
/// instead, since it must run regardless of any HTTP activity.
/// [`guardian_reorder`]/[`guardian_arrange`] deliberately don't call this
/// too -- see the comment in [`guardian_reorder`] for why.
fn trigger_forge_reorder_check(daemon: &Daemon, id: &str) {
    let store = daemon.store_handle();
    let sem = daemon.semaphore_handle();
    let cancellations = daemon.cancellations_handle();
    let sid = id.to_string();
    std::thread::spawn(move || {
        let runner: Arc<dyn Runner> =
            Arc::new(SubprocessRunner::from_env().with_cartographer(Arc::clone(&store)));
        crate::pr::check_and_apply_forge_reorder(
            &store,
            runner.as_ref(),
            &sid,
            &sem,
            &cancellations,
        );
    });
}

/// Explicit "sync PR" action (RAL-273): trigger the same forge-reorder check
/// the 5-minute poll uses, on demand, for either forge (GitHub or GitLab).
/// Runs in the background -- detection itself makes forge network calls,
/// and applying a detected reorder runs a full rebase -- so the caller
/// watches the guardian view (and its notice fields) for the result, the
/// same as every other guardian action that returns 202.
fn guardian_sync_pr(daemon: &Daemon, id: &str) -> Reply {
    if let Err(e) = daemon.lock().get_guardian(id) {
        return store_error(&e);
    }
    trigger_forge_reorder_check(daemon, id);
    json(202, &StateResponse { state: "checking" })
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
    //
    // Deliberately NOT also firing `trigger_forge_reorder_check` here
    // (RAL-273): it reads the forge's *live* PR bases back, and racing that
    // against this same resync's in-flight PATCH could read the pre-PATCH
    // base and misread this review's own just-applied local reorder as
    // external drift, undoing it. `guardian_approve`/`guardian_merge` cover
    // the "on every change to a ralphus review" path for changes that don't
    // themselves touch PR bases.
    result
}

/// The runner for a guardian review's own agent invocations -- the merge's
/// conflict resolver, proof-instruction synthesis, summary/manual-commands
/// generation, feedback and "set it for me" input resolution (RAL-201).
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
    // Stop any live merge worker first -- otherwise it keeps running
    // in-flight resolver agents to completion and its own end-of-pass
    // status write clobbers `cancelled` back to `in_review`. See
    // `stop_merge_worker_for_cancel`'s doc comment.
    crate::guardian_merge::stop_merge_worker_for_cancel(&daemon.cancellations, id);
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

/// Reopen a `cancelled` review (status → `collecting`) and immediately try a
/// fresh merge pass if the daemon has capacity -- see
/// [`crate::guardian_merge::reopen_cancelled_guardian_merge`].
fn guardian_reopen(daemon: &Daemon, id: &str) -> Reply {
    let runner = guardian_agent_runner(daemon);
    crate::guardian_merge::reopen_cancelled_guardian_merge(
        daemon.store_handle(),
        runner,
        id,
        daemon.semaphore_handle(),
        daemon.cancellations_handle(),
    )
}

/// Resolve `{name}` placeholders in `text` from a check's declared inputs
/// (RAL-164): a value submitted with this squad wins, then a previously
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

/// Validate every declared input's *effective* value -- the same precedence
/// [`substitute_check_inputs`] resolves: a value submitted with this request
/// wins, then a previously resolved/submitted value stored on the guardian,
/// then the input's own literal default -- against its declared
/// [`crate::guardian::CheckInputType`] (RAL-221). Applies uniformly
/// regardless of where the value came from: a value already sitting in
/// `input_values` from before a `type` was ever declared on that input is
/// checked exactly the same as a value freshly submitted with this request
/// -- there is no separate migration/backfill step, a stale non-conforming
/// stored value is simply rejected the next time the check that declares it
/// is run. Returns one message per failing input (empty when everything
/// passes).
fn check_input_type_failures(
    inputs: &[crate::guardian::CheckInput],
    submitted: &std::collections::HashMap<String, String>,
    stored: &std::collections::HashMap<String, String>,
) -> Vec<String> {
    inputs
        .iter()
        .filter_map(|input| {
            let value = submitted
                .get(&input.name)
                .or_else(|| stored.get(&input.name))
                .unwrap_or(&input.default);
            if input.r#type.accepts(value) {
                None
            } else {
                Some(format!(
                    "input '{}' must be {} but got {:?}",
                    input.name,
                    input.r#type.label(),
                    value
                ))
            }
        })
        .collect()
}

/// Gate shared by `guardian_run_manual_commands` and `guardian_run_action_hint`
/// (RAL-221): validates the submitted+stored input values against every check
/// about to run, before any command line is built, so both endpoints stay
/// covered by the same logic (they both funnel into
/// [`substitute_check_inputs`]/[`build_check_command_line`]). A type mismatch
/// isn't just routine bad input -- especially one carrying shell
/// metacharacters where a constrained type was expected is exactly the
/// injection shape this closes off -- so a failure is logged via `rlog!` and
/// recorded to Cartographer flagged as a possible injection attempt, in
/// addition to being reported back to the caller as a 400 naming the failing
/// input(s) and why. Returns `Some(reply)` to short-circuit the caller when
/// validation fails, `None` when it's safe to proceed.
fn reject_invalid_check_inputs(
    daemon: &Daemon,
    guardian_id: &str,
    checks: &[crate::guardian::GuardianCheck],
    submitted: &std::collections::HashMap<String, String>,
    stored: &std::collections::HashMap<String, String>,
) -> Option<Reply> {
    let mut failures: Vec<String> = checks
        .iter()
        .flat_map(|check| check_input_type_failures(&check.inputs, submitted, stored))
        .collect();
    if failures.is_empty() {
        return None;
    }
    failures.sort();
    failures.dedup();
    let summary = failures.join("; ");
    crate::rlog!(
        WARNING,
        "ralphus [server] guardian {guardian_id} rejected check input(s) — possible injection attempt: {summary}"
    );
    let _ = daemon
        .lock()
        .cartographer_log(crate::cartographer::CartographerEntry {
            level: crate::logging::LogLevel::WARNING,
            source: "server",
            message: "guardian check input rejected: type mismatch (possible injection attempt)",
            scope: Some("guardian"),
            squad_id: None,
            guardian_id: Some(guardian_id),
            cell_id: None,
            task: None,
            log_path: None,
            payload: serde_json::json!({"failures": failures}),
            admin_only: false,
        });
    Some(error(
        400,
        "invalid_check_input",
        &format!("one or more check inputs failed validation: {summary}"),
        vec![],
    ))
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

    if let Some(reply) =
        reject_invalid_check_inputs(daemon, id, &to_run, &req.inputs, &g.input_values)
    {
        return reply;
    }

    // Run against the built review worktree (e.g. `<id>-review`), not the
    // original repo — that's the checkout the commands are meant to verify
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

    if let Some(reply) = reject_invalid_check_inputs(
        daemon,
        id,
        std::slice::from_ref(&hint),
        &req.inputs,
        &g.input_values,
    ) {
        return reply;
    }

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

#[derive(Serialize)]
struct ChangeBaseAction {
    label: String,
    url: String,
}

#[derive(Serialize)]
struct ChangeBaseStatus {
    status: String,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    action: Option<ChangeBaseAction>,
}

#[derive(Serialize)]
struct ChangeBaseReply {
    guardian: crate::guardian::GuardianView,
    base_change: ChangeBaseStatus,
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
    let was_merging = guardian.status == "merging";
    drop(store);
    // RAL-277: synchronous by contract; a 2xx response means the complete
    // multi-branch PR/MR chain has already been retargeted on the forge.
    if let Err(e) = crate::pr::resync_pr_bases_synchronously(&daemon.store_handle(), id) {
        return error(
            502,
            "forge_error",
            &format!("base saved locally but PR/MR base sync failed: {e}"),
            vec![],
        );
    }
    let updated_guardian = match daemon.lock().get_guardian(id) {
        Ok(g) => g,
        Err(e) => return store_error(&e),
    };
    if !has_branches {
        return json(
            200,
            &ChangeBaseReply {
                guardian: updated_guardian,
                base_change: ChangeBaseStatus {
                    status: "saved".to_string(),
                    message: format!("Base branch saved as '{branch}'."),
                    action: None,
                },
            },
        );
    }
    if was_merging {
        return json(
            200,
            &ChangeBaseReply {
                guardian: updated_guardian,
                base_change: ChangeBaseStatus {
                    status: "rebase_in_progress".to_string(),
                    message: "You changed the base but there's a rebase in progress. We'll trigger a new rebase once this one completes.".to_string(),
                    action: Some(ChangeBaseAction {
                        label: "Stop and restart now".to_string(),
                        url: format!("/api/guardians/{id}/cancel_and_merge"),
                    }),
                },
            },
        );
    }
    let runner = guardian_agent_runner(daemon);
    let outcome = crate::guardian_merge::kickoff_merge(
        daemon.store_handle(),
        runner,
        id,
        daemon.semaphore_handle(),
        daemon.cancellations_handle(),
    );
    match outcome {
        Ok(crate::guardian_merge::StartMergeOutcome::Merging) => json(
            202,
            &ChangeBaseReply {
                guardian: updated_guardian,
                base_change: ChangeBaseStatus {
                    status: "merging".to_string(),
                    message: format!("Base branch saved as '{branch}' and the rebase restarted."),
                    action: None,
                },
            },
        ),
        Ok(crate::guardian_merge::StartMergeOutcome::Deferred) => json(
            200,
            &ChangeBaseReply {
                guardian: updated_guardian,
                base_change: ChangeBaseStatus {
                    status: "deferred".to_string(),
                    message: format!(
                        "We will use '{branch}' once the branches are ready to merge."
                    ),
                    action: None,
                },
            },
        ),
        Ok(crate::guardian_merge::StartMergeOutcome::AlreadyInProgress) => json(
            200,
            &ChangeBaseReply {
                guardian: updated_guardian,
                base_change: ChangeBaseStatus {
                    status: "rebase_in_progress".to_string(),
                    message: "You changed the base but there's a rebase in progress. We'll trigger a new rebase once this one completes.".to_string(),
                    action: Some(ChangeBaseAction {
                        label: "Stop and restart now".to_string(),
                        url: format!("/api/guardians/{id}/cancel_and_merge"),
                    }),
                },
            },
        ),
        Ok(crate::guardian_merge::StartMergeOutcome::AlreadyMerged) => {
            let approved_guardian = daemon.lock().get_guardian(id).unwrap_or(updated_guardian);
            json(
                200,
                &ChangeBaseReply {
                    guardian: approved_guardian,
                    base_change: ChangeBaseStatus {
                        status: "approved".to_string(),
                        message: "This review's work was already merged, so it was approved instead of rebased.".to_string(),
                        action: None,
                    },
                },
            )
        }
        Err(crate::guardian_merge::StartMergeError::NotFound(message)) => {
            error(404, "not_found", &message, vec![])
        }
        Err(crate::guardian_merge::StartMergeError::Preflight(message)) => {
            error(409, "pr_sync_failed", &message, vec![])
        }
        Err(crate::guardian_merge::StartMergeError::NoBranches) => json(
            200,
            &ChangeBaseReply {
                guardian: updated_guardian,
                base_change: ChangeBaseStatus {
                    status: "saved".to_string(),
                    message: format!("Base branch saved as '{branch}'."),
                    action: None,
                },
            },
        ),
        Err(crate::guardian_merge::StartMergeError::Store(message)) => {
            error(500, "store_error", &message, vec![])
        }
    }
}

/// Force-start a collecting review (RAL-69): disable all enabled branches whose
/// source cell is not yet done (or was never submitted), then kick off the
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
    // RAL-279: force_start only fires while status == "collecting", which
    // precedes PR submission, so this is a no-op today -- kept for
    // correctness/future-proofing if that invariant ever changes (see the
    // identical call in `guardian_reorder`).
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

    // RAL-279: the moved branch may have had PRs stacked on top of it in the
    // source review; retarget those onto the nearest remaining branch (or the
    // source's own base branch). The destination's newly-added branch is
    // handled by the normal new-PR-submission flow instead, since it has no
    // PR yet.
    crate::pr::start_resync_pr_bases(daemon.store_handle(), id);

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

/// Stop an in-progress rebase (status `merging`) at its next checkpoint,
/// leaving the review in the recoverable `merge_stopped` state — distinct from
/// [`guardian_cancel`] (which discards the review back to `collecting`).
fn guardian_stop(daemon: &Daemon, id: &str) -> Reply {
    crate::guardian_merge::stop_guardian_merge(
        daemon.store_handle(),
        daemon.cancellations_handle(),
        id,
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
    /// RAL-379: the registered user this feedback should be attributed to.
    /// Defaults to the resolved submitter when absent. Note there is
    /// deliberately no `submitted_by` field here -- the submitter always
    /// comes from the authenticated request context and can never be set by
    /// request data.
    #[serde(default)]
    author: Option<String>,
}

/// RAL-379: resolves `req.author` against the user registry, falling back to
/// `submitted_by` when the caller doesn't name one. Returns `Err(Reply)` if
/// an explicitly named author isn't a registered user.
fn resolve_feedback_author(
    daemon: &Daemon,
    author: Option<&str>,
    submitted_by: Option<&str>,
) -> Result<Option<String>, Reply> {
    let Some(name) = author.map(str::trim).filter(|name| !name.is_empty()) else {
        return Ok(submitted_by.map(str::to_string));
    };
    match daemon.lock().get_user(name) {
        Ok(Some(_)) => Ok(Some(name.to_string())),
        Ok(None) => Err(error(
            400,
            "unknown_user",
            &format!("user {name:?} is not registered"),
            vec![],
        )),
        Err(e) => Err(store_error(&e)),
    }
}

fn guardian_feedback(
    daemon: &Daemon,
    user_header: Option<&str>,
    id: &str,
    branch_id: &str,
    body: &str,
) -> Reply {
    let Ok(req) = serde_json::from_str::<FeedbackBody>(body) else {
        return error(400, "bad_request", "body must be {feedback}", vec![]);
    };
    if req.feedback.trim().is_empty() {
        return error(400, "bad_request", "feedback must not be empty", vec![]);
    }
    // Best-effort, like `guardian_create`'s `auto_watch_user`: an
    // unresolvable/unregistered ambient `[daemon].default_user` shouldn't
    // block posting feedback, it just means `submitted_by` stays `None`.
    let submitted_by = current_user(daemon, user_header).ok().flatten();
    let author =
        match resolve_feedback_author(daemon, req.author.as_deref(), submitted_by.as_deref()) {
            Ok(v) => v,
            Err(reply) => return reply,
        };
    let runner = guardian_agent_runner(daemon);
    crate::guardian_merge::start_feedback(
        daemon.store_handle(),
        runner,
        id,
        branch_id,
        req.feedback,
        author,
        submitted_by,
    )
}

#[derive(Serialize)]
struct MessagesResponse {
    messages: Vec<crate::guardian::MessageView>,
}

/// One review branch's read-only feedback thread (RAL-272) -- populated by
/// `POST .../branches/{branch_id}/feedback`.
fn guardian_branch_messages(daemon: &Daemon, id: &str, branch_id: &str) -> Reply {
    match daemon.lock().guardian_branch_messages(id, branch_id) {
        Ok(messages) => json(200, &MessagesResponse { messages }),
        Err(e) => store_error(&e),
    }
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
    // RAL-234: timed end-to-end so a slow first request after a restart is
    // diagnosable from the log/Cartographer alone instead of guesswork --
    // see the `startup` note emitted just before `run_http_loop` below, which
    // reports how much of this was schema-open vs. recovery+reap.
    let startup_started = Instant::now();
    let store = Store::open(db_path).map_err(|e| std::io::Error::other(e.to_string()))?;
    let schema_open_ms = startup_started.elapsed().as_millis();
    // RAL-332: apply `[daemon].default_user_is_admin` before serving any
    // request, so `GET /api/whoami` reflects it on the very first poll.
    bootstrap_default_user_admin(&store);
    // Bind the port before touching any squad state. `recover_orphaned_squads`
    // assumes "nothing is executing yet, so any running row is orphaned" —
    // true only for the process that actually wins the port. A second
    // `serve()` invocation racing against an already-running daemon (e.g. an
    // agent cell testing against the same shared db_path) would otherwise
    // stomp the live daemon's in-flight `running` rows to `pending` before
    // failing here on the bind, corrupting state without ever serving a
    // request. Binding first makes that race fail closed instead.
    let server = tiny_http::Server::http(addr).map_err(|e| std::io::Error::other(e.to_string()))?;
    // Crash recovery before anything schedules: a previous unclean shutdown may
    // have left squads `Running` with no worker. Reset them to `Pending` so the
    // scheduler resumes them; finished cells are preserved and skipped, so
    // only the unfinished tail re-runs (RAL-19).
    match store.recover_orphaned_squads() {
        Ok(ids) if !ids.is_empty() => {
            crate::rlog!(
                WARNING,
                "ralphus [recovery] {} orphaned squad(s) reset to pending: {}",
                ids.len(),
                ids.join(", ")
            );
            let _ = store.cartographer_log(crate::cartographer::CartographerEntry {
                level: crate::logging::LogLevel::WARNING,
                source: "recovery",
                message: "orphaned squads reset to pending",
                scope: Some("squad"),
                squad_id: None,
                guardian_id: None,
                cell_id: None,
                task: None,
                log_path: None,
                payload: serde_json::json!({"squad_ids": ids}),
                admin_only: false,
            });
        }
        Ok(_) => {}
        Err(e) => crate::rlog!(ERROR, "ralphus [recovery] squad recovery failed: {e}"),
    }
    // RAL-201: reconcile any remote `exec` handle left behind by the crash
    // `recover_orphaned_squads` just reset above -- same "nothing is executing
    // yet" invariant, so it belongs immediately alongside it rather than
    // deferred to first use.
    crate::remote_runner::reconcile_remote_exec_handles(&store);
    // RAL-375: guardians stuck in `merging` after an unclean shutdown (and any
    // branch whose feedback application was interrupted mid-run) are resumed
    // by `scheduler::recover_interrupted_reviews`, run once at the top of
    // `scheduler::run_loop` below -- NOT here. This used to eagerly reset
    // every `merging` guardian to `merge_failed` (RAL-48), which always beat
    // that scheduler-side auto-resume to the punch (this runs synchronously
    // before the scheduler thread is even spawned), silently defeating it and
    // requiring a human to manually retry every interrupted merge. See
    // `scheduler::recover_interrupted_reviews`'s doc comment for the full
    // ordering requirement.
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
                squad_id: None,
                guardian_id: None,
                cell_id: None,
                task: None,
                log_path: None,
                payload: serde_json::json!({"pairs": pairs}),
                admin_only: false,
            });
        }
        Ok(_) => {}
        Err(e) => crate::rlog!(
            ERROR,
            "ralphus [recovery] input resolution recovery failed: {e}"
        ),
    }
    // RAL-318 bug 3: repair any Triage pool/threshold/schedule row still
    // keyed by its pre-fix raw worktree path instead of the resolved
    // project name, and fire any pool that's now correctly counted and
    // already past its threshold. Naturally idempotent (see the function's
    // own doc comment), so unconditional on every restart is safe.
    crate::reviews::repair_triage_pool_keys(&store);
    // Zombie tmux.exe reaping: on the Windows tmux-alternative (psmux) this
    // project targets, `kill-session` frees a cell's *name* but never
    // actually terminates the backing OS process (see
    // PSMUX_CRASH_NOTES.local.md's "kill-session leaks the underlying OS
    // process" finding) -- every cell ever run, cancelled, or restarted
    // across this machine's history can leave one behind, and they persist
    // across daemon restarts. Safe to force-kill all `ralphus_`-named ones
    // unconditionally here, and only here: nothing has been dispatched yet
    // (same invariant `recover_orphaned_squads` above relies on), so anything
    // found is guaranteed orphaned. Never touches psmux's own `__warm__`
    // pool -- see `tmux::reap_orphaned_sessions_at_startup`'s doc comment.
    let reap_started = Instant::now();
    let reaped = crate::tmux::reap_orphaned_sessions_at_startup();
    let reap_ms = reap_started.elapsed().as_millis();
    // RAL-234: this whole block (recovery + reap) runs after the port is
    // bound but before `run_http_loop` starts accepting/processing requests
    // below, so a request that lands right at daemon start queues behind it
    // -- this is what showed up as "Reviews page load is slow, especially
    // right after the daemon starts". `reap_ms` used to isolate a large,
    // reliably-reproducible cost here: `reap_orphaned_sessions_at_startup`
    // shelled out to `powershell -Command "Get-CimInstance Win32_Process
    // ..."`, whose own CLR startup + WMI COM query routinely ran several
    // hundred ms to multiple seconds, on every single restart, unconditional
    // on there being anything to reap. `force_kill_tmux_processes` (see its
    // doc comment) has since been switched to a direct `sysinfo` process-
    // table scan, the same fix already proven for `find_server_pid_windows`;
    // this instrumentation is kept so a regression back to something equally
    // slow -- or a new cost elsewhere in this block -- shows up in the
    // numbers instead of only as a vague future bug report. Logged
    // unconditionally (not just when slow) so restarts are comparable.
    let total_startup_ms = startup_started.elapsed().as_millis();
    crate::rlog!(
        INFO,
        "ralphus [scheduler] daemon startup recovery completed in {total_startup_ms}ms (schema_open={schema_open_ms}ms, tmux_reap={reap_ms}ms) before serving requests"
    );
    let _ = store.cartographer_log(crate::cartographer::CartographerEntry {
        level: crate::logging::LogLevel::INFO,
        source: "scheduler",
        message: "daemon startup recovery completed before serving requests",
        scope: None,
        squad_id: None,
        guardian_id: None,
        cell_id: None,
        task: None,
        log_path: None,
        payload: serde_json::json!({
            "total_startup_ms": total_startup_ms,
            "schema_open_ms": schema_open_ms,
            "tmux_reap_ms": reap_ms,
        }),
        admin_only: false,
    });
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
            squad_id: None,
            guardian_id: None,
            cell_id: None,
            task: None,
            log_path: None,
            payload: serde_json::json!({"count": reaped}),
            admin_only: false,
        });
    }
    // RAL-219: every route requires this token (`Authorization: Bearer
    // <token>`), generated once and persisted to `state_dir()/daemon.token`
    // so restarts and other local processes (CLI, librarian) keep working
    // against the same value. Loaded before the scheduler/HTTP loop start so
    // a failure to persist it fails the whole startup rather than silently
    // serving an unauthenticated API.
    let token = crate::token::load_or_create(&crate::token_path())
        .map_err(|e| std::io::Error::other(format!("failed to load/create daemon token: {e}")))?;
    crate::rlog!(
        INFO,
        "ralphus [token] auth token ready at {}",
        crate::token_path().display()
    );
    let daemon = Arc::new(Daemon::new(store, max_concurrent).with_token(token));

    // The remote Open Agent terminal relay (RAL-355 Phase 10) listens one
    // port above the main API, on the same host it bound to -- so a remote
    // browser client reaches it exactly as it reaches the daemon API itself.
    // Deliberately non-fatal: a bind failure here (port already taken by
    // something else) disables *only* the remote terminal relay, not the
    // whole daemon -- `mint_terminal_ticket_route` already reports
    // `503 relay_unavailable` when `terminal_relay_port()` is still `0`.
    match server.server_addr().to_ip() {
        Some(bound) => {
            let relay_port = bound.port().saturating_add(1);
            if let Err(e) =
                crate::terminal_relay::start(bound.ip(), relay_port, Arc::clone(&daemon))
            {
                crate::rlog!(
                    WARNING,
                    "ralphus [terminal] could not start the terminal-relay listener on {}:{relay_port}: {e}",
                    bound.ip()
                );
            }
        }
        None => {
            crate::rlog!(
                WARNING,
                "ralphus [terminal] daemon is not bound to an IP socket; the remote terminal relay is disabled"
            );
        }
    }

    // Scheduler runs on its own thread, sharing the store via Arc<Mutex> and the
    // cancellation registry so a `cancel` request can reach its workers. The
    // runner shares the daemon's PID registry so `/api/resources` can attribute
    // OS metrics to the cells it spawns (RAL-11).
    let local_runner: Arc<dyn Runner> = Arc::new(
        SubprocessRunner::from_env()
            .with_registry(daemon.procs_handle())
            .with_cartographer(daemon.store_handle())
            .with_detachments(daemon.detachments_handle()),
    );
    // RAL-185: the scheduler holds a router rather than the local runner
    // directly, so a cell carrying a `machine` is dispatched to its provider
    // while every local cell takes exactly the path it always did.
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
    // RAL-279: periodically reconcile a PR stack's base(s) against what the
    // forge actually has recorded, so a base retargeted (or an intervening
    // branch's PR closed/merged) directly on GitHub/GitLab -- outside
    // ralphus entirely -- still gets pulled back into this guardian's branch
    // order instead of silently drifting forever.
    crate::pr::spawn_pr_base_drift_poller(daemon.store_handle());
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

    // tiny_http 0.12 treats any accept() error as fatal: its accept thread
    // reports the error via the `log` crate (no logger is installed here, so
    // the message vanishes) and exits, after which the loop above ends. A
    // transient WSAENOBUFS/WSAEMFILE under machine-wide socket pressure
    // (many agents + board polling churning connections) therefore used to
    // end the whole daemon silently mid-flight — and the kill-on-close job
    // object (`jobobject.rs`) took every running cell down with it. Treat it
    // as recoverable instead: log loudly, rebind the listener, keep serving.
    // Only a real shutdown request ends this loop.
    let bound_addr = server.server_addr().to_ip();
    let mut server = server;
    loop {
        match run_http_loop(server, &daemon) {
            HttpLoopEnd::Shutdown => break,
            HttpLoopEnd::AcceptError(e) => {
                crate::rlog!(
                    ERROR,
                    "ralphus [http] listener stopped accepting connections ({e}); rebinding"
                );
                let _ = daemon
                    .lock()
                    .cartographer_log(crate::cartographer::CartographerEntry {
                        level: crate::logging::LogLevel::ERROR,
                        source: "daemon",
                        message: "http listener died; rebinding",
                        scope: None,
                        squad_id: None,
                        guardian_id: None,
                        cell_id: None,
                        task: None,
                        log_path: None,
                        payload: serde_json::json!({ "error": e.to_string() }),
                        admin_only: false,
                    });
                let Some(addr) = bound_addr else {
                    return Err(std::io::Error::other(
                        "http listener died and the daemon is not bound to an IP socket; cannot rebind",
                    ));
                };
                server = rebind_http_listener(addr)?;
            }
        }
    }
    Ok(())
}

/// Re-create the HTTP listener after tiny_http's accept thread died (see the
/// rebind loop at the end of [`serve`]). Retries briefly: the port is freed
/// when the previous `Server` is dropped, but the same resource pressure
/// that killed the accept thread can make the first bind attempts fail too.
fn rebind_http_listener(addr: std::net::SocketAddr) -> std::io::Result<tiny_http::Server> {
    const ATTEMPTS: u32 = 20;
    let mut last_err = String::new();
    for attempt in 1..=ATTEMPTS {
        match tiny_http::Server::http(addr) {
            Ok(server) => {
                crate::rlog!(
                    INFO,
                    "ralphus [http] listener rebound on {addr} (attempt {attempt})"
                );
                return Ok(server);
            }
            Err(e) => {
                crate::rlog!(
                    WARNING,
                    "ralphus [http] rebind attempt {attempt}/{ATTEMPTS} on {addr} failed: {e}"
                );
                last_err = e.to_string();
                std::thread::sleep(Duration::from_millis(500));
            }
        }
    }
    Err(std::io::Error::other(format!(
        "could not rebind the http listener on {addr} after {ATTEMPTS} attempts: {last_err}"
    )))
}

/// Serve requests from an already-bound server against an already-open store.
/// Does NOT start the scheduler — exposed so tests can bind an ephemeral port
/// and drive the API without squads executing underneath them.
pub fn serve_with(server: tiny_http::Server, store: Store, max_concurrent: i64) {
    let daemon = Arc::new(Daemon::new(store, max_concurrent));
    run_http_loop(server, &daemon);
}

/// Like [`serve_with`], but requires `token` on every route (RAL-219) —
/// exposed so over-the-wire tests can exercise the auth gate without a full
/// `serve()`/`state_dir()` setup.
pub fn serve_with_token(
    server: tiny_http::Server,
    store: Store,
    max_concurrent: i64,
    token: String,
) {
    let daemon = Arc::new(Daemon::new(store, max_concurrent).with_token(token));
    run_http_loop(server, &daemon);
}

/// The SSE push endpoint (RAL-167). Handled specially in `run_http_loop`
/// below rather than through `route()`: every other endpoint returns a
/// bounded `Reply` immediately, but this one holds the connection open
/// indefinitely.
const EVENTS_PATH: &str = "/api/events";

/// Case-insensitive header lookup (`tiny_http::Header::field` compares
/// case-insensitively via `.equiv`).
fn header_value(request: &tiny_http::Request, name: &'static str) -> Option<String> {
    request
        .headers()
        .iter()
        .find(|h| h.field.equiv(name))
        .map(|h| h.value.as_str().to_string())
}

fn cors_header(name: &'static [u8], value: &str) -> tiny_http::Header {
    tiny_http::Header::from_bytes(name, value.as_bytes()).expect("valid header")
}

/// `Access-Control-Allow-Origin` (echoing the exact allowed origin, never a
/// wildcard, per RAL-220) plus `Vary: Origin` so a shared cache never serves
/// one origin's CORS-tagged response to another.
fn cors_response_headers(origin: &str) -> Vec<tiny_http::Header> {
    vec![
        cors_header(b"Access-Control-Allow-Origin", origin),
        cors_header(b"Vary", "Origin"),
    ]
}

/// The additional headers a preflight `OPTIONS` response needs beyond
/// [`cors_response_headers`], telling the browser which methods/headers the
/// real request may use.
fn cors_preflight_headers(origin: &str) -> Vec<tiny_http::Header> {
    let mut headers = cors_response_headers(origin);
    headers.push(cors_header(
        b"Access-Control-Allow-Methods",
        "GET, POST, DELETE, OPTIONS",
    ));
    headers.push(cors_header(
        b"Access-Control-Allow-Headers",
        "Content-Type, traceparent, X-Ralphus-User",
    ));
    headers
}

/// Resolve the CORS decision for one incoming request against the effective
/// (global + per-project `.ralphus.toml`) `[cors]` allow-list.
///
/// Reads the allow-list through [`crate::config::load_cors_config_cached`]
/// rather than [`crate::config::load_cors_config`]: this runs for every
/// inbound request, and the uncached loader does a global-config read, a
/// `find_project_config` directory walk, and two TOML parses each time.
fn resolve_cors(request: &tiny_http::Request) -> ralphus_core::cors::CorsDecision {
    let origin = header_value(request, "Origin");
    let host = header_value(request, "Host");
    let allowed = crate::config::load_cors_config_cached().allowed_origins;
    ralphus_core::cors::decide(origin.as_deref(), host.as_deref(), &allowed)
}

/// How many read-only (`GET`) requests the daemon answers concurrently.
///
/// Small on purpose: the point is that one slow read cannot stall the accept
/// loop, not that reads scale out. Sized for several actively-merging
/// reviews' worth of fan-out, not just one -- each review's own poll issues
/// several of these (`sync-status` runs `git fetch`, `comments` calls the
/// forge API, `base-branches` shells out to git), so two or three reviews
/// merging at once can otherwise saturate a smaller pool for many seconds,
/// starving unrelated GETs (Users/Projects/Triage) behind them even though
/// their own handlers are trivial. Still small enough to keep pressure on
/// the global `Mutex<Store>` low.
const READ_WORKERS: usize = 12;

/// Handler wall time at or above which a request is logged as slow.
///
/// One second is well past anything the API is expected to take -- the
/// endpoints that legitimately exceed it are the ones that shell out to git
/// or call a forge over the network, and naming them in the log is the point.
const SLOW_REQUEST_MS: u128 = 1000;

/// One accepted request, with everything read off the wire, waiting to be
/// answered either inline on the accept loop or on a [`ReadPool`] worker.
struct PendingRequest {
    request: tiny_http::Request,
    method: String,
    url: String,
    body: String,
    traceparent: Option<String>,
    auth_header: Option<String>,
    user_header: Option<String>,
    cors: ralphus_core::cors::CorsDecision,
    /// When the accept loop finished reading this request, so the time it
    /// then spent waiting for a worker is reportable separately from the
    /// time its handler took.
    accepted_at: Instant,
}

/// A fixed pool of threads answering read-only requests off the accept loop.
///
/// `tiny_http` is a synchronous, one-request-at-a-time server, so without
/// this every request -- including a `POST /api/guardians/{id}/merge`, which
/// is just a DB state transition plus a thread spawn -- waits for whatever
/// handler the accept loop is already inside. Several board endpoints
/// legitimately take seconds: `GET /api/pull-requests/{id}/sync-status` runs
/// `git fetch` against the remote, `GET /api/pull-requests/{id}/comments`
/// calls the forge HTTP API, `GET /api/guardians/{id}/base-branches` shells
/// out to git. The Reviews tab polls several of them per refresh, so a click
/// landing mid-poll queues behind all of them.
///
/// Only `GET` is dispatched here. Every mutating method stays on the accept
/// loop, which keeps the invariant the mutating handlers were written under:
/// **no two state-changing requests ever run at the same time**, so a handler
/// that reads then writes across separate `Store` lock acquisitions still
/// cannot be interleaved by another request. Concurrency is therefore bounded
/// to read-only handlers, which take the store lock only to read and can
/// safely observe a mutation mid-flight -- the board already tolerates that,
/// since the scheduler and merge workers mutate the store underneath it
/// constantly.
struct ReadPool {
    tx: std::sync::mpsc::Sender<PendingRequest>,
}

impl ReadPool {
    /// Spawn `workers` threads draining a shared queue of read requests.
    fn new(daemon: &Arc<Daemon>, workers: usize) -> Self {
        let (tx, rx) = std::sync::mpsc::channel::<PendingRequest>();
        let rx = Arc::new(Mutex::new(rx));
        for _ in 0..workers {
            let rx = Arc::clone(&rx);
            let daemon = Arc::clone(daemon);
            std::thread::spawn(move || {
                loop {
                    // The guard is dropped as this `let` statement ends, so
                    // exactly one idle worker blocks in `recv()` at a time
                    // and the rest are free the instant it takes a job.
                    let job = rx.lock().expect("read pool mutex poisoned").recv();
                    match job {
                        Ok(pending) => answer_request(&daemon, pending),
                        Err(_) => break,
                    }
                }
            });
        }
        Self { tx }
    }

    /// Queue `pending` for a worker, or hand it back if every worker thread
    /// has died (the channel is closed) so the caller can answer it inline.
    fn dispatch(&self, pending: PendingRequest) -> Option<PendingRequest> {
        self.tx.send(pending).err().map(|e| e.0)
    }
}

/// Authorize, route, and respond to one already-read request.
///
/// Runs either on the accept loop (mutating methods) or on a [`ReadPool`]
/// worker (`GET`), so it must not touch anything the accept loop owns
/// exclusively -- notably `Daemon::events_tickets`, which is consumed before
/// dispatch.
fn answer_request(daemon: &Daemon, pending: PendingRequest) {
    let PendingRequest {
        request,
        method,
        url,
        body,
        traceparent,
        auth_header,
        user_header,
        cors,
        accepted_at,
    } = pending;
    let queued_ms = accepted_at.elapsed().as_millis();
    let handler_started = Instant::now();
    // RAL-219: every route requires the configured bearer token (when
    // one is configured - see `Daemon::authorized`), checked here at the
    // HTTP boundary rather than inside `route()` so its ~100 in-process
    // unit tests stay auth-agnostic. `/api/events` never reaches this
    // point (handled in the accept loop); RAL-222 owns its auth separately.
    let reply = if daemon.authorized(auth_header.as_deref()) {
        route_with_trace_for_user(
            daemon,
            &method,
            &url,
            &body,
            traceparent.as_deref(),
            user_header.as_deref(),
        )
    } else {
        error(
            401,
            "unauthorized",
            "missing or invalid bearer token",
            vec![],
        )
    };
    let handler_ms = handler_started.elapsed().as_millis();
    let status = reply.status;
    let header = tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..])
        .expect("valid header");
    let mut response = tiny_http::Response::from_string(reply.body)
        .with_status_code(status)
        .with_header(header);
    if let ralphus_core::cors::CorsDecision::Allowed(origin) = &cors {
        for h in cors_response_headers(origin) {
            response = response.with_header(h);
        }
    }
    let _ = request.respond(response);
    // Logged after responding so the measurement never adds to the latency it
    // measures. Deliberately `rlog!`-only rather than a Cartographer row:
    // every Cartographer write publishes an SSE event, the board refreshes on
    // one, and that refresh issues more requests -- a per-request slow record
    // would feed itself.
    if handler_ms >= SLOW_REQUEST_MS {
        crate::rlog!(
            WARNING,
            "ralphus [http] {method} {url} -> {status} SLOW: handler {handler_ms}ms (queued {queued_ms}ms)"
        );
    } else {
        crate::rlog!(
            DEBUG,
            "ralphus [http] {method} {url} -> {status} ({handler_ms}ms, queued {queued_ms}ms)"
        );
    }
}

/// Why [`run_http_loop`] stopped serving.
enum HttpLoopEnd {
    /// `POST /api/daemon/shutdown` was handled — exit cleanly.
    Shutdown,
    /// `Server::recv()` failed: tiny_http 0.12 treats any `accept()` error
    /// as fatal — its accept thread pushes the error and exits, after which
    /// `recv()` yields it exactly once (and would block forever if called
    /// again). The listener must be rebuilt to keep serving.
    AcceptError(std::io::Error),
}

fn run_http_loop(server: tiny_http::Server, daemon: &Arc<Daemon>) -> HttpLoopEnd {
    let read_pool = ReadPool::new(daemon, READ_WORKERS);
    loop {
        let mut request = match server.recv() {
            Ok(r) => r,
            Err(e) => return HttpLoopEnd::AcceptError(e),
        };
        let method = request.method().as_str().to_string();
        // `route()` splits `path` on `?` itself (it needs the query string for
        // filtered/paginated endpoints like `/api/cartographer`), so the full
        // URL is forwarded verbatim rather than pre-stripped here.
        let url = request.url().to_string();

        // RAL-220: reject a disallowed cross-origin browser request outright,
        // before it ever reaches a handler -- omitting `Access-Control-*`
        // headers alone only stops the browser from *reading* the response,
        // not from the request executing (see `ralphus_core::cors`'s doc
        // comment). A request with no `Origin` header (same-origin browser
        // traffic, or any non-browser caller) is unaffected.
        let cors = resolve_cors(&request);
        if cors == ralphus_core::cors::CorsDecision::Denied {
            // ralphus[ignore-rlog-pair]: this transport-only diagnostic has no event entity; handlers emit the structured request or state record
            crate::rlog!(
                WARNING,
                "ralphus [http] {method} {url} blocked: cross-origin request denied"
            );
            let response = tiny_http::Response::from_string(
                error(
                    403,
                    "origin_not_allowed",
                    "cross-origin request denied",
                    vec![],
                )
                .body,
            )
            .with_status_code(403)
            .with_header(cors_header(b"Content-Type", "application/json"));
            let _ = request.respond(response);
            continue;
        }

        if method == "OPTIONS" {
            // A CORS preflight (or a stray OPTIONS from a non-browser
            // client) never reaches `route()` -- there is nothing to
            // dispatch, just headers to answer with.
            let mut response = tiny_http::Response::from_string("").with_status_code(204);
            if let ralphus_core::cors::CorsDecision::Allowed(origin) = &cors {
                for h in cors_preflight_headers(origin) {
                    response = response.with_header(h);
                }
            }
            let _ = request.respond(response);
            continue;
        }

        if method == "GET" && url.split('?').next().unwrap_or(&url) == EVENTS_PATH {
            // RAL-222: `/api/events` bypasses `route()` entirely (see below),
            // so RAL-219's bearer-token check in `daemon.authorized()` never
            // runs for it -- it needs its own check, right here, ahead of the
            // SSE branch splitting off onto its own thread. A bearer token
            // can't be used directly (an `EventSource` can't set headers), so
            // this validates the short-lived ticket minted by `POST
            // /api/events/ticket` instead -- see `crate::token`'s module doc
            // comment. Rejected before `serve_events_stream` starts writing
            // any data, and before the thread that would otherwise hold the
            // connection open is even spawned.
            let query = url.split_once('?').map_or("", |(_, q)| q);
            let ticket = query_param(query, "ticket").map(url_decode);
            if !daemon.consume_events_ticket(ticket.as_deref()) {
                let header =
                    tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..])
                        .expect("valid header");
                let response = tiny_http::Response::from_string(
                    error(
                        401,
                        "unauthorized",
                        "missing or invalid events ticket",
                        vec![],
                    )
                    .body,
                )
                .with_status_code(401)
                .with_header(header);
                let _ = request.respond(response);
                continue;
            }
            // A long-lived SSE stream must never block this accept loop from
            // serving anyone else -- tiny_http is otherwise a synchronous,
            // one-request-at-a-time server (RAL-167's own risk list). Hand it
            // off to its own thread; it lives until the client disconnects.
            let store = daemon.store_handle();
            let allow_origin = match cors {
                ralphus_core::cors::CorsDecision::Allowed(origin) => Some(origin),
                _ => None,
            };
            std::thread::spawn(move || {
                serve_events_stream(request, &store, allow_origin.as_deref())
            });
            continue;
        }

        let traceparent = header_value(&request, "traceparent");
        let auth_header = header_value(&request, "Authorization");
        let user_header = header_value(&request, "X-Ralphus-User");

        let mut body = String::new();
        let _ = request.as_reader().read_to_string(&mut body);

        let pending = PendingRequest {
            request,
            method,
            url,
            body,
            traceparent,
            auth_header,
            user_header,
            cors,
            accepted_at: Instant::now(),
        };
        // Read-only requests go to the pool so a slow one cannot stall the
        // accept loop; everything that mutates state is answered right here,
        // keeping mutating requests totally ordered. See `ReadPool`.
        if pending.method == "GET" {
            // `dispatch` only hands the request back if every worker thread
            // is gone (they all panicked); answering it inline then is
            // better than dropping the connection on the floor.
            if let Some(returned) = read_pool.dispatch(pending) {
                answer_request(daemon, returned);
            }
        } else {
            answer_request(daemon, pending);
        }
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
            return HttpLoopEnd::Shutdown;
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
/// data: ...\n\n` block (`event:` is the entity kind — `squad`/`guardian`/
/// `other`, see `crate::events::EventKind` — so the browser can react
/// differently per surface without a full-page refetch on every event); a
/// receive timeout with nothing pending becomes an SSE comment line so idle
/// proxies/browsers don't time the connection out. Any write failure (the
/// client went away) ends the loop and unsubscribes.
///
/// `allow_origin` carries the exact origin to echo as
/// `Access-Control-Allow-Origin` (RAL-220) when the caller already resolved
/// an `Allowed` CORS decision for this connection; `None` (same-origin, or
/// any non-browser caller) adds no CORS headers, matching the buffered reply
/// path in `run_http_loop`.
fn serve_events_stream(
    request: tiny_http::Request,
    store: &Arc<Mutex<Store>>,
    allow_origin: Option<&str>,
) {
    let (sub_id, rx) = store.lock().unwrap().event_bus().subscribe();
    // ralphus[ignore-rlog-pair]: this transport-only diagnostic has no event entity; handlers emit the structured request or state record
    crate::rlog!(DEBUG, "ralphus [http] SSE client connected sub_id={sub_id}");
    let mut writer = request.into_writer();
    let cors_lines = match allow_origin {
        Some(origin) => format!("Access-Control-Allow-Origin: {origin}\r\nVary: Origin\r\n"),
        None => String::new(),
    };
    let preamble = format!(
        "HTTP/1.1 200 OK\r\n\
Content-Type: text/event-stream\r\n\
Cache-Control: no-cache\r\n\
Connection: keep-alive\r\n\
X-Accel-Buffering: no\r\n\
{cors_lines}\r\n"
    );
    let mut ok = writer.write_all(preamble.as_bytes()).is_ok() && writer.flush().is_ok();
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
    // ralphus[ignore-rlog-pair]: this transport-only diagnostic has no event entity; handlers emit the structured request or state record
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

    const GOOD: &str = "[[task]]\nname=\"t\"\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\n";

    fn daemon() -> Daemon {
        Daemon::new(Store::open_in_memory().unwrap(), 12)
    }

    fn submit_body(toml: &str) -> String {
        serde_json::to_string(&serde_json::json!({ "toml": toml })).unwrap()
    }

    /// Submit [`GOOD`] and return its squad id.
    fn submit_squad(d: &Daemon) -> String {
        let r = route(d, "POST", "/api/squads", &submit_body(GOOD));
        assert_eq!(r.status, 201, "{}", r.body);
        let v: serde_json::Value = serde_json::from_str(&r.body).unwrap();
        v["squad_id"].as_str().unwrap().to_string()
    }

    /// Isolate this test's terminal-log storage (see
    /// `crate::terminal_log::set_test_root`). Every test that writes, reads, or
    /// deletes terminal logs -- or deletes a squad/guardian, which wipes the
    /// `ralphus_*_` log prefix -- runs against its own temp directory so
    /// parallel tests can't race on the shared `~/.ralphus/terminal_logs`
    /// namespace (e.g. a squad-delete test deleting `ralphus_squad-..._*` out
    /// from under the terminal-log reader mid-run).
    fn isolated_terminal_root() -> std::path::PathBuf {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static N: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "ralphus-server-tlog-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create isolated terminal-log root");
        crate::terminal_log::set_test_root(dir.clone());
        dir
    }

    // -----------------------------------------------------------------------
    // Structured check input substitution + cleanup chaining (RAL-164)
    // -----------------------------------------------------------------------

    fn check_input(name: &str, message: &str, default: &str) -> crate::guardian::CheckInput {
        crate::guardian::CheckInput {
            name: name.to_string(),
            message: message.to_string(),
            default: default.to_string(),
            r#type: crate::guardian::CheckInputType::String,
        }
    }

    fn check_input_typed(
        name: &str,
        message: &str,
        default: &str,
        r#type: crate::guardian::CheckInputType,
    ) -> crate::guardian::CheckInput {
        crate::guardian::CheckInput {
            name: name.to_string(),
            message: message.to_string(),
            default: default.to_string(),
            r#type,
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

    // -----------------------------------------------------------------------
    // RAL-221: CheckInput type validation
    // -----------------------------------------------------------------------

    #[test]
    fn check_input_type_failures_rejects_shell_metacharacters_where_int_expected() {
        // The original exploit shape from RAL-221: a value with shell
        // metacharacters submitted where an int-typed input was expected.
        let inputs = vec![check_input_typed(
            "port",
            "Port",
            "7890",
            crate::guardian::CheckInputType::Int,
        )];
        let submitted = std::collections::HashMap::from([(
            "port".to_string(),
            "1.0 & calc.exe & rem".to_string(),
        )]);
        let stored = std::collections::HashMap::new();
        let failures = check_input_type_failures(&inputs, &submitted, &stored);
        assert_eq!(failures.len(), 1);
        assert!(failures[0].contains("port"));
        assert!(failures[0].contains("int"));
    }

    #[test]
    fn check_input_type_failures_accepts_legitimate_int_value() {
        let inputs = vec![check_input_typed(
            "port",
            "Port",
            "7890",
            crate::guardian::CheckInputType::Int,
        )];
        let submitted = std::collections::HashMap::from([("port".to_string(), "9001".to_string())]);
        let stored = std::collections::HashMap::new();
        assert!(check_input_type_failures(&inputs, &submitted, &stored).is_empty());
    }

    #[test]
    fn check_input_type_failures_string_type_is_unconstrained() {
        // Existing legitimate free-text values (the default type) must keep
        // passing validation unchanged.
        let inputs = vec![check_input("branch", "Branch", "main")];
        let submitted = std::collections::HashMap::from([(
            "branch".to_string(),
            "feature/foo-bar_1".to_string(),
        )]);
        let stored = std::collections::HashMap::new();
        assert!(check_input_type_failures(&inputs, &submitted, &stored).is_empty());
    }

    #[test]
    fn check_input_type_failures_rejects_stale_stored_value_too() {
        // A value already sitting in a guardian's `input_values` from before
        // this input ever declared a type is checked exactly the same as a
        // freshly submitted one -- rejected on next read, not silently kept.
        let inputs = vec![check_input_typed(
            "port",
            "Port",
            "7890",
            crate::guardian::CheckInputType::Int,
        )];
        let submitted = std::collections::HashMap::new();
        let stored = std::collections::HashMap::from([(
            "port".to_string(),
            "1.0 & calc.exe & rem".to_string(),
        )]);
        let failures = check_input_type_failures(&inputs, &submitted, &stored);
        assert_eq!(failures.len(), 1);
        assert!(failures[0].contains("port"));
    }

    #[test]
    fn guardian_run_manual_commands_rejects_injection_shaped_int_input() {
        let d = daemon();
        let id = d.lock().create_guardian("g", "main", "/r").unwrap();
        d.lock()
            .set_guardian_manual_commands(
                &id,
                &[crate::guardian::GuardianCheck {
                    label: None,
                    command: Some("ralphus-daemon serve --port {port}".to_string()),
                    prompt: None,
                    cleanup_command: None,
                    inputs: vec![check_input_typed(
                        "port",
                        "Port",
                        "7890",
                        crate::guardian::CheckInputType::Int,
                    )],
                }],
                None,
                None,
            )
            .unwrap();

        let body = serde_json::to_string(&serde_json::json!({
            "index": 0,
            "inputs": {"port": "1.0 & calc.exe & rem"}
        }))
        .unwrap();
        let r = route(
            &d,
            "POST",
            &format!("/api/guardians/{id}/run-manual-commands"),
            &body,
        );
        assert_eq!(r.status, 400);
        assert!(r.body.contains("invalid_check_input"));
        assert!(r.body.contains("port"));
        // The rejected value must not have been persisted as the new stored
        // default for this input.
        let g = d.lock().get_guardian(&id).unwrap();
        assert!(g.input_values.is_empty());
    }

    #[test]
    fn guardian_run_action_hint_rejects_injection_shaped_int_input() {
        let d = daemon();
        let id = d.lock().create_guardian("g", "main", "/r").unwrap();
        d.lock()
            .set_guardian_action_hints(
                &id,
                &[crate::guardian::GuardianCheck {
                    label: Some("Serve locally".to_string()),
                    command: Some("ralphus-daemon serve --port {port}".to_string()),
                    prompt: None,
                    cleanup_command: None,
                    inputs: vec![check_input_typed(
                        "port",
                        "Port",
                        "7890",
                        crate::guardian::CheckInputType::Int,
                    )],
                }],
            )
            .unwrap();

        let body = serde_json::to_string(&serde_json::json!({
            "index": 0,
            "inputs": {"port": "1.0 & calc.exe & rem"}
        }))
        .unwrap();
        let r = route(
            &d,
            "POST",
            &format!("/api/guardians/{id}/run-action-hint"),
            &body,
        );
        assert_eq!(r.status, 400);
        assert!(r.body.contains("invalid_check_input"));
        assert!(r.body.contains("port"));
        let g = d.lock().get_guardian(&id).unwrap();
        assert!(g.input_values.is_empty());
    }

    // ── RAL-312: board.html's request field is `run_cleanup`, matching what
    // these two handlers deserialize (not the `squad_cleanup` name the board
    // briefly sent post-RAL-239) ────────────────────────────────────────────

    #[test]
    fn guardian_run_manual_commands_accepts_run_cleanup_body_field() {
        let d = daemon();
        let id = d.lock().create_guardian("g", "main", "/r").unwrap();
        d.lock()
            .set_guardian_manual_commands(
                &id,
                &[crate::guardian::GuardianCheck {
                    label: None,
                    command: Some("ralphus-daemon serve --port {port}".to_string()),
                    prompt: None,
                    cleanup_command: None,
                    inputs: vec![check_input_typed(
                        "port",
                        "Port",
                        "7890",
                        crate::guardian::CheckInputType::Int,
                    )],
                }],
                None,
                None,
            )
            .unwrap();

        // Same injection-shaped rejection as
        // `guardian_run_manual_commands_rejects_injection_shaped_int_input`,
        // but with a `run_cleanup` field on the body -- proves the field name
        // the handler expects round-trips through `route()` without being
        // silently dropped as an unrecognized key.
        let body = serde_json::to_string(&serde_json::json!({
            "index": 0,
            "inputs": {"port": "1.0 & calc.exe & rem"},
            "run_cleanup": true
        }))
        .unwrap();
        let r = route(
            &d,
            "POST",
            &format!("/api/guardians/{id}/run-manual-commands"),
            &body,
        );
        assert_eq!(r.status, 400);
        assert!(r.body.contains("invalid_check_input"));
    }

    #[test]
    fn guardian_run_action_hint_accepts_run_cleanup_body_field() {
        let d = daemon();
        let id = d.lock().create_guardian("g", "main", "/r").unwrap();
        d.lock()
            .set_guardian_action_hints(
                &id,
                &[crate::guardian::GuardianCheck {
                    label: Some("Serve locally".to_string()),
                    command: Some("ralphus-daemon serve --port {port}".to_string()),
                    prompt: None,
                    cleanup_command: None,
                    inputs: vec![check_input_typed(
                        "port",
                        "Port",
                        "7890",
                        crate::guardian::CheckInputType::Int,
                    )],
                }],
            )
            .unwrap();

        let body = serde_json::to_string(&serde_json::json!({
            "index": 0,
            "inputs": {"port": "1.0 & calc.exe & rem"},
            "run_cleanup": true
        }))
        .unwrap();
        let r = route(
            &d,
            "POST",
            &format!("/api/guardians/{id}/run-action-hint"),
            &body,
        );
        assert_eq!(r.status, 400);
        assert!(r.body.contains("invalid_check_input"));
    }

    #[test]
    fn guardian_run_action_hint_rejects_wrong_type_for_run_cleanup_field() {
        // A malformed `run_cleanup` (wrong JSON type) fails to deserialize
        // only if the handler's body struct actually declares a field named
        // `run_cleanup` -- if the field name regressed back to
        // `squad_cleanup`, this value would be silently ignored as an
        // unrecognized key and the request would proceed past body parsing
        // instead of 400ing here.
        let d = daemon();
        let id = d.lock().create_guardian("g", "main", "/r").unwrap();
        let body = r#"{"index": 0, "run_cleanup": "not-a-bool"}"#;
        let r = route(
            &d,
            "POST",
            &format!("/api/guardians/{id}/run-action-hint"),
            body,
        );
        assert_eq!(r.status, 400);
        assert!(r.body.contains("bad_request"));
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
        let r = route(&d, "POST", "/api/squads/validate", &submit_body(""));
        assert_eq!(r.status, 200);
        assert!(r.body.contains("\"valid\":false"));
    }

    #[test]
    fn validate_reports_valid() {
        let d = daemon();
        let r = route(&d, "POST", "/api/squads/validate", &submit_body(GOOD));
        assert_eq!(r.status, 200);
        assert!(r.body.contains("\"valid\":true"));
    }

    #[test]
    fn submit_rejects_invalid() {
        let d = daemon();
        let r = route(&d, "POST", "/api/squads", &submit_body("nope = true"));
        assert_eq!(r.status, 400);
        assert!(r.body.contains("validation_failed"));
    }

    #[test]
    fn submit_rejects_label_with_comma() {
        let d = daemon();
        let body = serde_json::json!({ "toml": GOOD, "label": "a,b" }).to_string();
        let r = route(&d, "POST", "/api/squads", &body);
        assert_eq!(r.status, 400);
        assert!(r.body.contains("invalid_label"));
        assert!(r.body.contains("a,b"));
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
    fn route_with_trace_persists_incoming_traceparent_onto_the_new_squad() {
        let d = daemon();
        let incoming = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
        let r = route_with_trace(
            &d,
            "POST",
            "/api/squads",
            &submit_body(GOOD),
            Some(incoming),
        );
        assert_eq!(r.status, 201);

        let stored = d
            .lock()
            .squad_trace_context("squad-000000000001")
            .unwrap()
            .expect("trace context recorded on submit");
        // Same trace id (the middle hex segment) as the incoming header — the
        // stored value is the daemon's own child span, not a bare copy.
        assert_eq!(
            stored.split('-').nth(1),
            incoming.split('-').nth(1),
            "squad must stay on the browser's trace"
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
        let r = route_with_trace(&d, "POST", "/api/squads", &submit_body(GOOD), None);
        assert_eq!(r.status, 201);
        let stored = d.lock().squad_trace_context("squad-000000000001").unwrap();
        assert!(stored.is_none());
    }

    #[test]
    fn route_with_trace_does_not_record_trace_context_for_non_submit_routes() {
        let d = daemon();
        let _ = route_with_trace(&d, "GET", "/api/daemon", "", Some("bogus"));
        // No squad exists at all, so looking one up is a NotFound error rather
        // than a panic — this just confirms the non-submit path is a no-op.
        assert!(d.lock().squad_trace_context("squad-000000000001").is_err());
    }

    // ── RAL-219: bearer-token auth ──────────────────────────────────────────

    #[test]
    fn a_daemon_with_no_token_configured_authorizes_everything() {
        let d = daemon();
        assert!(d.authorized(None));
        assert!(d.authorized(Some("Bearer whatever")));
        assert!(d.authorized(Some("garbage")));
    }

    #[test]
    fn a_daemon_with_a_token_rejects_a_missing_header() {
        let d = daemon().with_token("right-token".to_string());
        assert!(!d.authorized(None));
    }

    #[test]
    fn a_daemon_with_a_token_rejects_a_malformed_header() {
        let d = daemon().with_token("right-token".to_string());
        // Missing the "Bearer " scheme prefix.
        assert!(!d.authorized(Some("right-token")));
    }

    #[test]
    fn a_daemon_with_a_token_rejects_the_wrong_token() {
        let d = daemon().with_token("right-token".to_string());
        assert!(!d.authorized(Some("Bearer wrong-token")));
    }

    #[test]
    fn a_daemon_with_a_token_accepts_the_right_token() {
        let d = daemon().with_token("right-token".to_string());
        assert!(d.authorized(Some("Bearer right-token")));
    }

    #[test]
    fn submit_then_list_and_get() {
        let d = daemon();
        let r = route(&d, "POST", "/api/squads", &submit_body(GOOD));
        assert_eq!(r.status, 201);
        assert!(r.body.contains("squad-000000000001"));
        assert!(r.body.contains("\"state\":\"pending\""));

        let board = route(&d, "GET", "/api/tasks", "");
        assert_eq!(board.status, 200);
        assert!(board.body.contains("squad-000000000001"));
        assert!(board.body.contains("\"max_concurrent\":12"));

        let got = route(&d, "GET", "/api/squads/squad-000000000001", "");
        assert_eq!(got.status, 200);
        assert!(got.body.contains("\"name\":\"t\""));
    }

    #[test]
    fn board_reports_zero_max_concurrent_as_no_limit() {
        let d = Daemon::new(Store::open_in_memory().unwrap(), 0);
        let board = route(&d, "GET", "/api/tasks", "");
        assert_eq!(board.status, 200);
        assert!(board.body.contains("\"max_concurrent\":0"));
        assert!(board.body.contains("\"running\":0"));
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
        let squad = |args: &[&str]| {
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
        squad(&["init", "--initial-branch", "main"]);
        std::fs::write(dir.join("base.txt"), "base\n").unwrap();
        squad(&["add", "."]);
        squad(&["commit", "--message", "base"]);
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
    fn project_fork_crud_round_trip_including_default_user_row() {
        let d = daemon();

        // Create the project-wide default row (empty "user").
        let created = route(
            &d,
            "POST",
            "/api/projects/proj/forks",
            &serde_json::json!({"fork_url": "git@x:default/proj.git"}).to_string(),
        );
        assert_eq!(created.status, 201, "{}", created.body);
        assert!(created.body.contains("git@x:default/proj.git"));

        // Create a user-specific row.
        let created_user = route(
            &d,
            "POST",
            "/api/projects/proj/forks",
            &serde_json::json!({
                "user": "alice",
                "fork_url": "git@x:alice/proj.git",
                "remote_name": "fork-alice",
                "fork_owner": "alice",
            })
            .to_string(),
        );
        assert_eq!(created_user.status, 201, "{}", created_user.body);

        // List for the project sees both rows.
        let listed = route(&d, "GET", "/api/projects/proj/forks", "");
        assert_eq!(listed.status, 200, "{}", listed.body);
        assert!(listed.body.contains("git@x:default/proj.git"));
        assert!(listed.body.contains("git@x:alice/proj.git"));

        // The unscoped list also sees both.
        let all = route(&d, "GET", "/api/project-forks", "");
        assert_eq!(all.status, 200, "{}", all.body);
        assert!(all.body.contains("git@x:alice/proj.git"));

        // Patch the default row (no trailing user segment).
        let patched_default = route(
            &d,
            "PATCH",
            "/api/projects/proj/forks",
            &serde_json::json!({"fork_url": "git@x:default2/proj.git"}).to_string(),
        );
        assert_eq!(patched_default.status, 200, "{}", patched_default.body);
        assert!(patched_default.body.contains("git@x:default2/proj.git"));

        // Patch alice's row.
        let patched_alice = route(
            &d,
            "PATCH",
            "/api/projects/proj/forks/alice",
            &serde_json::json!({"remote_name": "fork-alice-2"}).to_string(),
        );
        assert_eq!(patched_alice.status, 200, "{}", patched_alice.body);
        assert!(patched_alice.body.contains("fork-alice-2"));
        // fork_url is unchanged by the selective patch.
        assert!(patched_alice.body.contains("git@x:alice/proj.git"));

        // Delete alice's row; the default row survives.
        let deleted = route(&d, "DELETE", "/api/projects/proj/forks/alice", "");
        assert_eq!(deleted.status, 200, "{}", deleted.body);
        let after_delete = route(&d, "GET", "/api/projects/proj/forks", "");
        assert!(!after_delete.body.contains("alice"));
        assert!(after_delete.body.contains("git@x:default2/proj.git"));

        // Delete the default row too.
        let deleted_default = route(&d, "DELETE", "/api/projects/proj/forks", "");
        assert_eq!(deleted_default.status, 200, "{}", deleted_default.body);
        let empty = route(&d, "GET", "/api/projects/proj/forks", "");
        assert_eq!(empty.status, 200, "{}", empty.body);
        assert!(empty.body.contains("\"forks\":[]"));
    }

    #[test]
    fn patch_or_delete_a_missing_fork_row_is_not_found() {
        let d = daemon();
        let patch = route(
            &d,
            "PATCH",
            "/api/projects/proj/forks/nobody",
            &serde_json::json!({"fork_url": "x"}).to_string(),
        );
        assert_eq!(patch.status, 404, "{}", patch.body);
        let delete = route(&d, "DELETE", "/api/projects/proj/forks/nobody", "");
        assert_eq!(delete.status, 404, "{}", delete.body);
    }

    #[test]
    fn project_forks_health_route_reports_one_check_set_per_registered_row() {
        let d = daemon();
        let created = route(
            &d,
            "POST",
            "/api/projects/proj/forks",
            &serde_json::json!({"fork_url": "git@x:default/proj.git"}).to_string(),
        );
        assert_eq!(created.status, 201, "{}", created.body);

        let health = route(&d, "GET", "/api/health/project-forks", "");
        assert_eq!(health.status, 200, "{}", health.body);
        let body: serde_json::Value = serde_json::from_str(&health.body).unwrap();
        let checks = body["checks"].as_array().unwrap();
        assert!(!checks.is_empty(), "{}", health.body);
        assert!(
            checks
                .iter()
                .any(|c| c["project"] == "proj" && c["name"] == "fork-project"),
            "unregistered project should fail the fork-project check: {}",
            health.body
        );
    }

    #[test]
    fn project_forks_health_route_is_empty_with_no_registered_forks() {
        let d = daemon();
        let health = route(&d, "GET", "/api/health/project-forks", "");
        assert_eq!(health.status, 200, "{}", health.body);
        assert!(health.body.contains("\"checks\":[]"));
    }

    #[test]
    fn worktree_retirements_route_reports_states_and_durable_history() {
        let d = daemon();
        // A review whose worktree last moved 10 days before the 30-day
        // threshold — still scheduled, not yet eligible.
        let id = d.lock().create_guardian("young", "main", "/r").unwrap();
        d.lock().add_guardian_branch(&id, "b").unwrap();
        let branch_id = d.lock().get_guardian(&id).unwrap().branches[0].id.clone();
        let recent = crate::store::now_ms() - crate::guardian_merge::WORKTREE_RETIREMENT_AGE_MS
            + 10 * 24 * 60 * 60 * 1_000;
        d.lock()
            .set_branch_review(&id, &branch_id, "gb", "C:/repo/wt-young")
            .unwrap();
        d.lock()
            .conn
            .execute(
                "UPDATE guardians SET status=?1, updated_at_ms=?2 WHERE id=?3",
                rusqlite::params!["deployed", recent, id],
            )
            .unwrap();

        let r = route(&d, "GET", "/api/worktree-retirements", "");
        assert_eq!(r.status, 200, "{}", r.body);
        let body: serde_json::Value = serde_json::from_str(&r.body).unwrap();
        assert_eq!(body["age_threshold_days"], 30);
        let entries = body["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 1, "{}", r.body);
        assert_eq!(entries[0]["state"], "scheduled");
        assert_eq!(entries[0]["guardian_name"], "young");
        assert_eq!(entries[0]["path"], "C:/repo/wt-young");
        assert!(entries[0]["eligible_at_ms"].as_i64().unwrap() > recent);

        // A failed retirement attempt shows up durably with its error, even
        // though the live path row is also present.
        d.lock()
            .record_guardian_worktree_retirement(
                &id,
                "C:/repo/wt-young",
                "failed",
                Some("git refused"),
                crate::store::now_ms(),
                crate::store::now_ms(),
                None,
            )
            .unwrap();
        let r2 = route(&d, "GET", "/api/worktree-retirements", "");
        let body2: serde_json::Value = serde_json::from_str(&r2.body).unwrap();
        let entries2 = body2["entries"].as_array().unwrap();
        assert_eq!(entries2.len(), 1);
        assert_eq!(entries2[0]["state"], "failed");
        assert_eq!(entries2[0]["error"], "git refused");

        // Recording the retry's success (which clears the live path) turns
        // the row into retired history without losing the entry.
        d.lock()
            .clear_guardian_worktree_path("C:/repo/wt-young")
            .unwrap();
        d.lock()
            .record_guardian_worktree_retirement(
                &id,
                "C:/repo/wt-young",
                "retired",
                None,
                crate::store::now_ms(),
                crate::store::now_ms(),
                None,
            )
            .unwrap();
        let r3 = route(&d, "GET", "/api/worktree-retirements", "");
        let body3: serde_json::Value = serde_json::from_str(&r3.body).unwrap();
        let entries3 = body3["entries"].as_array().unwrap();
        assert_eq!(entries3.len(), 1, "{}", r3.body);
        assert_eq!(entries3[0]["state"], "retired");
        assert_eq!(entries3[0]["guardian_name"], "young");

        // And deleting the review takes the retired history with it — the
        // history lives exactly as long as the review does.
        d.lock().delete_guardian(&id).unwrap();
        let r4 = route(&d, "GET", "/api/worktree-retirements", "");
        assert!(r4.body.contains("\"entries\":[]"), "{}", r4.body);
    }

    #[test]
    fn register_project_route_accepts_and_returns_clone_url() {
        let d = daemon();
        let repo = tmp_git_repo("register-clone-url");
        let body = serde_json::json!({
            "name": "proj",
            "path": repo.to_string_lossy(),
            "vcs": "git",
            "url": "git@example.invalid:team/proj.git",
        })
        .to_string();
        let registered = route(&d, "POST", "/api/projects", &body);
        assert_eq!(registered.status, 201, "{}", registered.body);
        let fetched = route(&d, "GET", "/api/projects/proj", "");
        assert_eq!(fetched.status, 200, "{}", fetched.body);
        assert!(
            fetched.body.contains("git@example.invalid:team/proj.git"),
            "{}",
            fetched.body
        );
    }

    #[test]
    fn register_project_route_warns_but_accepts_a_clone_url_with_an_inline_password() {
        let d = daemon();
        let repo = tmp_git_repo("register-clone-url-inline-password");
        let body = serde_json::json!({
            "name": "proj",
            "path": repo.to_string_lossy(),
            "vcs": "git",
            "url": "https://user:hunter2@example.invalid/team/proj.git",
        })
        .to_string();
        let registered = route(&d, "POST", "/api/projects", &body);
        assert_eq!(registered.status, 201, "{}", registered.body);
        assert!(registered.body.contains("warnings"), "{}", registered.body);
        assert!(
            !registered.body.contains("hunter2"),
            "the warning must not itself leak the password: {}",
            registered.body
        );
        assert!(
            registered.body.contains("https://***@example.invalid"),
            "{}",
            registered.body
        );
        // The password must still round-trip for the owner who registered
        // it -- the warning is advisory, not a rejection or a scrub of the
        // stored value.
        let fetched = route(&d, "GET", "/api/projects/proj", "");
        assert!(fetched.body.contains("hunter2"), "{}", fetched.body);
    }

    #[test]
    fn register_project_route_has_no_warnings_for_a_plain_clone_url() {
        let d = daemon();
        let repo = tmp_git_repo("register-clone-url-plain");
        let body = serde_json::json!({
            "name": "proj",
            "path": repo.to_string_lossy(),
            "vcs": "git",
            "url": "git@example.invalid:team/proj.git",
        })
        .to_string();
        let registered = route(&d, "POST", "/api/projects", &body);
        assert_eq!(registered.status, 201, "{}", registered.body);
        assert!(!registered.body.contains("warnings"), "{}", registered.body);
    }

    #[test]
    fn register_project_route_rejects_an_empty_clone_url() {
        let d = daemon();
        let repo = tmp_git_repo("register-empty-clone-url");
        let body = serde_json::json!({
            "name": "proj",
            "path": repo.to_string_lossy(),
            "vcs": "git",
            "clone_url": "  ",
        })
        .to_string();
        let response = route(&d, "POST", "/api/projects", &body);
        assert_eq!(response.status, 400, "{}", response.body);
        assert!(response.body.contains("clone_url"), "{}", response.body);
    }

    #[test]
    fn register_project_route_clears_a_previously_registered_clone_url() {
        let d = daemon();
        let repo = tmp_git_repo("register-clear-clone-url");
        let with_url = serde_json::json!({
            "name": "proj",
            "path": repo.to_string_lossy(),
            "vcs": "git",
            "url": "git@example.invalid:team/proj.git",
        })
        .to_string();
        let registered = route(&d, "POST", "/api/projects", &with_url);
        assert_eq!(registered.status, 201, "{}", registered.body);
        let fetched = route(&d, "GET", "/api/projects/proj", "");
        assert!(
            fetched.body.contains("git@example.invalid:team/proj.git"),
            "{}",
            fetched.body
        );

        let clear = serde_json::json!({
            "name": "proj",
            "path": repo.to_string_lossy(),
            "vcs": "git",
            "clear_clone_url": true,
        })
        .to_string();
        let cleared = route(&d, "POST", "/api/projects", &clear);
        assert_eq!(cleared.status, 201, "{}", cleared.body);
        let fetched = route(&d, "GET", "/api/projects/proj", "");
        assert!(
            !fetched.body.contains("git@example.invalid:team/proj.git"),
            "clone URL should have been cleared: {}",
            fetched.body
        );
        // `ProjectView::clone_url` skips serialization when `None`.
        assert!(!fetched.body.contains("clone_url"), "{}", fetched.body);
    }

    #[test]
    fn register_project_route_rejects_clear_clone_url_combined_with_a_url() {
        let d = daemon();
        let repo = tmp_git_repo("register-clear-and-set-clone-url");
        let body = serde_json::json!({
            "name": "proj",
            "path": repo.to_string_lossy(),
            "vcs": "git",
            "url": "git@example.invalid:team/proj.git",
            "clear_clone_url": true,
        })
        .to_string();
        let response = route(&d, "POST", "/api/projects", &body);
        assert_eq!(response.status, 400, "{}", response.body);
        assert!(
            response.body.contains("clear_clone_url"),
            "{}",
            response.body
        );
    }

    #[test]
    fn register_project_route_rejects_a_clone_url_containing_control_characters() {
        let d = daemon();
        let repo = tmp_git_repo("register-control-char-clone-url");
        let body = serde_json::json!({
            "name": "proj",
            "path": repo.to_string_lossy(),
            "vcs": "git",
            "clone_url": "git@example.invalid:team/proj.git\r\nEvil: header",
        })
        .to_string();
        let response = route(&d, "POST", "/api/projects", &body);
        assert_eq!(response.status, 400, "{}", response.body);
        assert!(response.body.contains("clone_url"), "{}", response.body);
        assert!(
            response.body.contains("control characters"),
            "{}",
            response.body
        );
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
    fn agent_profiles_health_route_requires_cwd() {
        let d = daemon();
        let r = route(&d, "GET", "/api/health/agent-profiles", "");
        assert_eq!(r.status, 400, "{}", r.body);
    }

    #[test]
    fn agent_profiles_health_route_reports_unresolved_from_env() {
        let d = daemon();
        let dir = std::env::temp_dir().join(format!(
            "ral-agent-profiles-health-route-{}-{}",
            std::process::id(),
            PROJ_TEST_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(".ralphus.toml"),
            r#"
[agent.profiles.deepseek]
backend = "anthropic"

[agent.profiles.deepseek.env]
ANTHROPIC_AUTH_TOKEN = { from_env = "RALPHUS_AGENT_PROFILES_HEALTH_ROUTE_TEST_VAR_UNSET" }
"#,
        )
        .unwrap();
        let r = route(
            &d,
            "GET",
            &format!("/api/health/agent-profiles?cwd={}", dir.display()),
            "",
        );
        assert_eq!(r.status, 200, "{}", r.body);
        assert!(r.body.contains("\"status\":\"fail\""), "{}", r.body);
        assert!(
            r.body.contains("ralphus-daemon serve"),
            "expected an actionable message pointing at the daemon process: {}",
            r.body
        );
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
    fn project_branches_route_lists_local_branches_for_a_registered_git_project() {
        let d = daemon();
        let repo = tmp_git_repo("branches");
        let status = std::process::Command::new("git")
            .args(["branch", "feature"])
            .current_dir(&repo)
            .status()
            .expect("git");
        assert!(status.success());
        route(
            &d,
            "POST",
            "/api/projects",
            &register_body("proj", &repo.to_string_lossy(), ""),
        );
        let r = route(&d, "GET", "/api/projects/proj/branches", "");
        assert_eq!(r.status, 200, "{}", r.body);
        assert!(r.body.contains("\"main\""), "{}", r.body);
        assert!(r.body.contains("\"feature\""), "{}", r.body);
    }

    #[test]
    fn project_branches_route_is_empty_for_a_non_git_project() {
        let d = daemon();
        d.lock()
            .register_project("proj", "", "C:/wherever", "none")
            .unwrap();
        let r = route(&d, "GET", "/api/projects/proj/branches", "");
        assert_eq!(r.status, 200, "{}", r.body);
        assert!(r.body.contains("\"branches\":[]"), "{}", r.body);
    }

    #[test]
    fn project_branches_route_unregistered_name_is_404() {
        let d = daemon();
        let r = route(&d, "GET", "/api/projects/nope/branches", "");
        assert_eq!(r.status, 404);
        assert!(r.body.contains("not_found"));
    }

    #[test]
    fn submit_rejects_placeholder_cwd_with_unregistered_project() {
        let d = daemon();
        let toml = "[[task]]\nname=\"t\"\nproject=\"ghost\"\n[[task.cell]]\ncwd=\"<<ralphus:new-worktree/feat?upstream=main>>\"\nprompt=\"p\"\n";
        let r = route(&d, "POST", "/api/squads", &submit_body(toml));
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
    fn cleanup_requires_project_and_reports_a_registered_projects_missing_clone_url() {
        let d = daemon();
        route(
            &d,
            "POST",
            "/api/machines",
            &machine_body("ib", "/definitely/not/a/real/provider"),
        );
        // Missing "project" is a 400, not a panic or a confusing provider dispatch.
        let r = route(
            &d,
            "POST",
            "/api/machines/cleanup",
            r#"{"machine": "ib:A"}"#,
        );
        assert_eq!(r.status, 400, "{}", r.body);

        // An unregistered project name is a 404.
        let r = route(
            &d,
            "POST",
            "/api/machines/cleanup",
            r#"{"machine": "ib:A", "project": "nope"}"#,
        );
        assert_eq!(r.status, 404, "{}", r.body);

        // A registered project with no clone URL is a 400 -- there is nothing
        // that could have been provisioned remotely for it.
        let repo = tmp_git_repo("cleanup-no-url");
        route(
            &d,
            "POST",
            "/api/projects",
            &register_body("proj", &repo.to_string_lossy(), ""),
        );
        let r = route(
            &d,
            "POST",
            "/api/machines/cleanup",
            r#"{"machine": "ib:A", "project": "proj"}"#,
        );
        assert_eq!(r.status, 400, "{}", r.body);
        assert!(r.body.contains("no registered clone URL"), "{}", r.body);
    }

    #[test]
    fn health_all_targets_route_returns_an_empty_report_with_no_configured_targets() {
        // No [machine.targets.*] configured in this test's environment --
        // proves the route dispatches and shapes its JSON correctly without
        // needing a live remote machine. `machine_targets.rs`'s own tests
        // cover config parsing; `health_targets.rs`'s own tests cover the
        // per-target check logic -- this is only proving the wiring between
        // them and the HTTP layer.
        let d = daemon();
        let r = route(&d, "GET", "/api/machines/targets/health", "");
        assert_eq!(r.status, 200, "{}", r.body);
        assert!(r.body.contains("\"ok\":true"), "{}", r.body);
        assert!(r.body.contains("\"targets\":[]"), "{}", r.body);
    }

    #[test]
    fn secret_env_name_add_list_rename_and_delete_roundtrip() {
        let d = daemon();
        let r = route(
            &d,
            "POST",
            "/api/secret-env-names",
            r#"{"name":"MY_TOKEN"}"#,
        );
        assert_eq!(r.status, 201, "{}", r.body);

        let r = route(&d, "GET", "/api/secret-env-names", "");
        assert_eq!(r.status, 200, "{}", r.body);
        assert!(r.body.contains("MY_TOKEN"), "{}", r.body);
        // Seeded defaults should show up alongside the freshly added name.
        assert!(r.body.contains("ANTHROPIC_API_KEY"), "{}", r.body);

        let r = route(
            &d,
            "POST",
            "/api/secret-env-names/MY_TOKEN/rename",
            r#"{"name":"MY_RENAMED_TOKEN"}"#,
        );
        assert_eq!(r.status, 200, "{}", r.body);
        let r = route(&d, "GET", "/api/secret-env-names", "");
        assert!(r.body.contains("MY_RENAMED_TOKEN"), "{}", r.body);
        assert!(!r.body.contains("\"MY_TOKEN\""), "{}", r.body);

        let r = route(&d, "DELETE", "/api/secret-env-names/MY_RENAMED_TOKEN", "");
        assert_eq!(r.status, 200, "{}", r.body);
        let r = route(&d, "DELETE", "/api/secret-env-names/MY_RENAMED_TOKEN", "");
        assert_eq!(r.status, 404, "{}", r.body);
    }

    #[test]
    fn secret_env_name_add_rejects_a_duplicate() {
        let d = daemon();
        route(
            &d,
            "POST",
            "/api/secret-env-names",
            r#"{"name":"DUP_TOKEN"}"#,
        );
        let r = route(
            &d,
            "POST",
            "/api/secret-env-names",
            r#"{"name":"DUP_TOKEN"}"#,
        );
        assert_eq!(r.status, 409, "{}", r.body);
    }

    #[test]
    fn secret_env_name_add_rejects_an_invalid_identifier() {
        let d = daemon();
        let r = route(
            &d,
            "POST",
            "/api/secret-env-names",
            r#"{"name":"not a valid key"}"#,
        );
        assert_eq!(r.status, 400, "{}", r.body);
    }

    #[test]
    fn secret_env_name_rename_rejects_colliding_with_an_existing_name() {
        let d = daemon();
        route(&d, "POST", "/api/secret-env-names", r#"{"name":"A_NAME"}"#);
        route(&d, "POST", "/api/secret-env-names", r#"{"name":"B_NAME"}"#);
        let r = route(
            &d,
            "POST",
            "/api/secret-env-names/A_NAME/rename",
            r#"{"name":"B_NAME"}"#,
        );
        assert_eq!(r.status, 409, "{}", r.body);
    }

    #[test]
    fn secret_env_name_rename_of_unregistered_name_is_not_found() {
        let d = daemon();
        let r = route(
            &d,
            "POST",
            "/api/secret-env-names/NOPE/rename",
            r#"{"name":"ALSO_NOPE"}"#,
        );
        assert_eq!(r.status, 404, "{}", r.body);
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
        let toml = "[[task]]\nname=\"t\"\nmachine=\"ghostfarm:A\"\n[[task.cell]]\ncwd=\".\"\nprompt=\"p\"\n";
        let r = route(&d, "POST", "/api/squads", &submit_body(toml));
        assert_eq!(r.status, 400, "{}", r.body);
        assert!(r.body.contains("machine_validation_failed"), "{}", r.body);
        assert!(r.body.contains("ghostfarm"), "{}", r.body);
    }

    #[test]
    fn submit_accepts_a_registered_remote_machine_and_persists_it_on_the_cell() {
        // Phase 2: a registered machine now submits successfully, and the
        // resolved value is stored on the cell so the scheduler's router can
        // dispatch it without re-deriving inheritance.
        let d = daemon();
        route(
            &d,
            "POST",
            "/api/machines",
            &machine_body("incredibuild", "/opt/ib.sh"),
        );
        let toml = "[[task]]\nname=\"t\"\nmachine=\"incredibuild:A\"\n[[task.cell]]\ncwd=\".\"\nprompt=\"p\"\n";
        let r = route(&d, "POST", "/api/squads", &submit_body(toml));
        assert_eq!(r.status, 201, "{}", r.body);
        let squad_id = serde_json::from_str::<serde_json::Value>(&r.body).unwrap()["squad_id"]
            .as_str()
            .unwrap()
            .to_string();
        let cells = d.lock().cells_of(&squad_id).unwrap();
        assert_eq!(cells[0].machine.as_deref(), Some("incredibuild:A"));
    }

    #[test]
    fn a_cell_inherits_its_tasks_machine_when_it_declares_none() {
        let d = daemon();
        route(
            &d,
            "POST",
            "/api/machines",
            &machine_body("incredibuild", "/opt/ib.sh"),
        );
        // Both cells resolve to the task's machine: the first by inheriting
        // it, the second by restating it. They may not diverge — see
        // `two_cells_in_one_task_may_not_run_on_different_machines`.
        let toml = "[[task]]\nname=\"t\"\nmachine=\"incredibuild:A\"\n\
                    [[task.cell]]\ncwd=\".\"\nprompt=\"p\"\n\
                    [[task.cell]]\ncwd=\".\"\nprompt=\"q\"\nmachine=\"incredibuild:A\"\n";
        let r = route(&d, "POST", "/api/squads", &submit_body(toml));
        assert_eq!(r.status, 201, "{}", r.body);
        let squad_id = serde_json::from_str::<serde_json::Value>(&r.body).unwrap()["squad_id"]
            .as_str()
            .unwrap()
            .to_string();
        let cells = d.lock().cells_of(&squad_id).unwrap();
        assert_eq!(cells[0].machine.as_deref(), Some("incredibuild:A"));
        assert_eq!(cells[1].machine.as_deref(), Some("incredibuild:A"));
    }

    #[test]
    fn submit_accepts_a_remote_cell_with_a_worktree_placeholder() {
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
                    [[task.cell]]\nid=\"work\"\ncwd=\"<<ralphus:new-worktree/feat?upstream=main>>\"\nprompt=\"p\"\n";
        let r = route(&d, "POST", "/api/squads", &submit_body(toml));
        assert_eq!(r.status, 201, "{}", r.body);
    }

    #[test]
    fn a_local_cell_may_still_use_a_worktree_placeholder() {
        // Regression guard: the check above must not affect ordinary local squads.
        let d = daemon();
        let repo = tmp_git_repo("local-placeholder-ok");
        route(
            &d,
            "POST",
            "/api/projects",
            &register_body("proj", &repo.to_string_lossy(), ""),
        );
        let toml = "[[task]]\nname=\"t\"\nproject=\"proj\"\nmachine=\"local\"\n\
                    [[task.cell]]\ncwd=\"<<ralphus:new-worktree/feat?upstream=main>>\"\nprompt=\"p\"\n";
        let r = route(&d, "POST", "/api/squads", &submit_body(toml));
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
        route(&d, "POST", "/api/squads", &submit_body(toml))
    }

    #[test]
    fn two_cells_in_one_task_may_not_run_on_different_machines() {
        // They share one workspace and hand off through files on disk, so
        // splitting them would leave the second reading a directory the first
        // never wrote to.
        let r = submit_with_machines(
            "[[task]]
name=\"t\"
             [[task.cell]]
id=\"a\"
cwd=\".\"
prompt=\"p\"
machine=\"incredibuild:A\"
             [[task.cell]]
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
        assert!(r.body.contains("cell \\\"b\\\""), "{}", r.body);
    }

    #[test]
    fn a_cell_may_not_diverge_from_its_own_tasks_machine() {
        let r = submit_with_machines(
            "[[task]]
name=\"t\"
machine=\"incredibuild:A\"
             [[task.cell]]
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
    fn a_proof_step_may_not_diverge_from_its_owning_cell() {
        let r = submit_with_machines(
            "[[task]]
name=\"t\"
machine=\"incredibuild:A\"
             [[task.cell]]
id=\"a\"
cwd=\".\"
prompt=\"p\"
             [[task.cell.proof]]
command=\"cargo test\"
machine=\"incredibuild:B\"
",
        );
        assert_eq!(r.status, 400, "{}", r.body);
        assert!(r.body.contains("proof step"), "{}", r.body);
    }

    #[test]
    fn a_task_scope_proof_may_not_diverge_from_its_task() {
        let r = submit_with_machines(
            "[[task]]
name=\"t\"
machine=\"incredibuild:A\"
             [[task.cell]]
cwd=\".\"
prompt=\"p\"
             [[task.proof]]
command=\"cargo fmt\"
machine=\"incredibuild:B\"
",
        );
        assert_eq!(r.status, 400, "{}", r.body);
        assert!(r.body.contains("task proof step"), "{}", r.body);
    }

    #[test]
    fn restating_the_same_machine_throughout_a_task_is_allowed() {
        // An explicit override that agrees with the task is redundant, not wrong.
        let r = submit_with_machines(
            "[[task]]
name=\"t\"
machine=\"incredibuild:A\"
             [[task.cell]]
cwd=\".\"
prompt=\"p\"
machine=\"incredibuild:A\"
             [[task.cell.proof]]
command=\"cargo test\"
machine=\"incredibuild:A\"
             [[task.proof]]
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
             [[task.cell]]
cwd=\".\"
prompt=\"p\"
             [[task]]
name=\"b\"
machine=\"incredibuild:B\"
             [[task.cell]]
cwd=\".\"
prompt=\"q\"
",
        );
        assert_eq!(r.status, 201, "{}", r.body);
    }

    #[test]
    fn a_task_whose_cells_are_all_unset_inherits_without_complaint() {
        // Regression guard: the rule must be inert for every pre-RAL-185 file.
        let r = submit_with_machines(
            "[[task]]
name=\"t\"
             [[task.cell]]
cwd=\".\"
prompt=\"p\"
             [[task.cell]]
cwd=\".\"
prompt=\"q\"
",
        );
        assert_eq!(r.status, 201, "{}", r.body);
    }

    #[test]
    fn one_cell_naming_a_machine_sets_the_expectation_for_its_siblings() {
        // The task itself is unset, so the first cell that names a machine
        // establishes it -- a sibling that disagrees is still a split task.
        let r = submit_with_machines(
            "[[task]]
name=\"t\"
             [[task.cell]]
id=\"a\"
cwd=\".\"
prompt=\"p\"
machine=\"incredibuild:A\"
             [[task.cell]]
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
    fn a_remote_cell_feeding_a_review_must_declare_the_reviews_upstream() {
        // A remote cell's worktree lives on another machine, so its upstream
        // cannot be read from that worktree's git upstream. Declaring it is the
        // only honest option -- guessing the project default would silently
        // review against the wrong upstream.
        let d = daemon();
        let repo = tmp_git_repo("remote-review-upstream");
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
                    [[task.cell]]\nid=\"work\"\ncwd=\"<<ralphus:new-worktree/feat?upstream=main>>\"\nprompt=\"p\"\nreview=\"<<review:r>>\"\n\
                    [[review]]\nid=\"r\"\n";
        let r = route(&d, "POST", "/api/squads", &submit_body(toml));
        assert_eq!(r.status, 400, "{}", r.body);
        assert!(
            r.body.contains("upstream = ") || r.body.contains("upstream branch"),
            "the error must point at the missing [[review]] upstream: {}",
            r.body
        );
    }

    #[test]
    fn a_remote_cell_feeding_a_review_needs_a_placeholder_cwd() {
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
                    [[task.cell]]\nid=\"work\"\ncwd=\"/remote/wt\"\nprompt=\"p\"\nreview=\"<<review:r>>\"\n\
                    [[review]]\nid=\"r\"\nupstream=\"main\"\n";
        let r = route(&d, "POST", "/api/squads", &submit_body(toml));
        assert_eq!(r.status, 400, "{}", r.body);
        assert!(r.body.contains("new-worktree"), "{}", r.body);
    }

    #[test]
    fn a_cell_without_any_machine_stores_none_not_a_sentinel_string() {
        // The router keys off `None` to take the local path, so a sentinel
        // like "local" persisted here would send every legacy cell through
        // machine resolution for no reason.
        let d = daemon();
        let toml = "[[task]]\nname=\"t\"\n[[task.cell]]\ncwd=\".\"\nprompt=\"p\"\n";
        let r = route(&d, "POST", "/api/squads", &submit_body(toml));
        assert_eq!(r.status, 201, "{}", r.body);
        let squad_id = serde_json::from_str::<serde_json::Value>(&r.body).unwrap()["squad_id"]
            .as_str()
            .unwrap()
            .to_string();
        let cells = d.lock().cells_of(&squad_id).unwrap();
        assert_eq!(cells[0].machine, None);
    }

    #[test]
    fn submit_accepts_an_explicitly_local_machine() {
        let d = daemon();
        let toml =
            "[[task]]\nname=\"t\"\nmachine=\"local\"\n[[task.cell]]\ncwd=\".\"\nprompt=\"p\"\n";
        let r = route(&d, "POST", "/api/squads", &submit_body(toml));
        assert_eq!(r.status, 201, "{}", r.body);
    }

    #[test]
    fn submit_is_unaffected_when_no_machine_is_declared_anywhere() {
        // Regression guard: every pre-RAL-185 task file must submit unchanged.
        let d = daemon();
        let toml = "[[task]]\nname=\"t\"\n[[task.cell]]\ncwd=\".\"\nprompt=\"p\"\n";
        let r = route(&d, "POST", "/api/squads", &submit_body(toml));
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
        let toml = "[[task]]\nname=\"t\"\nproject=\"proj\"\n[[task.cell]]\ncwd=\"<<ralphus:new-worktree/feat?upstream=main>>\"\nprompt=\"p\"\n";
        let r = route(&d, "POST", "/api/squads", &submit_body(toml));
        assert_eq!(r.status, 201, "{}", r.body);
    }

    #[test]
    fn get_missing_squad_is_404() {
        let d = daemon();
        let r = route(&d, "GET", "/api/squads/squad-999", "");
        assert_eq!(r.status, 404);
        assert!(r.body.contains("not_found"));
    }

    #[test]
    fn hold_then_activate() {
        let d = daemon();
        let body =
            serde_json::to_string(&serde_json::json!({ "toml": GOOD, "hold": true })).unwrap();
        let r = route(&d, "POST", "/api/squads", &body);
        assert!(r.body.contains("\"state\":\"queued\""));

        let act = route(&d, "POST", "/api/squads/squad-000000000001/activate", "");
        assert_eq!(act.status, 200);
        assert!(act.body.contains("\"state\":\"pending\""));
    }

    #[test]
    fn activate_pending_conflicts() {
        let d = daemon();
        route(&d, "POST", "/api/squads", &submit_body(GOOD));
        let act = route(&d, "POST", "/api/squads/squad-000000000001/activate", "");
        assert_eq!(act.status, 409);
    }

    #[test]
    fn edit_cell_resets_squad_to_pending() {
        let d = daemon();
        route(&d, "POST", "/api/squads", &submit_body(GOOD));
        // mark it done, then edit -> should go back to pending
        d.lock()
            .set_squad_state("squad-000000000001", SquadState::Done)
            .unwrap();
        let body = serde_json::json!({
            "kind": "cell", "task_idx": 0, "cell_idx": 0,
            "cwd": "/new", "agent": "ollama", "model": "qwen3:8b", "prompt": "changed"
        })
        .to_string();
        let r = route(&d, "POST", "/api/squads/squad-000000000001/edit", &body);
        assert_eq!(r.status, 200);
        assert!(r.body.contains("\"state\":\"pending\""));
        assert!(r.body.contains("\"cwd\":\"/new\""));
        assert!(r.body.contains("\"agent\":\"ollama\""));
    }

    #[test]
    fn edit_cell_partial_edit_preserves_unspecified_fields() {
        // Regression (RAL-239 incident): editing a cell with only *some* of
        // the edit fields must leave every field the caller didn't mention
        // exactly as it was, not silently null it out.
        let d = daemon();
        route(&d, "POST", "/api/squads", &submit_body(GOOD));
        // Establish known, non-default values for every field first.
        let body = serde_json::json!({
            "kind": "cell", "task_idx": 0, "cell_idx": 0,
            "cwd": "/original", "agent": "ollama", "model": "qwen3:8b", "prompt": "original prompt"
        })
        .to_string();
        let r = route(&d, "POST", "/api/squads/squad-000000000001/edit", &body);
        assert_eq!(r.status, 200, "{}", r.body);

        // Editing only `agent` must leave cwd/model/prompt untouched.
        let body = serde_json::json!({
            "kind": "cell", "task_idx": 0, "cell_idx": 0, "agent": "claude-code"
        })
        .to_string();
        let r = route(&d, "POST", "/api/squads/squad-000000000001/edit", &body);
        assert_eq!(r.status, 200, "{}", r.body);
        assert!(r.body.contains("\"cwd\":\"/original\""), "{}", r.body);
        assert!(r.body.contains("\"agent\":\"claude-code\""), "{}", r.body);
        assert!(r.body.contains("\"model\":\"qwen3:8b\""), "{}", r.body);
        assert!(
            r.body.contains("\"prompt\":\"original prompt\""),
            "{}",
            r.body
        );
    }

    #[test]
    fn edit_cell_omitted_agent_preserves_existing_agent() {
        // Companion to the above: omitting `--agent` used to silently
        // default the column to the literal string "claude" instead of
        // leaving whatever agent the cell already had.
        let d = daemon();
        route(&d, "POST", "/api/squads", &submit_body(GOOD));
        let body = serde_json::json!({
            "kind": "cell", "task_idx": 0, "cell_idx": 0, "agent": "ollama"
        })
        .to_string();
        let r = route(&d, "POST", "/api/squads/squad-000000000001/edit", &body);
        assert_eq!(r.status, 200, "{}", r.body);
        assert!(r.body.contains("\"agent\":\"ollama\""), "{}", r.body);

        let body = serde_json::json!({
            "kind": "cell", "task_idx": 0, "cell_idx": 0, "cwd": "/new"
        })
        .to_string();
        let r = route(&d, "POST", "/api/squads/squad-000000000001/edit", &body);
        assert_eq!(r.status, 200, "{}", r.body);
        assert!(r.body.contains("\"cwd\":\"/new\""), "{}", r.body);
        assert!(
            r.body.contains("\"agent\":\"ollama\""),
            "agent must stay 'ollama', not silently default to 'claude': {}",
            r.body
        );
    }

    #[test]
    fn edit_cell_command_and_prompt_stay_xor_only_when_explicitly_supplied() {
        let d = daemon();
        route(&d, "POST", "/api/squads", &submit_body(GOOD)); // starts with prompt="p"

        // Supplying `command` alone clears the existing `prompt`.
        let body = serde_json::json!({
            "kind": "cell", "task_idx": 0, "cell_idx": 0, "command": "echo hi"
        })
        .to_string();
        let r = route(&d, "POST", "/api/squads/squad-000000000001/edit", &body);
        assert_eq!(r.status, 200, "{}", r.body);
        assert!(r.body.contains("\"command\":\"echo hi\""), "{}", r.body);
        assert!(r.body.contains("\"prompt\":null"), "{}", r.body);

        // Supplying `prompt` alone clears the command back out.
        let body = serde_json::json!({
            "kind": "cell", "task_idx": 0, "cell_idx": 0, "prompt": "back to prompt"
        })
        .to_string();
        let r = route(&d, "POST", "/api/squads/squad-000000000001/edit", &body);
        assert_eq!(r.status, 200, "{}", r.body);
        assert!(
            r.body.contains("\"prompt\":\"back to prompt\""),
            "{}",
            r.body
        );
        assert!(r.body.contains("\"command\":null"), "{}", r.body);
    }

    #[test]
    fn edit_cell_system_prompt_rejected_for_unsupported_agent() {
        // GOOD's cell has no explicit `agent`, so it resolves to the default
        // "claude" -- which, unlike "claude-code", has no
        // append-system-prompt delivery mechanism (RAL-341).
        let d = daemon();
        route(&d, "POST", "/api/squads", &submit_body(GOOD));
        let body = serde_json::json!({
            "kind": "cell", "task_idx": 0, "cell_idx": 0, "system_prompt": "Do NOT push."
        })
        .to_string();
        let r = route(&d, "POST", "/api/squads/squad-000000000001/edit", &body);
        assert_eq!(r.status, 400, "{}", r.body);
        assert!(r.body.contains("system_prompt"), "{}", r.body);
    }

    #[test]
    fn edit_cell_system_prompt_accepted_with_supporting_agent_in_same_call() {
        let d = daemon();
        route(&d, "POST", "/api/squads", &submit_body(GOOD));
        let body = serde_json::json!({
            "kind": "cell", "task_idx": 0, "cell_idx": 0,
            "agent": "claude-code", "system_prompt": "Do NOT commit and do NOT push."
        })
        .to_string();
        let r = route(&d, "POST", "/api/squads/squad-000000000001/edit", &body);
        assert_eq!(r.status, 200, "{}", r.body);
        assert!(
            r.body.contains("Do NOT commit and do NOT push."),
            "{}",
            r.body
        );
    }

    #[test]
    fn edit_cell_system_prompt_accepted_for_existing_supporting_agent() {
        let d = daemon();
        let toml = "[[task]]\nname=\"t\"\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\nagent=\"claude-code\"\n";
        route(&d, "POST", "/api/squads", &submit_body(toml));
        let body = serde_json::json!({
            "kind": "cell", "task_idx": 0, "cell_idx": 0, "system_prompt": "Custom rail."
        })
        .to_string();
        let r = route(&d, "POST", "/api/squads/squad-000000000001/edit", &body);
        assert_eq!(r.status, 200, "{}", r.body);
        assert!(r.body.contains("Custom rail."), "{}", r.body);
    }

    #[test]
    fn edit_cell_system_prompt_clear_does_not_require_agent_support() {
        // Clearing the field back out is always allowed -- only *setting* a
        // real value requires the agent to support delivering it.
        let d = daemon();
        route(&d, "POST", "/api/squads", &submit_body(GOOD));
        let body = serde_json::json!({
            "kind": "cell", "task_idx": 0, "cell_idx": 0, "system_prompt": ""
        })
        .to_string();
        let r = route(&d, "POST", "/api/squads/squad-000000000001/edit", &body);
        assert_eq!(r.status, 200, "{}", r.body);
    }

    #[test]
    fn edit_cell_maximum_tool_output_tokens_set_and_clear() {
        let d = daemon();
        let toml = "[[task]]
name=\"t\"
[[task.cell]]
cwd=\"/r\"
prompt=\"p\"
agent=\"claude-code\"
";
        route(&d, "POST", "/api/squads", &submit_body(toml));
        let set = serde_json::json!({
            "kind": "cell", "task_idx": 0, "cell_idx": 0, "maximum_tool_output_tokens": "25000"
        })
        .to_string();
        let r = route(&d, "POST", "/api/squads/squad-000000000001/edit", &set);
        assert_eq!(r.status, 200, "{}", r.body);
        assert!(
            r.body.contains("\"maximum_tool_output_tokens\":25000"),
            "{}",
            r.body
        );

        // Present-but-empty clears it back to NULL, which `CellView` omits
        // from the JSON entirely (`skip_serializing_if = "Option::is_none"`)
        // rather than rendering as `null`.
        let clear = serde_json::json!({
            "kind": "cell", "task_idx": 0, "cell_idx": 0, "maximum_tool_output_tokens": ""
        })
        .to_string();
        let r = route(&d, "POST", "/api/squads/squad-000000000001/edit", &clear);
        assert_eq!(r.status, 200, "{}", r.body);
        assert!(!r.body.contains("maximum_tool_output_tokens"), "{}", r.body);
    }

    #[test]
    fn edit_cell_omitted_maximum_tool_output_tokens_is_untouched() {
        let d = daemon();
        let toml = "[[task]]
name=\"t\"
[[task.cell]]
cwd=\"/r\"
prompt=\"p\"
agent=\"claude-code\"
maximum_tool_output_tokens=8000
";
        route(&d, "POST", "/api/squads", &submit_body(toml));
        let body = serde_json::json!({
            "kind": "cell", "task_idx": 0, "cell_idx": 0, "model": "sonnet"
        })
        .to_string();
        let r = route(&d, "POST", "/api/squads/squad-000000000001/edit", &body);
        assert_eq!(r.status, 200, "{}", r.body);
        assert!(
            r.body.contains("\"maximum_tool_output_tokens\":8000"),
            "{}",
            r.body
        );
    }

    #[test]
    fn edit_cell_maximum_tool_output_tokens_rejected_for_unsupported_agent() {
        // GOOD's cell resolves to the default "claude", which has no
        // tool-output-cap delivery mechanism (RAL-333).
        let d = daemon();
        route(&d, "POST", "/api/squads", &submit_body(GOOD));
        let body = serde_json::json!({
            "kind": "cell", "task_idx": 0, "cell_idx": 0, "maximum_tool_output_tokens": "25000"
        })
        .to_string();
        let r = route(&d, "POST", "/api/squads/squad-000000000001/edit", &body);
        assert_eq!(r.status, 400, "{}", r.body);
        assert!(r.body.contains("maximum_tool_output_tokens"), "{}", r.body);
    }

    #[test]
    fn edit_cell_maximum_tool_output_tokens_accepted_with_supporting_agent_in_same_call() {
        let d = daemon();
        route(&d, "POST", "/api/squads", &submit_body(GOOD));
        let body = serde_json::json!({
            "kind": "cell", "task_idx": 0, "cell_idx": 0,
            "agent": "claude-code", "maximum_tool_output_tokens": "25000"
        })
        .to_string();
        let r = route(&d, "POST", "/api/squads/squad-000000000001/edit", &body);
        assert_eq!(r.status, 200, "{}", r.body);
    }

    #[test]
    fn edit_cell_maximum_tool_output_tokens_clear_does_not_require_agent_support() {
        let d = daemon();
        route(&d, "POST", "/api/squads", &submit_body(GOOD));
        let body = serde_json::json!({
            "kind": "cell", "task_idx": 0, "cell_idx": 0, "maximum_tool_output_tokens": ""
        })
        .to_string();
        let r = route(&d, "POST", "/api/squads/squad-000000000001/edit", &body);
        assert_eq!(r.status, 200, "{}", r.body);
    }

    #[test]
    fn edit_cell_rejects_non_positive_and_non_numeric_integer_fields() {
        let d = daemon();
        let toml = "[[task]]
name=\"t\"
[[task.cell]]
cwd=\"/r\"
prompt=\"p\"
agent=\"claude-code\"
";
        route(&d, "POST", "/api/squads", &submit_body(toml));
        for (field, value) in [
            ("maximum_tool_output_tokens", "0"),
            ("maximum_tool_output_tokens", "-5"),
            ("maximum_tool_output_tokens", "lots"),
            ("auto_compact_threshold", "0"),
            ("auto_compact_threshold", "-5"),
            ("auto_compact_threshold", "lots"),
        ] {
            let body = serde_json::json!({
                "kind": "cell", "task_idx": 0, "cell_idx": 0, field: value
            })
            .to_string();
            let r = route(&d, "POST", "/api/squads/squad-000000000001/edit", &body);
            assert_eq!(r.status, 400, "{field}={value}: {}", r.body);
            assert!(r.body.contains(field), "{field}={value}: {}", r.body);
        }
    }

    #[test]
    fn edit_cell_system_prompt_restarts_cell_to_pending() {
        let d = daemon();
        let toml = "[[task]]\nname=\"t\"\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\nagent=\"claude-code\"\n";
        route(&d, "POST", "/api/squads", &submit_body(toml));
        {
            let store = d.lock();
            store
                .set_cell_state("squad-000000000001", 0, 0, NodeState::Done)
                .unwrap();
            store
                .set_squad_state("squad-000000000001", SquadState::Done)
                .unwrap();
        }
        let body = serde_json::json!({
            "kind": "cell", "task_idx": 0, "cell_idx": 0, "system_prompt": "New rail."
        })
        .to_string();
        let r = route(&d, "POST", "/api/squads/squad-000000000001/edit", &body);
        assert_eq!(r.status, 200, "{}", r.body);
        assert!(r.body.contains("\"state\":\"pending\""), "{}", r.body);
    }

    #[test]
    fn edit_cell_prompt_only_edit_keeps_previously_set_system_prompt() {
        // A `--system-prompt` edit sets both `system_prompt` and the
        // recomputed `effective_system_prompt` directly; a later
        // `--prompt`-only edit must keep recomputing from that stored
        // `system_prompt` rather than clearing it (RAL-341).
        let d = daemon();
        let toml = "[[task]]\nname=\"t\"\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\nagent=\"claude-code\"\n";
        route(&d, "POST", "/api/squads", &submit_body(toml));
        let body = serde_json::json!({
            "kind": "cell", "task_idx": 0, "cell_idx": 0, "system_prompt": "Stays put."
        })
        .to_string();
        let r = route(&d, "POST", "/api/squads/squad-000000000001/edit", &body);
        assert_eq!(r.status, 200, "{}", r.body);
        assert!(r.body.contains("Stays put."), "{}", r.body);

        let body = serde_json::json!({
            "kind": "cell", "task_idx": 0, "cell_idx": 0, "prompt": "new prompt text"
        })
        .to_string();
        let r = route(&d, "POST", "/api/squads/squad-000000000001/edit", &body);
        assert_eq!(r.status, 200, "{}", r.body);
        assert!(
            r.body.contains("Stays put."),
            "system_prompt must survive a prompt-only edit: {}",
            r.body
        );
        assert!(
            r.body.contains("\"prompt\":\"new prompt text\""),
            "{}",
            r.body
        );
    }

    #[test]
    fn edit_cell_dirties_only_the_target_and_its_downstream_not_upstream() {
        // Regression: editing one cell used to call `reset_squad_to_pending`
        // unconditionally, resetting *every* task/cell in the squad -- so
        // editing `mid`'s prompt below used to also flip the already-`done`,
        // unrelated `up` task back to running. up -> mid -> down (task-level
        // `depends_on`); editing mid's cell must dirty mid and down (its
        // downstream) but leave up's `done` state untouched.
        let d = daemon();
        let toml = "[[task]]\nname=\"up\"\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\n\
            [[task]]\nname=\"mid\"\ndepends_on=[\"up\"]\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\n\
            [[task]]\nname=\"down\"\ndepends_on=[\"mid\"]\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\n";
        route(&d, "POST", "/api/squads", &submit_body(toml));
        {
            let store = d.lock();
            for task_idx in 0..3 {
                store
                    .set_cell_state("squad-000000000001", task_idx, 0, NodeState::Done)
                    .unwrap();
                store
                    .set_task_state("squad-000000000001", task_idx, NodeState::Done)
                    .unwrap();
            }
            store
                .set_squad_state("squad-000000000001", SquadState::Done)
                .unwrap();
        }

        let body = serde_json::json!({
            "kind": "cell", "task_idx": 1, "cell_idx": 0, "prompt": "changed"
        })
        .to_string();
        let r = route(&d, "POST", "/api/squads/squad-000000000001/edit", &body);
        assert_eq!(r.status, 200, "{}", r.body);

        let squad = d.lock().get_squad("squad-000000000001").unwrap();
        assert_eq!(squad.tasks[0].name, "up");
        assert_eq!(squad.tasks[0].state, "done", "upstream task must stay done");
        assert_eq!(squad.tasks[1].name, "mid");
        assert_eq!(squad.tasks[1].state, "pending", "edited task itself resets");
        assert_eq!(squad.tasks[2].name, "down");
        assert_eq!(
            squad.tasks[2].state, "pending",
            "downstream of the edited task must still reset"
        );
    }

    #[test]
    fn resume_agent_command_always_skips_permissions() {
        // "Open Agent" is only reachable for a cell that actually ran
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
        let a = readonly_viewer_copy_path(dir, "my-cell");
        let b = readonly_viewer_copy_path(dir, "my-cell");
        assert_ne!(a, b, "each click's copy must get its own path");
        assert!(a.starts_with(dir));
        assert!(a.to_string_lossy().contains("my-cell"));
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
        let r = open_readonly_snapshot_terminal("definitely-not-a-real-ralphus-cell-xyz");
        assert_eq!(r.status, 409);
        assert!(r.body.contains("no_tmux_session"));
    }

    #[test]
    fn resume_codex_agent_command_always_bypasses_approvals() {
        let cmd = resume_codex_agent_command("codex", "thread-abc-123");
        assert!(cmd.contains("--dangerously-bypass-approvals-and-sandbox"));
        assert!(cmd.contains("resume 'thread-abc-123'"));
        assert!(!cmd.contains("exec"));
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
    fn resume_pi_agent_command_uses_pi_flags() {
        let cmd = resume_pi_agent_command("pi", "sess-123");
        assert!(cmd.contains("--session 'sess-123'"));
        assert!(cmd.contains("--approve"));
    }

    #[test]
    fn edit_squad_label_does_not_reset_squad_state() {
        // Regression: renaming a squad (kind "squad", label only) used to also
        // call `reset_squad_to_pending` unconditionally, so renaming a
        // finished (or in-progress) squad silently kicked its tasks back to
        // Pending and the scheduler re-ran them -- even though the label is
        // purely cosmetic and unrelated to execution state or content.
        let d = daemon();
        route(&d, "POST", "/api/squads", &submit_body(GOOD));
        d.lock()
            .set_squad_state("squad-000000000001", SquadState::Done)
            .unwrap();
        let body = serde_json::json!({"kind": "squad", "label": "renamed"}).to_string();
        let r = route(&d, "POST", "/api/squads/squad-000000000001/edit", &body);
        assert_eq!(r.status, 200);
        assert!(r.body.contains("\"label\":\"renamed\""));
        assert!(r.body.contains("\"state\":\"done\""));
    }

    #[test]
    fn edit_squad_rejects_label_with_comma() {
        let d = daemon();
        route(&d, "POST", "/api/squads", &submit_body(GOOD));
        let body = serde_json::json!({"kind": "squad", "label": "a,b"}).to_string();
        let r = route(&d, "POST", "/api/squads/squad-000000000001/edit", &body);
        assert_eq!(r.status, 400);
        assert!(r.body.contains("invalid_label"));
        assert!(r.body.contains("a,b"));

        let got = route(&d, "GET", "/api/squads/squad-000000000001", "");
        assert!(!got.body.contains("\"label\":\"a,b\""));
    }

    #[test]
    fn edit_task_project() {
        let d = daemon();
        route(&d, "POST", "/api/squads", &submit_body(GOOD));
        let body = serde_json::json!({"kind":"task","task_idx":0,"name":"t","project":"myproj"})
            .to_string();
        let r = route(&d, "POST", "/api/squads/squad-000000000001/edit", &body);
        assert_eq!(r.status, 200);
        assert!(r.body.contains("\"project\":\"myproj\""));
    }

    #[test]
    fn edit_task_model() {
        let d = daemon();
        route(&d, "POST", "/api/squads", &submit_body(GOOD));
        let body =
            serde_json::json!({"kind":"task","task_idx":0,"name":"t","model":"gpt-5"}).to_string();
        let r = route(&d, "POST", "/api/squads/squad-000000000001/edit", &body);
        assert_eq!(r.status, 200, "{}", r.body);
        assert!(r.body.contains("\"model\":\"gpt-5\""), "{}", r.body);
    }

    #[test]
    fn edit_task_partial_edit_preserves_unspecified_fields() {
        // Regression companion to
        // `edit_cell_partial_edit_preserves_unspecified_fields`: editing a
        // task with only *some* of the edit fields must leave every field
        // the caller didn't mention exactly as it was, not silently null it
        // out or reset it to a placeholder (this used to always overwrite
        // `name` to the literal "task" and `project` to NULL whenever the
        // caller omitted them).
        let d = daemon();
        route(&d, "POST", "/api/squads", &submit_body(GOOD));
        let body = serde_json::json!({
            "kind": "task", "task_idx": 0, "name": "t", "project": "myproj", "model": "gpt-5"
        })
        .to_string();
        let r = route(&d, "POST", "/api/squads/squad-000000000001/edit", &body);
        assert_eq!(r.status, 200, "{}", r.body);

        // Editing only `name` must leave project/model untouched.
        let body =
            serde_json::json!({"kind": "task", "task_idx": 0, "name": "renamed"}).to_string();
        let r = route(&d, "POST", "/api/squads/squad-000000000001/edit", &body);
        assert_eq!(r.status, 200, "{}", r.body);
        assert!(r.body.contains("\"name\":\"renamed\""), "{}", r.body);
        assert!(r.body.contains("\"project\":\"myproj\""), "{}", r.body);
        assert!(r.body.contains("\"model\":\"gpt-5\""), "{}", r.body);
    }

    #[test]
    fn edit_cell_proof_model_resets_only_that_step_onward() {
        const ONE_CELL: &str = "[[task]]\nname=\"t\"\n[[task.cell]]\ncwd=\".\"\ncommand=\"x\"\n\
            [[task.cell.proof]]\ncommand=\"check\"\n";
        let d = daemon();
        route(&d, "POST", "/api/squads", &submit_body(ONE_CELL));
        d.lock()
            .set_squad_state("squad-000000000001", SquadState::Done)
            .unwrap();
        let body = serde_json::json!({
            "kind": "proof", "task_idx": 0, "proof_scope": "cell", "cell_idx": 0,
            "proof_idx": 0, "model": "gpt-5",
        })
        .to_string();
        let r = route(&d, "POST", "/api/squads/squad-000000000001/edit", &body);
        assert_eq!(r.status, 200, "{}", r.body);
        assert!(r.body.contains("\"model\":\"gpt-5\""), "{}", r.body);
        assert!(r.body.contains("\"state\":\"pending\""), "{}", r.body);
    }

    #[test]
    fn edit_proof_maximum_tool_output_tokens_set_and_clear() {
        // A proof step has no `agent` key of its own (not in `PROOF_KEYS`) --
        // `proofs.agent` is populated from the owning cell/task at submit, and
        // that stored value is what the cap is gated on (RAL-333).
        const ONE_CELL: &str = "[[task]]
name=\"t\"
[[task.cell]]
cwd=\".\"
prompt=\"p\"
agent=\"claude-code\"
            [[task.cell.proof]]
prompt=\"check\"
";
        let d = daemon();
        route(&d, "POST", "/api/squads", &submit_body(ONE_CELL));
        let set = serde_json::json!({
            "kind": "proof", "task_idx": 0, "proof_scope": "cell", "cell_idx": 0,
            "proof_idx": 0, "maximum_tool_output_tokens": "8000",
        })
        .to_string();
        let r = route(&d, "POST", "/api/squads/squad-000000000001/edit", &set);
        assert_eq!(r.status, 200, "{}", r.body);
        assert!(
            r.body.contains("\"maximum_tool_output_tokens\":8000"),
            "{}",
            r.body
        );

        let clear = serde_json::json!({
            "kind": "proof", "task_idx": 0, "proof_scope": "cell", "cell_idx": 0,
            "proof_idx": 0, "maximum_tool_output_tokens": "",
        })
        .to_string();
        let r = route(&d, "POST", "/api/squads/squad-000000000001/edit", &clear);
        assert_eq!(r.status, 200, "{}", r.body);
        assert!(!r.body.contains("maximum_tool_output_tokens"), "{}", r.body);
    }

    #[test]
    fn edit_proof_maximum_tool_output_tokens_rejected_for_unsupported_agent() {
        // No explicit proof `agent` -- resolves to the default "claude",
        // which cannot deliver the cap.
        const ONE_CELL: &str = "[[task]]
name=\"t\"
[[task.cell]]
cwd=\".\"
command=\"x\"
            [[task.cell.proof]]
command=\"check\"
";
        let d = daemon();
        route(&d, "POST", "/api/squads", &submit_body(ONE_CELL));
        let body = serde_json::json!({
            "kind": "proof", "task_idx": 0, "proof_scope": "cell", "cell_idx": 0,
            "proof_idx": 0, "maximum_tool_output_tokens": "8000",
        })
        .to_string();
        let r = route(&d, "POST", "/api/squads/squad-000000000001/edit", &body);
        assert_eq!(r.status, 400, "{}", r.body);
        assert!(r.body.contains("maximum_tool_output_tokens"), "{}", r.body);
    }

    #[test]
    fn edit_task_proof_model_resets_only_that_step_onward() {
        const TASK_PROOF: &str = "[[task]]\nname=\"t\"\n[[task.cell]]\ncwd=\".\"\ncommand=\"x\"\n[[task.proof]]\ncommand=\"check\"\n";
        let d = daemon();
        route(&d, "POST", "/api/squads", &submit_body(TASK_PROOF));
        d.lock()
            .set_squad_state("squad-000000000001", SquadState::Done)
            .unwrap();
        let body = serde_json::json!({
            "kind": "proof", "task_idx": 0, "proof_scope": "task", "cell_idx": -1,
            "proof_idx": 0, "model": "gpt-5",
        })
        .to_string();
        let r = route(&d, "POST", "/api/squads/squad-000000000001/edit", &body);
        assert_eq!(r.status, 200, "{}", r.body);
        assert!(r.body.contains("\"model\":\"gpt-5\""), "{}", r.body);
        assert!(r.body.contains("\"state\":\"pending\""), "{}", r.body);
    }

    #[test]
    fn edit_proof_missing_step_is_not_found() {
        let d = daemon();
        route(&d, "POST", "/api/squads", &submit_body(GOOD));
        let body = serde_json::json!({
            "kind": "proof", "task_idx": 0, "proof_scope": "cell", "cell_idx": 0,
            "proof_idx": 0, "model": "gpt-5",
        })
        .to_string();
        let r = route(&d, "POST", "/api/squads/squad-000000000001/edit", &body);
        assert_eq!(r.status, 404, "{}", r.body);
    }

    #[test]
    fn edit_unknown_kind_is_400() {
        let d = daemon();
        route(&d, "POST", "/api/squads", &submit_body(GOOD));
        let r = route(
            &d,
            "POST",
            "/api/squads/squad-000000000001/edit",
            "{\"kind\":\"wat\"}",
        );
        assert_eq!(r.status, 400);
    }

    #[test]
    fn retry_resets_squad_to_pending() {
        let d = daemon();
        route(&d, "POST", "/api/squads", &submit_body(GOOD));
        d.lock()
            .set_squad_state("squad-000000000001", SquadState::Failed)
            .unwrap();
        let r = route(&d, "POST", "/api/squads/squad-000000000001/retry", "");
        assert_eq!(r.status, 200);
        assert!(r.body.contains("\"state\":\"pending\""));
    }

    #[test]
    fn retry_missing_squad_is_404() {
        let d = daemon();
        assert_eq!(
            route(&d, "POST", "/api/squads/squad-999/retry", "").status,
            404
        );
    }

    #[test]
    fn live_view_config_endpoint_defaults_to_unchecked() {
        let d = daemon();
        let r = route(&d, "GET", "/api/config/live-view", "");
        assert_eq!(r.status, 200);
        assert!(r.body.contains("\"show_debug_messages_default\":false"));
    }

    #[test]
    fn cartographer_endpoint_returns_events_from_squad_logs() {
        let d = daemon();
        route(&d, "POST", "/api/squads", &submit_body(GOOD));
        d.lock()
            .set_squad_state("squad-000000000001", SquadState::Done)
            .unwrap();
        let r = route(&d, "GET", "/api/cartographer", "");
        assert_eq!(r.status, 200);
        assert!(r.body.contains("squad → done"));
        assert!(r.body.contains("\"total\":"));
    }

    #[test]
    fn cartographer_endpoint_filters_by_squad_id() {
        let d = daemon();
        route(&d, "POST", "/api/squads", &submit_body(GOOD));
        d.lock()
            .set_squad_state("squad-000000000001", SquadState::Done)
            .unwrap();
        let r = route(
            &d,
            "GET",
            "/api/cartographer?squad_id=squad-000000000001",
            "",
        );
        assert_eq!(r.status, 200);
        assert!(r.body.contains("squad-000000000001"));

        let r_missing = route(&d, "GET", "/api/cartographer?squad_id=squad-nope", "");
        assert_eq!(r_missing.status, 200);
        assert!(r_missing.body.contains("\"total\":0"));
    }

    #[test]
    fn cartographer_endpoint_paginates() {
        let d = daemon();
        route(&d, "POST", "/api/squads", &submit_body(GOOD));
        let r = route(&d, "GET", "/api/cartographer?limit=1&offset=0", "");
        assert_eq!(r.status, 200);
        let r2 = route(&d, "GET", "/api/cartographer?limit=1&offset=100", "");
        assert_eq!(r2.status, 200);
        assert!(r2.body.contains("\"rows\":[]"));
    }

    #[test]
    fn cartographer_get_by_id() {
        let d = daemon();
        route(&d, "POST", "/api/squads", &submit_body(GOOD));
        d.lock()
            .set_squad_state("squad-000000000001", SquadState::Done)
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
        route(&d, "POST", "/api/squads", &submit_body(GOOD));
        d.lock()
            .set_task_state("squad-000000000001", 0, NodeState::Done)
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
        route(&d, "POST", "/api/squads", &submit_body(GOOD));
        d.lock()
            .set_task_state("squad-000000000001", 0, NodeState::Done)
            .unwrap();
        let r = route(
            &d,
            "GET",
            "/api/cartographer?entity=task:squad-000000000001:0",
            "",
        );
        assert_eq!(r.status, 200);
        assert!(r.body.contains("task → done"));
    }

    #[test]
    fn cartographer_endpoint_entity_filter_squad_scopes_by_squad_id_only() {
        let d = daemon();
        route(&d, "POST", "/api/squads", &submit_body(GOOD));
        route(&d, "POST", "/api/squads", &submit_body(GOOD));
        let r = route(
            &d,
            "GET",
            "/api/cartographer?entity=squad:squad-000000000001",
            "",
        );
        assert_eq!(r.status, 200);
        assert!(r.body.contains("squad-000000000001"));
        assert!(!r.body.contains("squad-000000000002"));
    }

    #[test]
    fn cartographer_endpoint_entity_filter_bad_uri_is_400() {
        let d = daemon();
        let r = route(&d, "GET", "/api/cartographer?entity=bogus", "");
        assert_eq!(r.status, 400);
    }

    #[test]
    fn cartographer_endpoint_explicit_squad_id_wins_over_entity() {
        // An explicit `squad_id=` should not be clobbered by a same-request
        // `entity=` naming a different squad -- the more specific,
        // explicitly-given field always wins (see `apply_entity_filter`'s
        // doc comment).
        let d = daemon();
        route(&d, "POST", "/api/squads", &submit_body(GOOD)); // squad-1
        route(&d, "POST", "/api/squads", &submit_body(GOOD)); // squad-2
        let r = route(
            &d,
            "GET",
            "/api/cartographer?squad_id=squad-000000000002&entity=squad:squad-000000000001",
            "",
        );
        assert_eq!(r.status, 200);
        assert!(r.body.contains("squad-000000000002"));
        assert!(!r.body.contains("squad-000000000001"));
    }

    // ── Ghost memory (RAL-136) ───────────────────────────────────────────────

    #[test]
    fn ghost_get_returns_published_ghost_or_404() {
        let d = daemon();
        route(&d, "POST", "/api/squads", &submit_body(GOOD));
        let uri = crate::ghost::cell_uri("squad-000000000001", 0, 0);
        d.lock()
            .upsert_ghost(
                &uri,
                crate::ghost::KIND_CELL,
                Some("squad-000000000001"),
                None,
                "watch out for the flaky test",
                None,
            )
            .unwrap();

        let r = route(&d, "GET", &format!("/api/ghosts/{uri}"), "");
        assert_eq!(r.status, 200);
        assert!(r.body.contains("watch out for the flaky test"));

        let missing = route(&d, "GET", "/api/ghosts/cell:nope:0:0", "");
        assert_eq!(missing.status, 404);
    }

    #[test]
    fn ghost_copy_seeds_another_cell_and_validates_target_uri() {
        let d = daemon();
        route(&d, "POST", "/api/squads", &submit_body(GOOD));
        route(&d, "POST", "/api/squads", &submit_body(GOOD));
        let src = crate::ghost::cell_uri("squad-000000000001", 0, 0);
        let dst = crate::ghost::cell_uri("squad-000000000002", 0, 0);
        d.lock()
            .upsert_ghost(
                &src,
                crate::ghost::KIND_CELL,
                Some("squad-000000000001"),
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
            &serde_json::json!({ "source_uri": "cell:nope:0:0", "target_uri": dst }),
        )
        .unwrap();
        assert_eq!(
            route(&d, "POST", "/api/ghosts/copy", &missing_source).status,
            404
        );
    }

    #[test]
    fn mailbox_register_returns_a_client_id() {
        let d = daemon();
        let r = route(&d, "POST", "/api/mailbox/register", "");
        assert_eq!(r.status, 201, "body={}", r.body);
        let v: serde_json::Value = serde_json::from_str(&r.body).unwrap();
        assert!(v["client_id"].as_str().unwrap().starts_with("client-"));
    }

    #[test]
    fn mailbox_messages_requires_a_registered_client() {
        let d = daemon();
        assert_eq!(
            route(&d, "GET", "/api/mailbox/client-bogus/messages", "").status,
            404
        );
        assert_eq!(
            route(&d, "POST", "/api/mailbox/client-bogus/drain", "").status,
            404
        );
    }

    #[test]
    fn mailbox_end_to_end_enqueue_list_filter_drain() {
        let d = daemon();
        let register = route(&d, "POST", "/api/mailbox/register", "");
        let client_id =
            serde_json::from_str::<serde_json::Value>(&register.body).unwrap()["client_id"]
                .as_str()
                .unwrap()
                .to_string();

        d.lock()
            .enqueue_mailbox_message(
                crate::mailbox::MailboxPriority::Urgent,
                "cell failed",
                None,
                None,
                None,
                None,
            )
            .unwrap();
        d.lock()
            .enqueue_mailbox_message(
                crate::mailbox::MailboxPriority::Normal,
                "fyi",
                None,
                None,
                None,
                None,
            )
            .unwrap();

        let all = route(&d, "GET", &format!("/api/mailbox/{client_id}/messages"), "");
        assert_eq!(all.status, 200, "body={}", all.body);
        let all_json: serde_json::Value = serde_json::from_str(&all.body).unwrap();
        assert_eq!(all_json.as_array().unwrap().len(), 2);

        let urgent_only = route(
            &d,
            "GET",
            &format!("/api/mailbox/{client_id}/messages?priority=urgent"),
            "",
        );
        let urgent_json: serde_json::Value = serde_json::from_str(&urgent_only.body).unwrap();
        assert_eq!(urgent_json.as_array().unwrap().len(), 1);
        assert_eq!(urgent_json[0]["message"], "cell failed");

        let drain = route(&d, "POST", &format!("/api/mailbox/{client_id}/drain"), "");
        assert_eq!(drain.status, 200, "body={}", drain.body);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&drain.body).unwrap()["drained"],
            2
        );

        let unread_after = route(
            &d,
            "GET",
            &format!("/api/mailbox/{client_id}/messages?unread=true"),
            "",
        );
        let unread_json: serde_json::Value = serde_json::from_str(&unread_after.body).unwrap();
        assert!(unread_json.as_array().unwrap().is_empty());
    }

    #[test]
    fn squad_logs_records_transitions() {
        let d = daemon();
        route(&d, "POST", "/api/squads", &submit_body(GOOD));
        d.lock()
            .set_squad_state("squad-000000000001", SquadState::Done)
            .unwrap();
        let r = route(&d, "GET", "/api/squads/squad-000000000001/logs", "");
        assert_eq!(r.status, 200);
        assert!(r.body.contains("squad → done"));
        assert!(r.body.contains("\"scope\":\"squad\""));
    }

    #[test]
    fn logs_missing_squad_is_404() {
        let d = daemon();
        assert_eq!(
            route(&d, "GET", "/api/squads/squad-999/logs", "").status,
            404
        );
    }

    #[test]
    fn squad_timeline_route_returns_merged_view_with_metadata() {
        // See `timeline::TIMELINE_FILE_TEST_LOCK`: every fresh in-memory store's
        // first squad is "squad-000000000001", so this and `timeline.rs`'s own
        // tests would otherwise race on the same OS temp-file path.
        let _guard = crate::timeline::TIMELINE_FILE_TEST_LOCK.lock().unwrap();
        let d = daemon();
        route(&d, "POST", "/api/squads", &submit_body(GOOD));
        d.lock()
            .set_squad_state("squad-000000000001", SquadState::Done)
            .unwrap();
        let r = route(&d, "GET", "/api/squads/squad-000000000001/timeline", "");
        assert_eq!(r.status, 200);
        assert!(r.body.contains("\"squad_id\":\"squad-000000000001\""));
        assert!(r.body.contains("\"entries\":["));
        assert!(r.body.contains("\"text\":"));
        assert!(r.body.contains("\"file_path\":"));
        assert!(r.body.contains("squad → done"));
    }

    #[test]
    fn squad_timeline_missing_squad_is_404() {
        let d = daemon();
        assert_eq!(
            route(&d, "GET", "/api/squads/squad-999/timeline", "").status,
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
    fn squad_worktrees_lists_cells() {
        let d = daemon();
        route(&d, "POST", "/api/squads", &submit_body(GOOD));
        let r = route(&d, "GET", "/api/squads/squad-000000000001/worktrees", "");
        assert_eq!(r.status, 200);
        // The GOOD fixture's cell cwd is "/r"; not a git worktree -> project/upstream null.
        assert!(r.body.contains("\"worktree\":\"/r\""));
        assert!(r.body.contains("\"project\":null"));
        assert!(r.body.contains("\"upstream\":null"));
    }

    #[test]
    fn worktrees_missing_squad_is_404() {
        let d = daemon();
        assert_eq!(
            route(&d, "GET", "/api/squads/squad-999/worktrees", "").status,
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
    fn resolver_task_and_cell_id_is_stable_across_reorder() {
        // RAL-192: the historical-record lookup must keep resolving to the
        // SAME cell after branches are reordered, since it recomputes the
        // cell id fresh on every call rather than caching what it saw when
        // the resolver actually ran (see `write_pane_snapshot`'s key).
        // Before the fix, the cell id was keyed on the branch's mutable
        // stack `position`, so a reorder silently pointed this lookup at a
        // different branch's cell -- guaranteed a miss for the branch that
        // moved.
        let d = daemon();
        let id = d.lock().create_guardian("g", "main", "/r").unwrap();
        d.lock().add_guardian_branch(&id, "a").unwrap();
        d.lock().add_guardian_branch(&id, "b").unwrap();
        d.lock().add_guardian_branch(&id, "c").unwrap();
        let branches = d.lock().guardian_branches(&id).unwrap();
        let branch_id_a = branches[0].id.clone();
        assert_eq!(branches[0].branch, "a");

        let before = resolver_task_and_cell_id(&d, &id, &branch_id_a).unwrap();

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

        let after = resolver_task_and_cell_id(&d, &id, &branch_id_a).unwrap();
        assert_eq!(
            before, after,
            "cell id for branch a must not change on reorder"
        );
    }

    #[test]
    fn resolver_task_and_cell_id_picks_the_feedback_session_while_actioning() {
        // RAL-298: while reviewer feedback is being actioned, Live View must
        // show the feedback resolver's own session, not the merge/rebase
        // resolver's (stale, unrelated) one.
        let d = daemon();
        let id = d.lock().create_guardian("g", "main", "/r").unwrap();
        d.lock().add_guardian_branch(&id, "a").unwrap();
        let branch_id = d.lock().guardian_branches(&id).unwrap()[0].id.clone();
        d.lock()
            .set_branch_status(
                &id,
                &branch_id,
                crate::guardian::MergeStatus::Actioning,
                Some("applying reviewer feedback"),
            )
            .unwrap();

        let (task, cell_id) = resolver_task_and_cell_id(&d, &id, &branch_id).unwrap();
        assert_eq!(task, crate::guardian_merge::FEEDBACK_TASK);
        assert_eq!(cell_id, crate::guardian_merge::feedback_cell_id(&branch_id));
    }

    #[test]
    fn pick_freshest_prefers_the_more_recently_written_feedback_snapshot() {
        // RAL-298: once a feedback pass has ended, its historical record must
        // not be shadowed by an earlier, now-stale merge/rebase resolver log.
        use std::time::{Duration, SystemTime};
        let earlier = SystemTime::UNIX_EPOCH;
        let later = earlier + Duration::from_secs(60);
        assert_eq!(
            pick_freshest("resolver", "feedback", Some(earlier), Some(later)),
            "feedback"
        );
    }

    #[test]
    fn pick_freshest_prefers_resolver_when_it_is_more_recent() {
        use std::time::{Duration, SystemTime};
        let earlier = SystemTime::UNIX_EPOCH;
        let later = earlier + Duration::from_secs(60);
        assert_eq!(
            pick_freshest("resolver", "feedback", Some(later), Some(earlier)),
            "resolver"
        );
    }

    #[test]
    fn pick_freshest_defaults_to_resolver_when_feedback_never_ran() {
        assert_eq!(
            pick_freshest(
                "resolver",
                "feedback",
                Some(std::time::SystemTime::UNIX_EPOCH),
                None
            ),
            "resolver"
        );
    }

    #[test]
    fn pick_freshest_defaults_to_resolver_when_neither_ever_ran() {
        assert_eq!(
            pick_freshest("resolver", "feedback", None, None),
            "resolver"
        );
    }

    #[test]
    fn cancel_squad() {
        let d = daemon();
        route(&d, "POST", "/api/squads", &submit_body(GOOD));
        let c = route(&d, "POST", "/api/squads/squad-000000000001/cancel", "");
        assert_eq!(c.status, 200);
        assert!(c.body.contains("\"state\":\"cancelled\""));
        assert!(c.body.contains("\"cancelled\":[\"squad-000000000001\"]"));
    }

    #[test]
    fn cancel_squad_is_always_available_even_when_already_terminal() {
        let d = daemon();
        route(&d, "POST", "/api/squads", &submit_body(GOOD));
        d.lock()
            .set_squad_state("squad-000000000001", crate::store::SquadState::Done)
            .unwrap();
        // Per RAL-116, the cancel action must still succeed (not 4xx) on an
        // already-terminal squad, locking it out of ever being picked up again.
        let c = route(&d, "POST", "/api/squads/squad-000000000001/cancel", "");
        assert_eq!(c.status, 200);
        assert!(c.body.contains("\"state\":\"cancelled\""));
    }

    #[test]
    fn cancel_squad_cascades_to_dependent_squad() {
        let d = daemon();
        route(&d, "POST", "/api/squads", &submit_body(GOOD)); // squad-1
        let dependent = "[[default]]\ndepends_on=[\"squad-000000000001\"]\n".to_string() + GOOD;
        route(&d, "POST", "/api/squads", &submit_body(&dependent)); // squad-2

        let c = route(&d, "POST", "/api/squads/squad-000000000001/cancel", "");
        assert_eq!(c.status, 200);
        assert!(c.body.contains("squad-000000000001"));
        assert!(c.body.contains("squad-000000000002"));
        assert!(
            route(&d, "GET", "/api/squads/squad-000000000002", "")
                .body
                .contains("\"state\":\"cancelled\"")
        );
    }

    #[cfg_attr(windows, ignore = "CI-only on Windows: exercises a real psmux server")]
    #[test]
    fn live_tmux_shutdown_without_auto_cancel_leaves_squad_state_alone_but_requests_shutdown() {
        // Real production code kills each known squad/guardian's own
        // ralphus_{id}_-prefixed tmux.exe cells (see
        // `kill_squad_tmux_sessions`) — serialize against `tmux.rs`'s live
        // tests anyway, since a real `tmux list-sessions`/`kill-session`
        // round trip still happens. The squad_id itself is also given a
        // per-invocation-unique suffix (RAL-177), not just serialized:
        // `kill_squad_tmux_sessions` kills by `ralphus_{squad_id}_` *prefix*, so
        // a deterministic literal id here would let this shutdown call reach
        // — and kill — a same-prefixed real cell created by an identical
        // test running concurrently in a sibling worktree against the same
        // shared, machine-wide psmux server, even with a fully unique task
        // name on that other cell (the prefix kill never looks at the
        // task portion at all). See `Store::insert_squad_with_id`'s doc
        // comment.
        let _tmux_guard = crate::tmux::LIVE_TMUX_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        crate::tmux::sweep_dead_test_sessions_once();
        let d = daemon();
        let squad_id = crate::tmux::unique_test_tag("shutdown-no-auto-cancel");
        d.lock()
            .insert_squad_with_id(&squad_id, &parse(GOOD), None, false)
            .unwrap();
        d.lock()
            .set_squad_state(&squad_id, SquadState::Running)
            .unwrap();

        assert!(!d.shutdown_requested());
        let r = route(&d, "POST", "/api/daemon/shutdown", "");
        assert_eq!(r.status, 200, "body={}", r.body);
        assert!(r.body.contains("\"state\":\"stopping\""));
        assert!(r.body.contains("\"auto_cancel\":false"));
        assert!(d.shutdown_requested());

        // Left running for crash-recovery to resume on next `serve()` startup.
        assert!(
            route(&d, "GET", &format!("/api/squads/{squad_id}"), "")
                .body
                .contains("\"state\":\"running\"")
        );
    }

    #[cfg_attr(windows, ignore = "CI-only on Windows: exercises a real psmux server")]
    #[test]
    fn live_tmux_shutdown_with_auto_cancel_cancels_active_squads_and_guardians() {
        // See `live_tmux_shutdown_without_auto_cancel_leaves_squad_state_alone_but_requests_shutdown`'s
        // comment for why each squad_id here is per-invocation-unique
        // (RAL-177), not just the literal deterministic id.
        let _tmux_guard = crate::tmux::LIVE_TMUX_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        crate::tmux::sweep_dead_test_sessions_once();
        let d = daemon();
        let squad_id_1 = crate::tmux::unique_test_tag("shutdown-auto-cancel-1");
        let squad_id_2 = crate::tmux::unique_test_tag("shutdown-auto-cancel-2");
        d.lock()
            .insert_squad_with_id(&squad_id_1, &parse(GOOD), None, false)
            .unwrap(); // pending
        d.lock()
            .insert_squad_with_id(&squad_id_2, &parse(GOOD), None, false)
            .unwrap();
        d.lock()
            .set_squad_state(&squad_id_2, SquadState::Done)
            .unwrap();

        let gbody =
            serde_json::json!({"name":"r","base_branch":"main","git_root":"/repo"}).to_string();
        route(&d, "POST", "/api/guardians", &gbody);
        let gid = "guardian-000000000001";

        let body = serde_json::to_string(&serde_json::json!({ "auto_cancel": true })).unwrap();
        let r = route(&d, "POST", "/api/daemon/shutdown", &body);
        assert_eq!(r.status, 200, "body={}", r.body);
        assert!(r.body.contains("\"auto_cancel\":true"));
        assert!(r.body.contains(&squad_id_1));
        assert!(!r.body.contains(&squad_id_2)); // already terminal — left alone
        assert!(r.body.contains(gid));
        assert!(d.shutdown_requested());

        assert!(
            route(&d, "GET", &format!("/api/squads/{squad_id_1}"), "")
                .body
                .contains("\"state\":\"cancelled\"")
        );
        assert!(
            route(&d, "GET", &format!("/api/squads/{squad_id_2}"), "")
                .body
                .contains("\"state\":\"done\"")
        );
        assert!(
            route(&d, "GET", &format!("/api/guardians/{gid}"), "")
                .body
                .contains("\"status\":\"cancelled\"")
        );
    }

    #[cfg_attr(windows, ignore = "CI-only on Windows: exercises a real psmux server")]
    #[test]
    fn live_tmux_shutdown_body_defaults_auto_cancel_to_false() {
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
    fn cancel_squad_preview_reports_impact_without_mutating() {
        let d = daemon();
        route(&d, "POST", "/api/squads", &submit_body(GOOD)); // squad-1
        let dependent = "[[default]]\ndepends_on=[\"squad-000000000001\"]\n".to_string() + GOOD;
        route(&d, "POST", "/api/squads", &submit_body(&dependent)); // squad-2

        let p = route(
            &d,
            "POST",
            "/api/squads/squad-000000000001/cancel/preview",
            "",
        );
        assert_eq!(p.status, 200);
        assert!(p.body.contains("squad-000000000001"));
        assert!(p.body.contains("squad-000000000002"));
        // Nothing was actually mutated.
        assert!(
            route(&d, "GET", "/api/squads/squad-000000000001", "")
                .body
                .contains("\"state\":\"pending\"")
        );
        assert!(
            route(&d, "GET", "/api/squads/squad-000000000002", "")
                .body
                .contains("\"state\":\"pending\"")
        );
    }

    #[test]
    fn cancel_squad_preview_missing_is_404() {
        let d = daemon();
        assert_eq!(
            route(&d, "POST", "/api/squads/squad-999/cancel/preview", "").status,
            404
        );
    }

    #[test]
    fn delete_squad_route() {
        let d = daemon();
        let _troot = isolated_terminal_root();
        route(&d, "POST", "/api/squads", &submit_body(GOOD));
        let del = route(&d, "DELETE", "/api/squads/squad-000000000001", "");
        assert_eq!(del.status, 200);
        assert!(del.body.contains("\"state\":\"deleted\""));
        // Gone now.
        assert_eq!(
            route(&d, "GET", "/api/squads/squad-000000000001", "").status,
            404
        );
    }

    #[test]
    fn delete_missing_squad_is_404() {
        let d = daemon();
        assert_eq!(route(&d, "DELETE", "/api/squads/squad-999", "").status, 404);
    }

    #[test]
    fn cell_terminal_log_attempts_route_lists_and_reads_attempts() {
        let d = daemon();
        let _troot = isolated_terminal_root();
        route(&d, "POST", "/api/squads", &submit_body(GOOD));
        let squad_id = "squad-000000000001";

        // Nothing persisted yet — an empty list, not an error.
        let list = route(
            &d,
            "GET",
            &format!("/api/squads/{squad_id}/cells/0/0/terminal-log-attempts"),
            "",
        );
        assert_eq!(list.status, 200);
        assert!(list.body.contains("\"attempts\":[]"), "{}", list.body);

        // A non-existent attempt on a cell with no attempts at all is 404.
        let missing = route(
            &d,
            "GET",
            &format!("/api/squads/{squad_id}/cells/0/0/terminal-log-attempts/0"),
            "",
        );
        assert_eq!(missing.status, 404);

        // Seed a couple of attempts the same way `SubprocessRunner::run_via_tmux_attempt`
        // would persist them, keyed by the same deterministic cell name.
        let task = d.lock().get_task_name(squad_id, 0).unwrap();
        let cell_id = d.lock().get_cell_id(squad_id, 0, 0).unwrap();
        let name = crate::tmux::session_name(squad_id, &task, &cell_id);
        crate::terminal_log::write_attempt(&name, 0, "first attempt output", 100);
        crate::terminal_log::write_attempt(&name, 1, "second attempt output", 100);

        let list = route(
            &d,
            "GET",
            &format!("/api/squads/{squad_id}/cells/0/0/terminal-log-attempts"),
            "",
        );
        assert_eq!(list.status, 200);
        assert!(list.body.contains("\"attempt\":0"), "{}", list.body);
        assert!(list.body.contains("\"attempt\":1"), "{}", list.body);

        let attempt0 = route(
            &d,
            "GET",
            &format!("/api/squads/{squad_id}/cells/0/0/terminal-log-attempts/0"),
            "",
        );
        assert_eq!(attempt0.status, 200);
        assert!(attempt0.body.contains("first attempt output"));

        let attempt1 = route(
            &d,
            "GET",
            &format!("/api/squads/{squad_id}/cells/0/0/terminal-log-attempts/1"),
            "",
        );
        assert_eq!(attempt1.status, 200);
        assert!(attempt1.body.contains("second attempt output"));

        let attempt_missing = route(
            &d,
            "GET",
            &format!("/api/squads/{squad_id}/cells/0/0/terminal-log-attempts/9"),
            "",
        );
        assert_eq!(attempt_missing.status, 404);

        crate::terminal_log::delete_for_session(&name);
    }

    #[test]
    fn deleting_a_squad_deletes_its_terminal_logs() {
        let d = daemon();
        let _troot = isolated_terminal_root();
        route(&d, "POST", "/api/squads", &submit_body(GOOD));
        let squad_id = "squad-000000000001";
        let task = d.lock().get_task_name(squad_id, 0).unwrap();
        let cell_id = d.lock().get_cell_id(squad_id, 0, 0).unwrap();
        let name = crate::tmux::session_name(squad_id, &task, &cell_id);
        crate::terminal_log::write_attempt(&name, 0, "will be deleted", 100);
        assert!(!crate::terminal_log::list_attempts(&name).is_empty());

        let del = route(&d, "DELETE", &format!("/api/squads/{squad_id}"), "");
        assert_eq!(del.status, 200);

        assert!(crate::terminal_log::list_attempts(&name).is_empty());
    }

    #[test]
    fn set_status_squad() {
        let d = daemon();
        route(&d, "POST", "/api/squads", &submit_body(GOOD));
        let body = serde_json::json!({"kind": "squad", "state": "done"}).to_string();
        let r = route(
            &d,
            "POST",
            "/api/squads/squad-000000000001/set-status",
            &body,
        );
        assert_eq!(r.status, 200);
        assert!(r.body.contains("\"state\":\"done\""));
    }

    #[test]
    fn set_status_squad_cancelled_cascades_like_cancel_button() {
        // Set Status -> cancelled must go through the exact same cascading
        // cancel path as the "Cancel Squad" button (RAL-116), not a bare DB flip.
        let d = daemon();
        route(&d, "POST", "/api/squads", &submit_body(GOOD)); // squad-1
        let dependent = "[[default]]\ndepends_on=[\"squad-000000000001\"]\n".to_string() + GOOD;
        route(&d, "POST", "/api/squads", &submit_body(&dependent)); // squad-2

        let body = serde_json::json!({"kind": "squad", "state": "cancelled"}).to_string();
        let r = route(
            &d,
            "POST",
            "/api/squads/squad-000000000001/set-status",
            &body,
        );
        assert_eq!(r.status, 200);
        assert!(r.body.contains("\"state\":\"cancelled\""));
        // The dependent squad was cascade-cancelled too, same as the button.
        assert!(
            route(&d, "GET", "/api/squads/squad-000000000002", "")
                .body
                .contains("\"state\":\"cancelled\"")
        );
    }

    #[test]
    fn set_status_squad_cancelled_works_even_when_already_terminal() {
        let d = daemon();
        route(&d, "POST", "/api/squads", &submit_body(GOOD));
        d.lock()
            .set_squad_state("squad-000000000001", crate::store::SquadState::Done)
            .unwrap();
        let body = serde_json::json!({"kind": "squad", "state": "cancelled"}).to_string();
        let r = route(
            &d,
            "POST",
            "/api/squads/squad-000000000001/set-status",
            &body,
        );
        assert_eq!(r.status, 200);
        assert!(r.body.contains("\"state\":\"cancelled\""));
    }

    #[test]
    fn set_status_task() {
        let d = daemon();
        route(&d, "POST", "/api/squads", &submit_body(GOOD));
        let body = serde_json::json!({"kind": "task", "task_idx": 0, "state": "done"}).to_string();
        let r = route(
            &d,
            "POST",
            "/api/squads/squad-000000000001/set-status",
            &body,
        );
        assert_eq!(r.status, 200);
        let v: serde_json::Value = serde_json::from_str(&r.body).unwrap();
        assert_eq!(v["tasks"][0]["state"].as_str(), Some("done"));
    }

    // RAL-157: solo/unsolo a task within a squad.

    const TWO_TASKS: &str = "[[task]]\nname=\"a\"\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\n[[task]]\nname=\"b\"\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\n";
    const THREE_TASKS: &str = "[[task]]\nname=\"a\"\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\n[[task]]\nname=\"b\"\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\n[[task]]\nname=\"c\"\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\n";
    const STOP_CASCADE_RUN: &str = r#"
[[task]]
name = "alpha"
[[task.cell]]
id = "work"
cwd = "/r"
prompt = "p"
[[task.cell.proof]]
id = "check-a"
command = "true"
[[task.cell.proof]]
id = "check-b"
command = "true"
[[task.cell]]
id = "after"
cwd = "/r"
prompt = "p"
depends_on = ["work"]
[[task.cell.proof]]
id = "after-check"
command = "true"
[[task.proof]]
id = "task-check-a"
command = "true"
[[task.proof]]
id = "task-check-b"
command = "true"

[[task]]
name = "beta"
depends_on = ["alpha/after"]
[[task.cell]]
id = "downstream"
cwd = "/r"
prompt = "p"
[[task.cell.proof]]
id = "beta-check"
command = "true"
[[task.proof]]
id = "beta-task-check"
command = "true"

[[task]]
name = "side"
[[task.cell]]
id = "unrelated"
cwd = "/r"
prompt = "p"
[[task.cell.proof]]
id = "side-check"
command = "true"
[[task.proof]]
id = "side-task-check"
command = "true"
"#;
    const SQUAD_1: &str = "squad-000000000001";

    fn submit_stop_cascade_squad(d: &Daemon) {
        route(d, "POST", "/api/squads", &submit_body(STOP_CASCADE_RUN));
    }

    fn set_status_and_parse(d: &Daemon, body: serde_json::Value) -> serde_json::Value {
        let r = route(
            d,
            "POST",
            &format!("/api/squads/{SQUAD_1}/set-status"),
            &body.to_string(),
        );
        assert_eq!(r.status, 200, "body={}", r.body);
        serde_json::from_str(&r.body).unwrap()
    }

    fn seed_task_stop_states(d: &Daemon) {
        let guard = d.lock();
        guard
            .set_task_state(SQUAD_1, 0, NodeState::Running)
            .unwrap();
        guard
            .set_cell_state(SQUAD_1, 0, 0, NodeState::Running)
            .unwrap();
        guard
            .set_task_state(SQUAD_1, 2, NodeState::Running)
            .unwrap();
        guard
            .set_cell_state(SQUAD_1, 2, 0, NodeState::Running)
            .unwrap();
    }

    fn seed_cell_proof_stop_states(d: &Daemon) {
        let guard = d.lock();
        guard
            .set_task_state(SQUAD_1, 0, NodeState::Running)
            .unwrap();
        guard
            .set_cell_state(SQUAD_1, 0, 0, NodeState::Done)
            .unwrap();
        guard
            .set_proof_state(SQUAD_1, 0, "cell", 0, 0, NodeState::Running)
            .unwrap();
        guard
            .set_task_state(SQUAD_1, 2, NodeState::Running)
            .unwrap();
        guard
            .set_cell_state(SQUAD_1, 2, 0, NodeState::Running)
            .unwrap();
    }

    fn seed_task_proof_stop_states(d: &Daemon) {
        let guard = d.lock();
        guard
            .set_cell_state(SQUAD_1, 0, 0, NodeState::Done)
            .unwrap();
        guard
            .set_proof_state(SQUAD_1, 0, "cell", 0, 0, NodeState::Done)
            .unwrap();
        guard
            .set_proof_state(SQUAD_1, 0, "cell", 0, 1, NodeState::Done)
            .unwrap();
        guard
            .set_cell_state(SQUAD_1, 0, 1, NodeState::Done)
            .unwrap();
        guard
            .set_proof_state(SQUAD_1, 0, "cell", 1, 0, NodeState::Done)
            .unwrap();
        guard
            .set_task_state(SQUAD_1, 0, NodeState::Running)
            .unwrap();
        guard
            .set_proof_state(SQUAD_1, 0, "task", -1, 0, NodeState::Running)
            .unwrap();
        guard
            .set_task_state(SQUAD_1, 2, NodeState::Running)
            .unwrap();
        guard
            .set_cell_state(SQUAD_1, 2, 0, NodeState::Running)
            .unwrap();
    }

    #[test]
    fn solo_task_sets_soloed_and_returns_refreshed_squad() {
        let d = daemon();
        route(&d, "POST", "/api/squads", &submit_body(TWO_TASKS));
        let r = route(
            &d,
            "POST",
            "/api/squads/squad-000000000001/tasks/0/solo",
            "",
        );
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
        route(&d, "POST", "/api/squads", &submit_body(TWO_TASKS));
        route(
            &d,
            "POST",
            "/api/squads/squad-000000000001/tasks/0/solo",
            "",
        );
        let r = route(
            &d,
            "POST",
            "/api/squads/squad-000000000001/tasks/0/unsolo",
            "",
        );
        assert_eq!(r.status, 200);
        let v: serde_json::Value = serde_json::from_str(&r.body).unwrap();
        assert_eq!(v["tasks"][0]["soloed"].as_bool(), Some(false));
    }

    #[test]
    fn solo_task_multiple_at_once_has_no_auto_exclusivity() {
        let d = daemon();
        route(&d, "POST", "/api/squads", &submit_body(TWO_TASKS));
        route(
            &d,
            "POST",
            "/api/squads/squad-000000000001/tasks/0/solo",
            "",
        );
        let r = route(
            &d,
            "POST",
            "/api/squads/squad-000000000001/tasks/1/solo",
            "",
        );
        assert_eq!(r.status, 200);
        let v: serde_json::Value = serde_json::from_str(&r.body).unwrap();
        assert_eq!(v["tasks"][0]["soloed"].as_bool(), Some(true));
        assert_eq!(v["tasks"][1]["soloed"].as_bool(), Some(true));
    }

    #[test]
    fn solo_task_unknown_squad_is_not_found() {
        let d = daemon();
        let r = route(&d, "POST", "/api/squads/squad-nope/tasks/0/solo", "");
        assert_eq!(r.status, 404);
    }

    #[test]
    fn solo_task_unknown_task_index_is_not_found() {
        let d = daemon();
        route(&d, "POST", "/api/squads", &submit_body(GOOD));
        let r = route(
            &d,
            "POST",
            "/api/squads/squad-000000000001/tasks/99/solo",
            "",
        );
        assert_eq!(r.status, 404);
    }

    #[test]
    fn solo_task_non_integer_index_is_bad_request() {
        let d = daemon();
        route(&d, "POST", "/api/squads", &submit_body(GOOD));
        let r = route(
            &d,
            "POST",
            "/api/squads/squad-000000000001/tasks/nope/solo",
            "",
        );
        assert_eq!(r.status, 400);
    }

    #[test]
    fn set_status_cell() {
        let d = daemon();
        route(&d, "POST", "/api/squads", &submit_body(GOOD));
        let body = serde_json::json!({
            "kind": "cell", "task_idx": 0, "cell_idx": 0, "state": "done"
        })
        .to_string();
        let r = route(
            &d,
            "POST",
            "/api/squads/squad-000000000001/set-status",
            &body,
        );
        assert_eq!(r.status, 200);
        let v: serde_json::Value = serde_json::from_str(&r.body).unwrap();
        assert_eq!(v["tasks"][0]["cells"][0]["state"].as_str(), Some("done"));
    }

    #[test]
    fn set_status_task_cancelled_cascades_only_to_downstream_branch() {
        let d = daemon();
        submit_stop_cascade_squad(&d);
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
            v["tasks"][0]["cells"][0]["state"].as_str(),
            Some("cancelled")
        );
        assert_eq!(
            v["tasks"][0]["cells"][1]["state"].as_str(),
            Some("cancelled")
        );
        assert_eq!(
            v["tasks"][0]["cells"][0]["proof"][0]["state"].as_str(),
            Some("cancelled")
        );
        assert_eq!(
            v["tasks"][0]["proof"][0]["state"].as_str(),
            Some("cancelled")
        );
        assert_eq!(v["tasks"][1]["state"].as_str(), Some("cancelled"));
        assert_eq!(
            v["tasks"][1]["cells"][0]["state"].as_str(),
            Some("cancelled")
        );
        assert_eq!(
            v["tasks"][1]["cells"][0]["proof"][0]["state"].as_str(),
            Some("cancelled")
        );
        assert_eq!(
            v["tasks"][1]["proof"][0]["state"].as_str(),
            Some("cancelled")
        );
        assert_ne!(v["tasks"][2]["state"].as_str(), Some("cancelled"));
        assert_eq!(v["tasks"][2]["cells"][0]["state"].as_str(), Some("running"));
        assert_ne!(
            v["state"].as_str(),
            Some("cancelled"),
            "partial stop must not cancel the whole squad"
        );
    }

    #[test]
    fn set_status_cell_cancelled_cascades_only_to_downstream_branch() {
        let d = daemon();
        submit_stop_cascade_squad(&d);
        seed_task_stop_states(&d);
        let v = set_status_and_parse(
            &d,
            serde_json::json!({
                "kind": "cell", "task_idx": 0, "cell_idx": 0, "state": "cancelled"
            }),
        );
        assert_eq!(v["tasks"][0]["state"].as_str(), Some("cancelled"));
        assert_eq!(
            v["tasks"][0]["cells"][0]["state"].as_str(),
            Some("cancelled"),
            "target cell must be cancelled"
        );
        assert_eq!(
            v["tasks"][0]["cells"][1]["state"].as_str(),
            Some("cancelled")
        );
        assert_eq!(
            v["tasks"][0]["cells"][0]["proof"][0]["state"].as_str(),
            Some("cancelled")
        );
        assert_eq!(
            v["tasks"][0]["proof"][0]["state"].as_str(),
            Some("cancelled")
        );
        assert_eq!(v["tasks"][1]["state"].as_str(), Some("cancelled"));
        assert_eq!(
            v["tasks"][1]["cells"][0]["state"].as_str(),
            Some("cancelled")
        );
        assert_ne!(
            v["tasks"][2]["state"].as_str(),
            Some("cancelled"),
            "unrelated task must stay alive"
        );
        assert_eq!(v["tasks"][2]["cells"][0]["state"].as_str(), Some("running"));
        assert_ne!(
            v["state"].as_str(),
            Some("cancelled"),
            "partial stop must not cancel the whole squad"
        );
    }

    #[test]
    fn set_status_cell_proof_cancelled_cascades_only_to_downstream_branch() {
        let d = daemon();
        submit_stop_cascade_squad(&d);
        seed_cell_proof_stop_states(&d);
        let v = set_status_and_parse(
            &d,
            serde_json::json!({
                "kind": "proof",
                "task_idx": 0,
                "cell_idx": 0,
                "proof_idx": 0,
                "proof_scope": "cell",
                "state": "cancelled"
            }),
        );
        assert_eq!(v["tasks"][0]["state"].as_str(), Some("cancelled"));
        assert_eq!(
            v["tasks"][0]["cells"][0]["state"].as_str(),
            Some("done"),
            "owning cell body is upstream of the stopped proof"
        );
        assert_eq!(
            v["tasks"][0]["cells"][0]["proof"][0]["state"].as_str(),
            Some("cancelled")
        );
        assert_eq!(
            v["tasks"][0]["cells"][0]["proof"][1]["state"].as_str(),
            Some("cancelled")
        );
        assert_eq!(
            v["tasks"][0]["cells"][1]["state"].as_str(),
            Some("cancelled")
        );
        assert_eq!(
            v["tasks"][0]["cells"][1]["proof"][0]["state"].as_str(),
            Some("cancelled")
        );
        assert_eq!(
            v["tasks"][0]["proof"][0]["state"].as_str(),
            Some("cancelled")
        );
        assert_eq!(v["tasks"][1]["state"].as_str(), Some("cancelled"));
        assert_eq!(
            v["tasks"][1]["cells"][0]["state"].as_str(),
            Some("cancelled")
        );
        assert_ne!(v["tasks"][2]["state"].as_str(), Some("cancelled"));
        assert_eq!(v["tasks"][2]["cells"][0]["state"].as_str(), Some("running"));
        assert_ne!(v["state"].as_str(), Some("cancelled"));
    }

    #[test]
    fn set_status_task_proof_cancelled_cascades_only_to_downstream_branch() {
        let d = daemon();
        submit_stop_cascade_squad(&d);
        seed_task_proof_stop_states(&d);
        let v = set_status_and_parse(
            &d,
            serde_json::json!({
                "kind": "proof",
                "task_idx": 0,
                "cell_idx": -1,
                "proof_idx": 0,
                "proof_scope": "task",
                "state": "cancelled"
            }),
        );
        assert_eq!(v["tasks"][0]["state"].as_str(), Some("cancelled"));
        assert_eq!(
            v["tasks"][0]["cells"][0]["state"].as_str(),
            Some("done"),
            "task proof stop must not retroactively cancel upstream cells"
        );
        assert_eq!(
            v["tasks"][0]["cells"][0]["proof"][0]["state"].as_str(),
            Some("done")
        );
        assert_eq!(
            v["tasks"][0]["proof"][0]["state"].as_str(),
            Some("cancelled")
        );
        assert_eq!(
            v["tasks"][0]["proof"][1]["state"].as_str(),
            Some("cancelled")
        );
        assert_eq!(v["tasks"][1]["state"].as_str(), Some("cancelled"));
        assert_eq!(
            v["tasks"][1]["cells"][0]["state"].as_str(),
            Some("cancelled")
        );
        assert_ne!(v["tasks"][2]["state"].as_str(), Some("cancelled"));
        assert_eq!(v["tasks"][2]["cells"][0]["state"].as_str(), Some("running"));
        assert_ne!(v["state"].as_str(), Some("cancelled"));
    }

    // RAL-315: task-by-task cancellation via `set-status` (which the board's
    // "Stop" button also hits, since `stopNode` posts to this exact same
    // endpoint) must eventually propagate to the squad's own state, the same
    // way whole-squad `cancel_squad` and the scheduler's own end-of-dispatch
    // aggregation already do.

    #[test]
    fn set_status_task_cancel_one_at_a_time_transitions_squad_to_cancelled() {
        let d = daemon();
        route(&d, "POST", "/api/squads", &submit_body(TWO_TASKS));
        d.lock()
            .set_squad_state("squad-000000000001", SquadState::Running)
            .unwrap();

        let body =
            serde_json::json!({"kind": "task", "task_idx": 0, "state": "cancelled"}).to_string();
        let r = route(
            &d,
            "POST",
            "/api/squads/squad-000000000001/set-status",
            &body,
        );
        assert_eq!(r.status, 200, "body={}", r.body);
        let v: serde_json::Value = serde_json::from_str(&r.body).unwrap();
        assert_eq!(v["tasks"][0]["state"].as_str(), Some("cancelled"));
        assert_ne!(
            v["state"].as_str(),
            Some("cancelled"),
            "squad must stay alive while a sibling task is still pending"
        );

        let body =
            serde_json::json!({"kind": "task", "task_idx": 1, "state": "cancelled"}).to_string();
        let r = route(
            &d,
            "POST",
            "/api/squads/squad-000000000001/set-status",
            &body,
        );
        assert_eq!(r.status, 200, "body={}", r.body);
        let v: serde_json::Value = serde_json::from_str(&r.body).unwrap();
        assert_eq!(v["tasks"][1]["state"].as_str(), Some("cancelled"));
        assert_eq!(
            v["state"].as_str(),
            Some("cancelled"),
            "squad must transition to cancelled once every task has landed cancelled"
        );
    }

    #[test]
    fn set_status_cell_cancel_cascades_to_task_and_reconciles_squad() {
        // The board's per-cell "Stop" button hits the same `set-status`
        // endpoint with `kind: "cell"`; `apply_stop_cascade` cancels the
        // owning task too, which must be visible to the same reconciliation
        // that a direct `kind: "task"` cancel triggers.
        let d = daemon();
        route(&d, "POST", "/api/squads", &submit_body(TWO_TASKS));
        d.lock()
            .set_squad_state("squad-000000000001", SquadState::Running)
            .unwrap();
        d.lock()
            .set_task_state("squad-000000000001", 0, NodeState::Cancelled)
            .unwrap();

        let body = serde_json::json!({
            "kind": "cell", "task_idx": 1, "cell_idx": 0, "state": "cancelled"
        })
        .to_string();
        let r = route(
            &d,
            "POST",
            "/api/squads/squad-000000000001/set-status",
            &body,
        );
        assert_eq!(r.status, 200, "body={}", r.body);
        let v: serde_json::Value = serde_json::from_str(&r.body).unwrap();
        assert_eq!(v["tasks"][1]["state"].as_str(), Some("cancelled"));
        assert_eq!(
            v["state"].as_str(),
            Some("cancelled"),
            "cell-level stop cascading to its owning task must still reconcile the squad"
        );
    }

    #[test]
    fn set_status_task_cancel_with_sibling_still_running_does_not_cancel_squad() {
        let d = daemon();
        route(&d, "POST", "/api/squads", &submit_body(THREE_TASKS));
        d.lock()
            .set_squad_state("squad-000000000001", SquadState::Running)
            .unwrap();
        d.lock()
            .set_task_state("squad-000000000001", 0, NodeState::Done)
            .unwrap();
        d.lock()
            .set_task_state("squad-000000000001", 2, NodeState::Running)
            .unwrap();

        let body =
            serde_json::json!({"kind": "task", "task_idx": 1, "state": "cancelled"}).to_string();
        let r = route(
            &d,
            "POST",
            "/api/squads/squad-000000000001/set-status",
            &body,
        );
        assert_eq!(r.status, 200, "body={}", r.body);
        let v: serde_json::Value = serde_json::from_str(&r.body).unwrap();
        assert_eq!(v["tasks"][0]["state"].as_str(), Some("done"));
        assert_eq!(v["tasks"][1]["state"].as_str(), Some("cancelled"));
        assert_eq!(v["tasks"][2]["state"].as_str(), Some("running"));
        assert_ne!(
            v["state"].as_str(),
            Some("cancelled"),
            "a still-running sibling task must block the squad from being reported cancelled"
        );
    }

    #[test]
    fn set_status_task_cancel_with_sibling_failed_reports_squad_failed_not_cancelled() {
        let d = daemon();
        route(&d, "POST", "/api/squads", &submit_body(TWO_TASKS));
        d.lock()
            .set_squad_state("squad-000000000001", SquadState::Running)
            .unwrap();
        d.lock()
            .set_task_state("squad-000000000001", 0, NodeState::Failed)
            .unwrap();

        let body =
            serde_json::json!({"kind": "task", "task_idx": 1, "state": "cancelled"}).to_string();
        let r = route(
            &d,
            "POST",
            "/api/squads/squad-000000000001/set-status",
            &body,
        );
        assert_eq!(r.status, 200, "body={}", r.body);
        let v: serde_json::Value = serde_json::from_str(&r.body).unwrap();
        assert_eq!(v["tasks"][0]["state"].as_str(), Some("failed"));
        assert_eq!(v["tasks"][1]["state"].as_str(), Some("cancelled"));
        assert_eq!(
            v["state"].as_str(),
            Some("failed"),
            "a real failure must win over a sibling cancellation"
        );
    }

    #[test]
    fn set_status_bad_state_is_400() {
        let d = daemon();
        route(&d, "POST", "/api/squads", &submit_body(GOOD));
        let body = serde_json::json!({"kind": "squad", "state": "nope"}).to_string();
        let r = route(
            &d,
            "POST",
            "/api/squads/squad-000000000001/set-status",
            &body,
        );
        assert_eq!(r.status, 400);
    }

    #[test]
    fn set_status_bad_kind_is_400() {
        let d = daemon();
        route(&d, "POST", "/api/squads", &submit_body(GOOD));
        let body = serde_json::json!({"kind": "wat", "state": "done"}).to_string();
        let r = route(
            &d,
            "POST",
            "/api/squads/squad-000000000001/set-status",
            &body,
        );
        assert_eq!(r.status, 400);
    }

    fn status_req(kind: &str, task_idx: i64, cell_idx: i64, state: &str) -> SetStatusBody {
        SetStatusBody {
            kind: kind.to_string(),
            task_idx,
            cell_idx,
            proof_idx: 0,
            proof_scope: String::new(),
            state: state.to_string(),
        }
    }

    #[test]
    fn stop_targets_for_cell_kind_targets_that_exact_pane() {
        let d = daemon();
        route(&d, "POST", "/api/squads", &submit_body(GOOD));
        let squad_id = "squad-000000000001";
        let guard = d.lock();
        let req = status_req("cell", 0, 0, "done");
        let targets = stop_targets_for_status_change(&guard, squad_id, &req);
        let task = guard.get_task_name(squad_id, 0).unwrap();
        let sid = guard.get_cell_id(squad_id, 0, 0).unwrap();
        assert_eq!(
            targets,
            vec![(crate::tmux::session_name(squad_id, &task, &sid), 0)]
        );
    }

    #[test]
    fn stop_targets_for_task_kind_covers_every_cell_in_the_task() {
        let d = daemon();
        let toml = "[[task]]\nname=\"t\"\n\
            [[task.cell]]\nid=\"s1\"\ncwd=\"/r\"\nprompt=\"p\"\n\
            [[task.cell]]\nid=\"s2\"\ncwd=\"/r\"\nprompt=\"p\"\n";
        route(&d, "POST", "/api/squads", &submit_body(toml));
        let squad_id = "squad-000000000001";
        let guard = d.lock();
        let req = status_req("task", 0, 0, "failed");
        let targets = stop_targets_for_status_change(&guard, squad_id, &req);
        assert_eq!(
            targets,
            vec![
                (crate::tmux::session_name(squad_id, "t", "s1"), 0),
                (crate::tmux::session_name(squad_id, "t", "s2"), 1),
            ]
        );
    }

    #[test]
    fn stop_targets_for_cell_scope_proof_targets_the_proof_pane_but_the_cell_ghost() {
        let d = daemon();
        route(&d, "POST", "/api/squads", &submit_body(GOOD));
        let squad_id = "squad-000000000001";
        let guard = d.lock();
        let mut req = status_req("proof", 0, 0, "failed");
        req.proof_scope = "cell".to_string();
        req.proof_idx = 3;
        let targets = stop_targets_for_status_change(&guard, squad_id, &req);
        let task = guard.get_task_name(squad_id, 0).unwrap();
        assert_eq!(
            targets,
            vec![(
                crate::tmux::session_name(squad_id, &task, "proof-cell-3"),
                0
            )]
        );
    }

    #[test]
    fn stop_targets_for_task_scope_proof_attributes_ghost_to_the_first_cell() {
        let d = daemon();
        let toml = "[[task]]\nname=\"t\"\n\
            [[task.cell]]\nid=\"s1\"\ncwd=\"/r\"\nprompt=\"p\"\n\
            [[task.cell]]\nid=\"s2\"\ncwd=\"/r\"\nprompt=\"p\"\n";
        route(&d, "POST", "/api/squads", &submit_body(toml));
        let squad_id = "squad-000000000001";
        let guard = d.lock();
        let mut req = status_req("proof", 0, 0, "failed");
        req.proof_scope = "task".to_string();
        req.proof_idx = 1;
        let targets = stop_targets_for_status_change(&guard, squad_id, &req);
        assert_eq!(
            targets,
            // Pane is keyed by the task-scope proof's own name, but the
            // ghost is attributed to cell idx 0 (the task's first
            // cell) -- mirroring `Store::get_task_first_cell_cwd`.
            vec![(crate::tmux::session_name(squad_id, "t", "proof-task-1"), 0)]
        );
    }

    #[test]
    fn stop_targets_empty_for_unknown_task() {
        let d = daemon();
        route(&d, "POST", "/api/squads", &submit_body(GOOD));
        let squad_id = "squad-000000000001";
        let guard = d.lock();
        let req = status_req("cell", 99, 0, "done");
        assert_eq!(
            stop_targets_for_status_change(&guard, squad_id, &req),
            vec![]
        );
    }

    #[test]
    fn stop_targets_empty_for_squad_kind() {
        // "squad" isn't handled by capture_and_stop_node at all -- the squad-wide
        // cancel cascade (`kill_squad_tmux_sessions`) already covers it.
        let d = daemon();
        route(&d, "POST", "/api/squads", &submit_body(GOOD));
        let squad_id = "squad-000000000001";
        let guard = d.lock();
        let req = status_req("squad", 0, 0, "done");
        assert_eq!(
            stop_targets_for_status_change(&guard, squad_id, &req),
            vec![]
        );
    }

    fn tmux_available() -> bool {
        crate::tmux::Tmux::resolve().is_ok()
    }

    /// A one-task-one-cell TOML fixture like `GOOD`, but with a caller-
    /// chosen task name, so each live-tmux test below gets its own
    /// distinguishable task in the (already per-invocation-unique, via
    /// [`submit_unique_squad`]) `crate::tmux::session_name` it derives,
    /// instead of every test sharing `GOOD`'s fixed task name.
    fn good_with_task(task: &str) -> String {
        format!("[[task]]\nname=\"{task}\"\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\n")
    }

    fn parse(src: &str) -> ralphus_core::schema::TaskFile {
        toml::from_str(src).expect("valid toml")
    }

    /// Submit `good_with_task(task_label)` directly through the store
    /// (bypassing the HTTP `/api/squads` route, which always assigns the
    /// store's deterministic sequential id) with a per-invocation-unique
    /// squad_id instead (RAL-177) — see [`Store::insert_squad_with_id`]'s doc
    /// comment. A fresh in-memory `Store`'s first squad is always
    /// `squad-000000000001`, identical across every worktree's identical
    /// test; a live-tmux test below both builds a real tmux session name
    /// from this squad_id *and* can trigger a `ralphus_{squad_id}_`-prefix-
    /// scoped kill (`kill_squad_tmux_sessions`/`kill_guardian_tmux_sessions`)
    /// against the same shared, machine-wide psmux server, so a deterministic
    /// literal here is a collision risk on both counts, not just the exact
    /// cell name. Returns the squad_id actually used.
    fn submit_unique_squad(d: &Daemon, task_label: &str) -> String {
        let squad_id = crate::tmux::unique_test_tag(task_label);
        d.lock()
            .insert_squad_with_id(&squad_id, &parse(&good_with_task(task_label)), None, false)
            .unwrap();
        squad_id
    }

    /// Create a real, detached tmux session named for `(squad_id, task,
    /// cell_id)` that echoes `marker` into its pane, then polls until the
    /// echo actually shows up in `capture-pane` — mirrors
    /// `tmux.rs::live_tmux_new_session_capture_and_kill_roundtrip`'s idiom, so
    /// the RAL-163 tests below have a real, non-racy pane to capture from.
    fn spawn_marker_session(name: &str, marker: &str) -> crate::tmux::Tmux {
        let tmux = crate::tmux::Tmux::resolve().unwrap();
        let _ = tmux.kill_session(name);
        let cwd = std::env::temp_dir().to_string_lossy().into_owned();
        // A bare `echo` prints and exits almost immediately, which races the
        // test's own HTTP-triggered capture: real tmux repaints a dead pane
        // with a "Pane is dead (status N, <date>)" banner shortly after its
        // command exits, and on a loaded CI runner that repaint can beat the
        // capture to the punch and erase the marker before anyone reads it.
        // Every caller of this helper is specifically testing "while the
        // agent is still running" behavior, so the marker process must
        // actually still be alive for the whole test, not just have
        // recently printed something -- sleeping well past any test's own
        // runtime removes the race outright instead of narrowing it.
        tmux.new_detached_session_with_command(
            name,
            &cwd,
            &std::collections::BTreeMap::new(),
            "sh",
            &["-c".to_string(), format!("echo {marker}; sleep 60")],
        )
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

    #[cfg_attr(windows, ignore = "CI-only on Windows: exercises a real psmux server")]
    #[test]
    fn live_tmux_set_status_cell_captures_pane_into_ghost_and_kills_it() {
        // RAL-163: setting a cell to a terminal status while its agent is
        // still running must capture the pane's in-progress output into that
        // cell's ghost, then stop (kill) the pane -- turning the manual
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
        let squad_id = submit_unique_squad(&d, "ral163-captures");

        let (task, sid) = {
            let guard = d.lock();
            (
                guard.get_task_name(&squad_id, 0).unwrap(),
                guard.get_cell_id(&squad_id, 0, 0).unwrap(),
            )
        };
        let name = crate::tmux::session_name(&squad_id, &task, &sid);
        let marker = "ral-163-in-progress-marker";
        spawn_marker_session(&name, marker);

        let body = serde_json::json!({
            "kind": "cell", "task_idx": 0, "cell_idx": 0, "state": "done"
        })
        .to_string();
        let r = route(
            &d,
            "POST",
            &format!("/api/squads/{squad_id}/set-status"),
            &body,
        );
        assert_eq!(r.status, 200, "body={}", r.body);
        let v: serde_json::Value = serde_json::from_str(&r.body).unwrap();
        assert_eq!(
            v["tasks"][0]["cells"][0]["state"].as_str(),
            Some("done"),
            "the manually-set status must be applied: {}",
            r.body
        );

        let tmux = crate::tmux::Tmux::resolve().unwrap();
        assert!(
            !tmux.has_session(&name),
            "the agent's tmux pane must be stopped"
        );

        let uri = crate::ghost::cell_uri(&squad_id, 0, 0);
        let ghost = d.lock().get_ghost(&uri).unwrap();
        let ghost = ghost.expect("captured pane output must be saved to the cell's ghost");
        assert!(
            ghost.content.contains(marker),
            "ghost must contain the captured in-progress output: {}",
            ghost.content
        );
    }

    #[cfg_attr(windows, ignore = "CI-only on Windows: exercises a real psmux server")]
    #[test]
    fn live_tmux_set_status_pending_does_not_stop_a_running_agent() {
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
        let squad_id = submit_unique_squad(&d, "ral163-pending");

        let (task, sid) = {
            let guard = d.lock();
            (
                guard.get_task_name(&squad_id, 0).unwrap(),
                guard.get_cell_id(&squad_id, 0, 0).unwrap(),
            )
        };
        let name = crate::tmux::session_name(&squad_id, &task, &sid);
        let marker = "ral-163-pending-marker";
        let tmux = spawn_marker_session(&name, marker);
        let _cleanup = crate::tmux::KillSessionOnDrop(name.clone());

        let body = serde_json::json!({
            "kind": "cell", "task_idx": 0, "cell_idx": 0, "state": "pending"
        })
        .to_string();
        let r = route(
            &d,
            "POST",
            &format!("/api/squads/{squad_id}/set-status"),
            &body,
        );
        assert_eq!(r.status, 200, "body={}", r.body);

        assert!(
            tmux.has_session(&name),
            "setting status to pending must not stop a running agent"
        );
        let uri = crate::ghost::cell_uri(&squad_id, 0, 0);
        assert!(
            d.lock().get_ghost(&uri).unwrap().is_none(),
            "nothing should have been captured for a pending status change"
        );

        tmux.kill_session(&name).unwrap();
    }

    #[cfg_attr(windows, ignore = "CI-only on Windows: exercises a real psmux server")]
    #[test]
    fn live_tmux_set_status_ignored_still_captures_and_stops() {
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
        let squad_id = submit_unique_squad(&d, "ral163-ignored");

        let (task, sid) = {
            let guard = d.lock();
            (
                guard.get_task_name(&squad_id, 0).unwrap(),
                guard.get_cell_id(&squad_id, 0, 0).unwrap(),
            )
        };
        let name = crate::tmux::session_name(&squad_id, &task, &sid);
        let marker = "ral-163-ignored-marker";
        spawn_marker_session(&name, marker);

        let body = serde_json::json!({
            "kind": "cell", "task_idx": 0, "cell_idx": 0, "state": "ignored"
        })
        .to_string();
        let r = route(
            &d,
            "POST",
            &format!("/api/squads/{squad_id}/set-status"),
            &body,
        );
        assert_eq!(r.status, 200, "body={}", r.body);

        let tmux = crate::tmux::Tmux::resolve().unwrap();
        assert!(
            !tmux.has_session(&name),
            "ignored must still stop the running agent"
        );
        let uri = crate::ghost::cell_uri(&squad_id, 0, 0);
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
    fn restart_squad_route_returns_dirtied_and_is_pending() {
        let d = daemon();
        let r = route(&d, "POST", "/api/squads", &submit_body(GOOD));
        assert_eq!(r.status, 201);
        // First submitted squad has the deterministic id squad-000000000001.
        let rr = route(&d, "POST", "/api/squads/squad-000000000001/restart", "");
        assert_eq!(rr.status, 200);
        assert!(rr.body.contains("\"state\":\"pending\""));
        assert!(rr.body.contains("\"dirtied\":"));
    }

    /// RAL-174: a restart note in the body must actually attach without
    /// hanging. Regression test for a `Mutex` self-deadlock: matching
    /// directly on `daemon.lock().restart_squad(id)` keeps that `MutexGuard`
    /// alive for the whole arm (Rust extends a match scrutinee's temporaries
    /// to the arm body), so writing the note via a second `daemon.lock()`
    /// call inside that arm deadlocked whenever a note was actually present
    /// — every route test using an empty `""` body never exercised this path.
    #[test]
    fn restart_squad_route_with_note_attaches_ghost_without_hanging() {
        let d = daemon();
        route(&d, "POST", "/api/squads", &submit_body(GOOD));
        let body = serde_json::json!({"note": "you were stopped midway through the migration"})
            .to_string();
        let r = route(&d, "POST", "/api/squads/squad-000000000001/restart", &body);
        assert_eq!(r.status, 200);
        let ghost = d
            .lock()
            .get_ghost(&crate::ghost::cell_uri("squad-000000000001", 0, 0))
            .unwrap()
            .unwrap();
        assert_eq!(
            ghost.user_note.as_deref(),
            Some("you were stopped midway through the migration")
        );
    }

    #[test]
    fn restart_squad_missing_is_404() {
        let d = daemon();
        assert_eq!(
            route(&d, "POST", "/api/squads/squad-999/restart", "").status,
            404
        );
    }

    #[test]
    fn restart_cell_bad_index_is_400() {
        let d = daemon();
        let r = route(&d, "POST", "/api/squads/squad-1/cells/x/y/restart", "");
        assert_eq!(r.status, 400);
    }

    /// RAL-174 deadlock regression (see `restart_squad_route_with_note_attaches_ghost_without_hanging`).
    #[test]
    fn restart_cell_route_with_note_attaches_ghost_without_hanging() {
        let d = daemon();
        route(&d, "POST", "/api/squads", &submit_body(GOOD));
        let body = serde_json::json!({"note": "picking up where you left off"}).to_string();
        let r = route(
            &d,
            "POST",
            "/api/squads/squad-000000000001/cells/0/0/restart",
            &body,
        );
        assert_eq!(r.status, 200);
        let ghost = d
            .lock()
            .get_ghost(&crate::ghost::cell_uri("squad-000000000001", 0, 0))
            .unwrap()
            .unwrap();
        assert_eq!(
            ghost.user_note.as_deref(),
            Some("picking up where you left off")
        );
    }

    /// RAL-1xx regression: `restart_cell`/`restart_cell_proof`/
    /// `restart_task_proof` used to cancel a squad's *entire* worker thread
    /// unconditionally, even when the restart target had already finished.
    /// A squad's worker drives every one of that squad's independent cells
    /// concurrently over one shared `CancelToken` (see
    /// `scheduler::execute_squad_inner`), so restarting one already-terminal
    /// (failed) cell collaterally killed every other still-running,
    /// unrelated sibling cell in the same squad — observed in production as
    /// several healthy tasks in a batch squad dying the instant a different,
    /// already-failed task was restarted, despite having no `depends_on`
    /// relationship to it. Task "a" fails immediately here; task "b" is still
    /// genuinely in flight (blocked in `run_cancellable`, standing in for a
    /// live claude-code cell) when "a" is restarted. Before the fix, "b"
    /// observed `cancel.is_cancelled() == true` and aborted; after the fix,
    /// restarting "a" (already terminal) never touches the shared token at
    /// all, so "b" runs to completion undisturbed.
    #[test]
    fn restart_cell_does_not_cancel_unrelated_sibling_cell_in_same_squad() {
        use crate::cancel::CancelToken;
        use crate::runner::{RunnerResult, RunnerSpec};
        use std::sync::atomic::{AtomicBool, Ordering};

        const TWO_INDEPENDENT_TASKS: &str = "[[task]]\nname=\"a\"\n[[task.cell]]\ncwd=\".\"\ncommand=\"x\"\n[[task]]\nname=\"b\"\n[[task.cell]]\ncwd=\".\"\ncommand=\"y\"\n";

        /// Task "a" fails immediately (so its cell is already terminal by
        /// the time the test restarts it); task "b" blocks in
        /// `run_cancellable`, polling `cancel` like a real subprocess-backed
        /// cell would, so the test can observe whether it ever gets
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
                    cache_creation_tokens: 0,
                    cache_read_tokens: 0,
                    compaction_input_tokens: 0,
                    compaction_count: 0,
                    cost_usd: 0.0,
                    cost_is_estimated: false,
                    summary: "b finished undisturbed".to_string(),
                    error: None,
                    proofed: None,
                    agent_session_id: None,
                    ghost: None,
                }
            }
        }

        let d = daemon();
        let r = route(
            &d,
            "POST",
            "/api/squads",
            &submit_body(TWO_INDEPENDENT_TASKS),
        );
        assert_eq!(r.status, 201);
        let squad_id = "squad-000000000001".to_string();

        let b_started = Arc::new(AtomicBool::new(false));
        let b_cancelled = Arc::new(AtomicBool::new(false));
        let runner: Arc<dyn Runner> = Arc::new(TwoTaskRunner {
            b_started: Arc::clone(&b_started),
            b_cancelled: Arc::clone(&b_cancelled),
        });

        let store = d.store_handle();
        let cancellations = d.cancellations_handle();
        let worker = {
            let (store, runner, cancellations, squad_id) = (
                Arc::clone(&store),
                Arc::clone(&runner),
                cancellations.clone(),
                squad_id.clone(),
            );
            std::thread::spawn(move || {
                // Mirrors what `scheduler::tick` does for a claimed squad: register
                // a token in the shared registry before executing, remove it after.
                let token = cancellations.register(&squad_id);
                crate::scheduler::execute_squad_with(
                    &store,
                    runner.as_ref(),
                    &squad_id,
                    &token,
                    &cancellations,
                );
                cancellations.remove(&squad_id);
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
                d.lock().cell_state(&squad_id, 0, 0),
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

        // Restart task "a"'s already-failed cell while "b" is still running.
        let rr = route(
            &d,
            "POST",
            &format!("/api/squads/{squad_id}/cells/0/0/restart"),
            "",
        );
        assert_eq!(rr.status, 200);

        worker.join().unwrap();

        assert!(
            !b_cancelled.load(Ordering::SeqCst),
            "restarting task a's already-terminal cell must not cancel \
             task b's still-running, unrelated sibling cell"
        );
    }

    /// RAL-288 regression, the inverse mistake from the test above: caught
    /// live (RAL-239 verification pass) when a human's manual "Restart
    /// cell" on a cell whose *own* body had already reached `Done` --
    /// while that exact cell's own cell-scoped proof step was still being
    /// actively driven by the squad's live worker -- silently skipped
    /// cancelling that worker entirely. The old check inferred "no live
    /// worker to disturb" purely from the target cell's own `NodeState`
    /// (`!= Running` -> skip), which doesn't hold once a squad's worker can
    /// legitimately still be busy on that same cell's proof steps (or
    /// anything else in the squad) after the cell body itself finishes.
    /// `Store::restart_cell` then reset the cell to `pending` underneath
    /// the still-running, now-orphaned worker, which never noticed and
    /// never released its slot in the shared cancellation registry --
    /// permanently blocking every future re-claim of the squad. Simulates
    /// the same shape without needing real cell-then-proof sequencing: the
    /// cell's own row is manually forced to `Done` while the squad's
    /// worker (registered under the same squad id) is still genuinely
    /// blocked inside it, then asserts the restart route still cancels it.
    #[test]
    fn restart_cell_still_cancels_the_squads_worker_even_when_the_target_cells_own_row_already_reads_done()
     {
        use crate::cancel::CancelToken;
        use crate::runner::{RunnerResult, RunnerSpec};
        use std::sync::atomic::{AtomicBool, Ordering};

        const ONE_CELL: &str = "[[task]]\nname=\"t\"\n[[task.cell]]\ncwd=\".\"\ncommand=\"x\"\n\
            [[task.cell.proof]]\ncommand=\"check\"\n";

        /// Blocks in `run_cancellable`, polling `cancel` like a real
        /// subprocess-backed cell would, so the test can observe whether
        /// the restart route actually cancels the still-live worker.
        struct BlockingRunner {
            started: Arc<AtomicBool>,
            observed_cancel: Arc<AtomicBool>,
        }

        impl Runner for BlockingRunner {
            fn run(&self, _spec: &RunnerSpec) -> RunnerResult {
                RunnerResult::failure("unused")
            }
            fn run_cancellable(&self, _spec: &RunnerSpec, cancel: &CancelToken) -> RunnerResult {
                self.started.store(true, Ordering::SeqCst);
                for _ in 0..1000 {
                    if cancel.is_cancelled() {
                        self.observed_cancel.store(true, Ordering::SeqCst);
                        return RunnerResult::failure("cancelled");
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                RunnerResult {
                    status: "done".to_string(),
                    tokens_in: 1,
                    tokens_out: 1,
                    cache_creation_tokens: 0,
                    cache_read_tokens: 0,
                    compaction_input_tokens: 0,
                    compaction_count: 0,
                    cost_usd: 0.0,
                    cost_is_estimated: false,
                    summary: "never cancelled".to_string(),
                    error: None,
                    proofed: None,
                    agent_session_id: None,
                    ghost: None,
                }
            }
        }

        let d = daemon();
        let r = route(&d, "POST", "/api/squads", &submit_body(ONE_CELL));
        assert_eq!(r.status, 201);
        let squad_id = "squad-000000000001".to_string();

        let started = Arc::new(AtomicBool::new(false));
        let observed_cancel = Arc::new(AtomicBool::new(false));
        let runner: Arc<dyn Runner> = Arc::new(BlockingRunner {
            started: Arc::clone(&started),
            observed_cancel: Arc::clone(&observed_cancel),
        });

        let store = d.store_handle();
        let cancellations = d.cancellations_handle();
        let worker = {
            let (store, runner, cancellations, squad_id) = (
                Arc::clone(&store),
                Arc::clone(&runner),
                cancellations.clone(),
                squad_id.clone(),
            );
            std::thread::spawn(move || {
                let token = cancellations.register(&squad_id);
                crate::scheduler::execute_squad_with(
                    &store,
                    runner.as_ref(),
                    &squad_id,
                    &token,
                    &cancellations,
                );
                cancellations.remove(&squad_id);
            })
        };

        while !started.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(2));
        }

        // The exact blind spot: force the target cell's own row to `Done`
        // and its cell-scoped proof step's row to `Running` -- standing in
        // for "the cell body finished but its own proof step, driven by
        // the same still-blocked worker, is still running" -- which the
        // old cell-state-only heuristic misread as safe to skip
        // cancellation for entirely.
        {
            let guard = d.lock();
            guard
                .set_cell_state(&squad_id, 0, 0, crate::store::NodeState::Done)
                .unwrap();
            guard
                .set_proof_state(&squad_id, 0, "cell", 0, 0, crate::store::NodeState::Running)
                .unwrap();
        }

        let rr = route(
            &d,
            "POST",
            &format!("/api/squads/{squad_id}/cells/0/0/restart"),
            "",
        );
        assert_eq!(rr.status, 200);

        worker.join().unwrap();

        assert!(
            observed_cancel.load(Ordering::SeqCst),
            "restarting a cell must still cancel the squad's own live \
             worker even when the target cell's own row already reads a \
             non-Running state"
        );
    }

    #[test]
    fn add_dependency_route_appends_and_gates_readiness() {
        let d = daemon();
        route(&d, "POST", "/api/squads", &submit_body(GOOD)); // squad-000000000001
        route(&d, "POST", "/api/squads", &submit_body(GOOD)); // squad-000000000002
        let body = serde_json::json!({"target_id": "squad-000000000001"}).to_string();
        let r = route(
            &d,
            "POST",
            "/api/squads/squad-000000000002/add-dependency",
            &body,
        );
        assert_eq!(r.status, 200);
        assert!(r.body.contains("squad-000000000002"));

        let ready = route(&d, "GET", "/api/graph", "");
        assert_eq!(ready.status, 200);
        assert!(ready.body.contains("squad-000000000001"));
    }

    #[test]
    fn add_dependency_route_rejects_self_reference() {
        let d = daemon();
        route(&d, "POST", "/api/squads", &submit_body(GOOD));
        let body = serde_json::json!({"target_id": "squad-000000000001"}).to_string();
        let r = route(
            &d,
            "POST",
            "/api/squads/squad-000000000001/add-dependency",
            &body,
        );
        assert_eq!(r.status, 409);
    }

    #[test]
    fn add_dependency_route_rejects_cycle() {
        let d = daemon();
        route(&d, "POST", "/api/squads", &submit_body(GOOD)); // squad-1
        route(&d, "POST", "/api/squads", &submit_body(GOOD)); // squad-2
        let body = serde_json::json!({"target_id": "squad-000000000001"}).to_string();
        route(
            &d,
            "POST",
            "/api/squads/squad-000000000002/add-dependency",
            &body,
        );
        let reverse = serde_json::json!({"target_id": "squad-000000000002"}).to_string();
        let r = route(
            &d,
            "POST",
            "/api/squads/squad-000000000001/add-dependency",
            &reverse,
        );
        assert_eq!(r.status, 409);
    }

    #[test]
    fn add_dependency_route_missing_target_is_404() {
        let d = daemon();
        route(&d, "POST", "/api/squads", &submit_body(GOOD));
        let body = serde_json::json!({"target_id": "squad-999"}).to_string();
        let r = route(
            &d,
            "POST",
            "/api/squads/squad-000000000001/add-dependency",
            &body,
        );
        assert_eq!(r.status, 404);
    }

    #[test]
    fn add_dependency_route_bad_body_is_400() {
        let d = daemon();
        route(&d, "POST", "/api/squads", &submit_body(GOOD));
        let r = route(
            &d,
            "POST",
            "/api/squads/squad-000000000001/add-dependency",
            "not json",
        );
        assert_eq!(r.status, 400);
    }

    #[test]
    fn restart_squad_preview_reports_impact_without_mutating() {
        // RAL-104: the dry-run preview must report the same cells/tasks a
        // real restart would touch, and must leave the squad's state untouched.
        let d = daemon();
        route(&d, "POST", "/api/squads", &submit_body(GOOD));
        let r = route(
            &d,
            "POST",
            "/api/squads/squad-000000000001/restart/preview",
            "",
        );
        assert_eq!(r.status, 200);
        let body: serde_json::Value = serde_json::from_str(&r.body).unwrap();
        assert_eq!(body["cells"].as_array().unwrap().len(), 1);
        assert_eq!(body["tasks"].as_array().unwrap().len(), 1);
        assert_eq!(body["cells"][0]["task_name"], "t");

        // Previewing did not mutate anything: the squad is still fresh/pending,
        // not reset via the restart path.
        let squad = route(&d, "GET", "/api/squads/squad-000000000001", "");
        let squad: serde_json::Value = serde_json::from_str(&squad.body).unwrap();
        assert_eq!(squad["state"], "pending");
    }

    #[test]
    fn restart_squad_preview_missing_is_404() {
        let d = daemon();
        assert_eq!(
            route(&d, "POST", "/api/squads/squad-999/restart/preview", "").status,
            404
        );
    }

    #[test]
    fn restart_cell_preview_reports_impact_without_mutating() {
        let d = daemon();
        route(&d, "POST", "/api/squads", &submit_body(GOOD));
        let r = route(
            &d,
            "POST",
            "/api/squads/squad-000000000001/cells/0/0/restart/preview",
            "",
        );
        assert_eq!(r.status, 200);
        let body: serde_json::Value = serde_json::from_str(&r.body).unwrap();
        assert_eq!(body["cells"].as_array().unwrap().len(), 1);
        assert_eq!(body["cells"][0]["task_idx"], 0);
        assert_eq!(body["cells"][0]["idx"], 0);
    }

    #[test]
    fn restart_cell_preview_bad_index_is_400() {
        let d = daemon();
        let r = route(
            &d,
            "POST",
            "/api/squads/squad-1/cells/x/y/restart/preview",
            "",
        );
        assert_eq!(r.status, 400);
    }

    #[test]
    fn restart_cell_preview_missing_is_404() {
        let d = daemon();
        route(&d, "POST", "/api/squads", &submit_body(GOOD));
        let r = route(
            &d,
            "POST",
            "/api/squads/squad-000000000001/cells/9/9/restart/preview",
            "",
        );
        assert_eq!(r.status, 404);
    }

    #[test]
    fn clear_all_wipes_squads_and_resets_ids() {
        let d = daemon();
        route(&d, "POST", "/api/squads", &submit_body(GOOD));
        route(&d, "POST", "/api/squads", &submit_body(GOOD));
        let r = route(&d, "POST", "/api/clear", "");
        assert_eq!(r.status, 200);
        assert!(r.body.contains("\"squads_deleted\":2"));
        // Both squads are gone.
        assert_eq!(
            route(&d, "GET", "/api/squads/squad-000000000001", "").status,
            404
        );
        // Id sequence reset: the next submitted squad is squad-...001 again.
        let again = route(&d, "POST", "/api/squads", &submit_body(GOOD));
        assert!(again.body.contains("squad-000000000001"));
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
    fn clear_with_status_filter_keeps_unmatched_squads() {
        let d = daemon();
        route(&d, "POST", "/api/squads", &submit_body(GOOD));
        // Submitted squads are Pending; filtering on `done` matches nothing.
        let body = serde_json::json!({"states": ["done"]}).to_string();
        let r = route(&d, "POST", "/api/clear", &body);
        assert_eq!(r.status, 200);
        assert!(r.body.contains("\"squads_deleted\":0"));
        assert_eq!(
            route(&d, "GET", "/api/squads/squad-000000000001", "").status,
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
    fn guardian_branch_messages_endpoint_starts_empty() {
        let d = daemon();
        let body =
            serde_json::json!({"name":"r","base_branch":"main","git_root":"/repo"}).to_string();
        route(&d, "POST", "/api/guardians", &body);
        let r = route(
            &d,
            "GET",
            "/api/guardians/guardian-000000000001/branches/branch-x/messages",
            "",
        );
        assert_eq!(r.status, 200);
        assert!(r.body.contains("\"messages\":[]"));
    }

    #[test]
    fn guardian_branch_messages_endpoint_is_scoped_per_branch() {
        // RAL-272: a branch's own thread only contains messages posted with
        // that branch_id -- an unscoped message never leaks in, and one
        // branch never sees another's feedback.
        let d = daemon();
        let body =
            serde_json::json!({"name":"r","base_branch":"main","git_root":"/repo"}).to_string();
        route(&d, "POST", "/api/guardians", &body);
        let gid = "guardian-000000000001";
        d.lock().add_guardian_branch(gid, "feature/a").unwrap();
        let branch_id = d.lock().get_guardian(gid).unwrap().branches[0].id.clone();
        d.lock()
            .add_guardian_message(gid, "reviewer", "unscoped msg", None, None, None, None)
            .unwrap();
        d.lock()
            .add_guardian_message(
                gid,
                "reviewer",
                "branch feedback",
                None,
                Some(&branch_id),
                None,
                None,
            )
            .unwrap();
        d.lock()
            .add_guardian_message(
                gid,
                "guardian",
                "branch reply",
                None,
                Some(&branch_id),
                None,
                None,
            )
            .unwrap();

        let r = route(
            &d,
            "GET",
            &format!("/api/guardians/{gid}/branches/{branch_id}/messages"),
            "",
        );
        assert_eq!(r.status, 200);
        assert!(r.body.contains("branch feedback"));
        assert!(r.body.contains("branch reply"));
        assert!(!r.body.contains("unscoped msg"));
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
    fn feedback_author_defaults_to_submitter_when_absent() {
        // RAL-379 Q2: with no `author` in the request body, the attributed
        // author defaults to the resolved authenticated submitter.
        let d = daemon();
        d.lock().create_user("bob").unwrap();
        let body =
            serde_json::json!({"name":"r","base_branch":"main","git_root":"/repo"}).to_string();
        route(&d, "POST", "/api/guardians", &body);
        let gid = "guardian-000000000001";
        d.lock().add_guardian_branch(gid, "feature/a").unwrap();
        let branch_id = d.lock().get_guardian(gid).unwrap().branches[0].id.clone();
        // `start_feedback` requires the branch to already have a review
        // worktree; stub one in directly rather than running a real merge.
        d.lock()
            .set_branch_review(gid, &branch_id, "feature/a", "/tmp/wt")
            .unwrap();
        let r = route_for_user(
            &d,
            "POST",
            &format!("/api/guardians/{gid}/branches/{branch_id}/feedback"),
            "{\"feedback\":\"please fix\"}",
            Some("bob"),
        );
        assert_eq!(r.status, 202);
        let msgs = d.lock().guardian_branch_messages(gid, &branch_id).unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].author.as_deref(), Some("bob"));
        assert_eq!(msgs[0].submitted_by.as_deref(), Some("bob"));
    }

    #[test]
    fn feedback_author_can_differ_from_submitter_and_submitted_by_cannot_be_overridden() {
        // RAL-379 Q2/Q3: a caller can attribute feedback to a different
        // registered user than the one submitting it, but the submitter is
        // always the authenticated/default requester -- a `submitted_by` in
        // the request body is silently ignored, never trusted.
        let d = daemon();
        d.lock().create_user("alice").unwrap();
        d.lock().create_user("bob").unwrap();
        let body =
            serde_json::json!({"name":"r","base_branch":"main","git_root":"/repo"}).to_string();
        route(&d, "POST", "/api/guardians", &body);
        let gid = "guardian-000000000001";
        d.lock().add_guardian_branch(gid, "feature/a").unwrap();
        let branch_id = d.lock().get_guardian(gid).unwrap().branches[0].id.clone();
        d.lock()
            .set_branch_review(gid, &branch_id, "feature/a", "/tmp/wt")
            .unwrap();
        let r = route_for_user(
            &d,
            "POST",
            &format!("/api/guardians/{gid}/branches/{branch_id}/feedback"),
            "{\"feedback\":\"please fix\",\"author\":\"alice\",\"submitted_by\":\"eve\"}",
            Some("bob"),
        );
        assert_eq!(r.status, 202);
        let msgs = d.lock().guardian_branch_messages(gid, &branch_id).unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].author.as_deref(), Some("alice"));
        assert_eq!(msgs[0].submitted_by.as_deref(), Some("bob"));
    }

    #[test]
    fn feedback_message_is_received_immediately_and_exposed_via_the_messages_api() {
        // RAL-380: the board reads completion state from this same endpoint --
        // the reviewer message must carry `action_status` the moment the
        // `202` response comes back, before the background resolver-agent
        // pass even starts.
        let d = daemon();
        let body =
            serde_json::json!({"name":"r","base_branch":"main","git_root":"/repo"}).to_string();
        route(&d, "POST", "/api/guardians", &body);
        let gid = "guardian-000000000001";
        d.lock().add_guardian_branch(gid, "feature/a").unwrap();
        let branch_id = d.lock().get_guardian(gid).unwrap().branches[0].id.clone();
        d.lock()
            .set_branch_review(gid, &branch_id, "feature/a", "/tmp/wt")
            .unwrap();
        let r = route(
            &d,
            "POST",
            &format!("/api/guardians/{gid}/branches/{branch_id}/feedback"),
            "{\"feedback\":\"please fix\"}",
        );
        assert_eq!(r.status, 202);

        let r = route(
            &d,
            "GET",
            &format!("/api/guardians/{gid}/branches/{branch_id}/messages"),
            "",
        );
        assert_eq!(r.status, 200);
        assert!(r.body.contains("\"action_status\":\"received\""));
    }

    #[test]
    fn feedback_rejects_an_unregistered_author() {
        let d = daemon();
        let body =
            serde_json::json!({"name":"r","base_branch":"main","git_root":"/repo"}).to_string();
        route(&d, "POST", "/api/guardians", &body);
        let gid = "guardian-000000000001";
        d.lock().add_guardian_branch(gid, "feature/a").unwrap();
        let branch_id = d.lock().get_guardian(gid).unwrap().branches[0].id.clone();
        let r = route(
            &d,
            "POST",
            &format!("/api/guardians/{gid}/branches/{branch_id}/feedback"),
            "{\"feedback\":\"please fix\",\"author\":\"nobody\"}",
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
    fn user_registry_rename_and_delete_with_spaces_in_the_name() {
        let d = daemon();
        let create = route(&d, "POST", "/api/users", "{\"name\":\"Colin Kennedy\"}");
        assert_eq!(create.status, 200);
        let list = route(&d, "GET", "/api/users", "");
        assert!(list.body.contains("Colin Kennedy"));

        // rename, with the old name percent-encoded in the path the way the
        // board's `fetch` calls encode it
        let rn = route(
            &d,
            "POST",
            "/api/users/Colin%20Kennedy/rename",
            "{\"name\":\"Colin K.\"}",
        );
        assert_eq!(rn.status, 200, "{}", rn.body);
        assert!(rn.body.contains("\"name\":\"Colin K.\""));

        // renaming onto an existing user is a conflict, not a crash
        route(&d, "POST", "/api/users", "{\"name\":\"taken\"}");
        let clash = route(
            &d,
            "POST",
            "/api/users/Colin%20K./rename",
            "{\"name\":\"taken\"}",
        );
        assert_eq!(clash.status, 409, "{}", clash.body);

        // RAL-? regression: a percent-encoded space in the DELETE path used
        // to be looked up verbatim (including the "%20"), so removing a user
        // whose name has a space in it always 404'd.
        let del = route(&d, "DELETE", "/api/users/Colin%20K.", "");
        assert_eq!(del.status, 200, "{}", del.body);
        let list_after = route(&d, "GET", "/api/users", "");
        assert!(!list_after.body.contains("Colin K."));
    }

    #[test]
    fn deleting_a_guardian_deletes_its_terminal_logs() {
        let d = daemon();
        let _troot = isolated_terminal_root();
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
    fn delete_missing_guardian_is_404() {
        let d = daemon();
        assert_eq!(
            route(&d, "DELETE", "/api/guardians/guardian-000000000009", "").status,
            404
        );
    }

    #[test]
    fn resources_endpoint_is_empty_without_running_cells() {
        let d = daemon();
        route(&d, "POST", "/api/squads", &submit_body(GOOD));
        // Submitted squad is Pending and no subprocess is tracked, so nothing is
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
    fn restart_cell_proof_route_returns_pending() {
        let d = daemon();
        route(&d, "POST", "/api/squads", &submit_body(GOOD));
        let r = route(
            &d,
            "POST",
            "/api/squads/squad-000000000001/cells/0/0/proof/0/restart",
            "",
        );
        assert_eq!(r.status, 200);
        assert!(r.body.contains("\"state\":\"pending\""));
    }

    /// RAL-174 deadlock regression (see `restart_squad_route_with_note_attaches_ghost_without_hanging`).
    #[test]
    fn restart_cell_proof_route_with_note_attaches_ghost_without_hanging() {
        let d = daemon();
        route(&d, "POST", "/api/squads", &submit_body(GOOD));
        let body = serde_json::json!({"note": "proof was flaking on the last attempt"}).to_string();
        let r = route(
            &d,
            "POST",
            "/api/squads/squad-000000000001/cells/0/0/proof/0/restart",
            &body,
        );
        assert_eq!(r.status, 200);
        let ghost = d
            .lock()
            .get_ghost(&crate::ghost::cell_uri("squad-000000000001", 0, 0))
            .unwrap()
            .unwrap();
        assert_eq!(
            ghost.user_note.as_deref(),
            Some("proof was flaking on the last attempt")
        );
    }

    #[test]
    fn restart_cell_proof_bad_index_is_400() {
        let d = daemon();
        let r = route(
            &d,
            "POST",
            "/api/squads/squad-1/cells/x/y/proof/z/restart",
            "",
        );
        assert_eq!(r.status, 400);
    }

    #[test]
    fn restart_task_proof_route_returns_pending() {
        let d = daemon();
        route(&d, "POST", "/api/squads", &submit_body(GOOD));
        let r = route(
            &d,
            "POST",
            "/api/squads/squad-000000000001/tasks/0/proof/0/restart",
            "",
        );
        assert_eq!(r.status, 200);
        assert!(r.body.contains("\"state\":\"pending\""));
    }

    /// RAL-174 deadlock regression (see `restart_squad_route_with_note_attaches_ghost_without_hanging`).
    /// This is the endpoint that actually hung during development: unlike the
    /// other four, its handler unconditionally re-locks the store (via
    /// `cells_of`) inside the match arm to compute the task's owned
    /// cells, so it deadlocked even before reaching `apply_restart_note`.
    #[test]
    fn restart_task_proof_route_with_note_attaches_ghost_without_hanging() {
        let d = daemon();
        route(&d, "POST", "/api/squads", &submit_body(GOOD));
        let body = serde_json::json!({"note": "task proof needs a longer timeout"}).to_string();
        let r = route(
            &d,
            "POST",
            "/api/squads/squad-000000000001/tasks/0/proof/0/restart",
            &body,
        );
        assert_eq!(r.status, 200);
        let ghost = d
            .lock()
            .get_ghost(&crate::ghost::cell_uri("squad-000000000001", 0, 0))
            .unwrap()
            .unwrap();
        assert_eq!(
            ghost.user_note.as_deref(),
            Some("task proof needs a longer timeout")
        );
    }

    #[test]
    fn restart_task_proof_bad_index_is_400() {
        let d = daemon();
        let r = route(
            &d,
            "POST",
            "/api/squads/squad-1/tasks/x/proof/y/restart",
            "",
        );
        assert_eq!(r.status, 400);
    }

    // ── RAL-150: task restart + env overrides ───────────────────────────────

    #[test]
    fn restart_task_preview_reports_impact_without_mutating() {
        let d = daemon();
        route(&d, "POST", "/api/squads", &submit_body(GOOD));
        let r = route(
            &d,
            "POST",
            "/api/squads/squad-000000000001/tasks/0/restart/preview",
            "",
        );
        assert_eq!(r.status, 200);
        let body: serde_json::Value = serde_json::from_str(&r.body).unwrap();
        assert_eq!(body["cells"].as_array().unwrap().len(), 1);
        assert_eq!(body["tasks"].as_array().unwrap().len(), 1);

        // Previewing did not mutate anything.
        let squad = route(&d, "GET", "/api/squads/squad-000000000001", "");
        let squad: serde_json::Value = serde_json::from_str(&squad.body).unwrap();
        assert_eq!(squad["state"], "pending");
    }

    #[test]
    fn restart_task_preview_bad_index_is_400() {
        let d = daemon();
        let r = route(
            &d,
            "POST",
            "/api/squads/squad-1/tasks/x/restart/preview",
            "",
        );
        assert_eq!(r.status, 400);
    }

    #[test]
    fn restart_task_route_returns_pending() {
        let d = daemon();
        route(&d, "POST", "/api/squads", &submit_body(GOOD));
        let r = route(
            &d,
            "POST",
            "/api/squads/squad-000000000001/tasks/0/restart",
            "",
        );
        assert_eq!(r.status, 200);
        assert!(r.body.contains("\"state\":\"pending\""));
    }

    /// RAL-174 deadlock regression (see `restart_squad_route_with_note_attaches_ghost_without_hanging`).
    #[test]
    fn restart_task_route_with_note_attaches_ghost_without_hanging() {
        let d = daemon();
        route(&d, "POST", "/api/squads", &submit_body(GOOD));
        let body = serde_json::json!({"note": "task was mid-refactor"}).to_string();
        let r = route(
            &d,
            "POST",
            "/api/squads/squad-000000000001/tasks/0/restart",
            &body,
        );
        assert_eq!(r.status, 200);
        let ghost = d
            .lock()
            .get_ghost(&crate::ghost::cell_uri("squad-000000000001", 0, 0))
            .unwrap()
            .unwrap();
        assert_eq!(ghost.user_note.as_deref(), Some("task was mid-refactor"));
    }

    #[test]
    fn restart_task_bad_index_is_400() {
        let d = daemon();
        let r = route(&d, "POST", "/api/squads/squad-1/tasks/x/restart", "");
        assert_eq!(r.status, 400);
    }

    #[test]
    fn restart_task_missing_task_is_404() {
        let d = daemon();
        route(&d, "POST", "/api/squads", &submit_body(GOOD));
        let r = route(
            &d,
            "POST",
            "/api/squads/squad-000000000001/tasks/9/restart",
            "",
        );
        assert_eq!(r.status, 404);
    }

    // -- RAL-324: read-only resolved-environment views ----------------------

    /// A guardian with one branch, plus that branch's id -- the fixture every
    /// review-scoped env view needs.
    fn guardian_with_one_branch(d: &Daemon) -> (String, String) {
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
        (gid, bid)
    }

    #[test]
    fn every_env_surface_serves_a_resolved_view_on_the_same_path_it_is_set_on() {
        let d = daemon();
        route(&d, "POST", "/api/squads", &submit_body(GOOD));
        route(
            &d,
            "POST",
            "/api/squads/squad-000000000001/env",
            &serde_json::json!({"set": {"FROM_SQUAD": "1"}}).to_string(),
        );
        for path in [
            "/api/squads/squad-000000000001/env",
            "/api/squads/squad-000000000001/tasks/0/env",
            "/api/squads/squad-000000000001/tasks/0/proof/env",
            "/api/squads/squad-000000000001/cells/0/0/env",
            "/api/squads/squad-000000000001/cells/0/0/proof/env",
        ] {
            let r = route(&d, "GET", path, "");
            assert_eq!(r.status, 200, "{path}");
            let view: serde_json::Value = serde_json::from_str(&r.body).unwrap();
            let names: Vec<&str> = view["vars"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v["name"].as_str().unwrap())
                .collect();
            assert!(names.contains(&"FROM_SQUAD"), "{path}: {names:?}");
            assert!(view["note"].as_str().unwrap().contains("Secrets tab"));
        }
    }

    #[test]
    fn a_resolved_env_view_masks_a_registered_secret_name_only() {
        let d = daemon();
        route(&d, "POST", "/api/squads", &submit_body(GOOD));
        route(
            &d,
            "POST",
            "/api/squads/squad-000000000001/cells/0/0/env",
            &serde_json::json!({"set": {"MY_TOKEN": "s3kr3t", "PLAIN": "shown"}}).to_string(),
        );
        route(
            &d,
            "POST",
            "/api/secret-env-names",
            &serde_json::json!({"name": "MY_TOKEN"}).to_string(),
        );
        let r = route(
            &d,
            "GET",
            "/api/squads/squad-000000000001/cells/0/0/env",
            "",
        );
        assert_eq!(r.status, 200);
        assert!(!r.body.contains("s3kr3t"), "{}", r.body);
        assert!(r.body.contains("shown"));
        let view: serde_json::Value = serde_json::from_str(&r.body).unwrap();
        assert_eq!(view["redacted_count"], 1);
    }

    #[test]
    fn a_resolved_env_view_for_an_unknown_entity_is_404_and_a_bad_index_is_400() {
        let d = daemon();
        route(&d, "POST", "/api/squads", &submit_body(GOOD));
        assert_eq!(
            route(&d, "GET", "/api/squads/squad-999/env", "").status,
            404
        );
        assert_eq!(
            route(&d, "GET", "/api/squads/squad-000000000001/tasks/9/env", "").status,
            404
        );
        assert_eq!(
            route(&d, "GET", "/api/squads/squad-000000000001/tasks/x/env", "").status,
            400
        );
        assert_eq!(
            route(&d, "GET", "/api/guardians/guardian-nope/build-env", "").status,
            404
        );
    }

    #[test]
    fn a_proof_step_env_view_puts_the_steps_own_layer_on_top() {
        let d = daemon();
        route(&d, "POST", "/api/squads", &submit_body(PROOF_STEPS));
        route(
            &d,
            "POST",
            "/api/squads/squad-000000000001/cells/0/0/proof/env",
            &serde_json::json!({"set": {"RUST_LOG": "warn"}}).to_string(),
        );
        route(
            &d,
            "POST",
            "/api/squads/squad-000000000001/cells/0/0/proof/0/env",
            &serde_json::json!({"set": {"RUST_LOG": "debug"}}).to_string(),
        );
        let r = route(
            &d,
            "GET",
            "/api/squads/squad-000000000001/cells/0/0/proof/0/env",
            "",
        );
        assert_eq!(r.status, 200);
        let view: serde_json::Value = serde_json::from_str(&r.body).unwrap();
        assert_eq!(view["scope"], "proof");
        let row = view["vars"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["name"] == "RUST_LOG")
            .unwrap_or_else(|| panic!("no RUST_LOG: {}", r.body));
        assert_eq!(row["value"], "debug");
        assert_eq!(row["source"], "proof step");
        assert_eq!(row["layers"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn the_tests_env_view_resolves_the_build_steps_own_override_layer() {
        let d = daemon();
        let (gid, _bid) = guardian_with_one_branch(&d);
        route(
            &d,
            "POST",
            &format!("/api/guardians/{gid}/build-env"),
            &serde_json::json!({"set": {"CI": "1"}}).to_string(),
        );
        for (path, scope) in [("build-env", "review-build"), ("tests-env", "review-tests")] {
            let r = route(&d, "GET", &format!("/api/guardians/{gid}/{path}"), "");
            assert_eq!(r.status, 200, "{path}");
            let view: serde_json::Value = serde_json::from_str(&r.body).unwrap();
            assert_eq!(view["scope"], scope);
            let ci = view["vars"]
                .as_array()
                .unwrap()
                .iter()
                .find(|v| v["name"] == "CI")
                .unwrap_or_else(|| panic!("{path} is missing CI: {}", r.body));
            assert_eq!(ci["value"], "1");
            assert_eq!(ci["source"], "review build step");
        }
        // The manual-checks step is a separate layer -- the build override
        // must not leak into it.
        let r = route(
            &d,
            "GET",
            &format!("/api/guardians/{gid}/manual-checks-env"),
            "",
        );
        let view: serde_json::Value = serde_json::from_str(&r.body).unwrap();
        assert!(
            !view["vars"]
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v["name"] == "CI"),
            "{}",
            r.body
        );
    }

    #[test]
    fn a_review_worktree_env_view_lists_an_override_and_omits_a_tombstoned_key() {
        let d = daemon();
        let (gid, bid) = guardian_with_one_branch(&d);
        route(
            &d,
            "POST",
            &format!("/api/guardians/{gid}/branches/{bid}/env"),
            &serde_json::json!({"set": {"API_URL": "https://staging"}, "unset": ["DROP_ME"]})
                .to_string(),
        );
        let r = route(
            &d,
            "GET",
            &format!("/api/guardians/{gid}/branches/{bid}/env"),
            "",
        );
        assert_eq!(r.status, 200);
        let view: serde_json::Value = serde_json::from_str(&r.body).unwrap();
        assert_eq!(view["scope"], "review-worktree");
        let vars = view["vars"].as_array().unwrap();
        assert!(vars.iter().any(|v| v["name"] == "API_URL"), "{}", r.body);
        assert!(
            !vars.iter().any(|v| v["name"] == "DROP_ME"),
            "a tombstoned key is not part of the resolved environment: {}",
            r.body
        );
    }

    #[test]
    fn set_squad_env_sets_and_is_reflected_on_get() {
        let d = daemon();
        route(&d, "POST", "/api/squads", &submit_body(GOOD));
        let body = serde_json::json!({"set": {"A": "1", "B": "2"}}).to_string();
        let r = route(&d, "POST", "/api/squads/squad-000000000001/env", &body);
        assert_eq!(r.status, 200);
        let result: serde_json::Value = serde_json::from_str(&r.body).unwrap();
        assert_eq!(result["A"], "1");
        assert_eq!(result["B"], "2");

        let squad = route(&d, "GET", "/api/squads/squad-000000000001", "");
        let squad: serde_json::Value = serde_json::from_str(&squad.body).unwrap();
        assert_eq!(squad["env_overrides"]["A"], "1");
        assert_eq!(squad["env_overrides"]["B"], "2");
    }

    #[test]
    fn set_squad_env_unsets_a_previously_set_key() {
        let d = daemon();
        route(&d, "POST", "/api/squads", &submit_body(GOOD));
        route(
            &d,
            "POST",
            "/api/squads/squad-000000000001/env",
            &serde_json::json!({"set": {"A": "1"}}).to_string(),
        );
        let r = route(
            &d,
            "POST",
            "/api/squads/squad-000000000001/env",
            &serde_json::json!({"unset": ["A"]}).to_string(),
        );
        assert_eq!(r.status, 200);
        let result: serde_json::Value = serde_json::from_str(&r.body).unwrap();
        assert!(result.as_object().unwrap().is_empty());
    }

    #[test]
    fn set_squad_env_rejects_invalid_key() {
        let d = daemon();
        route(&d, "POST", "/api/squads", &submit_body(GOOD));
        let body = serde_json::json!({"set": {"NOT VALID": "1"}}).to_string();
        let r = route(&d, "POST", "/api/squads/squad-000000000001/env", &body);
        assert_eq!(r.status, 400);
    }

    // RAL-227: a `\n`/`\r`-carrying value must never reach
    // `build_command_line_with_env` -- on Windows it's delivered via
    // `send-keys` typed into a live pty, where an embedded newline acts like
    // pressing Enter mid-command. Rejected here at the same boundary as an
    // invalid key, before it ever reaches the store or the tmux layer.
    #[test]
    fn set_squad_env_rejects_newline_in_value() {
        let d = daemon();
        route(&d, "POST", "/api/squads", &submit_body(GOOD));
        let body = serde_json::json!({"set": {"A": "first\necho INJECTED"}}).to_string();
        let r = route(&d, "POST", "/api/squads/squad-000000000001/env", &body);
        assert_eq!(r.status, 400);

        // The rejected value must never have been persisted.
        let squad = route(&d, "GET", "/api/squads/squad-000000000001", "");
        let squad: serde_json::Value = serde_json::from_str(&squad.body).unwrap();
        assert!(squad["env_overrides"].get("A").is_none());
    }

    #[test]
    fn set_squad_env_rejects_carriage_return_in_value() {
        let d = daemon();
        route(&d, "POST", "/api/squads", &submit_body(GOOD));
        let body = serde_json::json!({"set": {"A": "first\revil"}}).to_string();
        let r = route(&d, "POST", "/api/squads/squad-000000000001/env", &body);
        assert_eq!(r.status, 400);
    }

    #[test]
    fn set_squad_env_allows_ordinary_multiword_value() {
        let d = daemon();
        route(&d, "POST", "/api/squads", &submit_body(GOOD));
        let body = serde_json::json!({"set": {"A": "it's a normal value with spaces"}}).to_string();
        let r = route(&d, "POST", "/api/squads/squad-000000000001/env", &body);
        assert_eq!(r.status, 200);
    }

    #[test]
    fn set_squad_env_requires_set_or_unset() {
        let d = daemon();
        route(&d, "POST", "/api/squads", &submit_body(GOOD));
        let r = route(&d, "POST", "/api/squads/squad-000000000001/env", "{}");
        assert_eq!(r.status, 400);
    }

    #[test]
    fn set_squad_env_missing_squad_is_404() {
        let d = daemon();
        let body = serde_json::json!({"set": {"A": "1"}}).to_string();
        let r = route(&d, "POST", "/api/squads/squad-999/env", &body);
        assert_eq!(r.status, 404);
    }

    // ── hierarchical env overrides (RAL-150 extension) ─────────────────────

    #[test]
    fn set_task_env_sets_and_is_reflected_on_get() {
        let d = daemon();
        route(&d, "POST", "/api/squads", &submit_body(GOOD));
        let body = serde_json::json!({"set": {"A": "1"}}).to_string();
        let r = route(
            &d,
            "POST",
            "/api/squads/squad-000000000001/tasks/0/env",
            &body,
        );
        assert_eq!(r.status, 200);
        let result: serde_json::Value = serde_json::from_str(&r.body).unwrap();
        assert_eq!(result["A"], "1");

        let squad = route(&d, "GET", "/api/squads/squad-000000000001", "");
        let squad: serde_json::Value = serde_json::from_str(&squad.body).unwrap();
        assert_eq!(squad["tasks"][0]["env_overrides"]["A"], "1");
    }

    /// RAL-271: editing a task's overrides marks the task and its cells "out
    /// of date" in the same round trip, and a `restart` clears it again.
    #[test]
    fn set_task_env_marks_task_and_cells_out_of_date_then_restart_clears_it() {
        let d = daemon();
        route(&d, "POST", "/api/squads", &submit_body(GOOD));
        let body = serde_json::json!({"set": {"A": "1"}}).to_string();
        route(
            &d,
            "POST",
            "/api/squads/squad-000000000001/tasks/0/env",
            &body,
        );

        let squad = route(&d, "GET", "/api/squads/squad-000000000001", "");
        let squad: serde_json::Value = serde_json::from_str(&squad.body).unwrap();
        assert_eq!(squad["tasks"][0]["env_out_of_date"], true);
        assert_eq!(squad["tasks"][0]["cells"][0]["env_out_of_date"], true);

        let r = route(
            &d,
            "POST",
            "/api/squads/squad-000000000001/cells/0/0/restart",
            "",
        );
        assert_eq!(r.status, 200);

        let squad = route(&d, "GET", "/api/squads/squad-000000000001", "");
        let squad: serde_json::Value = serde_json::from_str(&squad.body).unwrap();
        assert_eq!(squad["tasks"][0]["cells"][0]["env_out_of_date"], false);
    }

    /// RAL-271: `Set Status` is the second trigger (besides a restart) that
    /// must clear the badge -- exercised here via the cell env endpoint plus
    /// `set-status` to `done`, independent of any actual re-run.
    #[test]
    fn set_status_done_clears_cell_env_out_of_date() {
        let d = daemon();
        route(&d, "POST", "/api/squads", &submit_body(GOOD));
        let body = serde_json::json!({"set": {"A": "1"}}).to_string();
        route(
            &d,
            "POST",
            "/api/squads/squad-000000000001/cells/0/0/env",
            &body,
        );
        let squad = route(&d, "GET", "/api/squads/squad-000000000001", "");
        let squad: serde_json::Value = serde_json::from_str(&squad.body).unwrap();
        assert_eq!(squad["tasks"][0]["cells"][0]["env_out_of_date"], true);

        let status_body = serde_json::json!({
            "kind": "cell",
            "task_idx": 0,
            "cell_idx": 0,
            "state": "done",
        })
        .to_string();
        let r = route(
            &d,
            "POST",
            "/api/squads/squad-000000000001/set-status",
            &status_body,
        );
        assert_eq!(r.status, 200);
        let squad: serde_json::Value = serde_json::from_str(&r.body).unwrap();
        assert_eq!(squad["tasks"][0]["cells"][0]["env_out_of_date"], false);
    }

    /// RAL-271: the board's inline "Edit" button posts the same
    /// `{set: {key: value}}` body as "+ Add override" against an
    /// already-present key -- verify that round trip through the HTTP API
    /// replaces the value in place rather than duplicating or dropping it.
    #[test]
    fn editing_an_existing_task_env_override_via_set_replaces_its_value() {
        let d = daemon();
        route(&d, "POST", "/api/squads", &submit_body(GOOD));
        route(
            &d,
            "POST",
            "/api/squads/squad-000000000001/tasks/0/env",
            &serde_json::json!({"set": {"A": "1"}}).to_string(),
        );

        let r = route(
            &d,
            "POST",
            "/api/squads/squad-000000000001/tasks/0/env",
            &serde_json::json!({"set": {"A": "edited"}}).to_string(),
        );
        assert_eq!(r.status, 200);

        let squad = route(&d, "GET", "/api/squads/squad-000000000001", "");
        let squad: serde_json::Value = serde_json::from_str(&squad.body).unwrap();
        assert_eq!(squad["tasks"][0]["env_overrides"]["A"], "edited");
        assert_eq!(
            squad["tasks"][0]["env_overrides"]
                .as_object()
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn set_task_env_missing_task_is_404() {
        let d = daemon();
        route(&d, "POST", "/api/squads", &submit_body(GOOD));
        let body = serde_json::json!({"set": {"A": "1"}}).to_string();
        let r = route(
            &d,
            "POST",
            "/api/squads/squad-000000000001/tasks/9/env",
            &body,
        );
        assert_eq!(r.status, 404);
    }

    #[test]
    fn set_task_env_bad_index_is_400() {
        let d = daemon();
        let body = serde_json::json!({"set": {"A": "1"}}).to_string();
        let r = route(&d, "POST", "/api/squads/squad-1/tasks/x/env", &body);
        assert_eq!(r.status, 400);
    }

    #[test]
    fn set_task_env_rejects_invalid_key() {
        let d = daemon();
        route(&d, "POST", "/api/squads", &submit_body(GOOD));
        let body = serde_json::json!({"set": {"NOT VALID": "1"}}).to_string();
        let r = route(
            &d,
            "POST",
            "/api/squads/squad-000000000001/tasks/0/env",
            &body,
        );
        assert_eq!(r.status, 400);
    }

    #[test]
    fn set_task_proof_env_sets_and_is_reflected_on_get() {
        let d = daemon();
        route(&d, "POST", "/api/squads", &submit_body(GOOD));
        let body = serde_json::json!({"set": {"A": "1"}}).to_string();
        let r = route(
            &d,
            "POST",
            "/api/squads/squad-000000000001/tasks/0/proof/env",
            &body,
        );
        assert_eq!(r.status, 200);
        let result: serde_json::Value = serde_json::from_str(&r.body).unwrap();
        assert_eq!(result["A"], "1");

        let squad = route(&d, "GET", "/api/squads/squad-000000000001", "");
        let squad: serde_json::Value = serde_json::from_str(&squad.body).unwrap();
        assert_eq!(squad["tasks"][0]["proof_env_overrides"]["A"], "1");
        // The plain task-level map is untouched.
        assert!(squad["tasks"][0].get("env_overrides").is_none());
    }

    #[test]
    fn set_task_proof_env_missing_task_is_404() {
        let d = daemon();
        route(&d, "POST", "/api/squads", &submit_body(GOOD));
        let body = serde_json::json!({"set": {"A": "1"}}).to_string();
        let r = route(
            &d,
            "POST",
            "/api/squads/squad-000000000001/tasks/9/proof/env",
            &body,
        );
        assert_eq!(r.status, 404);
    }

    // ── RAL-191: per-proof-step and per-review-branch env endpoints ──────────

    /// A task with two proof steps plus a cell proof step, so the
    /// per-step routes have distinct indices to address.
    const PROOF_STEPS: &str = "[[task]]\nname=\"t\"\n\
        [[task.proof]]\ncommand=\"a\"\n\
        [[task.proof]]\ncommand=\"b\"\n\
        [[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\n\
        [[task.cell.proof]]\ncommand=\"c\"\n";

    #[test]
    fn set_task_proof_step_env_targets_one_step_not_the_whole_scope() {
        let d = daemon();
        route(&d, "POST", "/api/squads", &submit_body(PROOF_STEPS));
        let body = serde_json::json!({"set": {"A": "step-1"}}).to_string();
        let r = route(
            &d,
            "POST",
            "/api/squads/squad-000000000001/tasks/0/proof/1/env",
            &body,
        );
        assert_eq!(r.status, 200);

        let squad = route(&d, "GET", "/api/squads/squad-000000000001", "");
        let squad: serde_json::Value = serde_json::from_str(&squad.body).unwrap();
        assert_eq!(
            squad["tasks"][0]["proof"][1]["env_overrides"]["A"],
            "step-1"
        );
        // The sibling step and the scope-wide layer are both untouched.
        assert!(squad["tasks"][0]["proof"][0].get("env_overrides").is_none());
        assert!(squad["tasks"][0].get("proof_env_overrides").is_none());
    }

    #[test]
    fn set_cell_proof_step_env_targets_one_step() {
        let d = daemon();
        route(&d, "POST", "/api/squads", &submit_body(PROOF_STEPS));
        let body = serde_json::json!({"set": {"CI": "1"}}).to_string();
        let r = route(
            &d,
            "POST",
            "/api/squads/squad-000000000001/cells/0/0/proof/0/env",
            &body,
        );
        assert_eq!(r.status, 200);

        let squad = route(&d, "GET", "/api/squads/squad-000000000001", "");
        let squad: serde_json::Value = serde_json::from_str(&squad.body).unwrap();
        assert_eq!(
            squad["tasks"][0]["cells"][0]["proof"][0]["env_overrides"]["CI"],
            "1"
        );
    }

    #[test]
    fn the_scope_wide_proof_env_route_still_resolves_alongside_the_per_step_one() {
        // `/proof/env` and `/proof/{vi}/env` must not shadow each other.
        let d = daemon();
        route(&d, "POST", "/api/squads", &submit_body(PROOF_STEPS));
        let body = serde_json::json!({"set": {"A": "scope"}}).to_string();
        let r = route(
            &d,
            "POST",
            "/api/squads/squad-000000000001/tasks/0/proof/env",
            &body,
        );
        assert_eq!(r.status, 200);

        let squad = route(&d, "GET", "/api/squads/squad-000000000001", "");
        let squad: serde_json::Value = serde_json::from_str(&squad.body).unwrap();
        assert_eq!(squad["tasks"][0]["proof_env_overrides"]["A"], "scope");
        assert!(squad["tasks"][0]["proof"][0].get("env_overrides").is_none());
    }

    #[test]
    fn set_proof_step_env_missing_step_is_404() {
        let d = daemon();
        route(&d, "POST", "/api/squads", &submit_body(PROOF_STEPS));
        let body = serde_json::json!({"set": {"A": "1"}}).to_string();
        let r = route(
            &d,
            "POST",
            "/api/squads/squad-000000000001/tasks/0/proof/9/env",
            &body,
        );
        assert_eq!(r.status, 404);
    }

    #[test]
    fn set_proof_step_env_bad_index_is_400() {
        let d = daemon();
        let body = serde_json::json!({"set": {"A": "1"}}).to_string();
        let r = route(&d, "POST", "/api/squads/squad-1/tasks/0/proof/x/env", &body);
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
        // With no source cell there is nothing to inherit, so the resolved
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
    fn set_cell_env_sets_and_is_reflected_on_get() {
        let d = daemon();
        route(&d, "POST", "/api/squads", &submit_body(GOOD));
        let body = serde_json::json!({"set": {"A": "1"}}).to_string();
        let r = route(
            &d,
            "POST",
            "/api/squads/squad-000000000001/cells/0/0/env",
            &body,
        );
        assert_eq!(r.status, 200);
        let result: serde_json::Value = serde_json::from_str(&r.body).unwrap();
        assert_eq!(result["A"], "1");

        let squad = route(&d, "GET", "/api/squads/squad-000000000001", "");
        let squad: serde_json::Value = serde_json::from_str(&squad.body).unwrap();
        assert_eq!(squad["tasks"][0]["cells"][0]["env_overrides"]["A"], "1");
    }

    #[test]
    fn set_cell_env_missing_cell_is_404() {
        let d = daemon();
        route(&d, "POST", "/api/squads", &submit_body(GOOD));
        let body = serde_json::json!({"set": {"A": "1"}}).to_string();
        let r = route(
            &d,
            "POST",
            "/api/squads/squad-000000000001/cells/0/9/env",
            &body,
        );
        assert_eq!(r.status, 404);
    }

    #[test]
    fn set_cell_env_bad_index_is_400() {
        let d = daemon();
        let body = serde_json::json!({"set": {"A": "1"}}).to_string();
        let r = route(&d, "POST", "/api/squads/squad-1/cells/x/0/env", &body);
        assert_eq!(r.status, 400);
    }

    #[test]
    fn strip_ralphus_pane_markers_drops_event_and_done_lines_only() {
        let text = "Claude Code · model=sonnet\n\
             RALPHUS_EVENT: {\"source\":\"claude-code\",\"message\":\"hi\"}\n\
             [tool] Bash(command=\"ls\")\n\
             RALPHUS_TMUX_DONE: done\n\
             final assistant text";
        assert_eq!(
            strip_ralphus_pane_markers(text),
            "Claude Code · model=sonnet\n[tool] Bash(command=\"ls\")\nfinal assistant text"
        );
    }

    #[test]
    fn strip_ralphus_pane_markers_matches_only_a_line_start_not_a_mid_line_occurrence() {
        // An agent quoting/grepping these exact strings in its own output
        // must not have that output misclassified as a ralphus marker.
        let text = "the code searches for RALPHUS_EVENT: prefixed lines";
        assert_eq!(strip_ralphus_pane_markers(text), text);
    }

    #[test]
    fn strip_ralphus_pane_markers_tolerates_leading_whitespace() {
        let text = "  RALPHUS_EVENT: {}\nkept line";
        assert_eq!(strip_ralphus_pane_markers(text), "kept line");
    }

    #[test]
    fn strip_ralphus_pane_markers_empty_input_stays_empty() {
        assert_eq!(strip_ralphus_pane_markers(""), "");
    }

    #[test]
    fn cell_debug_events_lists_only_this_cells_cartographer_rows() {
        let d = daemon();
        let toml = "[[task]]\nname=\"t\"\n\
            [[task.cell]]\nid=\"a\"\ncwd=\"/r\"\nagent=\"claude-code\"\nprompt=\"p\"\n\
            [[task.cell]]\nid=\"b\"\ncwd=\"/r2\"\nagent=\"claude-code\"\nprompt=\"p2\"\n";
        route(&d, "POST", "/api/squads", &submit_body(toml));
        {
            let store = d.lock();
            let _ = store.cartographer_log(crate::cartographer::CartographerEntry {
                level: crate::logging::LogLevel::INFO,
                source: "runner",
                message: "llm start",
                scope: Some("cell"),
                squad_id: Some("squad-000000000001"),
                guardian_id: None,
                cell_id: Some("a"),
                task: Some("t"),
                log_path: None,
                payload: serde_json::json!({}),
                admin_only: false,
            });
            let _ = store.cartographer_log(crate::cartographer::CartographerEntry {
                level: crate::logging::LogLevel::INFO,
                source: "runner",
                message: "llm start (other cell)",
                scope: Some("cell"),
                squad_id: Some("squad-000000000001"),
                guardian_id: None,
                cell_id: Some("b"),
                task: Some("t"),
                log_path: None,
                payload: serde_json::json!({}),
                admin_only: false,
            });
        }
        let r = route(
            &d,
            "GET",
            "/api/squads/squad-000000000001/cells/0/0/debug-events",
            "",
        );
        assert_eq!(r.status, 200, "{}", r.body);
        let entries: serde_json::Value = serde_json::from_str(&r.body).unwrap();
        let entries = entries.as_array().unwrap();
        assert_eq!(entries.len(), 1, "{entries:?}");
        assert_eq!(entries[0]["message"], "llm start");
    }

    #[test]
    fn cell_debug_events_missing_cell_is_404() {
        let d = daemon();
        route(&d, "POST", "/api/squads", &submit_body(GOOD));
        let r = route(
            &d,
            "GET",
            "/api/squads/squad-000000000001/cells/0/9/debug-events",
            "",
        );
        assert_eq!(r.status, 404);
    }

    #[test]
    fn proof_debug_events_lists_only_this_proof_steps_cartographer_rows() {
        let d = daemon();
        let toml = "[[task]]\nname=\"t\"\n\
            [[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\n\
            [[task.cell.proof]]\ncommand=\"c\"\n";
        route(&d, "POST", "/api/squads", &submit_body(toml));
        {
            let store = d.lock();
            let _ = store.cartographer_log(crate::cartographer::CartographerEntry {
                level: crate::logging::LogLevel::INFO,
                source: "runner",
                message: "invoked",
                scope: Some("proof"),
                squad_id: Some("squad-000000000001"),
                guardian_id: None,
                cell_id: Some("proof-cell-0"),
                task: Some("t"),
                log_path: None,
                payload: serde_json::json!({}),
                admin_only: false,
            });
            let _ = store.cartographer_log(crate::cartographer::CartographerEntry {
                level: crate::logging::LogLevel::INFO,
                source: "runner",
                message: "invoked (other cell's own cell-level event)",
                scope: Some("cell"),
                squad_id: Some("squad-000000000001"),
                guardian_id: None,
                cell_id: Some("cell-0"),
                task: Some("t"),
                log_path: None,
                payload: serde_json::json!({}),
                admin_only: false,
            });
        }
        let r = route(
            &d,
            "GET",
            "/api/squads/squad-000000000001/proofs/0/cell/0/0/debug-events",
            "",
        );
        assert_eq!(r.status, 200, "{}", r.body);
        let entries: serde_json::Value = serde_json::from_str(&r.body).unwrap();
        let entries = entries.as_array().unwrap();
        assert_eq!(entries.len(), 1, "{entries:?}");
        assert_eq!(entries[0]["message"], "invoked");
    }

    #[test]
    fn proof_debug_events_missing_squad_is_404() {
        let d = daemon();
        let r = route(
            &d,
            "GET",
            "/api/squads/squad-999/proofs/0/cell/0/0/debug-events",
            "",
        );
        assert_eq!(r.status, 404);
    }

    #[test]
    fn guardian_branch_debug_events_lists_only_this_branchs_cartographer_rows() {
        let d = daemon();
        let id = d.lock().create_guardian("g", "main", "/r").unwrap();
        d.lock().add_guardian_branch(&id, "feature/a").unwrap();
        d.lock().add_guardian_branch(&id, "feature/b").unwrap();
        let branches = d.lock().guardian_branches(&id).unwrap();
        let branch_a = branches[0].id.clone();
        let branch_b = branches[1].id.clone();
        {
            let store = d.lock();
            let _ = store.cartographer_log(crate::cartographer::CartographerEntry {
                level: crate::logging::LogLevel::INFO,
                source: "runner",
                message: "invoked",
                scope: Some("cell"),
                squad_id: Some(&format!("guardian-{id}")),
                guardian_id: None,
                cell_id: Some(&format!(
                    "resolver-{}",
                    branches.iter().find(|b| b.id == branch_a).unwrap().position
                )),
                task: Some(crate::guardian_merge::RESOLVER_TASK),
                log_path: None,
                payload: serde_json::json!({}),
                admin_only: false,
            });
            let _ = store.cartographer_log(crate::cartographer::CartographerEntry {
                level: crate::logging::LogLevel::INFO,
                source: "runner",
                message: "invoked (other branch)",
                scope: Some("cell"),
                squad_id: Some(&format!("guardian-{id}")),
                guardian_id: None,
                cell_id: Some(&format!(
                    "resolver-{}",
                    branches.iter().find(|b| b.id == branch_b).unwrap().position
                )),
                task: Some(crate::guardian_merge::RESOLVER_TASK),
                log_path: None,
                payload: serde_json::json!({}),
                admin_only: false,
            });
        }
        let r = route(
            &d,
            "GET",
            &format!("/api/guardians/{id}/branches/{branch_a}/debug-events"),
            "",
        );
        assert_eq!(r.status, 200, "{}", r.body);
        let entries: serde_json::Value = serde_json::from_str(&r.body).unwrap();
        let entries = entries.as_array().unwrap();
        assert_eq!(entries.len(), 1, "{entries:?}");
        assert_eq!(entries[0]["message"], "invoked");
    }

    #[test]
    fn guardian_branch_debug_events_missing_branch_is_404() {
        let d = daemon();
        let id = d.lock().create_guardian("g", "main", "/r").unwrap();
        let r = route(
            &d,
            "GET",
            &format!("/api/guardians/{id}/branches/branch-999/debug-events"),
            "",
        );
        assert_eq!(r.status, 404);
    }

    #[test]
    fn guardian_manual_checks_debug_events_lists_this_guardians_rows() {
        let d = daemon();
        let id = d.lock().create_guardian("g", "main", "/r").unwrap();
        {
            let store = d.lock();
            let _ = store.cartographer_log(crate::cartographer::CartographerEntry {
                level: crate::logging::LogLevel::INFO,
                source: "runner",
                message: "invoked",
                scope: Some("cell"),
                squad_id: Some(&format!("guardian-{id}")),
                guardian_id: None,
                cell_id: Some(crate::guardian_merge::MANUAL_COMMANDS_SESSION),
                task: Some(crate::guardian_merge::MANUAL_COMMANDS_TASK),
                log_path: None,
                payload: serde_json::json!({}),
                admin_only: false,
            });
        }
        let r = route(
            &d,
            "GET",
            &format!("/api/guardians/{id}/manual-checks/debug-events"),
            "",
        );
        assert_eq!(r.status, 200, "{}", r.body);
        let entries: serde_json::Value = serde_json::from_str(&r.body).unwrap();
        let entries = entries.as_array().unwrap();
        assert_eq!(entries.len(), 1, "{entries:?}");
        assert_eq!(entries[0]["message"], "invoked");
    }

    #[test]
    fn guardian_manual_checks_debug_events_missing_guardian_is_404() {
        let d = daemon();
        let r = route(
            &d,
            "GET",
            "/api/guardians/guardian-999/manual-checks/debug-events",
            "",
        );
        assert_eq!(r.status, 404);
    }

    #[test]
    fn set_cell_proof_env_sets_and_is_reflected_on_get() {
        let d = daemon();
        route(&d, "POST", "/api/squads", &submit_body(GOOD));
        let body = serde_json::json!({"set": {"A": "1"}}).to_string();
        let r = route(
            &d,
            "POST",
            "/api/squads/squad-000000000001/cells/0/0/proof/env",
            &body,
        );
        assert_eq!(r.status, 200);
        let result: serde_json::Value = serde_json::from_str(&r.body).unwrap();
        assert_eq!(result["A"], "1");

        let squad = route(&d, "GET", "/api/squads/squad-000000000001", "");
        let squad: serde_json::Value = serde_json::from_str(&squad.body).unwrap();
        assert_eq!(
            squad["tasks"][0]["cells"][0]["proof_env_overrides"]["A"],
            "1"
        );
    }

    #[test]
    fn set_cell_proof_env_missing_cell_is_404() {
        let d = daemon();
        route(&d, "POST", "/api/squads", &submit_body(GOOD));
        let body = serde_json::json!({"set": {"A": "1"}}).to_string();
        let r = route(
            &d,
            "POST",
            "/api/squads/squad-000000000001/cells/0/9/proof/env",
            &body,
        );
        assert_eq!(r.status, 404);
    }

    #[test]
    fn set_env_overrides_requires_set_or_unset_for_every_scope() {
        let d = daemon();
        route(&d, "POST", "/api/squads", &submit_body(GOOD));
        for path in [
            "/api/squads/squad-000000000001/tasks/0/env",
            "/api/squads/squad-000000000001/tasks/0/proof/env",
            "/api/squads/squad-000000000001/cells/0/0/env",
            "/api/squads/squad-000000000001/cells/0/0/proof/env",
        ] {
            let r = route(&d, "POST", path, "{}");
            assert_eq!(r.status, 400, "{path}");
        }
    }

    // ── CLI_PARITY_PLAN.local.md Phase 1 / Q1: server-side `/api/tasks` filter ──

    #[test]
    fn board_filters_squads_by_status() {
        let d = daemon();
        route(&d, "POST", "/api/squads", &submit_body(GOOD)); // squad-1: pending
        route(&d, "POST", "/api/squads", &submit_body(GOOD)); // squad-2: pending
        d.lock()
            .set_squad_state("squad-000000000002", crate::store::SquadState::Done)
            .unwrap();
        let r = route(&d, "GET", "/api/tasks?status=done", "");
        assert_eq!(r.status, 200);
        assert!(r.body.contains("squad-000000000002"));
        assert!(!r.body.contains("squad-000000000001"));
    }

    #[test]
    fn board_filters_squads_by_status_is_case_insensitive_and_comma_separated() {
        let d = daemon();
        route(&d, "POST", "/api/squads", &submit_body(GOOD));
        let r = route(&d, "GET", "/api/tasks?status=Done,PENDING", "");
        assert_eq!(r.status, 200);
        assert!(r.body.contains("squad-000000000001"));
    }

    #[test]
    fn board_filters_squads_by_name_substring() {
        let d = daemon();
        let body = serde_json::to_string(
            &serde_json::json!({ "toml": GOOD, "label": "fix the flaky test" }),
        )
        .unwrap();
        route(&d, "POST", "/api/squads", &body);
        route(&d, "POST", "/api/squads", &submit_body(GOOD)); // no label
        let r = route(&d, "GET", "/api/tasks?name=flaky", "");
        assert_eq!(r.status, 200);
        assert!(r.body.contains("squad-000000000001"));
        assert!(!r.body.contains("squad-000000000002"));
    }

    #[test]
    fn board_filters_squads_by_name_is_case_insensitive_comma_separated_and_trims_whitespace() {
        let d = daemon();
        for label in ["fix the flaky test", "add dark mode"] {
            let body = serde_json::to_string(&serde_json::json!({ "toml": GOOD, "label": label }))
                .unwrap();
            route(&d, "POST", "/api/squads", &body);
        }
        route(&d, "POST", "/api/squads", &submit_body(GOOD)); // squad-3: no label
        let r = route(&d, "GET", "/api/tasks?name=Flaky, DARK", "");
        assert_eq!(r.status, 200);
        assert!(r.body.contains("squad-000000000001"));
        assert!(r.body.contains("squad-000000000002"));
        assert!(!r.body.contains("squad-000000000003"));
    }

    #[test]
    fn board_sort_by_name_orders_labels_ascending() {
        let d = daemon();
        for label in ["zebra", "apple"] {
            let body = serde_json::to_string(&serde_json::json!({ "toml": GOOD, "label": label }))
                .unwrap();
            route(&d, "POST", "/api/squads", &body);
        }
        let r = route(&d, "GET", "/api/tasks?sort=name", "");
        assert_eq!(r.status, 200);
        // "apple" (squad-2) must appear before "zebra" (squad-1) in the serialized array.
        let apple_pos = r.body.find("apple").unwrap();
        let zebra_pos = r.body.find("zebra").unwrap();
        assert!(apple_pos < zebra_pos);
    }

    #[test]
    fn board_with_no_query_keeps_newest_first_order() {
        let d = daemon();
        route(&d, "POST", "/api/squads", &submit_body(GOOD));
        route(&d, "POST", "/api/squads", &submit_body(GOOD));
        let r = route(&d, "GET", "/api/tasks", "");
        assert_eq!(r.status, 200);
        let first_pos = r.body.find("squad-000000000002").unwrap();
        let second_pos = r.body.find("squad-000000000001").unwrap();
        assert!(first_pos < second_pos, "newest squad must be listed first");
    }

    // ── CLI_PARITY_PLAN.local.md Phase 6: graph endpoints ───────────────────────

    #[test]
    fn squad_graph_route_returns_nodes_and_edges() {
        let d = daemon();
        let toml = "[[task]]\nname=\"build\"\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\n\
             [[task]]\nname=\"test\"\ndepends_on=[\"build\"]\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\n";
        route(&d, "POST", "/api/squads", &submit_body(toml));
        let r = route(&d, "GET", "/api/squads/squad-000000000001/graph", "");
        assert_eq!(r.status, 200);
        assert!(r.body.contains("\"id\":\"t0s0\""));
        assert!(r.body.contains("\"id\":\"t1s0\""));
        assert!(r.body.contains("\"from\":\"t0s0\""));
        assert!(r.body.contains("\"to\":\"t1s0\""));
    }

    #[test]
    fn squad_graph_missing_squad_is_404() {
        let d = daemon();
        let r = route(&d, "GET", "/api/squads/squad-nope/graph", "");
        assert_eq!(r.status, 404);
    }

    #[test]
    fn global_graph_edge_from_dependency_to_dependent() {
        let d = daemon();
        route(&d, "POST", "/api/squads", &submit_body(GOOD)); // squad-1
        let dependent = "[[default]]\ndepends_on=[\"squad-000000000001\"]\n".to_string() + GOOD;
        route(&d, "POST", "/api/squads", &submit_body(&dependent)); // squad-2
        let r = route(&d, "GET", "/api/graph", "");
        assert_eq!(r.status, 200);
        assert!(r.body.contains("\"from\":\"squad-000000000001\""));
        assert!(r.body.contains("\"to\":\"squad-000000000002\""));
    }

    #[test]
    fn global_graph_excludes_terminal_squads_by_default_but_all_includes_them() {
        let d = daemon();
        route(&d, "POST", "/api/squads", &submit_body(GOOD)); // squad-1: pending
        route(&d, "POST", "/api/squads", &submit_body(GOOD)); // squad-2: pending
        d.lock()
            .set_squad_state("squad-000000000002", crate::store::SquadState::Done)
            .unwrap();
        let default_view = route(&d, "GET", "/api/graph", "");
        assert!(default_view.body.contains("squad-000000000001"));
        assert!(!default_view.body.contains("squad-000000000002"));
        let all_view = route(&d, "GET", "/api/graph?all=1", "");
        assert!(all_view.body.contains("squad-000000000001"));
        assert!(all_view.body.contains("squad-000000000002"));
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
    fn unlink_prs_drops_open_rows_clears_stack_number_and_keeps_history() {
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
                "",
                Some(1),
                None,
            )
            .unwrap();
        d.lock().set_guardian_forge_stack_number(&gid, 7).unwrap();

        let r = route(
            &d,
            "POST",
            &format!("/api/guardians/{gid}/pull-requests/unlink"),
            "",
        );
        assert_eq!(r.status, 200);
        assert_eq!(r.body, "{\"dropped\":1}");

        let pr = d.lock().get_pull_request(&pr_id).unwrap();
        assert_eq!(pr.state, "dropped");
        assert_eq!(
            d.lock().get_guardian_forge_stack_number(&gid).unwrap(),
            None
        );

        // History is preserved (never hard-deleted) via pull-request-stacks.
        let stacks = route(
            &d,
            "GET",
            &format!("/api/guardians/{gid}/pull-request-stacks"),
            "",
        );
        assert_eq!(stacks.status, 200);
        assert!(stacks.body.contains(&pr_id));

        // Calling it again with nothing open left is a no-op, not an error.
        let r2 = route(
            &d,
            "POST",
            &format!("/api/guardians/{gid}/pull-requests/unlink"),
            "",
        );
        assert_eq!(r2.status, 200);
        assert_eq!(r2.body, "{\"dropped\":0}");
    }

    #[test]
    fn unlink_prs_missing_guardian_is_404() {
        let d = daemon();
        let r = route(
            &d,
            "POST",
            "/api/guardians/guardian-999/pull-requests/unlink",
            "",
        );
        assert_eq!(r.status, 404);
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
    fn guardian_settings_sets_and_resets_proof_scope() {
        let d = daemon();
        let gid = make_guardian(&d);
        let r = route(&d, "GET", &format!("/api/guardians/{gid}"), "");
        assert!(r.body.contains("\"proof_scope\":null"));
        assert!(r.body.contains("\"effective_proof_scope\":\"each_branch\""));

        let body = serde_json::json!({"proof_scope": "final_branch"}).to_string();
        let r = route(&d, "POST", &format!("/api/guardians/{gid}/settings"), &body);
        assert_eq!(r.status, 200);
        assert!(r.body.contains("\"proof_scope\":\"final_branch\""));
        assert!(
            r.body
                .contains("\"effective_proof_scope\":\"final_branch\"")
        );

        // Empty string resets it back to "inherit the project default".
        let body = serde_json::json!({"proof_scope": ""}).to_string();
        let r = route(&d, "POST", &format!("/api/guardians/{gid}/settings"), &body);
        assert_eq!(r.status, 200);
        assert!(r.body.contains("\"proof_scope\":null"));
        assert!(r.body.contains("\"effective_proof_scope\":\"each_branch\""));
    }

    #[test]
    fn guardian_settings_sets_proof_skip_auto_clean() {
        let d = daemon();
        let gid = make_guardian(&d);
        let body = serde_json::json!({"proof_skip_auto_clean": true}).to_string();
        let r = route(&d, "POST", &format!("/api/guardians/{gid}/settings"), &body);
        assert_eq!(r.status, 200);
        assert!(r.body.contains("\"proof_skip_auto_clean\":true"));
        assert!(r.body.contains("\"effective_proof_skip_auto_clean\":true"));
    }

    #[test]
    fn guardian_settings_sets_and_resets_skip_base_updates() {
        let d = daemon();
        let gid = make_guardian(&d);
        // Set the per-review opt-out on.
        let body = serde_json::json!({"skip_base_updates": true}).to_string();
        let r = route(&d, "POST", &format!("/api/guardians/{gid}/settings"), &body);
        assert_eq!(r.status, 200);
        assert!(r.body.contains("\"skip_base_updates\":true"));
        assert!(r.body.contains("\"effective_skip_base_updates\":true"));
        // Flip it explicitly back off (don't auto-update skip).
        let body = serde_json::json!({"skip_base_updates": false}).to_string();
        let r = route(&d, "POST", &format!("/api/guardians/{gid}/settings"), &body);
        assert_eq!(r.status, 200);
        assert!(r.body.contains("\"skip_base_updates\":false"));
        assert!(r.body.contains("\"effective_skip_base_updates\":false"));
    }

    #[test]
    fn guardian_settings_sets_and_resets_match_pr_branch_name() {
        let d = daemon();
        let gid = make_guardian(&d);
        let body = serde_json::json!({"match_pr_branch_name": true}).to_string();
        let r = route(&d, "POST", &format!("/api/guardians/{gid}/settings"), &body);
        assert_eq!(r.status, 200);
        assert!(r.body.contains("\"match_pr_branch_name\":true"));
        assert!(r.body.contains("\"effective_match_pr_branch_name\":true"));
        let body = serde_json::json!({"match_pr_branch_name": false}).to_string();
        let r = route(&d, "POST", &format!("/api/guardians/{gid}/settings"), &body);
        assert_eq!(r.status, 200);
        assert!(r.body.contains("\"match_pr_branch_name\":false"));
        assert!(r.body.contains("\"effective_match_pr_branch_name\":false"));
    }

    #[test]
    fn guardian_settings_sets_and_resets_auto_submit_pr_stack() {
        let d = daemon();
        let gid = make_guardian(&d);
        let body = serde_json::json!({"auto_submit_pr_stack": true}).to_string();
        let r = route(&d, "POST", &format!("/api/guardians/{gid}/settings"), &body);
        assert_eq!(r.status, 200);
        assert!(r.body.contains("\"auto_submit_pr_stack\":true"));
        assert!(r.body.contains("\"effective_auto_submit_pr_stack\":true"));
        let body = serde_json::json!({"auto_submit_pr_stack": false}).to_string();
        let r = route(&d, "POST", &format!("/api/guardians/{gid}/settings"), &body);
        assert_eq!(r.status, 200);
        assert!(r.body.contains("\"auto_submit_pr_stack\":false"));
        assert!(r.body.contains("\"effective_auto_submit_pr_stack\":false"));
    }

    // ── RAL-410: batched `/details` endpoint (board "Edit Details" modal) ───

    #[test]
    fn guardian_details_batches_multiple_fields_in_one_request() {
        let d = daemon();
        let gid = make_guardian(&d);
        let body = serde_json::json!({
            "skip_auto_build": true,
            "proof_scope": "final_branch",
            "match_pr_branch_name": true,
        })
        .to_string();
        let r = route(&d, "POST", &format!("/api/guardians/{gid}/details"), &body);
        assert_eq!(r.status, 200, "{}", r.body);
        assert!(r.body.contains("\"skip_auto_build\":true"));
        assert!(r.body.contains("\"proof_scope\":\"final_branch\""));
        assert!(r.body.contains("\"match_pr_branch_name\":true"));
    }

    #[test]
    fn guardian_details_renames_and_changes_base_together() {
        let d = daemon();
        let gid = make_guardian(&d);
        let body = serde_json::json!({
            "name": "renamed via details",
            "base_branch": "develop",
        })
        .to_string();
        let r = route(&d, "POST", &format!("/api/guardians/{gid}/details"), &body);
        assert_eq!(r.status, 200, "{}", r.body);
        assert!(r.body.contains("\"name\":\"renamed via details\""));
        assert!(r.body.contains("\"base_branch\":\"develop\""));
    }

    #[test]
    fn guardian_details_frozen_fields_rejected_when_approved_but_others_still_apply() {
        let d = daemon();
        let gid = make_guardian(&d);
        d.lock()
            .set_guardian_status(&gid, crate::guardian::GuardianStatus::Approved, None)
            .unwrap();

        let frozen = serde_json::json!({"base_branch": "develop"}).to_string();
        let r = route(
            &d,
            "POST",
            &format!("/api/guardians/{gid}/details"),
            &frozen,
        );
        assert_eq!(r.status, 409, "{}", r.body);

        let frozen = serde_json::json!({"resolver_agent": "codex"}).to_string();
        let r = route(
            &d,
            "POST",
            &format!("/api/guardians/{gid}/details"),
            &frozen,
        );
        assert_eq!(r.status, 409, "{}", r.body);

        // A non-frozen field on the same (approved) guardian still applies.
        let allowed = serde_json::json!({"name": "still renamable"}).to_string();
        let r = route(
            &d,
            "POST",
            &format!("/api/guardians/{gid}/details"),
            &allowed,
        );
        assert_eq!(r.status, 200, "{}", r.body);
        assert!(r.body.contains("\"name\":\"still renamable\""));
    }

    #[test]
    fn guardian_details_squash_is_a_full_membership_diff() {
        let d = daemon();
        let gid = make_guardian(&d);
        let project = "/repo".to_string();

        let on = serde_json::json!({"squash_projects": [project.clone()]}).to_string();
        let r = route(&d, "POST", &format!("/api/guardians/{gid}/details"), &on);
        assert_eq!(r.status, 200, "{}", r.body);
        let v: serde_json::Value = serde_json::from_str(&r.body).unwrap();
        assert_eq!(
            v["guardian"]["squash_projects"],
            serde_json::json!([project])
        );

        let off = serde_json::json!({"squash_projects": []}).to_string();
        let r = route(&d, "POST", &format!("/api/guardians/{gid}/details"), &off);
        assert_eq!(r.status, 200, "{}", r.body);
        let v: serde_json::Value = serde_json::from_str(&r.body).unwrap();
        assert_eq!(
            v["guardian"]["squash_projects"],
            serde_json::json!([]),
            "an empty desired list turns every project's squash back off"
        );
    }

    #[test]
    fn guardian_details_env_overrides_set_unset_and_clear_in_one_request() {
        let d = daemon();
        let gid = {
            let store = d.lock();
            let id = store.create_guardian("r", "main", "/repo").unwrap();
            store.add_guardian_branch(&id, "feat").unwrap();
            id
        };
        let bid = d.lock().get_guardian(&gid).unwrap().branches[0].id.clone();

        let body = serde_json::json!({
            "build_env": {"set": {"A": "1"}},
            "manual_checks_env": {"set": {"B": "2"}},
            "branch_env": {bid.clone(): {"set": {"C": "3"}}},
        })
        .to_string();
        let r = route(&d, "POST", &format!("/api/guardians/{gid}/details"), &body);
        assert_eq!(r.status, 200, "{}", r.body);
        let v: serde_json::Value = serde_json::from_str(&r.body).unwrap();
        assert_eq!(v["guardian"]["build_env_overrides"]["A"], "1");
        assert_eq!(v["guardian"]["manual_checks_env_overrides"]["B"], "2");
        assert_eq!(v["guardian"]["branches"][0]["env_overrides"]["C"], "3");

        // unset tombstones a key, clear reverts a key back to inherited --
        // exercise both in the same follow-up batch.
        let body = serde_json::json!({
            "build_env": {"unset": ["A"]},
            "manual_checks_env": {"clear": ["B"]},
        })
        .to_string();
        let r = route(&d, "POST", &format!("/api/guardians/{gid}/details"), &body);
        assert_eq!(r.status, 200, "{}", r.body);
        let v: serde_json::Value = serde_json::from_str(&r.body).unwrap();
        assert_eq!(
            v["guardian"]["build_env_overrides"]["A"],
            serde_json::Value::Null
        );
        assert!(
            !v["guardian"]["manual_checks_env_overrides"]
                .as_object()
                .unwrap()
                .contains_key("B")
        );
    }

    #[test]
    fn guardian_details_validates_env_keys_and_values() {
        let d = daemon();
        let gid = make_guardian(&d);

        let bad_key = serde_json::json!({"build_env": {"set": {"NOT VALID": "1"}}}).to_string();
        let r = route(
            &d,
            "POST",
            &format!("/api/guardians/{gid}/details"),
            &bad_key,
        );
        assert_eq!(r.status, 400, "{}", r.body);

        let bad_value =
            serde_json::json!({"build_env": {"set": {"A": "first\necho INJECTED"}}}).to_string();
        let r = route(
            &d,
            "POST",
            &format!("/api/guardians/{gid}/details"),
            &bad_value,
        );
        assert_eq!(r.status, 400, "{}", r.body);

        // The rejected batch must not have partially applied.
        let g = route(&d, "GET", &format!("/api/guardians/{gid}"), "");
        assert!(!g.body.contains("\"A\":\"first"));
    }

    /// RAL-410 core regression: the whole point of one batched endpoint with
    /// one decision point is that a save touching only cosmetic fields must
    /// never restart an in-flight merge, while a save touching a
    /// rebase-relevant field must restart it exactly once -- not once per
    /// field the way the pre-RAL-410 per-field endpoints could. Uses a
    /// branchless guardian so the restart path's eventual `kickoff_merge`
    /// call resolves synchronously (`NoBranches`) instead of racing a spawned
    /// background merge thread -- this test asserts the *decision*, not
    /// whether a real rebase later succeeds (that's `guardian_merge.rs`'s
    /// job, see `settings_change_restarts_a_stuck_merge_and_new_setting_takes_effect`).
    #[test]
    fn guardian_details_restarts_merge_only_when_something_rebase_relevant_changed() {
        let d = daemon();
        let gid = make_guardian(&d);
        d.lock()
            .set_guardian_status(&gid, crate::guardian::GuardianStatus::Merging, None)
            .unwrap();

        // Cosmetic-only batch: must NOT touch merge state at all.
        let cosmetic = serde_json::json!({
            "name": "cosmetic rename",
            "separate_pr_branch": true,
        })
        .to_string();
        let r = route(
            &d,
            "POST",
            &format!("/api/guardians/{gid}/details"),
            &cosmetic,
        );
        assert_eq!(r.status, 200, "{}", r.body);
        let v: serde_json::Value = serde_json::from_str(&r.body).unwrap();
        assert_eq!(
            v["guardian"]["status"], "merging",
            "a cosmetic-only batch must never restart an in-flight merge: {}",
            r.body
        );
        assert!(v.get("base_change").is_none());

        // Rebase-relevant field in the same request: must restart exactly
        // once. `restart_guardian_merge` resets to `collecting` before
        // re-attempting; with no branches the re-attempt fails fast
        // (`NoBranches`) and never re-claims `merging`, so landing on
        // `collecting` here is proof the restart ran exactly once.
        let rebase_relevant = serde_json::json!({"skip_auto_build": true}).to_string();
        let r = route(
            &d,
            "POST",
            &format!("/api/guardians/{gid}/details"),
            &rebase_relevant,
        );
        assert_eq!(r.status, 200, "{}", r.body);
        let v: serde_json::Value = serde_json::from_str(&r.body).unwrap();
        assert_eq!(
            v["guardian"]["status"], "collecting",
            "a rebase-relevant field must trigger the restart path exactly once: {}",
            r.body
        );
    }

    // -----------------------------------------------------------------------
    // RAL-188: GET /api/resolve?uri=
    // -----------------------------------------------------------------------

    /// A squad whose names exercise every addressing case the URI grammar has:
    /// a named cell, a named task proof, and an *anonymous* cell proof
    /// that is only reachable as `PROOF[~0]`.
    const URI_RUN: &str = "\
[[task]]
name=\"ral-178\"
[[task.cell]]
name=\"work\"
cwd=\"/r\"
prompt=\"p\"
[[task.cell.proof]]
command=\"lint\"
[[task.proof]]
id=\"test\"
command=\"cargo test\"
";

    /// Submit `URI_RUN` and return its squad id.
    fn uri_squad(d: &Daemon) -> String {
        let r = route(d, "POST", "/api/squads", &submit_body(URI_RUN));
        assert_eq!(r.status, 201, "{}", r.body);
        let v: serde_json::Value = serde_json::from_str(&r.body).unwrap();
        v["squad_id"].as_str().unwrap().to_string()
    }

    fn resolve(d: &Daemon, uri: &str) -> Reply {
        route(d, "GET", &format!("/api/resolve?uri={uri}"), "")
    }

    #[test]
    fn resolve_uri_maps_a_cell_uri_to_positional_coordinates() {
        let d = daemon();
        let id = uri_squad(&d);
        let r = resolve(
            &d,
            &format!("ralphus:/SQUAD[{id}]/TASK[ral-178]/CELL[work]?id={id}"),
        );
        assert_eq!(r.status, 200, "{}", r.body);
        let v: serde_json::Value = serde_json::from_str(&r.body).unwrap();
        assert_eq!(v["kind"], "cell");
        assert_eq!(v["squad_id"], id);
        assert_eq!(v["task_idx"], 0);
        assert_eq!(v["cell_idx"], 0);
    }

    #[test]
    fn resolve_uri_addresses_a_task_scope_proof_by_its_id() {
        let d = daemon();
        let id = uri_squad(&d);
        let r = resolve(
            &d,
            &format!("ralphus:/SQUAD[{id}]/TASK[ral-178]/PROOF[test]?id={id}"),
        );
        assert_eq!(r.status, 200, "{}", r.body);
        let v: serde_json::Value = serde_json::from_str(&r.body).unwrap();
        assert_eq!(v["kind"], "proof");
        assert_eq!(v["proof_scope"], "task");
        assert_eq!(v["proof_idx"], 0);
        assert_eq!(v["cell_idx"], serde_json::Value::Null);
    }

    #[test]
    fn resolve_uri_addresses_an_anonymous_proof_only_by_its_positional_index() {
        let d = daemon();
        let id = uri_squad(&d);
        let base = format!("ralphus:/SQUAD[{id}]/TASK[ral-178]/CELL[work]");
        let r = resolve(&d, &format!("{base}/PROOF[~0]?id={id}"));
        assert_eq!(r.status, 200, "{}", r.body);
        let v: serde_json::Value = serde_json::from_str(&r.body).unwrap();
        assert_eq!(v["proof_scope"], "cell");
        assert_eq!(v["proof_idx"], 0);
        // The step has no author-supplied id, so it has no name to match --
        // a bare `PROOF[0]` is a *name* lookup and must not silently hit it.
        assert_eq!(resolve(&d, &format!("{base}/PROOF[0]?id={id}")).status, 409);
    }

    #[test]
    fn resolve_uri_echoes_the_canonical_form_with_the_id_sidecar() {
        let d = daemon();
        let id = uri_squad(&d);
        // Addressed by id (the squad has no label), no `?id=` sidecar supplied.
        let r = resolve(&d, &format!("ralphus:/SQUAD[{id}]/TASK[ral-178]"));
        assert_eq!(r.status, 200, "{}", r.body);
        let v: serde_json::Value = serde_json::from_str(&r.body).unwrap();
        assert_eq!(
            v["uri"],
            format!("ralphus:/SQUAD[{id}]/TASK[ral-178]?id={id}")
        );
    }

    #[test]
    fn resolve_uri_rejects_a_malformed_uri_with_400_and_a_missing_squad_with_404() {
        let d = daemon();
        assert_eq!(resolve(&d, "ralphus:/SQUAD[unterminated").status, 400);
        assert_eq!(resolve(&d, "ralphus:/BOGUS[x]").status, 400);
        assert_eq!(route(&d, "GET", "/api/resolve", "").status, 400);
        assert_eq!(resolve(&d, "ralphus:/SQUAD[nope]").status, 404);
    }

    #[test]
    fn resolve_uri_reports_an_unknown_name_rather_than_guessing() {
        let d = daemon();
        let id = uri_squad(&d);
        let r = resolve(&d, &format!("ralphus:/SQUAD[{id}]/TASK[nope]?id={id}"));
        assert_eq!(r.status, 409, "{}", r.body);
        assert!(r.body.contains("ral-178"), "{}", r.body);
    }

    #[test]
    fn resolve_uri_accepts_a_percent_encoded_uri_query_value() {
        let d = daemon();
        let id = uri_squad(&d);
        let raw = format!("ralphus:/SQUAD[{id}]/TASK[ral-178]?id={id}");
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
            uri_query_value("mode=readonly&uri=ralphus:/SQUAD[r]"),
            Some("ralphus:/SQUAD[r]")
        );
        assert_eq!(uri_query_value("mode=readonly"), None);
    }

    // ── Triage type registry routes (RAL-318) ────────────────────────────────

    #[test]
    fn triage_type_routes_register_list_get_deregister() {
        let d = daemon();
        let r = route(&d, "GET", "/api/triage/types", "");
        assert_eq!(r.status, 200, "{}", r.body);
        assert!(r.body.contains(crate::triage::UNCLASSIFIED_TYPE));

        let r = route(
            &d,
            "POST",
            "/api/triage/types",
            r#"{"name":"security","label":"Security","description":"sensitive changes"}"#,
        );
        assert_eq!(r.status, 201, "{}", r.body);

        let r = route(&d, "GET", "/api/triage/types/security", "");
        assert_eq!(r.status, 200, "{}", r.body);
        assert!(r.body.contains("sensitive changes"));

        let r = route(&d, "GET", "/api/triage/types/nope", "");
        assert_eq!(r.status, 404);

        let r = route(&d, "DELETE", "/api/triage/types/security", "");
        assert_eq!(r.status, 200, "{}", r.body);
        let r = route(&d, "GET", "/api/triage/types/security", "");
        assert_eq!(r.status, 404);
    }

    #[test]
    fn triage_type_register_rejects_empty_name() {
        let d = daemon();
        let r = route(&d, "POST", "/api/triage/types", r#"{"name":"  "}"#);
        assert_eq!(r.status, 400);
        assert!(r.body.contains("invalid_value"));
    }

    #[test]
    fn triage_type_deregister_refuses_the_builtin_unclassified_type() {
        let d = daemon();
        let r = route(
            &d,
            "DELETE",
            &format!("/api/triage/types/{}", crate::triage::UNCLASSIFIED_TYPE),
            "",
        );
        assert_eq!(r.status, 400);
        assert!(r.body.contains("never be deregistered"));
    }

    #[test]
    fn triage_pool_and_schedule_routes() {
        let d = daemon();
        // Empty at first.
        let r = route(&d, "GET", "/api/triage/pools", "");
        assert_eq!(r.status, 200, "{}", r.body);
        assert!(r.body.contains("\"pools\":[]"));

        // Populate a pool row directly via the store (submit-time pooling is
        // exercised in `reviews.rs`'s own tests).
        d.lock()
            .record_triage_pool_cell("proj", "security", "squad-1", 0, 0, "b1", "main")
            .unwrap();
        let r = route(&d, "GET", "/api/triage/pools", "");
        assert_eq!(r.status, 200, "{}", r.body);
        assert!(r.body.contains("\"count\":1"));

        let r = route(
            &d,
            "POST",
            "/api/triage/pools/threshold",
            r#"{"project":"proj","triage_type":"security","threshold":3}"#,
        );
        assert_eq!(r.status, 200, "{}", r.body);
        let r = route(&d, "GET", "/api/triage/pools", "");
        assert!(r.body.contains("\"threshold\":3"));

        let r = route(
            &d,
            "POST",
            "/api/triage/schedules",
            r#"{"project":"proj","triage_type":"security","cron_expr":"0 0 0 * * *","anchor_date_ms":0,"every_n":2}"#,
        );
        assert_eq!(r.status, 201, "{}", r.body);
        let id = serde_json::from_str::<serde_json::Value>(&r.body).unwrap()["id"]
            .as_i64()
            .unwrap();

        let r = route(
            &d,
            "GET",
            "/api/triage/schedules?project=proj&triage_type=security",
            "",
        );
        assert_eq!(r.status, 200, "{}", r.body);
        assert!(r.body.contains("\"every_n\":2"));

        let r = route(&d, "DELETE", &format!("/api/triage/schedules/{id}"), "");
        assert_eq!(r.status, 200, "{}", r.body);
        let r = route(&d, "DELETE", &format!("/api/triage/schedules/{id}"), "");
        assert_eq!(r.status, 404);
    }

    /// RAL-374: setting a threshold via `POST /api/triage/pools/threshold`
    /// with a registered project's filesystem *path* (not its name) must
    /// land on the exact same `(project, triage_type)` row a real pooled
    /// cell for that project uses -- not a second, disconnected, path-keyed
    /// row that the pool count check in `derive_triage_pools` never sees.
    #[test]
    fn triage_pool_threshold_route_resolves_a_project_path_to_its_registered_name() {
        let d = daemon();
        let dir = std::env::temp_dir().join(format!(
            "ral374-server-threshold-by-path-{}-{}",
            std::process::id(),
            PROJ_TEST_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        d.lock()
            .register_project("proj", "", &dir.to_string_lossy(), "none")
            .unwrap();

        // The same key submit-time pooling would use for a cell whose
        // worktree root is this project's path.
        let expected_key = crate::triage::pool_key_for_path(&d.lock(), &dir);
        assert_eq!(expected_key, "proj");

        // Set the threshold via the raw path, exactly what the CLI's
        // `ralphus triage pool threshold <project>` positional argument
        // accepts with no validation.
        let r = route(
            &d,
            "POST",
            "/api/triage/pools/threshold",
            &serde_json::json!({
                "project": dir.to_string_lossy(),
                "triage_type": "bug",
                "threshold": 3,
            })
            .to_string(),
        );
        assert_eq!(r.status, 200, "{}", r.body);

        // A cell pooled under the project's resolved name must land in the
        // very same row the path-set threshold configured.
        d.lock()
            .record_triage_pool_cell("proj", "bug", "squad-1", 0, 0, "b1", "main")
            .unwrap();

        let r = route(&d, "GET", "/api/triage/pools", "");
        assert_eq!(r.status, 200, "{}", r.body);
        let v: serde_json::Value = serde_json::from_str(&r.body).unwrap();
        let pools = v["pools"].as_array().unwrap();
        assert_eq!(
            pools.len(),
            1,
            "path-set threshold and the resolved-name pooled cell must be one row, not two: {}",
            r.body
        );
        assert_eq!(pools[0]["project"], "proj");
        assert_eq!(pools[0]["count"], 1);
        assert_eq!(pools[0]["threshold"], 3);
    }

    #[test]
    fn triage_candidates_route_lists_an_unresolved_cell() {
        let d = daemon();
        let r = route(&d, "GET", "/api/triage/candidates", "");
        assert_eq!(r.status, 200, "{}", r.body);
        assert!(r.body.contains("\"candidates\":[]"));

        let src = "[[task]]\nname=\"t\"\nproject=\"proj\"\n[[task.cell]]\ncwd=\"/repo\"\nprompt=\"p\"\ntriage=true\ntriage_type=\"security\"\n";
        let file: ralphus_core::schema::TaskFile = toml::from_str(src).unwrap();
        let squad_id = {
            let mut guard = d.lock();
            let squad_id = guard.insert_squad(&file, None, false).unwrap();
            guard
                .set_cell_triage_types(&squad_id, 0, 0, &["security".to_string()])
                .unwrap();
            squad_id
        };

        let r = route(&d, "GET", "/api/triage/candidates", "");
        assert_eq!(r.status, 200, "{}", r.body);
        assert!(r.body.contains(&squad_id));
        assert!(r.body.contains("\"project\":\"proj\""));
        assert!(r.body.contains("\"triage_types\":[\"security\"]"));

        // Once linked to a review, it drops off the candidate list.
        d.lock()
            .set_cell_review_guardian(&squad_id, 0, 0, "guardian-1")
            .unwrap();
        let r = route(&d, "GET", "/api/triage/candidates", "");
        assert!(r.body.contains("\"candidates\":[]"));
    }

    #[test]
    fn triage_pools_lists_a_threshold_configured_before_any_cell_is_pooled() {
        let d = daemon();
        // Setting a threshold on a (project, type) key with zero pooled
        // cells must still make it appear in GET /api/triage/pools -- a
        // count-based trigger configured ahead of the first pooled cell
        // (e.g. "fire every 4 bug fixes for this project") must be visible
        // and editable, the same way a cron schedule already is.
        let r = route(
            &d,
            "POST",
            "/api/triage/pools/threshold",
            r#"{"project":"proj","triage_type":"bug","threshold":4}"#,
        );
        assert_eq!(r.status, 200, "{}", r.body);
        let r = route(&d, "GET", "/api/triage/pools", "");
        assert_eq!(r.status, 200, "{}", r.body);
        assert!(r.body.contains("\"project\":\"proj\""), "{}", r.body);
        assert!(r.body.contains("\"triage_type\":\"bug\""), "{}", r.body);
        assert!(r.body.contains("\"count\":0"), "{}", r.body);
        assert!(r.body.contains("\"threshold\":4"), "{}", r.body);
    }

    #[test]
    fn add_triage_schedule_route_rejects_invalid_cron() {
        let d = daemon();
        let r = route(
            &d,
            "POST",
            "/api/triage/schedules",
            r#"{"project":"proj","triage_type":"security","cron_expr":"nope","anchor_date_ms":0}"#,
        );
        assert_eq!(r.status, 400);
    }

    // `POST /api/health/arbiter` itself is deliberately not route-tested here:
    // with no `[arbiter]` config, `crate::arbiter::Arbiter::current()`
    // resolves to the "ollama" default, and exercising the route for real
    // would make a live network call to Ollama -- this crate's convention
    // (`AGENTS.md`) is that any such call must be `#[ignore]`d, gated, and
    // never on by default. `crate::arbiter::health_check`'s own unit tests
    // (`arbiter.rs`) already cover the cap-reached and unsupported-backend
    // paths without touching the network; this endpoint is a thin,
    // one-line `match` over that function.

    // ── Remote terminal-relay ticket minting (RAL-355 Phase 10) ─────────────

    /// `machine="incredibuild:A"`, `agent="claude-code"`, `id="a"` -- submits
    /// and returns the deterministic first-squad id, so callers only need to
    /// layer in an `agent_session_id` (via `set_cell_agent_session_id_live`)
    /// and/or a relay port before minting a ticket.
    fn submit_remote_claude_cell(d: &Daemon) -> String {
        route(
            d,
            "POST",
            "/api/machines",
            &machine_body("incredibuild", "/opt/ib.sh"),
        );
        let toml = "[[task]]\nname=\"t\"\nmachine=\"incredibuild:A\"\n\
                     [[task.cell]]\nid=\"a\"\ncwd=\".\"\nagent=\"claude-code\"\nprompt=\"p\"\n";
        let r = route(d, "POST", "/api/squads", &submit_body(toml));
        assert_eq!(r.status, 201, "{}", r.body);
        "squad-000000000001".to_string()
    }

    #[test]
    fn terminal_session_slot_rejects_a_second_concurrent_holder_but_allows_reacquire_after_release()
    {
        let d = daemon();
        assert!(d.try_acquire_terminal_session("squad-1", 0, 0));
        assert!(!d.try_acquire_terminal_session("squad-1", 0, 0));
        // A different cell is independent -- not blocked by the first hold.
        assert!(d.try_acquire_terminal_session("squad-1", 0, 1));
        d.release_terminal_session("squad-1", 0, 0);
        assert!(d.try_acquire_terminal_session("squad-1", 0, 0));
    }

    #[test]
    fn terminal_ticket_rejects_a_still_running_cell() {
        let d = daemon();
        let squad_id = submit_remote_claude_cell(&d);
        d.lock()
            .set_cell_agent_session_id_live(&squad_id, "t", "a", "sess-123")
            .unwrap();
        let r = route(
            &d,
            "POST",
            &format!("/api/squads/{squad_id}/set-status"),
            &serde_json::json!({
                "state": "running", "kind": "cell", "task_idx": 0,
                "cell_idx": 0, "proof_idx": -1, "proof_scope": "",
            })
            .to_string(),
        );
        assert_eq!(r.status, 200, "{}", r.body);
        let r = route(
            &d,
            "POST",
            &format!("/api/squads/{squad_id}/cells/0/0/terminal-ticket"),
            "",
        );
        assert_eq!(r.status, 409, "{}", r.body);
        assert!(r.body.contains("still_running"), "{}", r.body);
    }

    #[test]
    fn terminal_ticket_rejects_a_local_cell() {
        let d = daemon();
        let r = route(&d, "POST", "/api/squads", &submit_body(GOOD));
        assert_eq!(r.status, 201, "{}", r.body);
        let r = route(
            &d,
            "POST",
            "/api/squads/squad-000000000001/cells/0/0/terminal-ticket",
            "",
        );
        assert_eq!(r.status, 400, "{}", r.body);
        assert!(r.body.contains("not_remote"), "{}", r.body);
    }

    #[test]
    fn terminal_ticket_rejects_a_non_claude_agent() {
        let d = daemon();
        route(
            &d,
            "POST",
            "/api/machines",
            &machine_body("incredibuild", "/opt/ib.sh"),
        );
        let toml = "[[task]]\nname=\"t\"\nmachine=\"incredibuild:A\"\n\
                     [[task.cell]]\nid=\"a\"\ncwd=\".\"\nagent=\"codex\"\nprompt=\"p\"\n";
        let r = route(&d, "POST", "/api/squads", &submit_body(toml));
        assert_eq!(r.status, 201, "{}", r.body);
        let r = route(
            &d,
            "POST",
            "/api/squads/squad-000000000001/cells/0/0/terminal-ticket",
            "",
        );
        assert_eq!(r.status, 400, "{}", r.body);
        assert!(r.body.contains("unsupported_agent"), "{}", r.body);
    }

    #[test]
    fn terminal_ticket_requires_a_resumable_session() {
        let d = daemon();
        let squad_id = submit_remote_claude_cell(&d);
        let r = route(
            &d,
            "POST",
            &format!("/api/squads/{squad_id}/cells/0/0/terminal-ticket"),
            "",
        );
        assert_eq!(r.status, 409, "{}", r.body);
        assert!(r.body.contains("no_claude_session"), "{}", r.body);
    }

    #[test]
    fn terminal_ticket_reports_relay_unavailable_when_the_listener_never_started() {
        let d = daemon();
        let squad_id = submit_remote_claude_cell(&d);
        d.lock()
            .set_cell_agent_session_id_live(&squad_id, "t", "a", "sess-123")
            .unwrap();
        let r = route(
            &d,
            "POST",
            &format!("/api/squads/{squad_id}/cells/0/0/terminal-ticket"),
            "",
        );
        assert_eq!(r.status, 503, "{}", r.body);
        assert!(r.body.contains("relay_unavailable"), "{}", r.body);
    }

    #[test]
    fn terminal_ticket_mints_once_every_check_passes() {
        let d = daemon();
        let squad_id = submit_remote_claude_cell(&d);
        d.lock()
            .set_cell_agent_session_id_live(&squad_id, "t", "a", "sess-123")
            .unwrap();
        d.set_terminal_relay_port(7891);
        let r = route(
            &d,
            "POST",
            &format!("/api/squads/{squad_id}/cells/0/0/terminal-ticket"),
            "",
        );
        assert_eq!(r.status, 200, "{}", r.body);
        assert!(r.body.contains("\"port\":7891"), "{}", r.body);
        assert!(r.body.contains("\"path\":\"/terminal\""), "{}", r.body);
        let parsed: serde_json::Value = serde_json::from_str(&r.body).unwrap();
        assert!(
            parsed["ticket"].as_str().is_some_and(|t| !t.is_empty()),
            "{}",
            r.body
        );
    }

    #[test]
    fn terminal_ticket_rejects_a_bad_task_or_cell_index() {
        let d = daemon();
        let squad_id = submit_remote_claude_cell(&d);
        let r = route(
            &d,
            "POST",
            &format!("/api/squads/{squad_id}/cells/nope/0/terminal-ticket"),
            "",
        );
        assert_eq!(r.status, 400, "{}", r.body);
    }

    // ── Monitor watches + notification preferences ─────────────────────────

    #[test]
    fn watches_require_an_acting_user() {
        let d = daemon();
        let body =
            serde_json::to_string(&serde_json::json!({"entity_uri": "squad:squad-1"})).unwrap();
        assert_eq!(route(&d, "GET", "/api/watches", "").status, 400);
        assert_eq!(route(&d, "POST", "/api/watches", &body).status, 400);
    }

    #[test]
    fn watches_reject_an_unrecognized_entity_uri() {
        let d = daemon();
        let body = serde_json::to_string(&serde_json::json!({"entity_uri": "not-a-uri"})).unwrap();
        let r = route(&d, "POST", "/api/watches?user=colin", &body);
        assert_eq!(r.status, 400, "{}", r.body);
    }

    #[test]
    fn watches_accept_task_and_cell_entity_uris() {
        let d = daemon();
        for entity_uri in ["task:squad-1:0", "cell:squad-1:0:1"] {
            let body =
                serde_json::to_string(&serde_json::json!({ "entity_uri": entity_uri })).unwrap();
            let r = route(&d, "POST", "/api/watches?user=colin", &body);
            assert_eq!(r.status, 201, "{}", r.body);
        }
        let list = route(&d, "GET", "/api/watches?user=colin", "");
        let parsed: serde_json::Value = serde_json::from_str(&list.body).unwrap();
        let uris: Vec<&str> = parsed["watches"]
            .as_array()
            .unwrap()
            .iter()
            .map(|w| w["entity_uri"].as_str().unwrap())
            .collect();
        assert!(uris.contains(&"task:squad-1:0"), "{uris:?}");
        assert!(uris.contains(&"cell:squad-1:0:1"), "{uris:?}");
    }

    #[test]
    fn watches_reject_a_proof_entity_uri() {
        let d = daemon();
        let body = serde_json::to_string(&serde_json::json!({
            "entity_uri": "proof:squad-1:0:task:-1:0"
        }))
        .unwrap();
        let r = route(&d, "POST", "/api/watches?user=colin", &body);
        assert_eq!(r.status, 400, "{}", r.body);
        assert!(r.body.contains("only squads, tasks, cells, and reviews"));
    }

    #[test]
    fn watch_lifecycle_over_http() {
        let d = daemon();
        let squad_id = submit_squad(&d);
        let entity_uri = format!("squad:{squad_id}");

        let body = serde_json::to_string(&serde_json::json!({
            "entity_uri": entity_uri,
            "notify_tiers": ["urgent", "high"],
        }))
        .unwrap();
        let r = route(&d, "POST", "/api/watches?user=colin", &body);
        assert_eq!(r.status, 201, "{}", r.body);

        let list = route(&d, "GET", "/api/watches?user=colin", "");
        assert_eq!(list.status, 200, "{}", list.body);
        let parsed: serde_json::Value = serde_json::from_str(&list.body).unwrap();
        let watches = parsed["watches"].as_array().unwrap();
        assert_eq!(watches.len(), 1);
        assert_eq!(watches[0]["entity_uri"], entity_uri);

        // A watch is scoped per user -- someone else's list stays empty.
        let others_list = route(&d, "GET", "/api/watches?user=alex", "");
        let others: serde_json::Value = serde_json::from_str(&others_list.body).unwrap();
        assert!(others["watches"].as_array().unwrap().is_empty());

        let delete = route(
            &d,
            "DELETE",
            &format!("/api/watches/{entity_uri}?user=colin"),
            "",
        );
        assert_eq!(delete.status, 200, "{}", delete.body);
        let after: serde_json::Value =
            serde_json::from_str(&route(&d, "GET", "/api/watches?user=colin", "").body).unwrap();
        assert!(after["watches"].as_array().unwrap().is_empty());

        // Deleting again is a 404, not a repeat success.
        assert_eq!(
            route(
                &d,
                "DELETE",
                &format!("/api/watches/{entity_uri}?user=colin"),
                "",
            )
            .status,
            404
        );
    }

    #[test]
    fn personal_mailbox_filters_by_watched_notify_tiers() {
        let d = daemon();
        let squad_id = submit_squad(&d);
        let entity_uri = format!("squad:{squad_id}");
        let body = serde_json::to_string(&serde_json::json!({
            "entity_uri": entity_uri,
            "notify_tiers": ["urgent"],
        }))
        .unwrap();
        assert_eq!(
            route(&d, "POST", "/api/watches?user=colin", &body).status,
            201
        );

        d.lock()
            .enqueue_mailbox_message(
                crate::mailbox::MailboxPriority::Urgent,
                "urgent thing happened",
                Some(squad_id.as_str()),
                None,
                None,
                Some(entity_uri.as_str()),
            )
            .unwrap();
        d.lock()
            .enqueue_mailbox_message(
                crate::mailbox::MailboxPriority::Normal,
                "normal thing happened",
                Some(squad_id.as_str()),
                None,
                None,
                Some(entity_uri.as_str()),
            )
            .unwrap();

        let r = route(&d, "GET", "/api/mailbox/personal/messages?user=colin", "");
        assert_eq!(r.status, 200, "{}", r.body);
        let messages: Vec<serde_json::Value> = serde_json::from_str(&r.body).unwrap();
        assert_eq!(messages.len(), 1, "{}", r.body);
        assert_eq!(messages[0]["message"], "urgent thing happened");

        // Someone with no watches at all sees nothing, even unfiltered.
        let empty = route(&d, "GET", "/api/mailbox/personal/messages?user=alex", "");
        let empty_messages: Vec<serde_json::Value> = serde_json::from_str(&empty.body).unwrap();
        assert!(empty_messages.is_empty());
    }

    #[test]
    fn personal_mailbox_cascades_from_a_watched_parent() {
        let d = daemon();
        let squad_id = submit_squad(&d);
        // Watch the whole squad; a message scoped to one of its cells should
        // still reach the watcher -- `EntityUri::covers` cascade.
        let body = serde_json::to_string(&serde_json::json!({
            "entity_uri": format!("squad:{squad_id}"),
        }))
        .unwrap();
        assert_eq!(
            route(&d, "POST", "/api/watches?user=colin", &body).status,
            201
        );

        let cell_uri = format!("cell:{squad_id}:0:0");
        d.lock()
            .enqueue_mailbox_message(
                crate::mailbox::MailboxPriority::High,
                "cell 0/0 failed",
                Some(squad_id.as_str()),
                None,
                None,
                Some(cell_uri.as_str()),
            )
            .unwrap();

        let r = route(&d, "GET", "/api/mailbox/personal/messages?user=colin", "");
        let messages: Vec<serde_json::Value> = serde_json::from_str(&r.body).unwrap();
        assert_eq!(messages.len(), 1, "{}", r.body);
        assert_eq!(messages[0]["entity_uri"], cell_uri);
    }

    #[test]
    fn personal_mailbox_task_watch_cascades_to_its_cells_but_not_sibling_tasks() {
        let d = daemon();
        let squad_id = submit_squad(&d);
        let body = serde_json::to_string(&serde_json::json!({
            "entity_uri": format!("task:{squad_id}:0"),
        }))
        .unwrap();
        assert_eq!(
            route(&d, "POST", "/api/watches?user=colin", &body).status,
            201
        );

        d.lock()
            .enqueue_mailbox_message(
                crate::mailbox::MailboxPriority::High,
                "task 0 cell 0 failed",
                Some(squad_id.as_str()),
                None,
                None,
                Some(format!("cell:{squad_id}:0:0").as_str()),
            )
            .unwrap();
        d.lock()
            .enqueue_mailbox_message(
                crate::mailbox::MailboxPriority::High,
                "task 1 cell 0 failed",
                Some(squad_id.as_str()),
                None,
                None,
                Some(format!("cell:{squad_id}:1:0").as_str()),
            )
            .unwrap();

        let r = route(&d, "GET", "/api/mailbox/personal/messages?user=colin", "");
        let messages: Vec<serde_json::Value> = serde_json::from_str(&r.body).unwrap();
        assert_eq!(messages.len(), 1, "{}", r.body);
        assert_eq!(messages[0]["message"], "task 0 cell 0 failed");
    }

    #[test]
    fn personal_mailbox_cell_watch_does_not_cascade_to_sibling_cells() {
        let d = daemon();
        let squad_id = submit_squad(&d);
        let body = serde_json::to_string(&serde_json::json!({
            "entity_uri": format!("cell:{squad_id}:0:0"),
        }))
        .unwrap();
        assert_eq!(
            route(&d, "POST", "/api/watches?user=colin", &body).status,
            201
        );

        d.lock()
            .enqueue_mailbox_message(
                crate::mailbox::MailboxPriority::High,
                "cell 0/0 failed",
                Some(squad_id.as_str()),
                None,
                None,
                Some(format!("cell:{squad_id}:0:0").as_str()),
            )
            .unwrap();
        d.lock()
            .enqueue_mailbox_message(
                crate::mailbox::MailboxPriority::High,
                "cell 0/1 failed",
                Some(squad_id.as_str()),
                None,
                None,
                Some(format!("cell:{squad_id}:0:1").as_str()),
            )
            .unwrap();

        let r = route(&d, "GET", "/api/mailbox/personal/messages?user=colin", "");
        let messages: Vec<serde_json::Value> = serde_json::from_str(&r.body).unwrap();
        assert_eq!(messages.len(), 1, "{}", r.body);
        assert_eq!(messages[0]["message"], "cell 0/0 failed");
    }

    #[test]
    fn personal_mailbox_drain_marks_messages_read_for_that_user_only() {
        let d = daemon();
        let squad_id = submit_squad(&d);
        let entity_uri = format!("squad:{squad_id}");
        for user in ["colin", "alex"] {
            let body =
                serde_json::to_string(&serde_json::json!({"entity_uri": entity_uri})).unwrap();
            assert_eq!(
                route(&d, "POST", &format!("/api/watches?user={user}"), &body).status,
                201
            );
        }
        d.lock()
            .enqueue_mailbox_message(
                crate::mailbox::MailboxPriority::Urgent,
                "urgent thing",
                Some(squad_id.as_str()),
                None,
                None,
                Some(entity_uri.as_str()),
            )
            .unwrap();

        let drain = route(&d, "POST", "/api/mailbox/personal/drain?user=colin", "");
        assert_eq!(drain.status, 200, "{}", drain.body);
        let drained: serde_json::Value = serde_json::from_str(&drain.body).unwrap();
        assert_eq!(drained["drained"], 1);

        let colin_unread = route(
            &d,
            "GET",
            "/api/mailbox/personal/messages?user=colin&unread=1",
            "",
        );
        let colin_msgs: Vec<serde_json::Value> = serde_json::from_str(&colin_unread.body).unwrap();
        assert!(colin_msgs.is_empty(), "{}", colin_unread.body);

        // Draining as colin must not affect alex's own read state.
        let alex_unread = route(
            &d,
            "GET",
            "/api/mailbox/personal/messages?user=alex&unread=1",
            "",
        );
        let alex_msgs: Vec<serde_json::Value> = serde_json::from_str(&alex_unread.body).unwrap();
        assert_eq!(alex_msgs.len(), 1, "{}", alex_unread.body);
    }

    #[test]
    fn user_preferences_round_trip_over_http() {
        let d = daemon();
        let body = serde_json::to_string(&serde_json::json!({
            "auto_watch": true,
            "default_notify_tiers": ["urgent", "high"],
        }))
        .unwrap();
        let set = route(&d, "POST", "/api/users/colin/preferences", &body);
        assert_eq!(set.status, 200, "{}", set.body);

        let get = route(&d, "GET", "/api/users/colin/preferences", "");
        assert_eq!(get.status, 200, "{}", get.body);
        let parsed: serde_json::Value = serde_json::from_str(&get.body).unwrap();
        assert_eq!(parsed["auto_watch"], true);
        assert_eq!(
            parsed["default_notify_tiers"],
            serde_json::json!(["urgent", "high"])
        );

        assert_eq!(
            route(&d, "GET", "/api/users/nobody/preferences", "").status,
            404
        );
    }

    #[test]
    fn auto_watch_on_submit_uses_the_acting_users_preferences() {
        let d = daemon();
        let prefs = serde_json::to_string(&serde_json::json!({
            "auto_watch": true,
            "default_notify_tiers": ["urgent"],
        }))
        .unwrap();
        assert_eq!(
            route(&d, "POST", "/api/users/colin/preferences", &prefs).status,
            200
        );

        let r = route(&d, "POST", "/api/squads?user=colin", &submit_body(GOOD));
        assert_eq!(r.status, 201, "{}", r.body);
        let submitted: serde_json::Value = serde_json::from_str(&r.body).unwrap();
        let squad_id = submitted["squad_id"].as_str().unwrap();

        let list = route(&d, "GET", "/api/watches?user=colin", "");
        let parsed: serde_json::Value = serde_json::from_str(&list.body).unwrap();
        let watches = parsed["watches"].as_array().unwrap();
        assert_eq!(watches.len(), 1, "{}", list.body);
        assert_eq!(watches[0]["entity_uri"], format!("squad:{squad_id}"));
        assert_eq!(watches[0]["notify_tiers"], serde_json::json!(["urgent"]));
    }

    #[test]
    fn submit_without_auto_watch_creates_no_watch() {
        let d = daemon();
        // colin isn't registered at all -- auto-watch-on-submit must be a
        // silent no-op, not an error, for an anonymous/unregistered submitter.
        let r = route(&d, "POST", "/api/squads?user=colin", &submit_body(GOOD));
        assert_eq!(r.status, 201, "{}", r.body);
        let list = route(&d, "GET", "/api/watches?user=colin", "");
        let parsed: serde_json::Value = serde_json::from_str(&list.body).unwrap();
        assert!(parsed["watches"].as_array().unwrap().is_empty());
    }

    // ── apply_default_user_admin (RAL-332) ──────────────────────────────

    #[test]
    fn default_user_is_admin_unset_is_a_noop() {
        let store = Store::open_in_memory().unwrap();
        let cfg = crate::config::DaemonConfig {
            default_user: Some("colin".to_string()),
            ..Default::default()
        };
        apply_default_user_admin(&store, &cfg);
        assert!(store.get_user("colin").unwrap().is_none());
    }

    #[test]
    fn default_user_is_admin_true_registers_and_promotes() {
        let store = Store::open_in_memory().unwrap();
        let cfg = crate::config::DaemonConfig {
            default_user: Some("colin".to_string()),
            default_user_is_admin: Some(true),
            ..Default::default()
        };
        apply_default_user_admin(&store, &cfg);
        assert!(store.get_user("colin").unwrap().unwrap().is_admin);

        // Idempotent: a second application with no state change touches
        // neither the DB nor errors.
        apply_default_user_admin(&store, &cfg);
        assert!(store.get_user("colin").unwrap().unwrap().is_admin);
    }

    #[test]
    fn default_user_is_admin_false_demotes_an_existing_admin() {
        let store = Store::open_in_memory().unwrap();
        store.create_user("colin").unwrap();
        store.set_user_admin("colin", true).unwrap();
        let cfg = crate::config::DaemonConfig {
            default_user: Some("colin".to_string()),
            default_user_is_admin: Some(false),
            ..Default::default()
        };
        apply_default_user_admin(&store, &cfg);
        assert!(!store.get_user("colin").unwrap().unwrap().is_admin);
    }

    #[test]
    fn default_user_is_admin_false_never_registers_an_unregistered_user() {
        let store = Store::open_in_memory().unwrap();
        let cfg = crate::config::DaemonConfig {
            default_user: Some("colin".to_string()),
            default_user_is_admin: Some(false),
            ..Default::default()
        };
        apply_default_user_admin(&store, &cfg);
        assert!(store.get_user("colin").unwrap().is_none());
    }

    #[test]
    fn default_user_is_admin_true_without_default_user_is_a_noop() {
        let store = Store::open_in_memory().unwrap();
        let cfg = crate::config::DaemonConfig {
            default_user_is_admin: Some(true),
            ..Default::default()
        };
        apply_default_user_admin(&store, &cfg);
        assert!(store.list_users().unwrap().is_empty());
    }
}
