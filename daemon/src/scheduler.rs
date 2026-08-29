//! The scheduler: turns Pending squads into real work.
//!
//! A single scheduler thread polls the store; each `tick` claims up to the
//! available concurrency slots of ready squads, marks each Running, and hands it
//! to a worker thread. The worker executes the squad's cells via the [`Runner`]
//! (the subprocess wait happens *outside* the store lock, so many squads progress
//! concurrently), records each result, and finalizes task/squad states.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use opentelemetry::trace::SpanKind;

use crate::cancel::{CancelToken, Cancellations};
use crate::otel;
use crate::runner::{Runner, RunnerResult, RunnerSpec, SubprocessRunner};
use crate::store::{CellOutcome, NodeState, SquadState, Store};

/// How long a worker holds nothing; the poll interval between ticks.
pub const POLL_INTERVAL: Duration = Duration::from_millis(200);

/// How often to check reviews for a base-branch shift and auto-rebuild them.
pub const REVIEW_MAINT_INTERVAL: Duration = Duration::from_secs(5);

/// How often to sweep for guardians whose debounced final change-summary
/// regen request (RAL-208) has gone quiet long enough to fire the LLM call.
/// Finer-grained than [`REVIEW_MAINT_INTERVAL`] since it's checked against
/// `guardian_merge::FINAL_SUMMARY_DEBOUNCE_MS`, a much shorter window.
pub const SUMMARY_SWEEP_INTERVAL: Duration = Duration::from_secs(1);

/// How often to enforce Cartographer's retention caps (RAL-98). Pruning is
/// cheap (indexed deletes) so a coarse interval is fine.
pub const CARTOGRAPHER_PRUNE_INTERVAL: Duration = Duration::from_secs(600);

/// How often to enforce the durable terminal-log retention caps (RAL-154).
/// Same cadence as [`CARTOGRAPHER_PRUNE_INTERVAL`] — a filesystem walk over a
/// bounded `max_files` cap is cheap enough that a coarse interval is fine
/// here too.
pub const TERMINAL_LOG_PRUNE_INTERVAL: Duration = Duration::from_secs(600);

/// RAL-288: the sole turn content sent when resuming via resume-automation
/// -- deliberately generic (not the cell's own task text, which the
/// resumed session already has from its first turn) since `claude -p`/
/// `codex exec`/etc. all require *some* prompt to run at all.
const RESUME_AUTOMATION_PROMPT: &str = "Continue the task from exactly where \
    you left off, using everything already in this conversation -- \
    including anything a human did in your interactive session before \
    handing it back.";

/// How often to poll active reviews' forge PRs for a stack reorder made
/// outside ralphus (RAL-273). Five minutes, scoped (by
/// [`crate::pr::poll_forge_reorders`]) to guardians with an active stack
/// (`in_review`/`merging` only) to keep GitHub/GitLab API usage trivial.
pub const FORGE_REORDER_POLL_INTERVAL: Duration = Duration::from_secs(300);

fn resolve_agent_selection(
    agent: &str,
    cwd: &str,
) -> Result<crate::agent_profiles::ResolvedAgentSelection, String> {
    crate::agent_profiles::resolve_agent_for_path(agent, Path::new(cwd))
}

/// Whether a resolved backend name is Claude Code (`claude-code`, `claude-cli`).
fn is_claude_code_backend(backend: &str) -> bool {
    backend == "claude-code" || backend == "claude-cli"
}

/// RAL-248: returns a human reason when the current cell must NOT resume a
/// dependency's agent session — `None` when sharing is safe.
///
/// The only blocking case today: two `claude-code` cells on different models.
/// Claude Code keys its session cache on the model, so
/// `claude -p --resume <id> --model <other>` silently starts a cold session,
/// discarding the very context cross-cell sharing exists to carry forward.
/// Any other pairing (a non-claude-code cell, or matching models) resumes
/// fine — a fresh `--append-system-prompt`/prompt is applied on top of the
/// resumed session without corrupting it (RAL-248 AC3; the claude-code
/// backend already passes both flags together).
#[must_use]
fn sharing_blocked_reason(
    current_backend: &str,
    current_model: Option<&str>,
    dep_backend: &str,
    dep_model: Option<&str>,
) -> Option<String> {
    if is_claude_code_backend(current_backend)
        && is_claude_code_backend(dep_backend)
        && current_model != dep_model
    {
        Some(format!(
            "claude-code model mismatch (dependency {} vs this cell {})",
            dep_model.unwrap_or("default"),
            current_model.unwrap_or("default")
        ))
    } else {
        None
    }
}

/// Outcome of cross-cell session-sharing resolution (RAL-248).
enum SessionShare {
    /// Resume the named completed dependency's agent session.
    Resume {
        session_id: String,
        from_cell: String,
        from_task: String,
    },
    /// A dependency produced a resumable session but it must not be shared —
    /// start fresh instead, logging why.
    Skip { reason: String },
    /// No completed dependency produced a resumable session id.
    None,
}

/// RAL-248: decide whether cell `i` should resume one of its completed
/// dependencies' agent sessions (cross-cell session sharing) rather than
/// start fresh. See [`sharing_blocked_reason`] for the guard.
///
/// Because ready cells dispatch in parallel, the only cells guaranteed
/// finished when `i` starts are its `plan.deps[i]` — so the prior session (if
/// any) has to come from one of them. We share the first session-bearing,
/// guard-passing dependency's id.
fn resolve_shared_session_id(
    store: &Arc<Mutex<Store>>,
    squad_id: &str,
    row: &crate::store::CellRow,
    cells: &[crate::store::CellRow],
    plan: &crate::plan::ExecutionPlan,
    i: usize,
    current_backend: &str,
) -> SessionShare {
    // Collect each dependency's stored session id in one short DB read, then
    // release the store lock before doing any config-file I/O below
    // (`resolve_agent_selection` reads `.ralphus.toml` from disk, which must
    // not happen while holding the global store mutex).
    let dep_sessions: Vec<(usize, String)> = {
        let guard = store.lock().expect("store mutex poisoned");
        plan.deps[i]
            .iter()
            .filter_map(|&d| {
                let dep = &cells[d];
                guard
                    .get_cell_agent_resume(squad_id, dep.task_idx, dep.idx)
                    .ok()
                    .and_then(|(_, _, sid)| sid.map(|sid| (d, sid)))
                // Only resumable when the dependency actually produced a
                // session id; a dependency without one can't be shared.
            })
            .collect()
    };
    let mut blocked_reason: Option<String> = None;
    for (d, session_id) in dep_sessions {
        let dep = &cells[d];
        let dep_backend = resolve_agent_selection(&dep.agent, dep.cwd.as_deref().unwrap_or("."))
            .map(|s| s.backend)
            .unwrap_or_else(|_| dep.agent.clone());
        if let Some(reason) = sharing_blocked_reason(
            current_backend,
            row.model.as_deref(),
            &dep_backend,
            dep.model.as_deref(),
        ) {
            blocked_reason = Some(reason);
            continue;
        }
        return SessionShare::Resume {
            session_id,
            from_cell: dep.cell_id.clone(),
            from_task: dep.task_name.clone(),
        };
    }
    if let Some(reason) = blocked_reason {
        SessionShare::Skip { reason }
    } else {
        SessionShare::None
    }
}

/// A dependency-free counting semaphore that bounds how many cells execute
/// at once. One instance is shared across every squad's worker threads, so the
/// concurrency cap is *global and task-level* — independent tasks (across one
/// squad or many) run in parallel up to this many at a time, rather than the old
/// "one squad at a time" limit.
#[derive(Debug)]
pub struct Semaphore {
    permits: Mutex<i64>,
    available: Condvar,
    capacity: i64,
}

impl Semaphore {
    /// A semaphore with `permits` slots. `0` means "no limit" — internally
    /// represented as `i64::MAX` so [`acquire`](Self::acquire) never blocks.
    /// Any other non-positive value is clamped to 1 so it can never deadlock.
    #[must_use]
    pub fn new(permits: i64) -> Self {
        let capacity = if permits == 0 {
            i64::MAX
        } else {
            permits.max(1)
        };
        Self {
            permits: Mutex::new(capacity),
            available: Condvar::new(),
            capacity,
        }
    }

    /// Block until a slot is free, returning a guard that frees it on drop.
    pub(crate) fn acquire(&self) -> SemaphorePermit<'_> {
        let mut permits = self.permits.lock().expect("semaphore mutex poisoned");
        while *permits <= 0 {
            permits = self
                .available
                .wait(permits)
                .expect("semaphore mutex poisoned");
        }
        *permits -= 1;
        SemaphorePermit { sem: self }
    }

    /// Slots currently held (`capacity - free`) — the ground truth for the
    /// board API's concurrency counter. A permit is acquired for a cell (or
    /// task-level proof, or guardian review merge) for its *entire* time in
    /// flight, which spans windows where no single DB row reads `running` (a
    /// cell's own `state` column flips to `done` before its proof steps
    /// run, per RAL-64, and a not-yet-started proof step still reads
    /// `pending`) — so counting raw `state='running'` rows undercounts actual
    /// concurrency-slot usage. This is exact regardless of DB row timing.
    pub(crate) fn in_use(&self) -> i64 {
        let permits = self.permits.lock().expect("semaphore mutex poisoned");
        self.capacity - *permits
    }
}

/// Frees its semaphore slot when dropped.
pub(crate) struct SemaphorePermit<'a> {
    sem: &'a Semaphore,
}

impl Drop for SemaphorePermit<'_> {
    fn drop(&mut self) {
        let mut permits = self.sem.permits.lock().expect("semaphore mutex poisoned");
        *permits += 1;
        self.sem.available.notify_one();
    }
}

/// Run the scheduler forever, ticking every [`POLL_INTERVAL`] and sweeping
/// reviews for base-branch shifts every [`REVIEW_MAINT_INTERVAL`].
///
/// Takes the shared `sem` from [`crate::server::Daemon`] so that cells,
/// task-level proofs, and guardian review merges all count against the same
/// global concurrency cap. `summary_queue` is likewise the daemon's shared
/// [`crate::summary_worker::SummaryQueue`] — its background worker threads are
/// spawned separately (`server::serve`), so this loop only ever enqueues into
/// it, never processes it inline.
pub fn run_loop(
    store: Arc<Mutex<Store>>,
    runner: Arc<dyn Runner>,
    _max_concurrent: i64,
    cancellations: Cancellations,
    sem: Arc<Semaphore>,
    summary_queue: Arc<crate::summary_worker::SummaryQueue>,
) {
    let mut last_maintenance = std::time::Instant::now();
    let mut last_summary_sweep = std::time::Instant::now();
    let mut last_prune = std::time::Instant::now();
    let mut last_terminal_log_prune = std::time::Instant::now();
    let mut last_forge_reorder_poll = std::time::Instant::now();
    // Recovery: restart merges that were interrupted by a daemon shutdown.
    // Guardians stuck in `merging` have no live background thread; reset them to
    // `collecting` so `claim_guardian_merge` can claim them again. Notably we do
    // NOT promote their branches to `ready` here: since RAL-265 the staged merge
    // can run while only part of the stack's cells are done, so a blindly
    // promoted `pending` branch (its cell not yet done) would be wrongly built.
    // Once `collecting`, the all-cells-done case below and the partial-ready case
    // further down re-trigger the right level of work.
    {
        let ids = {
            let guard = store.lock().expect("store mutex poisoned");
            let ids = guard.interrupted_merges().unwrap_or_default();
            for gid in &ids {
                let _ = guard.reset_guardian_to_collecting(gid);
            }
            ids
        };
        start_reviews(&store, ids, &sem, &cancellations);
    }
    // Recovery: start collecting guardians whose contributing cells are all
    // Done. This handles the case where the daemon was restarted after the squad
    // completed but before the guardian auto-started, and a full-merge crash (its
    // branches were reset to `pending` by `run_merge_cancellable`).
    {
        let ids = {
            let guard = store.lock().expect("store mutex poisoned");
            let ids = guard.collecting_guardians_ready().unwrap_or_default();
            for gid in &ids {
                let _ = guard.mark_guardian_branches_ready(gid);
            }
            ids
        };
        start_reviews(&store, ids, &sem, &cancellations);
    }
    // Recovery: start collecting guardians that are mid-incremental (RAL-265) —
    // a buildable-but-not-terminal branch (`ready`, or one a crashed pass left
    // half-built) but not all branches done. A staged merge builds their
    // already-`Done` prefix plus the next buildable branch. Branches are left in
    // whatever state the crash left them; the pass re-evaluates.
    {
        let ids = {
            let guard = store.lock().expect("store mutex poisoned");
            guard.collecting_guardians_resumable().unwrap_or_default()
        };
        start_reviews(&store, ids, &sem, &cancellations);
    }
    loop {
        tick(&store, &runner, &sem, &cancellations, &summary_queue);
        if last_maintenance.elapsed() >= REVIEW_MAINT_INTERVAL {
            crate::guardian_merge::review_maintenance(&store, &sem, &cancellations);
            last_maintenance = std::time::Instant::now();
        }
        if last_summary_sweep.elapsed() >= SUMMARY_SWEEP_INTERVAL {
            crate::guardian_merge::sweep_pending_summaries(&store, &sem);
            last_summary_sweep = std::time::Instant::now();
        }
        if last_prune.elapsed() >= CARTOGRAPHER_PRUNE_INTERVAL {
            let cfg = crate::config::load_cartographer_config();
            let guard = store.lock().expect("store mutex poisoned");
            match guard.cartographer_prune(cfg.retention_days(), cfg.max_rows()) {
                Ok(deleted) if deleted > 0 => {
                    crate::cartographer::Note::new("scheduler").emit(
                        &guard,
                        format!("cartographer pruned {deleted} rows"),
                        serde_json::json!({
                            "deleted": deleted,
                            "retention_days": cfg.retention_days(),
                            "max_rows": cfg.max_rows(),
                        }),
                    );
                }
                Ok(_) => {}
                Err(e) => crate::rlog!(
                    WARNING,
                    "ralphus [scheduler] cartographer prune failed: {e}"
                ),
            }
            drop(guard);
            last_prune = std::time::Instant::now();
        }
        if last_terminal_log_prune.elapsed() >= TERMINAL_LOG_PRUNE_INTERVAL {
            let cfg = crate::config::load_terminal_log_config();
            // Filesystem walk, deliberately done without holding the store
            // lock (unlike `cartographer_prune`, which is itself the DB
            // operation) — only briefly re-acquired below to emit the
            // breadcrumb note.
            let deleted = crate::terminal_log::prune(cfg.retention_days(), cfg.max_files());
            if deleted > 0 {
                let guard = store.lock().expect("store mutex poisoned");
                crate::cartographer::Note::new("scheduler").emit(
                    &guard,
                    format!("terminal-log pruned {deleted} attempt file(s)"),
                    serde_json::json!({
                        "deleted": deleted,
                        "retention_days": cfg.retention_days(),
                        "max_files": cfg.max_files(),
                    }),
                );
            }
            last_terminal_log_prune = std::time::Instant::now();
        }
        if last_forge_reorder_poll.elapsed() >= FORGE_REORDER_POLL_INTERVAL {
            crate::pr::poll_forge_reorders(&store, &sem, &cancellations);
            last_forge_reorder_poll = std::time::Instant::now();
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// One scheduling pass: start a worker for every ready squad. Squads are no longer
/// rationed by a squad-count limit — the shared `sem` bounds concurrency at the
/// cell (task) level instead, so every ready squad gets a worker and its
/// cells compete for the global slots.
pub fn tick(
    store: &Arc<Mutex<Store>>,
    runner: &Arc<dyn Runner>,
    sem: &Arc<Semaphore>,
    cancellations: &Cancellations,
    summary_queue: &Arc<crate::summary_worker::SummaryQueue>,
) {
    // RAL-122: a configured down-time window blocks only the scheduler's own
    // automatic claiming of new Pending squads, checked fresh every tick so a
    // window boundary takes effect within one POLL_INTERVAL. Cells/tasks
    // already claimed keep running via their own already-spawned worker
    // threads, untouched by this short-circuit — and any explicit user action
    // (set-status, activate) never goes through `tick`, so it is unaffected.
    if crate::config::scheduler_in_downtime() {
        return;
    }
    let to_start = claim_ready(store, cancellations);
    for squad_id in to_start {
        let store = Arc::clone(store);
        let runner = Arc::clone(runner);
        let sem = Arc::clone(sem);
        let cancellations = cancellations.clone();
        let summary_queue = Arc::clone(summary_queue);
        std::thread::spawn(move || {
            // Register a cancel token so a user `cancel` can stop this worker
            // (and its subprocess); drop it once the squad is done.
            let token = cancellations.register(&squad_id);
            execute_squad_inner(
                &store,
                runner.as_ref(),
                &squad_id,
                &token,
                &sem,
                &summary_queue,
                &cancellations,
            );
            cancellations.remove(&squad_id);
        });
    }
}

/// Under the lock: mark every ready squad Running (so the next tick won't re-claim
/// it) and return the claimed ids. Readiness — Pending with cross-squad deps Done
/// — is decided by [`Store::list_ready`]; the concurrency cap is enforced later,
/// per cell, by the shared [`Semaphore`].
///
/// A squad whose worker is still registered (`cancellations.is_active`) is
/// skipped even if the store shows it `Pending` — a targeted restart
/// (`server::restart_cell`/`restart_cell_proof`/`restart_task_proof`)
/// may reset a squad to `Pending` without stopping that squad's still-live
/// worker when the restarted target is already terminal, specifically so it
/// doesn't have to cancel unrelated sibling cells the same worker is
/// still driving (RAL-1xx). Claiming the squad again here, while that worker
/// is still going, would double-dispatch it — the exact race the old
/// unconditional cancel-and-wait dance existed to prevent. Deferring is
/// safe: once the live worker finishes and removes its token, the next tick
/// claims the squad fresh and picks up whatever was reset in the meantime.
fn claim_ready(store: &Arc<Mutex<Store>>, cancellations: &Cancellations) -> Vec<String> {
    let guard = store.lock().expect("store mutex poisoned");
    let ready = guard.list_ready().unwrap_or_default();
    let mut claimed = Vec::new();
    for squad_id in ready {
        if cancellations.is_active(&squad_id) {
            continue;
        }
        if guard
            .set_squad_state(&squad_id, SquadState::Running)
            .is_ok()
        {
            // A short-lived span for the claim itself (RAL-96) — continues the
            // trace persisted at submit time, if any. Cell/proof execution
            // get their own spans later, once the worker thread starts.
            let trace_context = guard.squad_trace_context(&squad_id).unwrap_or_default();
            let cx = otel::context_from_traceparent(trace_context.as_deref());
            let _span = otel::start_span("scheduler.claim_ready", &cx, SpanKind::Internal);
            _span.set_attribute("squad_id", squad_id.clone());
            crate::rlog!(
                INFO,
                "ralphus [scheduler] squad {squad_id} claimed → running"
            );
            let _ = guard.cartographer_log(crate::cartographer::CartographerEntry {
                level: crate::logging::LogLevel::INFO,
                source: "scheduler",
                message: "squad claimed → running",
                scope: Some("squad"),
                squad_id: Some(&squad_id),
                guardian_id: None,
                cell_id: None,
                task: None,
                log_path: None,
                payload: serde_json::json!({}),
            });
            claimed.push(squad_id);
        }
    }
    claimed
}

/// Run `f` against a private, single-worker [`crate::summary_worker::SummaryQueue`]
/// that is drained and shut down before returning. Standalone entry points
/// (`execute_squad`/`execute_squad_with`, used only by tests) don't share the
/// daemon's real background summary workers spawned in `server::serve`, but
/// still need any guardian-summary work their squad triggers to actually run
/// (and finish) rather than being enqueued into the void.
fn with_standalone_summary_queue<F>(store: &Arc<Mutex<Store>>, f: F)
where
    F: FnOnce(&Arc<crate::summary_worker::SummaryQueue>),
{
    let queue = crate::summary_worker::SummaryQueue::new();
    let workers = crate::summary_worker::spawn_workers(&queue, store, 1);
    f(&queue);
    queue.shutdown();
    for w in workers {
        let _ = w.join();
    }
}

/// Execute one already-claimed (Running) squad to completion, honouring the
/// dependency order of its cells. Uses a never-firing cancel token — the
/// cancellable path goes through [`execute_squad_with`].
///
/// This single-squad entry point owns a private [`Semaphore`], so its independent
/// cells still run concurrently up to the default limit.
pub fn execute_squad(store: &Arc<Mutex<Store>>, runner: &dyn Runner, squad_id: &str) {
    let sem = Arc::new(Semaphore::new(crate::DEFAULT_MAX_CONCURRENT));
    with_standalone_summary_queue(store, |queue| {
        execute_squad_inner(
            store,
            runner,
            squad_id,
            &CancelToken::never(),
            &sem,
            queue,
            &Cancellations::new(),
        );
    });
}

/// Execute one already-claimed (Running) squad to completion, stopping early if
/// `cancel` trips. When cancelled mid-flight the worker abandons the squad
/// without finalizing it: [`Store::cancel`] has already flipped the squad and its
/// non-terminal nodes to `cancelled`, and the runner has killed any subprocess.
///
/// Owns a private [`Semaphore`]; the scheduler's [`tick`] uses the concurrent
/// path directly with the *shared* global semaphore instead.
///
/// RAL-213: `cancellations` is the daemon-wide registry a task-completion
/// -triggered guardian merge (via `try_start_ready_reviews_for_task`) registers
/// its own `guardian:{id}` token into, so such a merge is stoppable through the
/// same registry a settings change/`cancel_and_merge` looks up -- not just the
/// caller's own `cancel` token, which only covers this squad's own cells.
pub fn execute_squad_with(
    store: &Arc<Mutex<Store>>,
    runner: &dyn Runner,
    squad_id: &str,
    cancel: &CancelToken,
    cancellations: &Cancellations,
) {
    let sem = Arc::new(Semaphore::new(crate::DEFAULT_MAX_CONCURRENT));
    with_standalone_summary_queue(store, |queue| {
        execute_squad_inner(store, runner, squad_id, cancel, &sem, queue, cancellations);
    });
}

/// Per-cell execution state, shared across a squad's concurrent workers.
#[derive(Clone, Copy, PartialEq, Eq)]
enum CellState {
    Pending,
    Running,
    Done,
    Failed,
    /// Terminal, but *not* a failure (RAL-185). Seeded from
    /// [`Store::cancelled_cells`] for a cell a squad-level cancel left
    /// behind, and cascaded onto any still-Pending cell whose prerequisite
    /// is itself Cancelled. Never dispatched, never satisfies a dependency,
    /// and — unlike [`CellState::Failed`] — never folded into
    /// `Progress.failed`, so cancelling a task does not mislabel it as failed.
    Cancelled,
}

/// Shared progress for a squad's cells, behind one mutex.
///
/// `failed` lives here (rather than in a separate mutex) so a cell's status
/// transition and its task's failure flag are published together under one
/// lock. The dispatcher decides when a task's cells have all finished *and*
/// whether that task failed from the same locked snapshot, so a per-task
/// finalizer can never launch in the window between "status is terminal" and
/// "failure recorded".
struct Progress {
    status: Vec<CellState>,
    summaries: Vec<Option<String>>,
    /// Task indices with at least one failed cell, cell-proof, or
    /// task-proof — i.e. tasks that must finalize as `Failed`.
    failed: HashSet<i64>,
    /// Task indices whose finalizer (`run_task_finalizer`) has *completed* —
    /// its own task-level proof has run and its terminal state is decided
    /// (check `failed` for which). Distinct from a task's cells merely
    /// reaching `CellState::Done`: a `task_deps` entry (see `plan::ExecutionPlan`)
    /// must wait for membership here, not just for its cells, so a
    /// task-name dependent never starts before the upstream task's own
    /// fmt/clippy/test-style proof has actually finished.
    task_finalized: HashSet<i64>,
}

/// Execute a squad's cells concurrently: a cell starts the moment all its
/// dependencies are `Done`, and each worker holds a permit from the shared
/// `sem` only while doing real work — so the number of cells running at once
/// is bounded globally (the task-level concurrency cap). After every cell is
/// terminal, task-level proofs and finalization run exactly as before.
#[allow(clippy::too_many_arguments)]
fn execute_squad_inner(
    store: &Arc<Mutex<Store>>,
    runner: &dyn Runner,
    squad_id: &str,
    cancel: &CancelToken,
    sem: &Arc<Semaphore>,
    summary_queue: &Arc<crate::summary_worker::SummaryQueue>,
    cancellations: &Cancellations,
) {
    let trace_context = store
        .lock()
        .expect("store mutex poisoned")
        .squad_trace_context(squad_id)
        .unwrap_or_default();
    let squad_cx = otel::context_from_traceparent(trace_context.as_deref());
    let _squad_span = otel::start_span("scheduler.run_execute", &squad_cx, SpanKind::Internal);
    _squad_span.set_attribute("squad_id", squad_id.to_string());
    // Rebuild the traceparent from this span's own context (rather than
    // reusing `trace_context` verbatim) so downstream cell/proof spans
    // nest under *this* span instead of becoming its siblings.
    let trace_context = otel::traceparent_from_context(&_squad_span.cx);

    let (
        mut cells,
        tasks,
        already_done,
        already_failed_tasks,
        already_failed_cells,
        proof_only_set,
        ignored_set,
        cancelled_cell_set,
        cancelled_task_set,
    ) = {
        let guard = store.lock().expect("store mutex poisoned");
        // Mark Running up front so the finalization guard (which leaves an
        // edit-reset squad Pending) has a Running baseline to compare against.
        let _ = guard.set_squad_state(squad_id, SquadState::Running);
        (
            guard.cells_of(squad_id).unwrap_or_default(),
            guard.tasks_of(squad_id).unwrap_or_default(),
            // Cells already Done are skipped, so a restarted squad only re-runs
            // its dirty (reset-to-pending) subset instead of redoing them (RAL-19).
            guard.done_cells(squad_id).unwrap_or_default(),
            // Skipped cells whose cell-level proof previously failed still
            // condemn their task — without this seed, `already_failed` in the
            // task finalizer is false and the task is incorrectly marked Done.
            guard
                .done_cells_with_failed_proof(squad_id)
                .unwrap_or_default(),
            // Cells already Failed for an unrelated reason must stay
            // terminally Failed, not fall through to Pending and get
            // redispatched just because *some other* cell's scoped restart
            // flipped the whole squad back to Pending (RAL-1xx: `restart_cell`
            // on one cell resurrected every other still-failed independent
            // task in the squad).
            guard.failed_cells(squad_id).unwrap_or_default(),
            // Cells that are Done but have pending proofs (e.g. after
            // restart_cell_proof). These skip the cell body and re-run
            // only their proof steps via run_proof_only_worker.
            guard.cells_needing_proof_only(squad_id).unwrap_or_default(),
            // Cells manually set to `ignored` are seeded satisfied (like Done)
            // so their downstream runs, but are themselves never executed.
            guard.ignored_cells(squad_id).unwrap_or_default(),
            // RAL-185: same class of bug as `failed_cells` above, for the
            // state that seed never covered. A cell left `cancelled` by an
            // earlier squad-level cancel must stay terminal instead of falling
            // through to Pending and being redispatched just because *some
            // other*, dependency-unrelated cell's scoped restart flipped the
            // whole squad back to Pending. Anything the restart genuinely revived
            // is already `pending` in the DB here, so it is not in this set.
            guard.cancelled_cells(squad_id).unwrap_or_default(),
            // ... and the task rows the same cancel flipped, so no finalizer
            // launches for them (see `cancelled_tasks`' doc comment).
            guard.cancelled_tasks(squad_id).unwrap_or_default(),
        )
    };
    // Resolve every worktree-placeholder `cwd` (RAL-100) before planning: a
    // placeholder repeated across cells/tasks materializes exactly one
    // worktree, and the resolved real path is persisted immediately, so a
    // restarted squad never re-resolves (or re-creates) it. A cell whose
    // project/worktree can no longer be resolved (e.g. deregistered after
    // submit) fails the whole squad cleanly rather than panicking mid-dispatch.
    {
        let guard = store.lock().expect("store mutex poisoned");
        let result = crate::worktrees::resolve_placeholders(
            &guard,
            squad_id,
            &mut cells,
            &tasks,
            &_squad_span.cx,
        );
        drop(guard);
        if let Err(e) = result {
            _squad_span.set_status(opentelemetry::trace::Status::error(e.clone()));
            finalize_all_failed(store, squad_id, &tasks, &e);
            return;
        }
    }
    crate::rlog!(
        INFO,
        "ralphus [scheduler] squad {squad_id} executing ({} cells, {} tasks)",
        cells.len(),
        tasks.len()
    );
    {
        let guard = store.lock().expect("store mutex poisoned");
        let _ = guard.cartographer_log(crate::cartographer::CartographerEntry {
            level: crate::logging::LogLevel::INFO,
            source: "scheduler",
            message: "squad executing",
            scope: Some("squad"),
            squad_id: Some(squad_id),
            guardian_id: None,
            cell_id: None,
            task: None,
            log_path: None,
            payload: serde_json::json!({"cells": cells.len(), "tasks": tasks.len()}),
        });
    }

    let plan = match crate::plan::plan(&cells, &tasks) {
        Ok(p) => p,
        Err(msg) => {
            finalize_all_failed(store, squad_id, &tasks, &msg);
            return;
        }
    };

    let n = cells.len();
    // An already-Done cell (restarted squad's untouched upstream) starts
    // satisfied so its downstream can proceed (RAL-19).
    // A proof-only cell (Done but with pending proofs) starts Running so
    // the dispatcher doesn't re-dispatch the cell body; a dedicated
    // proof-only worker is spawned immediately before the dispatcher loop.
    let progress = Mutex::new(Progress {
        status: (0..n)
            .map(|i| {
                let key = (cells[i].task_idx, cells[i].idx);
                if already_done.contains(&key) || ignored_set.contains(&key) {
                    // Ignored cells are treated exactly like Done: satisfied
                    // for downstream, and never dispatched (the Done arm is a no-op).
                    CellState::Done
                } else if proof_only_set.contains(&key) {
                    CellState::Running
                } else if already_failed_cells.contains(&key) {
                    CellState::Failed
                } else if cancelled_cell_set.contains(&key) {
                    // RAL-185: terminal, but deliberately NOT `Failed` —
                    // reusing `Failed` here would fold this cell's
                    // `task_idx` into `prog.failed` below and report a task
                    // the user merely *cancelled* as having failed.
                    CellState::Cancelled
                } else {
                    CellState::Pending
                }
            })
            .collect(),
        summaries: vec![None; n],
        failed: already_failed_tasks
            .into_iter()
            .chain(already_failed_cells.iter().map(|&(task_idx, _)| task_idx))
            .collect(),
        task_finalized: HashSet::new(),
    });

    // Indices of cells that need proof-only dispatch (precomputed so the
    // borrows inside std::thread::scope are satisfied).
    let proof_only_indices: Vec<usize> = (0..n)
        .filter(|&i| proof_only_set.contains(&(cells[i].task_idx, cells[i].idx)))
        .collect();
    // Borrowed (not moved) into the scope below so every worker thread can
    // rebuild a `Context` that continues this squad's trace (RAL-96).
    let trace_ref: Option<&str> = trace_context.as_deref();

    // Each task's cell indices, so the dispatcher can tell when a task's own
    // cells have all finished and finalize *that* task on its own — rather
    // than holding every finished task at `running` until the slowest sibling
    // task in the squad completes (the "all cells done but task still running"
    // bug).
    let mut task_cells: HashMap<i64, Vec<usize>> = HashMap::new();
    for (i, s) in cells.iter().enumerate() {
        task_cells.entry(s.task_idx).or_default().push(i);
    }
    let task_indices: Vec<i64> = tasks.iter().map(|t| t.idx).collect();
    // Task indices currently considered cancelled (dispatcher-thread-local).
    // Seeded from the DB and extended whenever a cell is cascade-cancelled
    // below; entries are dropped again by the reclaim step when a restart
    // resets that task to Pending, so it never goes stale (RAL-185).
    let mut cancelled_tasks: HashSet<i64> = cancelled_task_set;
    // Tasks whose finalizer has already been launched (dispatcher-thread-local,
    // so no lock needed). A cancelled task counts as already launched: its
    // cells are seeded terminal, so the `all_terminal` check below would
    // otherwise fire, run its task-level proofs, and flip a task the user
    // explicitly cancelled to Done (RAL-185).
    let mut finalized: HashSet<i64> = cancelled_tasks.clone();

    // Dispatcher loop: each pass launches every cell whose prerequisites are
    // all Done and fails every cell whose prerequisite failed, and launches a
    // per-task finalizer the moment a task's cells are all terminal — until
    // no cell is left Pending or Running.
    std::thread::scope(|scope| {
        // Hoist borrows outside the loop so proof-only workers (spawned below)
        // and normal workers (spawned inside the loop) share the same references.
        let cells_ref = &cells;
        let progress_ref = &progress;
        let plan_ref = &plan;

        // Spawn proof-only workers immediately for cells that are Done but
        // have pending proofs (restart_cell_proof or crash recovery). They
        // start as Running in progress so the dispatcher won't re-dispatch the
        // cell body; after proofs finish they transition to Done/Failed.
        for &i in &proof_only_indices {
            scope.spawn(move || {
                run_proof_only_worker(
                    store,
                    runner,
                    squad_id,
                    cancel,
                    sem,
                    cells_ref,
                    progress_ref,
                    i,
                    trace_ref,
                );
            });
        }

        loop {
            // Stop launching the moment the squad is cancelled; in-flight workers
            // observe the same token and unwind.
            if cancel.is_cancelled() {
                return;
            }

            // RAL-1xx: a sibling task in this same squad may have been restarted
            // via `server::restart_cell`/`restart_cell_proof`/
            // `restart_task_proof` without cancelling *this* worker — those
            // handlers skip cancellation when the restart target was already
            // terminal, specifically so they don't collaterally kill cells
            // this worker is still actively driving (see their doc comments).
            // That means a task this worker already finalized as Done/Failed
            // can have its row (and its cells') reset to Pending in the
            // store out from under it. Reconcile that before deciding what to
            // dispatch this pass, so the restarted sibling starts on this same
            // worker immediately — with whatever concurrency slots are free —
            // instead of sitting Pending until the whole squad drains and a
            // fresh worker gets claimed for it.
            {
                let (reclaimed_tasks, reclaimed_cells, reclaimed_proof_only): (
                    Vec<i64>,
                    Vec<usize>,
                    Vec<usize>,
                ) = {
                    let guard = store.lock().expect("store mutex poisoned");
                    let reclaimed_tasks: Vec<i64> = finalized
                        .iter()
                        .copied()
                        .filter(|&t| {
                            matches!(guard.task_state(squad_id, t), Ok(Some(NodeState::Pending)))
                        })
                        .collect();
                    let reclaimed_cells: Vec<usize> = reclaimed_tasks
                        .iter()
                        .flat_map(|t| task_cells.get(t).into_iter().flatten().copied())
                        .filter(|&i| {
                            let row = &cells[i];
                            matches!(
                                guard.cell_state(squad_id, row.task_idx, row.idx),
                                Ok(Some(NodeState::Pending))
                            )
                        })
                        .collect();
                    // A cell-scoped proof restart (`restart_cell_proof`/
                    // `restart_task_proof`) deliberately leaves the cell row
                    // itself `done` — only its proof row(s) go back to `pending`
                    // (see those functions' doc comments) — so such a cell
                    // never matches the `cell_state == Pending` filter above
                    // and would otherwise fall through this reclaim entirely: its
                    // stale in-memory status (whatever it was before the restart,
                    // typically `Failed`) would never be refreshed, so the retried
                    // proof would silently never actually re-run and any
                    // dependent would stay wrongly blocked on a prerequisite this
                    // worker thinks already failed. Reclaim these the same way the
                    // initial `proof_only_indices` pass does: seed `Running` and
                    // spawn a fresh proof-only worker for each (RAL-1xx, mirrors
                    // squad-000000000148/ral-171's `test` proof restarted while
                    // ral-169/170/172 kept this squad's worker alive).
                    let proof_only_now =
                        guard.cells_needing_proof_only(squad_id).unwrap_or_default();
                    let reclaimed_proof_only: Vec<usize> = reclaimed_tasks
                        .iter()
                        .flat_map(|t| task_cells.get(t).into_iter().flatten().copied())
                        .filter(|&i| {
                            let row = &cells[i];
                            proof_only_now.contains(&(row.task_idx, row.idx))
                        })
                        .collect();
                    (reclaimed_tasks, reclaimed_cells, reclaimed_proof_only)
                };
                // Cells to actually (re)spawn a proof-only worker for this
                // pass -- excludes any `reclaimed_proof_only` entry whose worker
                // is already running (spawned by this same reclaim step on an
                // earlier tick, or by the initial `proof_only_indices` pass), so
                // a still-in-flight proof is never spawned twice concurrently.
                let mut to_spawn_proof_only: Vec<usize> = Vec::new();
                if !reclaimed_tasks.is_empty() {
                    for &t in &reclaimed_tasks {
                        finalized.remove(&t);
                        // A restart that reset this task back to Pending
                        // revives it: it is no longer cancelled, so its
                        // still-Pending dependents must stop cascading off it
                        // (RAL-185).
                        cancelled_tasks.remove(&t);
                    }
                    let mut prog = progress.lock().expect("progress mutex poisoned");
                    for &t in &reclaimed_tasks {
                        prog.failed.remove(&t);
                        prog.task_finalized.remove(&t);
                    }
                    for &i in &reclaimed_cells {
                        prog.status[i] = CellState::Pending;
                        prog.summaries[i] = None;
                    }
                    for &i in &reclaimed_proof_only {
                        if prog.status[i] != CellState::Running {
                            prog.status[i] = CellState::Running;
                            to_spawn_proof_only.push(i);
                        }
                    }
                }
                for &i in &to_spawn_proof_only {
                    scope.spawn(move || {
                        run_proof_only_worker(
                            store,
                            runner,
                            squad_id,
                            cancel,
                            sem,
                            cells_ref,
                            progress_ref,
                            i,
                            trace_ref,
                        );
                    });
                }
            }

            // RAL-157: read live so a solo/unsolo toggled mid-squad takes effect
            // on this very pass, not just at the squad's next (re)start. While
            // any task in the squad is soloed, a not-yet-started cell may
            // only dispatch if its own task is one of the soloed ones — this
            // applies uniformly regardless of whether `deps`/`task_deps` are
            // already satisfied, so a dependent of a *finished* soloed task
            // still stays paused until the squad is un-soloed (a soloed task
            // completing must not let its dependents race ahead). Cells
            // already `Running` are left alone — there is no per-cell
            // interrupt in this codebase (see `Cancellations`), so pausing an
            // in-flight cell's task only takes effect at its next cell.
            let soloed_tasks = store
                .lock()
                .expect("store mutex poisoned")
                .soloed_task_indices(squad_id)
                .unwrap_or_default();
            let any_soloed = !soloed_tasks.is_empty();

            let mut to_dispatch: Vec<usize> = Vec::new();
            let mut blocked: Vec<usize> = Vec::new();
            let mut blocked_cancelled: Vec<usize> = Vec::new();
            let mut to_finalize: Vec<i64> = Vec::new();
            let mut active = false;
            {
                let mut prog = progress.lock().expect("progress mutex poisoned");
                // Indexes several parallel collections (status, deps, cells),
                // so a range loop is the natural form here.
                #[allow(clippy::needless_range_loop)]
                for i in 0..n {
                    match prog.status[i] {
                        CellState::Done | CellState::Failed | CellState::Cancelled => {}
                        CellState::Running => active = true,
                        CellState::Pending => {
                            let deps = &plan.deps[i];
                            let task_deps = &plan.task_deps[i];
                            let cell_dep_failed =
                                deps.iter().any(|&d| prog.status[d] == CellState::Failed);
                            // A task-name dependency only counts as failed once that
                            // task has actually finalized as Failed — a task whose
                            // cells are still in flight isn't failed yet.
                            let task_dep_failed = task_deps.iter().any(|&t| {
                                prog.task_finalized.contains(&t) && prog.failed.contains(&t)
                            });
                            // RAL-185: a `Cancelled` prerequisite is terminal
                            // and will never reach Done, so a dependent left
                            // Pending would keep `active` set forever and spin
                            // the dispatcher. Resolve it the same way a failed
                            // prerequisite is resolved — terminally — but as
                            // Cancelled, since nothing here actually failed.
                            let cell_dep_cancelled =
                                deps.iter().any(|&d| prog.status[d] == CellState::Cancelled);
                            let task_dep_cancelled =
                                task_deps.iter().any(|&t| cancelled_tasks.contains(&t));
                            if cell_dep_failed || task_dep_failed {
                                prog.status[i] = CellState::Failed;
                                prog.failed.insert(cells[i].task_idx);
                                blocked.push(i);
                            } else if cell_dep_cancelled || task_dep_cancelled {
                                prog.status[i] = CellState::Cancelled;
                                blocked_cancelled.push(i);
                            } else {
                                let cells_ready =
                                    deps.iter().all(|&d| prog.status[d] == CellState::Done);
                                // A task-name dependency isn't satisfied by its
                                // cells reaching Done alone — it must wait for
                                // that task's own finalizer (task-level proof) to
                                // actually complete, or a dependent starts racing
                                // ahead of its upstream's fmt/clippy/test.
                                let tasks_ready =
                                    task_deps.iter().all(|&t| prog.task_finalized.contains(&t));
                                let solo_ok =
                                    !any_soloed || soloed_tasks.contains(&cells[i].task_idx);
                                if cells_ready && tasks_ready && solo_ok {
                                    prog.status[i] = CellState::Running;
                                    to_dispatch.push(i);
                                    active = true;
                                } else {
                                    active = true;
                                }
                            }
                        }
                    }
                }
                // A cell cascade-cancelled above never ran, so its owning
                // task must not finalize as Done off the back of it — mark the
                // task cancelled too (RAL-185). This also lets a `task_deps`
                // dependent of that task cascade on the next pass instead of
                // waiting forever for a finalizer that will never launch.
                for &i in &blocked_cancelled {
                    let t = cells[i].task_idx;
                    cancelled_tasks.insert(t);
                    finalized.insert(t);
                }
                // A task is ready to finalize once every one of its cells is
                // terminal. Read this from the same snapshot as `prog.failed` so
                // the finalizer sees a consistent (done, failed?) pair.
                for &t in &task_indices {
                    if finalized.contains(&t) {
                        continue;
                    }
                    let idxs = task_cells.get(&t);
                    let all_terminal = idxs.is_some_and(|idxs| {
                        !idxs.is_empty()
                            && idxs.iter().all(|&i| {
                                matches!(
                                    prog.status[i],
                                    CellState::Done | CellState::Failed | CellState::Cancelled
                                )
                            })
                    });
                    if all_terminal {
                        to_finalize.push(t);
                    }
                }
            }

            // Record cells blocked by a failed prerequisite (store writes
            // happen without the progress lock held; the failure flag was set
            // above under the lock).
            for &i in &blocked {
                let row = &cells[i];
                let outcome = CellOutcome {
                    state: NodeState::Failed,
                    tokens_in: 0,
                    tokens_out: 0,
                    cost_usd: 0.0,
                    error: Some("blocked by a failed dependency".to_string()),
                    agent_session_id: None,
                };
                let guard = store.lock().expect("store mutex poisoned");
                let _ = guard.set_cell_state(squad_id, row.task_idx, row.idx, NodeState::Failed);
                let _ = guard.record_cell_result(squad_id, row.task_idx, row.idx, &outcome);
            }

            // Same, for cells resolved terminally because a prerequisite was
            // cancelled (RAL-185). Recorded as `cancelled` rather than `failed`
            // so the board doesn't report a cancellation as a failure.
            for &i in &blocked_cancelled {
                let row = &cells[i];
                let outcome = CellOutcome {
                    state: NodeState::Cancelled,
                    tokens_in: 0,
                    tokens_out: 0,
                    cost_usd: 0.0,
                    error: Some("blocked by a cancelled dependency".to_string()),
                    agent_session_id: None,
                };
                let guard = store.lock().expect("store mutex poisoned");
                let _ = guard.set_cell_state(squad_id, row.task_idx, row.idx, NodeState::Cancelled);
                let _ = guard.record_cell_result(squad_id, row.task_idx, row.idx, &outcome);
                let _ = guard.set_task_state(squad_id, row.task_idx, NodeState::Cancelled);
                let _ = guard.cartographer_log(crate::cartographer::CartographerEntry {
                    level: crate::logging::LogLevel::WARNING,
                    source: "scheduler",
                    message: "cell cancelled: blocked by a cancelled dependency",
                    scope: Some("cell"),
                    squad_id: Some(squad_id),
                    guardian_id: None,
                    cell_id: Some(&row.cell_id),
                    task: Some(&row.task_name),
                    log_path: None,
                    payload: serde_json::json!({}),
                });
            }

            for i in to_dispatch {
                scope.spawn(move || {
                    run_cell_worker(
                        store,
                        runner,
                        squad_id,
                        cancel,
                        sem,
                        cells_ref,
                        plan_ref,
                        progress_ref,
                        i,
                        trace_ref,
                    );
                });
            }

            // Launch a finalizer for each task that just finished all its
            // cells: it runs the task-level proofs and flips the task to
            // Done/Failed immediately, concurrently with the rest of the squad.
            for t in to_finalize {
                finalized.insert(t);
                scope.spawn(move || {
                    run_task_finalizer(
                        store,
                        runner,
                        squad_id,
                        cancel,
                        cells_ref,
                        progress_ref,
                        t,
                        sem,
                        summary_queue,
                        trace_ref,
                        cancellations,
                    );
                });
            }

            if !active {
                return;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    });

    // A cancellation abandons the squad without finalizing: the store already set
    // the squad + its non-terminal nodes to `cancelled`.
    if cancel.is_cancelled() {
        return;
    }
    // All cell workers and per-task finalizers have joined at the end of the
    // scope, so `failed` is final and every task's state has been written.
    let failed_tasks = progress
        .into_inner()
        .expect("progress mutex poisoned")
        .failed;

    let guard = store.lock().expect("store mutex poisoned");
    // If an edit reset this squad to Pending mid-flight, don't clobber it with a
    // terminal state — leave it Pending so it re-runs with the new values.
    if !matches!(guard.squad_state(squad_id), Ok(SquadState::Running)) {
        return;
    }
    // A task left `cancelled` never ran, so the squad did not actually complete —
    // reporting it Done would be a lie, and would let cross-squad dependency
    // gating (which treats Done as "all its work happened") release dependents
    // off work that was skipped. `failed` still wins: a real failure is the
    // more important verdict. Read straight from the DB rather than threading
    // the dispatcher's local set out of the scope — the finalizers have all
    // joined, so the rows are settled (RAL-185).
    let any_cancelled = guard
        .cancelled_tasks(squad_id)
        .map(|t| !t.is_empty())
        .unwrap_or(false);
    let squad_state = if !failed_tasks.is_empty() {
        SquadState::Failed
    } else if any_cancelled {
        SquadState::Cancelled
    } else {
        SquadState::Done
    };
    let _ = guard.set_squad_state(squad_id, squad_state);
    // Per-task review triggers fire from `run_task_finalizer` as each task
    // completes, so no squad-level sweep is needed here.
    drop(guard);
}

/// Before running `row`'s cell, check whether it declares an
/// `upstream = "<<task:...>>"` sentinel. If so, rebase `row`'s branch onto
/// the named dependency's current branch tip.
///
/// Same-repo worktrees get a real `git rebase`; cross-repo upstreams are
/// skipped with a logged warning (rebase cannot happen without shared history).
///
/// Returns `Some(error_message)` when the rebase was attempted but failed
/// (the caller must fail the cell), or `None` when either no rebase was
/// needed or the rebase succeeded.
fn try_upstream_rebase(
    row: &crate::store::CellRow,
    cells: &[crate::store::CellRow],
) -> Option<String> {
    let upstream = row.upstream.as_deref()?;
    let ref_str = ralphus_core::schema::parse_upstream_task_ref(upstream)?;

    // Split "task-name" or "task-name/cell-id".
    let (task_name, cell_id_filter) = ref_str
        .split_once('/')
        .map_or((ref_str, None), |(t, s)| (t, Some(s)));

    let dep = cells
        .iter()
        .find(|s| s.task_name == task_name && cell_id_filter.is_none_or(|sid| s.cell_id == sid))?;

    let b_cwd = std::path::Path::new(row.cwd.as_deref()?);
    let a_cwd = std::path::Path::new(dep.cwd.as_deref()?);

    if !crate::reviews::same_git_repo(a_cwd, b_cwd) {
        // ralphus[ignore-rlog-pair]: this low-level helper has no Store; its Store-owning caller records the structured workflow outcome
        crate::rlog!(
            WARNING,
            "ralphus: upstream rebase skipped — '{}' and '{}' are in different repositories; \
             task '{}' will start from its own branch base",
            a_cwd.display(),
            b_cwd.display(),
            row.task_name,
        );
        return None;
    }

    let a_branch = match crate::reviews::worktree_branch(a_cwd) {
        Ok(b) => b,
        Err(e) => {
            return Some(format!(
                "upstream rebase: could not read branch of '{}': {e}",
                a_cwd.display()
            ));
        }
    };

    if let Err(e) = crate::reviews::rebase_onto(b_cwd, &a_branch) {
        // A rebase conflict is transient: `rebase_onto` already aborted and left
        // the worktree clean.  Failing the cell permanently means the developer
        // can never run it until they manually fix the branch — even though the
        // cell's own code is unaffected.  Log the conflict and let the cell
        // run on its current branch instead.
        // ralphus[ignore-rlog-pair]: this low-level helper has no Store; its Store-owning caller records the structured workflow outcome
        crate::rlog!(
            WARNING,
            "ralphus: upstream rebase of cell '{}' onto '{a_branch}' has a conflict; \
             running cell on its current branch: {e}",
            row.cell_id
        );
        return None;
    }

    // ralphus[ignore-rlog-pair]: this low-level helper has no Store; its Store-owning caller records the structured workflow outcome
    crate::rlog!(
        DEBUG,
        "ralphus: rebased cell '{}' onto upstream branch '{a_branch}'",
        row.cell_id
    );
    None
}

/// RAL-241: broadcast an `urgent` mailbox message for a cell that just
/// failed, so a supervising human/agent doesn't have to go digging through
/// the board or Cartographer to notice. Called from every place a cell's
/// state is written as [`NodeState::Failed`] (the upstream-rebase
/// precondition failure and the ordinary post-execution outcome) — never for
/// `Cancelled`, which is a deliberate stop, not a failure. Best-effort: a
/// store error here is swallowed, matching every other Cartographer/mailbox
/// write in this file.
fn enqueue_cell_failure_mailbox(
    guard: &Store,
    squad_id: &str,
    cell_id: &str,
    task_name: &str,
    error: Option<&str>,
) {
    let text = match error {
        Some(err) => {
            format!("cell '{cell_id}' in task '{task_name}' (squad {squad_id}) failed: {err}")
        }
        None => format!("cell '{cell_id}' in task '{task_name}' (squad {squad_id}) failed"),
    };
    if let Ok(message_id) = guard.enqueue_mailbox_message(
        crate::mailbox::MailboxPriority::Urgent,
        &text,
        Some(squad_id),
        Some(task_name),
        Some(cell_id),
    ) {
        crate::cartographer::Note::new("scheduler")
            .level(crate::logging::LogLevel::INFO)
            .squad(squad_id)
            .cell(cell_id)
            .task(task_name)
            .scope("mailbox")
            .emit(
                guard,
                "mailbox message enqueued for cell failure",
                serde_json::json!({"message_id": message_id, "priority": "urgent"}),
            );
    }
}

/// RAL-241 follow-up: broadcast an `urgent` mailbox message when a proof
/// step fails *after* its owning cell/task body already succeeded — the one
/// failure path [`enqueue_cell_failure_mailbox`] doesn't cover, since that
/// helper only fires from the body's own outcome (recorded and checked
/// before any proof step has even run). Covers all three proof scopes:
/// cell-level (`run_cell_worker`), a standalone proof-only restart
/// (`run_proof_only_worker`), and task-level (`run_task_finalizer`).
/// `scope` ("cell" or "task") drives the message wording; `cell_id` is
/// carried separately as an entity-linkage hint for the mailbox row and
/// Cartographer note even on a task-scope proof — `run_task_finalizer` has
/// no single *owning* cell, but does have a representative one (the same
/// one [`note_proof_outcome`] already folds the ground-truth outcome onto),
/// so board/API filtering by cell id still surfaces the escalation.
/// Best-effort, matching `enqueue_cell_failure_mailbox`.
fn enqueue_proof_failure_mailbox(
    guard: &Store,
    squad_id: &str,
    scope: &str,
    cell_id: Option<&str>,
    task_name: &str,
) {
    let text = if scope == "cell" {
        format!(
            "proof for cell '{}' in task '{task_name}' (squad {squad_id}) failed",
            cell_id.unwrap_or("?")
        )
    } else {
        format!("task-level proof for task '{task_name}' (squad {squad_id}) failed")
    };
    if let Ok(message_id) = guard.enqueue_mailbox_message(
        crate::mailbox::MailboxPriority::Urgent,
        &text,
        Some(squad_id),
        Some(task_name),
        cell_id,
    ) {
        let mut note = crate::cartographer::Note::new("scheduler")
            .level(crate::logging::LogLevel::INFO)
            .squad(squad_id)
            .task(task_name)
            .scope("mailbox");
        if let Some(cell_id) = cell_id {
            note = note.cell(cell_id);
        }
        note.emit(
            guard,
            "mailbox message enqueued for proof failure",
            serde_json::json!({"message_id": message_id, "priority": "urgent"}),
        );
    }
}

/// Run one cell — and, on success, its cell-level proofs — while
/// holding a permit from the shared semaphore, then publish the result into the
/// shared `progress` (status + failure flag). Dependency-waiting is the
/// dispatcher's job, so a permit is only ever held during real work, never
/// while parked.
#[allow(clippy::too_many_arguments)]
fn run_cell_worker(
    store: &Arc<Mutex<Store>>,
    runner: &dyn Runner,
    squad_id: &str,
    cancel: &CancelToken,
    sem: &Semaphore,
    cells: &[crate::store::CellRow],
    plan: &crate::plan::ExecutionPlan,
    progress: &Mutex<Progress>,
    i: usize,
    trace_context: Option<&str>,
) {
    let row = &cells[i];
    // Held for the worker's whole lifetime so every early return below still
    // ends the span (RAII), and so the span's own traceparent — not the raw
    // `trace_context` string — becomes the parent of this cell's runner
    // subprocess span and its proof-step spans (RAL-96).
    let cell_cx = otel::context_from_traceparent(trace_context);
    let _span = otel::start_span("scheduler.cell", &cell_cx, SpanKind::Internal);
    _span.set_attribute("cell_id", row.cell_id.clone());
    _span.set_attribute("agent", row.agent.clone());
    let cell_trace_context = otel::traceparent_from_context(&_span.cx);
    if cancel.is_cancelled() {
        return;
    }
    crate::rlog!(
        INFO,
        "ralphus [scheduler] cell {squad_id}/{} start agent={} model={}",
        row.cell_id,
        row.agent,
        row.model.as_deref().unwrap_or("default"),
    );
    {
        let guard = store.lock().expect("store mutex poisoned");
        let _ = guard.cartographer_log(crate::cartographer::CartographerEntry {
            level: crate::logging::LogLevel::INFO,
            source: "scheduler",
            message: "cell start",
            scope: Some("cell"),
            squad_id: Some(squad_id),
            guardian_id: None,
            cell_id: Some(&row.cell_id),
            task: Some(&row.task_name),
            log_path: None,
            payload: serde_json::json!({
                "agent": row.agent,
                "model": row.model,
            }),
        });
    }
    // Acquire a global slot; released when `_permit` drops at function end.
    let _permit = sem.acquire();
    if cancel.is_cancelled() {
        return;
    }
    {
        let guard = store.lock().expect("store mutex poisoned");
        let _ = guard.set_cell_state(squad_id, row.task_idx, row.idx, NodeState::Running);
        let _ = guard.set_task_state(squad_id, row.task_idx, NodeState::Running);
    }
    capture_task_baseline_if_needed(store, squad_id, row);

    // Resolve handoff placeholders against completed upstream summaries.
    let summaries = progress
        .lock()
        .expect("progress mutex poisoned")
        .summaries
        .clone();
    // RAL-136: ghost memory. Look up this cell's own prior ghost (so a
    // restart can pick up where it left off instead of rescanning from disk)
    // and its direct dependencies' ghosts -- one level up only, via
    // `plan.deps[i]` -- so downstream work doesn't have to re-derive what an
    // upstream cell already learned. This goes into the prompt text
    // itself (not `system_prompt`), since `append_system_prompt` is only
    // honoured by the claude-code backend today and every agent must see it.
    let ghost_context = {
        let guard = store.lock().expect("store mutex poisoned");
        let own_uri = crate::ghost::cell_uri(squad_id, row.task_idx, row.idx);
        let own = guard.get_ghost(&own_uri).ok().flatten();
        let parents: Vec<(String, crate::ghost::GhostView)> = plan.deps[i]
            .iter()
            .filter_map(|&d| {
                let dep = &cells[d];
                let dep_uri = crate::ghost::cell_uri(squad_id, dep.task_idx, dep.idx);
                guard
                    .get_ghost(&dep_uri)
                    .ok()
                    .flatten()
                    .map(|g| (format!("{}/{}", dep.task_name, dep.cell_id), g))
            })
            .collect();
        crate::ghost::format_context_block(own.as_ref(), &parents)
    };
    let selection = match resolve_agent_selection(&row.agent, row.cwd.as_deref().unwrap_or(".")) {
        Ok(selection) => selection,
        Err(message) => {
            let outcome = crate::store::CellOutcome {
                state: crate::store::NodeState::Failed,
                tokens_in: 0,
                tokens_out: 0,
                cost_usd: 0.0,
                error: Some(message.clone()),
                agent_session_id: None,
            };
            {
                let guard = store.lock().expect("store mutex poisoned");
                let _ = guard.record_cell_result(squad_id, row.task_idx, row.idx, &outcome);
            }
            let mut prog = progress.lock().expect("progress mutex poisoned");
            prog.status[i] = CellState::Failed;
            prog.failed.insert(row.task_idx);
            return;
        }
    };
    let mut spec = RunnerSpec::from_row(squad_id, row);
    spec.agent = selection.backend.clone();
    spec.executable = selection.executable.clone();
    spec.prompt = spec
        .prompt
        .map(|p| resolve_handoffs(&p, &plan.deps[i], &summaries));
    if let Some(ctx) = &ghost_context {
        spec.prompt = spec.prompt.map(|p| format!("{ctx}{p}"));
    }
    spec.trace_context = cell_trace_context.clone();
    spec.env_overrides = {
        let guard = store.lock().expect("store mutex poisoned");
        if let Some(snapshot) = guard
            .get_cell_materialized_env_overrides(squad_id, row.task_idx, row.idx)
            .unwrap_or_default()
        {
            snapshot
        } else {
            let merged = guard
                .resolve_cell_env_overrides(squad_id, row.task_idx, row.idx)
                .unwrap_or_default();
            let materialized = crate::worktrees::materialize_env_overrides(
                &guard,
                crate::worktrees::PlaceholderContext {
                    squad_id,
                    task_idx: row.task_idx,
                    cell_idx: row.idx,
                    task_name: &row.task_name,
                    cell_id: &row.cell_id,
                    machine: row.machine.as_deref(),
                },
                &merged,
            )
            .unwrap_or(merged);
            let _ = guard.set_cell_materialized_env_overrides(
                squad_id,
                row.task_idx,
                row.idx,
                &materialized,
            );
            materialized
        }
    };
    let mut merged_profile_env = selection.env;
    merged_profile_env.extend(spec.env_overrides.clone());
    spec.env_overrides = merged_profile_env;

    // RAL-248: continue from a completed dependency's agent session when safe
    // (cross-cell session sharing), so this cell resumes with the context the
    // prior cell already built instead of re-reading files from scratch.
    // Start fresh otherwise, logging why (model mismatch / no session to resume).
    match resolve_shared_session_id(store, squad_id, row, cells, plan, i, &selection.backend) {
        SessionShare::Resume {
            session_id,
            from_cell,
            from_task,
        } => {
            crate::rlog!(
                INFO,
                "ralphus [scheduler] cell {squad_id}/{} resuming agent session {} from {}/{}",
                row.cell_id,
                session_id,
                from_task,
                from_cell,
            );
            spec.resume_agent_session_id = Some(session_id);
        }
        SessionShare::Skip { reason } => {
            crate::rlog!(
                INFO,
                "ralphus [scheduler] cell {squad_id}/{} NOT sharing agent session (fresh cell): {reason}",
                row.cell_id,
            );
        }
        SessionShare::None => {}
    }
    {
        let guard = store.lock().expect("store mutex poisoned");
        let _ = guard.set_cell_effective_system_prompt(
            squad_id,
            row.task_idx,
            row.idx,
            spec.effective_system_prompt().as_deref(),
        );
    }

    // If this cell declares an upstream task ref, rebase its branch onto
    // the dependency's current branch tip before handing off to the runner.
    if let Some(rebase_err) = try_upstream_rebase(row, cells) {
        let outcome = crate::store::CellOutcome {
            state: crate::store::NodeState::Failed,
            tokens_in: 0,
            tokens_out: 0,
            cost_usd: 0.0,
            error: Some(rebase_err.clone()),
            agent_session_id: None,
        };
        {
            let guard = store.lock().expect("store mutex poisoned");
            let _ = guard.record_cell_result(squad_id, row.task_idx, row.idx, &outcome);
            let _ = guard.cartographer_log(crate::cartographer::CartographerEntry {
                level: crate::logging::LogLevel::WARNING,
                source: "scheduler",
                message: "cell failed: upstream rebase error",
                scope: Some("cell"),
                squad_id: Some(squad_id),
                guardian_id: None,
                cell_id: Some(&row.cell_id),
                task: Some(&row.task_name),
                log_path: None,
                payload: serde_json::json!({"error": rebase_err}),
            });
            enqueue_cell_failure_mailbox(
                &guard,
                squad_id,
                &row.cell_id,
                &row.task_name,
                Some(&rebase_err),
            );
        }
        let mut prog = progress.lock().expect("progress mutex poisoned");
        prog.status[i] = CellState::Failed;
        prog.failed.insert(row.task_idx);
        return;
    }

    let result = runner.run_cancellable(&spec, cancel);
    // A cancellation that landed while the runner was working: leave the
    // (already `cancelled`) node as the store set it and abandon this cell.
    if cancel.is_cancelled() {
        return;
    }
    crate::rlog!(
        INFO,
        "ralphus [scheduler] cell {squad_id}/{} completed status={} tokens_in={} tokens_out={}",
        row.cell_id,
        result.status,
        result.tokens_in,
        result.tokens_out,
    );
    let outcome = CellOutcome {
        state: result.node_state(),
        tokens_in: result.tokens_in,
        tokens_out: result.tokens_out,
        cost_usd: result.cost_usd,
        error: result.error.clone(),
        agent_session_id: result.agent_session_id.clone(),
    };
    {
        let guard = store.lock().expect("store mutex poisoned");
        let _ = guard.record_cell_result(squad_id, row.task_idx, row.idx, &outcome);
        let _ = guard.cartographer_log(crate::cartographer::CartographerEntry {
            level: if result.is_done() {
                crate::logging::LogLevel::INFO
            } else {
                crate::logging::LogLevel::WARNING
            },
            source: "scheduler",
            message: "cell completed",
            scope: Some("cell"),
            squad_id: Some(squad_id),
            guardian_id: None,
            cell_id: Some(&row.cell_id),
            task: Some(&row.task_name),
            log_path: None,
            payload: serde_json::json!({
                "status": result.status,
                "tokens_in": result.tokens_in,
                "tokens_out": result.tokens_out,
                "cost_usd": result.cost_usd,
                "error": result.error,
            }),
        });
        if outcome.state == NodeState::Failed {
            enqueue_cell_failure_mailbox(
                &guard,
                squad_id,
                &row.cell_id,
                &row.task_name,
                result.error.as_deref(),
            );
        }
    }

    // RAL-136: persist the cell's self-summarized handoff note, if it
    // produced one (see `cli/src/ralphus/runner/execute.py::_parse_ghost` --
    // the daemon-side one-off-LLM-call fallback from the ticket's Q1 is not
    // implemented; this path only ever sees what the running agent itself
    // reported). A rewrite merges onto whatever this cell already
    // published rather than overwriting it (Q4).
    //
    // RAL-135 (trust-kill) is not on this branch yet: when it lands, a
    // trust-kill of this cell should also publish a ghost through this
    // same path before the cell's state flips to `Killed`, so the partial
    // work isn't lost -- there is no hook for that here today.
    if let Some(ghost_text) = result.ghost.as_deref().map(str::trim) {
        if !ghost_text.is_empty() {
            let uri = crate::ghost::cell_uri(squad_id, row.task_idx, row.idx);
            let revision = crate::ghost::current_revision(row.cwd.as_deref().unwrap_or_default());
            let guard = store.lock().expect("store mutex poisoned");
            if guard
                .upsert_ghost(
                    &uri,
                    crate::ghost::KIND_CELL,
                    Some(squad_id),
                    None,
                    ghost_text,
                    revision.as_deref(),
                )
                .is_ok()
            {
                crate::cartographer::Note::new("ghost")
                    .squad(squad_id)
                    .cell(&row.cell_id)
                    .task(&row.task_name)
                    .scope("cell")
                    .emit(
                        &guard,
                        "ghost published",
                        serde_json::json!({"len": ghost_text.len()}),
                    );
            }
        }
    }

    if !result.is_done() {
        let mut prog = progress.lock().expect("progress mutex poisoned");
        prog.status[i] = CellState::Failed;
        prog.failed.insert(row.task_idx);
        return;
    }

    // Cell-level proof steps run in the cell's own working directory,
    // with `prompt`-kind steps using this cell's resolved backend
    // (agent/model) unless the step overrides `model` itself. They run *before*
    // the cell is published `Done`, so a dependant (e.g. finalize) still
    // waits for the proofs to finish — matching the old sequential order.
    let cwd = row.cwd.clone().unwrap_or_default();
    let proof_outcome = run_proofs(
        store,
        runner,
        squad_id,
        row.task_idx,
        &row.task_name,
        "cell",
        row.idx,
        Some(&row.cell_id),
        &cwd,
        &row.agent,
        row.model.as_deref(),
        row.machine.as_deref(),
        cancel,
        cell_trace_context.as_deref(),
    );
    // RAL-152: fold the ground-truth proof outcome onto this cell's own
    // ghost, independent of whatever the agent self-reported above -- so a
    // restarted cell reading its own ghost gets a reliable "the prior
    // attempt was already working" signal.
    note_proof_outcome(
        store,
        &proof_outcome,
        &crate::ghost::cell_uri(squad_id, row.task_idx, row.idx),
        squad_id,
        &row.cell_id,
        &row.task_name,
        &cwd,
        cancel,
    );
    // RAL-241 follow-up: a failing cell-level proof previously only updated
    // the in-memory `prog.status` below (for same-run dependency blocking) —
    // no mailbox message ever went out, unlike an outright body failure
    // (`enqueue_cell_failure_mailbox` above). The cell's *displayed* state
    // already reads "failed" for this case via `effective_cell_state`'s
    // read-time fold over each proof step's own persisted state (set by
    // `set_proof_result` inside `run_proofs`) — no extra write needed here,
    // and writing one would be actively wrong: it would collapse the
    // deliberate cancelled-vs-failed distinction `effective_cell_state`
    // preserves (a step a user cancelled via RAL Queue must not retroactively
    // mark a cell that otherwise finished cleanly as "failed"). Gate on
    // `steps_passed < steps_run` rather than `!all_ok`, since `all_ok` alone
    // is also false for a pure cancellation with zero genuine failures.
    if proof_outcome.steps_passed < proof_outcome.steps_run {
        let guard = store.lock().expect("store mutex poisoned");
        enqueue_proof_failure_mailbox(&guard, squad_id, "cell", Some(&row.cell_id), &row.task_name);
    }
    // Publish the terminal status and the failure flag together, so the
    // dispatcher never sees this cell Done before its proof verdict is
    // recorded — otherwise a task finalizer could launch and mark the task Done
    // while a failing cell-proof is still in flight. A failed proof
    // publishes `CellState::Failed`, not `Done` — the dispatch loop's
    // same-squad `depends_on` check (above, in the caller's caller) only ever
    // consults `CellState`, never `prog.failed` (task-scoped, used by
    // `run_task_finalizer`), so a same-squad dependent (e.g. `finalize`) would
    // otherwise be dispatched right past a failing prerequisite the instant
    // its proof verdict lands. This reuses the exact same `CellState::Failed`
    // handling the body-failure branch above already relies on to block/
    // cascade to dependents — no dispatcher change needed.
    let mut prog = progress.lock().expect("progress mutex poisoned");
    prog.summaries[i] = Some(result.summary.clone());
    prog.status[i] = if proof_outcome.all_ok {
        CellState::Done
    } else {
        CellState::Failed
    };
    if !proof_outcome.all_ok {
        prog.failed.insert(row.task_idx);
    }
}

/// Run only the proof steps for a cell whose body is already Done (e.g.
/// after `restart_cell_proof`). Skips the runner entirely; transitions the
/// cell from Running to Done/Failed in `progress` after proofs complete.
#[allow(clippy::too_many_arguments)]
fn run_proof_only_worker(
    store: &Arc<Mutex<Store>>,
    runner: &dyn Runner,
    squad_id: &str,
    cancel: &CancelToken,
    sem: &Arc<Semaphore>,
    cells: &[crate::store::CellRow],
    progress: &Mutex<Progress>,
    i: usize,
    trace_context: Option<&str>,
) {
    let row = &cells[i];
    let cx = otel::context_from_traceparent(trace_context);
    let _span = otel::start_span("scheduler.proof_only_cell", &cx, SpanKind::Internal);
    _span.set_attribute("cell_id", row.cell_id.clone());
    let span_trace_context = otel::traceparent_from_context(&_span.cx);
    if cancel.is_cancelled() {
        let mut prog = progress.lock().expect("progress mutex poisoned");
        prog.status[i] = CellState::Done;
        return;
    }
    let _permit = sem.acquire();
    if cancel.is_cancelled() {
        let mut prog = progress.lock().expect("progress mutex poisoned");
        prog.status[i] = CellState::Done;
        return;
    }
    // Mark the owning task Running while proofs execute so the board doesn't
    // show a task as Done while its cell-level proofs are still in flight.
    {
        let guard = store.lock().expect("store mutex poisoned");
        let _ = guard.set_task_state(squad_id, row.task_idx, NodeState::Running);
    }
    let cwd = row.cwd.clone().unwrap_or_default();
    let proof_outcome = run_proofs(
        store,
        runner,
        squad_id,
        row.task_idx,
        &row.task_name,
        "cell",
        row.idx,
        Some(&row.cell_id),
        &cwd,
        &row.agent,
        row.model.as_deref(),
        row.machine.as_deref(),
        cancel,
        span_trace_context.as_deref(),
    );
    // RAL-152: same ground-truth ghost note as `run_cell_worker` — applies
    // here too since a proof-only restart (`restart_cell_proof`) is one
    // of the paths this ticket calls out explicitly.
    note_proof_outcome(
        store,
        &proof_outcome,
        &crate::ghost::cell_uri(squad_id, row.task_idx, row.idx),
        squad_id,
        &row.cell_id,
        &row.task_name,
        &cwd,
        cancel,
    );
    // Same fix as `run_cell_worker`'s equivalent publish step (see its
    // comment): a failed proof must publish `CellState::Failed`, not
    // `Done`, so the same-squad `depends_on` dispatch loop actually blocks a
    // dependent instead of only recording the failure in the task-scoped
    // `prog.failed` set that loop never reads.
    //
    // RAL-241 follow-up: broadcast the same urgent mailbox escalation an
    // outright body failure gets — see `run_cell_worker`'s identical block
    // for why no NodeState write belongs here (the display fold already
    // handles it, and a write would break the cancelled-vs-failed
    // distinction).
    if proof_outcome.steps_passed < proof_outcome.steps_run {
        let guard = store.lock().expect("store mutex poisoned");
        enqueue_proof_failure_mailbox(&guard, squad_id, "cell", Some(&row.cell_id), &row.task_name);
    }
    let mut prog = progress.lock().expect("progress mutex poisoned");
    prog.status[i] = if proof_outcome.all_ok {
        CellState::Done
    } else {
        CellState::Failed
    };
    if !proof_outcome.all_ok {
        prog.failed.insert(row.task_idx);
    }
}

/// Capture a git-backed task's RAL-156 baseline commit sha the first time any
/// of its cells reaches `Running` (a no-op once one is already set — see
/// [`Store::set_task_baseline_commit`]). Skipped entirely for a task whose
/// `project` isn't registered as git, or that has no `cwd`, so the common
/// non-git-backed task never pays for a `git` subprocess call. The `git`
/// subprocess itself runs outside the store lock, matching this module's
/// "subprocess waits happen outside the store lock" rule.
fn capture_task_baseline_if_needed(
    store: &Arc<Mutex<Store>>,
    squad_id: &str,
    row: &crate::store::CellRow,
) {
    let Some(cwd) = row.cwd.as_deref() else {
        return;
    };
    let is_git = {
        let guard = store.lock().expect("store mutex poisoned");
        let Ok(Some(info)) = guard.task_commit_guard_info(squad_id, row.task_idx) else {
            return;
        };
        if info.baseline_commit_sha.is_some() {
            return;
        }
        info.project
            .as_deref()
            .and_then(|name| guard.get_project(name).ok().flatten())
            .is_some_and(|p| p.vcs == "git")
    };
    if !is_git {
        return;
    }
    if let Some(sha) = crate::proof::git_head_sha(cwd) {
        let guard = store.lock().expect("store mutex poisoned");
        let _ = guard.set_task_baseline_commit(squad_id, row.task_idx, &sha);
    }
}

/// The RAL-156 no-new-commits-since-baseline guard: for a git-backed task
/// (per [`Store::task_commit_guard_info`]'s registered `project`) that hasn't
/// opted out via `no_commit_required`, fails closed unless at least one of
/// the task's cells moved its `cwd`'s `HEAD` past the captured baseline.
/// Returns `true` (pass) whenever the guard doesn't apply — not git-backed,
/// opted out, or the `(squad_id, task_idx)` row can't be found. On failure,
/// logs both the `proof`-typed `rlog!` line and a matching Cartographer
/// event per the Logging Policy. `git` subprocess calls run outside the store
/// lock, matching this module's "subprocess waits happen outside the store
/// lock" rule.
fn check_task_no_commits_guard(
    store: &Arc<Mutex<Store>>,
    squad_id: &str,
    task_idx: i64,
    task_name: &str,
    cells: &[crate::store::CellRow],
) -> bool {
    let (info, is_git) = {
        let guard = store.lock().expect("store mutex poisoned");
        let Ok(Some(info)) = guard.task_commit_guard_info(squad_id, task_idx) else {
            return true;
        };
        let is_git = info
            .project
            .as_deref()
            .and_then(|name| guard.get_project(name).ok().flatten())
            .is_some_and(|p| p.vcs == "git");
        (info, is_git)
    };
    if info.no_commit_required || !is_git {
        return true;
    }
    let cwds: Vec<&str> = cells
        .iter()
        .filter(|s| s.task_idx == task_idx)
        .filter_map(|s| s.cwd.as_deref())
        .collect();
    let has_commit = info
        .baseline_commit_sha
        .as_deref()
        .is_some_and(|baseline| crate::proof::any_cwd_has_new_commit(&cwds, baseline));
    if has_commit {
        return true;
    }
    crate::rlog!(
        WARNING,
        "ralphus [proof] task {squad_id}/{task_name} failed: marked done with zero new commits since baseline"
    );
    let guard = store.lock().expect("store mutex poisoned");
    let _ = guard.cartographer_log(crate::cartographer::CartographerEntry {
        level: crate::logging::LogLevel::WARNING,
        source: "proof",
        message: "task failed: no commits since baseline",
        scope: Some("task"),
        squad_id: Some(squad_id),
        guardian_id: None,
        cell_id: None,
        task: Some(task_name),
        log_path: None,
        payload: serde_json::json!({
            "baseline_commit_sha": info.baseline_commit_sha,
            "cells_checked": cwds.len(),
        }),
    });
    false
}

/// Finalize a single task once all its cells are terminal: run its
/// task-level proof steps (skipped if any cell already failed) in the
/// task's first cell's working directory and resolved backend, then flip the
/// task to `Done`/`Failed` in the store. Runs concurrently with the rest of the
/// squad, so a task that finishes early no longer waits at `running` for its
/// slower siblings.
///
/// Task-level proofs acquire a slot from `sem` so they count against the same
/// global concurrency cap as cells and guardian review merges.
#[allow(clippy::too_many_arguments)]
fn run_task_finalizer(
    store: &Arc<Mutex<Store>>,
    runner: &dyn Runner,
    squad_id: &str,
    cancel: &CancelToken,
    cells: &[crate::store::CellRow],
    progress: &Mutex<Progress>,
    task_idx: i64,
    sem: &Arc<Semaphore>,
    summary_queue: &Arc<crate::summary_worker::SummaryQueue>,
    trace_context: Option<&str>,
    cancellations: &Cancellations,
) {
    let cx = otel::context_from_traceparent(trace_context);
    let _span = otel::start_span("scheduler.task_finalize", &cx, SpanKind::Internal);
    _span.set_attribute("task_idx", task_idx);
    let span_trace_context = otel::traceparent_from_context(&_span.cx);
    if cancel.is_cancelled() {
        return;
    }
    // A cell (or cell-proof) failure already condemns the task; skip the
    // task-level proofs in that case, matching the old batch behaviour.
    let already_failed = progress
        .lock()
        .expect("progress mutex poisoned")
        .failed
        .contains(&task_idx);
    let mut failed = already_failed;
    if !failed {
        let task_cell = cells.iter().find(|s| s.task_idx == task_idx);
        let cwd = task_cell.and_then(|s| s.cwd.clone()).unwrap_or_default();
        let task_name = task_cell.map(|s| s.task_name.clone()).unwrap_or_default();
        let agent = task_cell
            .map(|s| s.agent.clone())
            .unwrap_or_else(|| "claude".to_string());
        let model = task_cell.and_then(|s| s.model.clone());
        // RAL-185: a task-scope proof runs on the task's machine. Every
        // cell under one task resolves to the same machine (the affinity
        // rule), so borrowing the representative cell's value is exact
        // rather than approximate.
        let machine = task_cell.and_then(|s| s.machine.clone());
        // Acquire a slot so task-level proofs count against max_concurrent.
        // Released before calling try_start_ready_reviews_for_task so the review
        // worker can acquire a slot of its own.
        let _permit = sem.acquire();
        if cancel.is_cancelled() {
            return;
        }
        // Mark the task Running before executing its proof steps so the board
        // never shows a task as Done while proofs are in-flight.  In normal
        // first-run execution this is a no-op (the task is already Running), but
        // when all of a task's cells were already Done from a prior run (e.g.
        // a partial restart) the task can be Done in the DB while its finalizer
        // re-runs the proofs — this guard closes that window.
        {
            let guard = store.lock().expect("store mutex poisoned");
            if matches!(guard.squad_state(squad_id), Ok(SquadState::Running)) {
                let _ = guard.set_task_state(squad_id, task_idx, NodeState::Running);
            }
        }
        let proof_outcome = run_proofs(
            store,
            runner,
            squad_id,
            task_idx,
            &task_name,
            "task",
            -1,
            None,
            &cwd,
            &agent,
            model.as_deref(),
            machine.as_deref(),
            cancel,
            span_trace_context.as_deref(),
        );
        // RAL-152: task-level proof has no ghost of its own (ghosts are keyed
        // per cell/review, see `ghost.rs`), so fold the ground-truth
        // outcome onto the task's representative cell's ghost instead —
        // the same cell whose `cwd`/`agent`/`model` this task-level proof
        // already borrowed above, and the one a restart of that cell
        // actually reads back.
        if let Some(ts) = task_cell {
            note_proof_outcome(
                store,
                &proof_outcome,
                &crate::ghost::cell_uri(squad_id, task_idx, ts.idx),
                squad_id,
                &ts.cell_id,
                &task_name,
                &cwd,
                cancel,
            );
        }
        if !proof_outcome.all_ok {
            failed = true;
            progress
                .lock()
                .expect("progress mutex poisoned")
                .failed
                .insert(task_idx);
            // RAL-241 follow-up: the task's own NodeState is already flipped
            // to Failed below (the shared `state = if failed {...}` write),
            // but that alone never broadcasts an escalation — unlike a cell
            // body failing outright. Gate on `steps_passed < steps_run`
            // (a genuine failure), not just `!all_ok`, since a step a user
            // cancelled via RAL Queue also makes the scope not-all-ok without
            // anything having actually failed — same distinction
            // `run_cell_worker`'s identical block preserves. `task_cell`
            // supplies a representative cell id for the message, matching
            // `note_proof_outcome` above.
            if proof_outcome.steps_passed < proof_outcome.steps_run {
                let guard = store.lock().expect("store mutex poisoned");
                enqueue_proof_failure_mailbox(
                    &guard,
                    squad_id,
                    "task",
                    task_cell.map(|ts| ts.cell_id.as_str()),
                    &task_name,
                );
            }
        }
        drop(_permit);
    }
    if cancel.is_cancelled() {
        return;
    }
    // RAL-156: for a git-backed task, deterministically fail it here if none
    // of its cells produced a commit since the task started — the same
    // unconditional finalizer-side guard as the task-level proofs above,
    // not a TOML-configured proof kind, and not run by the manual
    // `set_status → Done` override (RAL-74 intentionally bypasses it).
    if !failed {
        let task_name = cells
            .iter()
            .find(|s| s.task_idx == task_idx)
            .map(|s| s.task_name.clone())
            .unwrap_or_default();
        if !check_task_no_commits_guard(store, squad_id, task_idx, &task_name, cells) {
            failed = true;
            progress
                .lock()
                .expect("progress mutex poisoned")
                .failed
                .insert(task_idx);
        }
    }
    // A cancellation abandons the task without a terminal write: the store has
    // already flipped its non-terminal nodes to `cancelled`.
    if cancel.is_cancelled() {
        return;
    }
    let state = if failed {
        NodeState::Failed
    } else {
        NodeState::Done
    };
    // Mark this task finalized (regardless of `did_write` below — even if the
    // store write is skipped because a mid-flight edit reset the squad, this
    // dispatcher pass's `progress` instance is being abandoned either way, so
    // any dependent still consulting `task_finalized` this pass should see
    // the true outcome) so a `task_deps` entry waiting on this task's own
    // finalizer (not just its cells) can now proceed or cascade-fail.
    progress
        .lock()
        .expect("progress mutex poisoned")
        .task_finalized
        .insert(task_idx);
    let did_write = {
        let guard = store.lock().expect("store mutex poisoned");
        // Don't clobber a squad an edit reset to Pending mid-flight (mirrors the
        // squad-level guard); leave the task for the re-run.
        if matches!(guard.squad_state(squad_id), Ok(SquadState::Running)) {
            let _ = guard.set_task_state(squad_id, task_idx, state);
            true
        } else {
            false
        }
    };
    // Guard dropped above; now check per-task review readiness without holding
    // the lock.
    if did_write && state == NodeState::Done {
        try_start_ready_reviews_for_task(
            store,
            squad_id,
            cells,
            task_idx,
            sem,
            summary_queue,
            cancellations,
        );
    }
}

/// Spawn a background merge for each guardian that is still `collecting`.
/// Uses an atomic DB transition (`collecting → merging`) so that concurrent
/// calls for the same guardian — possible when multiple task finalizers finish
/// simultaneously — never both proceed to the merge. A user can still start
/// one earlier via the merge endpoint ("allow review"); that path uses the same
/// guard. Conflict resolution uses a fresh subprocess runner, independent of the
/// cell runner.
///
/// Each spawned merge worker acquires a slot from `sem` before calling
/// [`crate::guardian_merge::run_merge_staged`], so review merges count against
/// the same global concurrency cap as cells and task-level proofs. The staged
/// merge (RAL-265) rebases just the ready prefix of branches, returning to
/// `Collecting` when only part of the stack is done, so this is safe to invoke
/// on any `collecting` guardian the new task completion may have advanced.
///
/// RAL-213: registers/removes a `guardian:{id}`-keyed cancel token around the
/// merge, same as `guardian_merge::start_merge`, so a settings change made
/// while one of these guardians is merging can stop it.
fn start_reviews(
    store: &Arc<Mutex<Store>>,
    guardian_ids: Vec<String>,
    sem: &Arc<Semaphore>,
    cancellations: &Cancellations,
) {
    for gid in guardian_ids {
        crate::rlog!(INFO, "ralphus [scheduler] review {gid} starting merge");
        {
            let guard = store.lock().expect("store mutex poisoned");
            let _ = guard.cartographer_log(crate::cartographer::CartographerEntry {
                level: crate::logging::LogLevel::INFO,
                source: "scheduler",
                message: "review starting merge",
                scope: Some("guardian"),
                squad_id: None,
                guardian_id: Some(&gid),
                cell_id: None,
                task: None,
                log_path: None,
                payload: serde_json::json!({}),
            });
        }
        let store = Arc::clone(store);
        let sem = Arc::clone(sem);
        let cancellations = cancellations.clone();
        std::thread::spawn(move || {
            // Atomically claim the merge: the first caller to win the
            // collecting→merging transition proceeds; others bail out.
            let claimed = store
                .lock()
                .expect("store mutex poisoned")
                .claim_guardian_merge(&gid)
                .unwrap_or(false);
            if claimed {
                let _permit = sem.acquire();
                // RAL-201: wrapped in `MachineRouter` (matching every
                // `server.rs` guardian-merge entry point via
                // `guardian_agent_runner`) so a review's resolver/summary/
                // etc agent invocations dispatch to its assigned machine
                // instead of always the daemon's own host.
                let local: Arc<dyn Runner> =
                    Arc::new(SubprocessRunner::from_env().with_cartographer(Arc::clone(&store)));
                let runner: Arc<dyn Runner> = Arc::new(crate::remote_runner::MachineRouter::new(
                    local,
                    Arc::clone(&store),
                ));
                let token = cancellations.register(&format!("guardian:{gid}"));
                // RAL-265: use the incremental staged merge so a guardian can
                // begin rebasing as soon as its first branch's cell is done,
                // resuming from already-built `Done` tips as later branches
                // finish, and only finalizing to `InReview` once every branch is
                // rebased.
                crate::guardian_merge::run_merge_staged(&store, runner.as_ref(), &gid, &token);
                cancellations.remove(&format!("guardian:{gid}"));
            }
        });
    }
}

/// Return the set of task indices that have at least one cell whose git
/// project root matches `git_root`. These are the tasks that must be `Done`
/// before the guardian rooted at `git_root` may begin rebasing.
///
/// Attribution is via [`crate::reviews::project_root_of`]: a cell's cwd is
/// resolved to its git project root (the parent of its shared git-common-dir),
/// then compared against the guardian's stored `git_root`. This correctly
/// handles linked worktrees, which are siblings of the main repo rather than
/// subdirectories of it.
fn guardian_blocking_tasks(cells: &[crate::store::CellRow], git_root: &str) -> HashSet<i64> {
    let root_path = Path::new(git_root);
    cells
        .iter()
        .filter(|s| {
            s.cwd.as_deref().is_some_and(|c| {
                crate::reviews::project_root_of(c)
                    .as_deref()
                    .map(|proj| Path::new(proj) == root_path)
                    .unwrap_or(false)
            })
        })
        .map(|s| s.task_idx)
        .collect()
}

/// After task `completed_task_idx` finishes Done, check every guardian derived
/// from `squad_id`: promote any of ITS branches whose own contributing cell is
/// now done to `ready` (RAL-103 — independent of sibling branches still waiting
/// on a different task, so the change summary can recompute per-branch as the
/// guardian collects), and kick off an incremental staged merge
/// (RAL-265). The staged merge rebases the contiguous prefix of branches whose
/// cells are done, stopping at the first branch still waiting on its upstream
/// task, so it needs only that `completed_task_idx` to have advanced the stack
/// — not every blocking task finished. When the completed task's branch isn't
/// yet buildable (an earlier branch is still `pending`), the staged merge
/// no-ops back to `Collecting` and a later completion re-triggers it.
#[allow(clippy::too_many_arguments)]
fn try_start_ready_reviews_for_task(
    store: &Arc<Mutex<Store>>,
    squad_id: &str,
    cells: &[crate::store::CellRow],
    completed_task_idx: i64,
    sem: &Arc<Semaphore>,
    summary_queue: &Arc<crate::summary_worker::SummaryQueue>,
    cancellations: &Cancellations,
) {
    let guardian_ids = {
        let guard = store.lock().expect("store mutex poisoned");
        // Combine guardians tagged with this squad_id AND collecting guardians
        // that have branches contributed by this squad's cells. The latter
        // handles linked reviews whose guardian was created by an earlier
        // submission and therefore carries a different squad_id.
        let mut ids = guard.guardians_for_squad(squad_id).unwrap_or_default();
        for gid in guard
            .collecting_guardians_for_cells(squad_id)
            .unwrap_or_default()
        {
            if !ids.contains(&gid) {
                ids.push(gid);
            }
        }
        ids
    };
    if guardian_ids.is_empty() {
        return;
    }
    let mut ready = Vec::new();
    for gid in &guardian_ids {
        let git_root = {
            let guard = store.lock().expect("store mutex poisoned");
            match guard.get_guardian(gid) {
                Ok(g) => g.git_root,
                Err(_) => continue,
            }
        };
        let blocking = guardian_blocking_tasks(cells, &git_root);
        // Only proceed when the just-completed task is one of the blockers; skip
        // guardians that are unrelated to this task.
        if blocking.is_empty() || !blocking.contains(&completed_task_idx) {
            continue;
        }
        // RAL-103: promote THIS guardian's own branches to `ready` as soon as
        // their own contributing cell is done, independent of any sibling
        // branch still waiting on a different task -- so the change-summary
        // panel can recompute per-branch while the guardian is still
        // collecting, instead of only learning about readiness once every
        // blocking task finishes. Mirrors the straggler path
        // (`mark_ready_branches_with_done_cells`), just invoked eagerly
        // here rather than on the periodic maintenance sweep.
        let needs_summary = {
            let guard = store.lock().expect("store mutex poisoned");
            let promoted = guard.mark_ready_branches_with_done_cells(gid).unwrap_or(0);
            promoted > 0
        };
        if needs_summary {
            // RAL-121: enqueue instead of recomputing synchronously under the
            // just-released lock -- a newly-ready branch's git-log summary is
            // background work computed by a `SummaryQueue` worker thread, not
            // something that should hold the store mutex (and therefore every
            // other daemon request, including a review page's own reads)
            // hostage for a `git log` subprocess per branch. High priority
            // because this guardian just had user-visible progress.
            summary_queue.enqueue(gid, crate::summary_worker::Priority::High);
        }
        // RAL-265: the staged merge needs only this completed task to advance
        // the stack, not all blocking tasks done — so fire it on every blocking
        // completion of a still-`collecting` guardian. It no-ops back to
        // `Collecting` if the next buildable branch isn't ready yet.
        {
            let guard = store.lock().expect("store mutex poisoned");
            let collecting = guard
                .get_guardian(gid)
                .map(|g| g.status == "collecting")
                .unwrap_or(false);
            if collecting {
                ready.push(gid.clone());
            }
            // A guardian that has already left `collecting` (e.g. `in_review`)
            // is not re-merged here — a late task completion for one of its
            // branches is handled by the maintenance sweep's straggler path,
            // not by restarting the whole review.
        }
    }
    // RAL-213: the real daemon-wide registry, threaded all the way down from
    // `execute_squad_inner`/`execute_squad_with`/`tick`, so a merge started by a
    // task finishing (the most common way a review starts) is stoppable
    // through the same `guardian:{id}` key a settings-change/cancel-and-restart
    // looks up -- not a throwaway registry `restart_guardian_merge` could
    // never see into.
    start_reviews(store, ready, sem, cancellations);
}

/// Mark every task and the squad failed — used when the squad cannot even be
/// planned (a dependency cycle) or a precondition (e.g. RAL-100 worktree
/// placeholder resolution) fails before any cell starts.
///
/// `set_squad_state`/`set_task_state` already log the state transition itself
/// (`squad X → Failed`), but not *why* — so `reason` is logged and recorded to
/// Cartographer here, once, rather than being dropped on the floor.
fn finalize_all_failed(
    store: &Arc<Mutex<Store>>,
    squad_id: &str,
    tasks: &[crate::store::TaskRow],
    reason: &str,
) {
    let guard = store.lock().expect("store mutex poisoned");
    crate::rlog!(
        ERROR,
        "ralphus [scheduler] squad {squad_id} failed before execution: {reason}"
    );
    let _ = guard.cartographer_log(crate::cartographer::CartographerEntry {
        level: crate::logging::LogLevel::ERROR,
        source: "scheduler",
        message: "squad failed before execution",
        scope: Some("squad"),
        squad_id: Some(squad_id),
        guardian_id: None,
        cell_id: None,
        task: None,
        log_path: None,
        payload: serde_json::json!({"reason": reason}),
    });
    for task in tasks {
        let _ = guard.set_task_state(squad_id, task.idx, NodeState::Failed);
    }
    let _ = guard.set_squad_state(squad_id, SquadState::Failed);
}

/// Substitute `{handoff:<task>}` placeholders in a prompt with the summary of a
/// completed upstream cell in that task (a dependency of this cell).
fn resolve_handoffs(prompt: &str, dep_positions: &[usize], summaries: &[Option<String>]) -> String {
    if !prompt.contains("{handoff:") {
        return prompt.to_string();
    }
    // Join all available upstream summaries for a generic {handoff:*} and also
    // support {handoff:<taskname>} — but since we only have positions here, a
    // simple, predictable rule: replace every {handoff:...} with the summaries
    // of this cell's completed dependencies, in order.
    let joined: String = dep_positions
        .iter()
        .filter_map(|&d| summaries.get(d).and_then(|s| s.clone()))
        .collect::<Vec<_>>()
        .join("\n---\n");
    let mut out = String::with_capacity(prompt.len());
    let mut rest = prompt;
    while let Some(start) = rest.find("{handoff:") {
        out.push_str(&rest[..start]);
        if let Some(end) = rest[start..].find('}') {
            out.push_str(&joined);
            rest = &rest[start + end + 1..];
        } else {
            out.push_str(&rest[start..]);
            rest = "";
        }
    }
    out.push_str(rest);
    out
}

/// Ground-truth outcome of a [`run_proofs`] pass, for the RAL-152
/// daemon-injected ghost note (see [`crate::ghost::proof_outcome_note`]) —
/// distinct from a plain `bool`, since callers need to know whether any step
/// actually ran before claiming anything was validated (a scope with no
/// proof steps at all must not be reported as "passed").
struct ProofOutcome {
    /// `true` iff every step that ran passed (vacuously `true` when none ran).
    all_ok: bool,
    /// How many proof steps actually executed (`ignored` steps and
    /// deferred/unimplemented kinds skipped via `continue` don't count).
    steps_run: usize,
    /// How many of `steps_run` passed.
    steps_passed: usize,
}

/// Run the `command` and `prompt` proof steps of one scope, updating each
/// step's state. `all_ok` is false if any of them fails. Other proof kinds
/// (`brain` / `approval`) are still deferred and left pending.
///
/// `agent` proofs run through the same [`Runner`] as cells do — a
/// [`RunnerSpec`] is built with `proof: true`, `prompt` set to the step's
/// instruction text, and `agent`/`model` taken from the owning cell's
/// resolved backend (`cell_agent`/`cell_model`), with the step's own
/// `model` (from `proof_specs`) overriding it when set.
#[allow(clippy::too_many_arguments)]
fn run_proofs(
    store: &Arc<Mutex<Store>>,
    runner: &dyn Runner,
    squad_id: &str,
    task_idx: i64,
    task_name: &str,
    scope: &str,
    cell_idx: i64,
    cell_sid: Option<&str>,
    cwd: &str,
    cell_agent: &str,
    cell_model: Option<&str>,
    cell_machine: Option<&str>,
    cancel: &CancelToken,
    trace_context: Option<&str>,
) -> ProofOutcome {
    let cx = otel::context_from_traceparent(trace_context);
    let selection = match resolve_agent_selection(cell_agent, cwd) {
        Ok(selection) => selection,
        Err(message) => {
            // ralphus[ignore-rlog-pair]: this low-level helper has no Store; its Store-owning caller records the structured workflow outcome
            crate::rlog!(
                WARNING,
                "ralphus [scheduler] proof scope {squad_id}/t{task_idx}/{scope} cannot resolve agent {cell_agent:?}: {message}"
            );
            return ProofOutcome {
                all_ok: false,
                steps_run: 0,
                steps_passed: 0,
            };
        }
    };
    let specs = {
        let guard = store.lock().expect("store mutex poisoned");
        guard
            .proof_specs(squad_id, task_idx, scope, cell_idx)
            .unwrap_or_default()
    };
    let mut all_ok = true;
    let mut steps_run = 0usize;
    let mut steps_passed = 0usize;
    for (idx, kind, spec, proof_model, proof_timeout, proof_budget) in specs {
        if cancel.is_cancelled() {
            return ProofOutcome {
                all_ok,
                steps_run,
                steps_passed,
            };
        }
        // A user may have manually set this step to `ignored` (RAL Queue /
        // set-status) while an earlier step in this same scope was still
        // running. Re-check fresh (not the snapshot `specs` was built from)
        // right before running it: pass it through untouched instead of
        // executing it for real and clobbering the state the user
        // deliberately set. `done` is deliberately NOT skipped here — re-
        // running an already-done proof on a full re-execution of the
        // owning task/squad is intentional (RAL-64).
        let current_state = {
            let guard = store.lock().expect("store mutex poisoned");
            guard
                .proof_state(squad_id, task_idx, scope, cell_idx, idx)
                .unwrap_or_default()
        };
        if current_state.as_deref() == Some("ignored") {
            continue;
        }
        // A manual cancel (RAL Queue / set-status) on an earlier step in this
        // same scope cascades to every step from that index onward
        // (`stop_cascade_plan`/`apply_stop_cascade` in server.rs), writing
        // `Cancelled` to each one before this loop reaches it. Unlike
        // `ignored`, a cancelled step must not satisfy the ordering gate:
        // skip it without executing and record the scope as not-all-ok so
        // the owning cell/task ends up Failed rather than Done.
        if current_state.as_deref() == Some("cancelled") {
            all_ok = false;
            continue;
        }
        // RAL-191: resolved per-step, not once per scope -- each proof step
        // carries its own narrowest env layer on top of the scope's, so two
        // steps under the same task can set the same key to different values.
        let env_overrides = {
            let guard = store.lock().expect("store mutex poisoned");
            if let Some(snapshot) = guard
                .get_proof_materialized_env_overrides(squad_id, task_idx, scope, cell_idx, idx)
                .unwrap_or_default()
            {
                snapshot
            } else {
                let merged = if scope == "task" {
                    guard.resolve_task_proof_step_env_overrides(squad_id, task_idx, idx)
                } else {
                    guard.resolve_cell_proof_step_env_overrides(squad_id, task_idx, cell_idx, idx)
                }
                .unwrap_or_default();
                let materialized = crate::worktrees::materialize_env_overrides(
                    &guard,
                    crate::worktrees::PlaceholderContext {
                        squad_id,
                        task_idx,
                        cell_idx,
                        task_name,
                        cell_id: cell_sid.unwrap_or(scope),
                        machine: cell_machine,
                    },
                    &merged,
                )
                .unwrap_or(merged);
                let _ = guard.set_proof_materialized_env_overrides(
                    squad_id,
                    task_idx,
                    scope,
                    cell_idx,
                    idx,
                    &materialized,
                );
                materialized
            }
        };
        let mut env_overrides = {
            let mut merged = selection.env.clone();
            merged.extend(env_overrides);
            merged
        };
        let (passed, output, proof_claude_id, proof_tokens_in, proof_tokens_out, proof_cost_usd) =
            match kind.as_str() {
                "command" => {
                    crate::rlog!(
                        DEBUG,
                        "ralphus [scheduler] proof {squad_id}/t{task_idx}/{scope}/#{idx} kind=command starting"
                    );
                    {
                        let guard = store.lock().expect("store mutex poisoned");
                        let _ = guard.cartographer_log(crate::cartographer::CartographerEntry {
                            level: crate::logging::LogLevel::DEBUG,
                            source: "scheduler",
                            message: "proof starting",
                            scope: Some("proof"),
                            squad_id: Some(squad_id),
                            guardian_id: None,
                            cell_id: None,
                            task: Some(task_name),
                            log_path: None,
                            payload: serde_json::json!({
                                "task_idx": task_idx,
                                "proof_scope": scope,
                                "cell_idx": cell_idx,
                                "idx": idx,
                                "kind": "command",
                            }),
                        });
                    }
                    set_proof_running(store, squad_id, task_idx, scope, cell_idx, idx);
                    // RAL-151: run through the same `Runner` (tmux-wrapped) path
                    // a `prompt`-kind proof step already does, rather than
                    // `proof::run_command_proof_capture` directly, so it can
                    // be watched live and reuses the exact same peek/pane-
                    // capture endpoints (keyed by `proof-{scope}-{idx}`, which
                    // `server.rs::proof_tmux_keys` already assumes for any
                    // proof kind).
                    let proof_span =
                        otel::start_span("scheduler.proof_command", &cx, SpanKind::Internal);
                    let mut runner_spec = RunnerSpec::for_command_proof(
                        squad_id,
                        task_name,
                        &format!("proof-{scope}-{idx}"),
                        cwd,
                        &spec,
                        &selection.backend,
                        proof_timeout.and_then(|s| u64::try_from(s).ok()),
                    );
                    runner_spec.executable = selection.executable.clone();
                    runner_spec.trace_context = otel::traceparent_from_context(&proof_span.cx);
                    runner_spec.env_overrides = std::mem::take(&mut env_overrides);
                    // RAL-185: a proof step runs where its owning cell/task does.
                    runner_spec.machine = cell_machine.map(str::to_string);
                    let result: RunnerResult = runner.run_cancellable(&runner_spec, cancel);
                    let passed = result.is_done();
                    let output = match &result.error {
                        Some(err) if result.summary.is_empty() => err.clone(),
                        Some(err) => format!("{}\n{err}", result.summary),
                        None => result.summary.clone(),
                    };
                    (
                        passed,
                        output,
                        None,
                        result.tokens_in,
                        result.tokens_out,
                        result.cost_usd,
                    )
                }
                "prompt" => {
                    let model = proof_model.as_deref().or(cell_model);
                    crate::rlog!(
                        DEBUG,
                        "ralphus [scheduler] proof {squad_id}/t{task_idx}/{scope}/#{idx} kind=prompt agent={cell_agent} model={} starting",
                        model.unwrap_or("default"),
                    );
                    {
                        let guard = store.lock().expect("store mutex poisoned");
                        let _ = guard.cartographer_log(crate::cartographer::CartographerEntry {
                            level: crate::logging::LogLevel::DEBUG,
                            source: "scheduler",
                            message: "proof starting",
                            scope: Some("proof"),
                            squad_id: Some(squad_id),
                            guardian_id: None,
                            cell_id: None,
                            task: Some(task_name),
                            log_path: None,
                            payload: serde_json::json!({
                                "task_idx": task_idx,
                                "proof_scope": scope,
                                "cell_idx": cell_idx,
                                "idx": idx,
                                "kind": "prompt",
                                "agent": cell_agent,
                                "model": model,
                            }),
                        });
                    }
                    set_proof_running(store, squad_id, task_idx, scope, cell_idx, idx);
                    let proof_span =
                        otel::start_span("scheduler.proof_prompt", &cx, SpanKind::Internal);
                    let mut runner_spec = RunnerSpec::for_proof(
                        squad_id,
                        task_name,
                        &format!("proof-{scope}-{idx}"),
                        cwd,
                        &spec,
                        &selection.backend,
                        model,
                        proof_timeout.and_then(|s| u64::try_from(s).ok()),
                        proof_budget.and_then(|b| u64::try_from(b).ok()),
                    );
                    runner_spec.executable = selection.executable.clone();
                    runner_spec.trace_context = otel::traceparent_from_context(&proof_span.cx);
                    runner_spec.env_overrides = std::mem::take(&mut env_overrides);
                    // RAL-185: a proof step runs where its owning cell/task does.
                    runner_spec.machine = cell_machine.map(str::to_string);
                    {
                        let guard = store.lock().expect("store mutex poisoned");
                        let _ = guard.set_proof_effective_system_prompt(
                            squad_id,
                            task_idx,
                            scope,
                            cell_idx,
                            idx,
                            runner_spec.effective_system_prompt().as_deref(),
                        );
                    }
                    let result: RunnerResult = runner.run_cancellable(&runner_spec, cancel);
                    let passed = result.proof_passed();
                    let output = match &result.error {
                        Some(err) if result.summary.is_empty() => err.clone(),
                        Some(err) => format!("{}\n{err}", result.summary),
                        None => result.summary.clone(),
                    };
                    (
                        passed,
                        output,
                        result.agent_session_id,
                        result.tokens_in,
                        result.tokens_out,
                        result.cost_usd,
                    )
                }
                "brain" | "approval" | "unknown" => continue, // deferred (intentional; not yet built)
                other => {
                    // Not producible by any currently-supported schema path (see
                    // `core/src/schema.rs::ProofStep` and `insert_proof`) — most
                    // likely stale data left behind by a since-renamed/removed proof
                    // kind. Left to fall into the `continue` above like brain/approval,
                    // this stayed `pending` forever, which `effective_cell_state`
                    // folds into a cell that reads "running" indefinitely even
                    // though the squad itself has already finished. Fail it instead so
                    // the state is honest and terminal.
                    let msg = format!(
                        "proof kind '{other}' is not recognized by this daemon build and can never run (likely stale data from an older schema)"
                    );
                    crate::rlog!(
                        WARNING,
                        "ralphus [scheduler] proof {squad_id}/t{task_idx}/{scope}/#{idx} kind={other} unrecognized; failing: {msg}"
                    );
                    (false, msg, None, 0, 0, 0.0)
                }
            };
        steps_run += 1;
        if passed {
            steps_passed += 1;
        }
        let state = if passed {
            NodeState::Done
        } else {
            NodeState::Failed
        };
        // RAL-160-style lifetime tracking for proof steps: a stable key
        // scoped to this exact step (owning cell's sid + step position for
        // a cell-scope step, or just the step position for a task-scope
        // one) rather than the squad's own `cell_id`/`task_idx`/`idx` --
        // those are per-squad positional identifiers, so a rerun of the same
        // TOML wouldn't otherwise let the board fold restarts together.
        let proof_key = match cell_sid {
            Some(sid) => format!("cell-proof:{sid}:{idx}"),
            None => format!("task-proof:{idx}"),
        };
        {
            let guard = store.lock().expect("store mutex poisoned");
            let _ = guard.set_proof_result(
                squad_id,
                task_idx,
                scope,
                cell_idx,
                idx,
                state,
                &output,
                proof_claude_id.as_deref(),
                proof_tokens_in,
                proof_tokens_out,
                proof_cost_usd,
            );
            let _ = guard.cartographer_log(crate::cartographer::CartographerEntry {
                level: if passed {
                    crate::logging::LogLevel::INFO
                } else {
                    crate::logging::LogLevel::WARNING
                },
                source: "scheduler",
                message: "proof completed",
                scope: Some("proof"),
                squad_id: Some(squad_id),
                guardian_id: None,
                cell_id: Some(&proof_key),
                task: Some(task_name),
                log_path: None,
                payload: serde_json::json!({
                    "proof_scope": scope,
                    "task_idx": task_idx,
                    "cell_idx": cell_idx,
                    "idx": idx,
                    "kind": kind,
                    "status": if passed { "done" } else { "failed" },
                    "tokens_in": proof_tokens_in,
                    "tokens_out": proof_tokens_out,
                    "cost_usd": proof_cost_usd,
                }),
            });
        }
        if !passed {
            all_ok = false;
        }
    }
    ProofOutcome {
        all_ok,
        steps_run,
        steps_passed,
    }
}

/// Fold a daemon-observed (ground-truth) proof outcome onto the ghost at
/// `uri` (RAL-152), independent of whatever the owning agent self-reported —
/// so a restarted cell/resolver has a reliable "was the prior attempt
/// already working" signal even when the agent didn't self-report a ghost at
/// all. No-op when no proof step actually ran (`steps_run == 0`, so there is
/// nothing to ground the claim in) or the squad was cancelled mid-proof (a
/// partial pass is not a meaningful signal either way).
#[allow(clippy::too_many_arguments)]
fn note_proof_outcome(
    store: &Arc<Mutex<Store>>,
    outcome: &ProofOutcome,
    uri: &str,
    squad_id: &str,
    cell_id: &str,
    task_name: &str,
    cwd: &str,
    cancel: &CancelToken,
) {
    if outcome.steps_run == 0 || cancel.is_cancelled() {
        return;
    }
    let note = crate::ghost::proof_outcome_note(outcome.steps_passed, outcome.steps_run);
    let revision = crate::ghost::current_revision(cwd);
    let guard = store.lock().expect("store mutex poisoned");
    if guard
        .upsert_ghost(
            uri,
            crate::ghost::KIND_CELL,
            Some(squad_id),
            None,
            &note,
            revision.as_deref(),
        )
        .is_ok()
    {
        crate::cartographer::Note::new("ghost")
            .squad(squad_id)
            .cell(cell_id)
            .task(task_name)
            .scope("cell")
            .emit(
                &guard,
                "ghost proof-outcome note recorded",
                serde_json::json!({
                    "passed": outcome.all_ok,
                    "steps_run": outcome.steps_run,
                    "steps_passed": outcome.steps_passed,
                }),
            );
    }
}

/// Mark one proof step `Running` before executing it.
fn set_proof_running(
    store: &Arc<Mutex<Store>>,
    squad_id: &str,
    task_idx: i64,
    scope: &str,
    cell_idx: i64,
    idx: i64,
) {
    let guard = store.lock().expect("store mutex poisoned");
    let _ = guard.set_proof_state(squad_id, task_idx, scope, cell_idx, idx, NodeState::Running);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::{RunnerResult, RunnerSpec};
    use std::collections::BTreeMap;

    /// Simulates the `"exit N"` shell-command convention this test module's
    /// TOML fixtures use for a `command`-kind proof step. Since RAL-151
    /// routes `command`-kind proof steps through the same `Runner` cells
    /// already use (rather than a real subprocess via
    /// `proof::run_command_proof_capture`, which genuinely executed the
    /// shell and observed its exit code), a fake `Runner` that always
    /// reports success would silently break every test relying on
    /// `"exit 1"` (or any other nonzero exit) actually failing.
    fn fake_exit_code_fails(text: &str) -> bool {
        text.strip_prefix("exit ")
            .and_then(|n| n.trim().parse::<i32>().ok())
            .is_some_and(|code| code != 0)
    }

    /// A runner that succeeds or fails based on the cell command (or, for
    /// an agent proof spec, its prompt), without any subprocess — lets us
    /// test the scheduler deterministically. Agent proof specs pass unless
    /// their prompt matches `fail_on`.
    struct FakeRunner {
        fail_on: Option<String>,
    }

    impl Runner for FakeRunner {
        fn run(&self, spec: &RunnerSpec) -> RunnerResult {
            let text = spec
                .command
                .clone()
                .or_else(|| spec.prompt.clone())
                .unwrap_or_default();
            if self.fail_on.as_deref() == Some(text.as_str()) || fake_exit_code_fails(&text) {
                RunnerResult::failure("intentional failure")
            } else {
                RunnerResult {
                    status: "done".to_string(),
                    tokens_in: 1,
                    tokens_out: 2,
                    cost_usd: 0.5,
                    summary: "ok".to_string(),
                    error: None,
                    proofed: spec.proof.then_some(true),
                    agent_session_id: None,
                    ghost: None,
                }
            }
        }
    }

    fn store_with(toml: &str) -> (Arc<Mutex<Store>>, String) {
        let mut store = Store::open_in_memory().unwrap();
        let file = toml::from_str(toml).unwrap();
        let id = store.insert_squad(&file, None, false).unwrap();
        (Arc::new(Mutex::new(store)), id)
    }

    const ONE_CELL: &str =
        "[[task]]\nname=\"build\"\n[[task.cell]]\ncwd=\".\"\ncommand=\"do-thing\"\n";

    /// Two tasks with no dependency between them — they must run in parallel.
    const TWO_INDEPENDENT_TASKS: &str = "[[task]]\nname=\"a\"\n[[task.cell]]\ncwd=\".\"\ncommand=\"x\"\n[[task]]\nname=\"b\"\n[[task.cell]]\ncwd=\".\"\ncommand=\"y\"\n";

    /// Records the peak number of cells executing at the same instant.
    ///
    /// Cells **rendezvous** rather than sleeping a fixed interval and hoping
    /// they overlap: each arrival bumps `current`, then waits until `expect`
    /// cells have arrived together (or `RENDEZVOUS_TIMEOUT` elapses).
    ///
    /// A fixed `sleep` here was flaky under full-suite parallel load — the OS
    /// could finish cell A's entire sleep before ever scheduling cell B,
    /// so `peak` observed 1 even though the scheduler had correctly dispatched
    /// both. The rendezvous inverts that: when the scheduler *is* concurrent
    /// both threads meet and release immediately (deterministically `peak ==
    /// expect`, and faster than the old sleep), and when it is genuinely
    /// serialized the first waits out the timeout and `peak` stays 1 — so a
    /// real regression is still caught, just no longer confused with CPU
    /// contention.
    struct ConcurrencyRunner {
        current: Arc<std::sync::atomic::AtomicI64>,
        peak: Arc<std::sync::atomic::AtomicI64>,
        /// How many cells must be in flight at once for the rendezvous to
        /// release early.
        expect: i64,
        /// Arrival count + condvar the waiters block on.
        gate: Arc<(Mutex<i64>, Condvar)>,
    }

    /// Upper bound on how long a [`ConcurrencyRunner`] cell waits for its
    /// peers. Only reached when the scheduler failed to run them in parallel,
    /// i.e. when the test is about to fail anyway — so it is generous enough to
    /// never be hit by mere slowness on a loaded machine.
    const RENDEZVOUS_TIMEOUT: Duration = Duration::from_secs(30);

    impl ConcurrencyRunner {
        fn new(expect: i64) -> Self {
            Self {
                current: Arc::new(std::sync::atomic::AtomicI64::new(0)),
                peak: Arc::new(std::sync::atomic::AtomicI64::new(0)),
                expect,
                gate: Arc::new((Mutex::new(0), Condvar::new())),
            }
        }

        /// Block until `expect` cells have arrived, or the timeout elapses.
        fn rendezvous(&self) {
            let (lock, cv) = &*self.gate;
            let mut arrived = lock.lock().expect("rendezvous mutex poisoned");
            *arrived += 1;
            if *arrived >= self.expect {
                cv.notify_all();
                return;
            }
            let deadline = std::time::Instant::now() + RENDEZVOUS_TIMEOUT;
            while *arrived < self.expect {
                let Some(remaining) = deadline.checked_duration_since(std::time::Instant::now())
                else {
                    break;
                };
                let (guard, timeout) = cv
                    .wait_timeout(arrived, remaining)
                    .expect("rendezvous mutex poisoned");
                arrived = guard;
                if timeout.timed_out() {
                    break;
                }
            }
        }
    }

    impl Runner for ConcurrencyRunner {
        fn run(&self, _spec: &RunnerSpec) -> RunnerResult {
            use std::sync::atomic::Ordering;
            let now = self.current.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(now, Ordering::SeqCst);
            self.rendezvous();
            self.current.fetch_sub(1, Ordering::SeqCst);
            RunnerResult {
                status: "done".to_string(),
                tokens_in: 0,
                tokens_out: 0,
                cost_usd: 0.0,
                summary: "ok".to_string(),
                error: None,
                proofed: None,
                agent_session_id: None,
                ghost: None,
            }
        }
    }

    // The whole point of the refactor: two independent tasks in one squad execute
    // concurrently (peak concurrency 2), not one-at-a-time.
    #[test]
    fn independent_cells_run_concurrently() {
        use std::sync::atomic::Ordering;
        let (store, id) = store_with(TWO_INDEPENDENT_TASKS);
        let concurrency = Arc::new(ConcurrencyRunner::new(2));
        let peak = Arc::clone(&concurrency.peak);
        let runner: Arc<dyn Runner> = concurrency;
        execute_squad(&store, runner.as_ref(), &id);

        assert_eq!(
            store.lock().unwrap().squad_state(&id).unwrap(),
            SquadState::Done
        );
        assert_eq!(
            peak.load(Ordering::SeqCst),
            2,
            "independent cells should run in parallel"
        );
    }

    #[test]
    fn successful_squad_reaches_done() {
        let (store, id) = store_with(ONE_CELL);
        let runner: Arc<dyn Runner> = Arc::new(FakeRunner { fail_on: None });
        execute_squad(&store, runner.as_ref(), &id);

        let guard = store.lock().unwrap();
        assert_eq!(guard.squad_state(&id).unwrap(), SquadState::Done);
        let squad = guard.get_squad(&id).unwrap();
        assert_eq!(squad.tasks[0].state, "done");
        assert_eq!(squad.tasks[0].cells[0].state, "done");
        assert_eq!(squad.tasks[0].cells[0].tokens_in, 1);
        assert_eq!(squad.tasks[0].cells[0].cost_usd, 0.5);
    }

    #[test]
    fn failing_cell_fails_squad() {
        let (store, id) = store_with(ONE_CELL);
        let runner: Arc<dyn Runner> = Arc::new(FakeRunner {
            fail_on: Some("do-thing".to_string()),
        });
        execute_squad(&store, runner.as_ref(), &id);

        let guard = store.lock().unwrap();
        assert_eq!(guard.squad_state(&id).unwrap(), SquadState::Failed);
        assert_eq!(guard.get_squad(&id).unwrap().tasks[0].state, "failed");
    }

    // RAL-157: soloing task 0 ("a") must keep task 1 ("b")'s cell Pending
    // for as long as the solo is active, even though the two tasks have no
    // dependency between them and would otherwise run concurrently (as
    // `independent_cells_run_concurrently` above proves) -- and un-soloing
    // must let the paused sibling dispatch and the squad reach Done.
    #[test]
    fn soloing_a_task_pauses_its_independent_sibling_until_unsoloed() {
        let (store, id) = store_with(TWO_INDEPENDENT_TASKS);
        store.lock().unwrap().solo_task(&id, 0).unwrap();

        let runner: Arc<dyn Runner> = Arc::new(FakeRunner { fail_on: None });
        let squad_store = Arc::clone(&store);
        let squad_id = id.clone();
        let handle = std::thread::spawn(move || {
            execute_squad(&squad_store, runner.as_ref(), &squad_id);
        });

        // Wait for the soloed task to finish.
        let mut waited = 0;
        loop {
            let done = store.lock().unwrap().get_squad(&id).unwrap().tasks[0].state == "done";
            if done {
                break;
            }
            assert!(waited < 200, "soloed task a never finished");
            std::thread::sleep(Duration::from_millis(10));
            waited += 1;
        }
        // Give the dispatcher several more passes' worth of time to (wrongly)
        // dispatch task b's cell if the solo gate didn't hold.
        std::thread::sleep(Duration::from_millis(150));
        {
            let guard = store.lock().unwrap();
            let squad = guard.get_squad(&id).unwrap();
            assert_eq!(
                squad.tasks[1].state, "pending",
                "un-soloed task's sibling must stay paused while any task in the squad is soloed"
            );
            assert_eq!(squad.tasks[1].cells[0].state, "pending");
            assert_eq!(
                guard.squad_state(&id).unwrap(),
                SquadState::Running,
                "squad can't finish while a task is gated by an active solo"
            );
        }

        store.lock().unwrap().unsolo_task(&id, 0).unwrap();
        handle.join().unwrap();

        let guard = store.lock().unwrap();
        assert_eq!(guard.squad_state(&id).unwrap(), SquadState::Done);
        assert_eq!(
            guard.get_squad(&id).unwrap().tasks[1].state,
            "done",
            "un-soloing resumes the paused sibling"
        );
    }

    #[test]
    fn tick_claims_and_runs_pending() {
        let (store, id) = store_with(ONE_CELL);
        let runner: Arc<dyn Runner> = Arc::new(FakeRunner { fail_on: None });

        // Tick spawns a worker thread; poll until it finishes.
        let sem = Arc::new(Semaphore::new(4));
        let summary_queue = crate::summary_worker::SummaryQueue::new();
        tick(&store, &runner, &sem, &Cancellations::new(), &summary_queue);
        let mut waited = 0;
        loop {
            let state = store.lock().unwrap().squad_state(&id).unwrap();
            if state.is_terminal() || waited > 200 {
                assert_eq!(state, SquadState::Done);
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
            waited += 1;
        }
    }

    #[test]
    fn claim_ready_claims_all_pending() {
        let (store, id) = store_with(ONE_CELL);
        // Claiming is no longer rationed by a squad-count limit; every ready squad
        // is claimed and the semaphore bounds concurrency per cell instead.
        assert_eq!(claim_ready(&store, &Cancellations::new()), vec![id.clone()]);
        let page = store
            .lock()
            .unwrap()
            .cartographer_query(&crate::cartographer::CartographerFilter {
                squad_id: Some(id),
                source: Some("scheduler".to_string()),
                limit: 10,
                ..Default::default()
            })
            .unwrap();
        assert!(
            page.rows.iter().any(|r| r.message.contains("claimed")),
            "claiming a squad should emit a Cartographer record: {:?}",
            page.rows
        );
    }

    #[test]
    fn semaphore_bounds_and_releases() {
        let sem = Semaphore::new(1);
        let a = sem.acquire();
        // Second acquire in a scoped thread must block until `a` is dropped.
        let acquired = Arc::new(std::sync::atomic::AtomicBool::new(false));
        std::thread::scope(|s| {
            let flag = Arc::clone(&acquired);
            let sem_ref = &sem;
            s.spawn(move || {
                let _b = sem_ref.acquire();
                flag.store(true, std::sync::atomic::Ordering::SeqCst);
            });
            std::thread::sleep(Duration::from_millis(30));
            assert!(!acquired.load(std::sync::atomic::Ordering::SeqCst));
            drop(a);
        });
        assert!(acquired.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[test]
    fn semaphore_zero_means_unlimited() {
        let sem = Semaphore::new(0);
        // Hold far more permits at once than any positive cap would allow.
        let permits: Vec<_> = (0..64).map(|_| sem.acquire()).collect();
        assert_eq!(sem.in_use(), 64);
        drop(permits);
        assert_eq!(sem.in_use(), 0);
    }

    /// Stands in for a long-running cell: blocks in `run_cancellable` until
    /// its token trips (as the real subprocess would be killed), then returns.
    struct BlockingRunner {
        started: Arc<std::sync::atomic::AtomicBool>,
    }

    impl Runner for BlockingRunner {
        fn run(&self, _spec: &RunnerSpec) -> RunnerResult {
            RunnerResult::failure("unused")
        }
        fn run_cancellable(&self, _spec: &RunnerSpec, cancel: &CancelToken) -> RunnerResult {
            self.started
                .store(true, std::sync::atomic::Ordering::SeqCst);
            for _ in 0..400 {
                if cancel.is_cancelled() {
                    return RunnerResult::failure("cancelled");
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            RunnerResult::failure("blocking runner was never cancelled")
        }
    }

    // Cancelling a squad mid-cell must (a) stop the worker and (b) leave the
    // squad + its nodes `cancelled`, not clobbered to done/failed by the worker.
    #[test]
    fn cancelling_mid_squad_stops_and_leaves_cancelled() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let (store, id) = store_with(ONE_CELL);
        let started = Arc::new(AtomicBool::new(false));
        let runner: Arc<dyn Runner> = Arc::new(BlockingRunner {
            started: Arc::clone(&started),
        });
        let token = CancelToken::new();

        let worker = {
            let (store, runner, token, id) = (
                Arc::clone(&store),
                Arc::clone(&runner),
                token.clone(),
                id.clone(),
            );
            std::thread::spawn(move || {
                // RAL-213: a fresh, private registry -- these tests only exercise
                // squad-level cancellation via `token`, not a guardian-merge restart.
                execute_squad_with(&store, runner.as_ref(), &id, &token, &Cancellations::new())
            })
        };

        // Wait until the cell is in flight, then cancel as the API does.
        while !started.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(2));
        }
        store.lock().unwrap().cancel(&id).unwrap();
        token.cancel();
        worker.join().unwrap();

        let guard = store.lock().unwrap();
        assert_eq!(guard.squad_state(&id).unwrap(), SquadState::Cancelled);
        let squad = guard.get_squad(&id).unwrap();
        assert_eq!(squad.tasks[0].state, "cancelled");
        assert_eq!(squad.tasks[0].cells[0].state, "cancelled");
    }

    /// RAL-1xx follow-up: `server::restart_cell` (and friends) now skip
    /// cancelling a squad's worker when the restart target is already
    /// terminal, so an unrelated still-running sibling isn't collaterally
    /// killed (see `restart_cell_does_not_cancel_unrelated_sibling_cell_in_same_squad`
    /// in `server.rs`). But without this reconciliation, the squad's *worker*
    /// itself never learns the restarted task was reset — it already
    /// finalized that task and won't look at it again — so the restarted
    /// task would just sit `Pending` in the store until the whole squad drains
    /// and a fresh worker gets claimed for it, silently serializing what
    /// should be two independent tasks running in parallel. The dispatcher
    /// loop's reclaim step must pick the restarted task back up on *this*
    /// same worker, immediately, while task "b" is still genuinely in flight.
    #[test]
    fn dispatcher_loop_picks_up_a_sibling_task_restarted_while_the_worker_is_still_running() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

        /// Task "a" fails on its first invocation, succeeds on any later one
        /// (standing in for "restarted after being fixed"). Task "b" blocks
        /// for a while so the worker's dispatcher loop stays alive long
        /// enough to observe "a" being externally restarted mid-flight.
        struct ReclaimRunner {
            a_calls: Arc<AtomicUsize>,
            b_started: Arc<AtomicBool>,
        }
        impl Runner for ReclaimRunner {
            fn run(&self, spec: &RunnerSpec) -> RunnerResult {
                if spec.task == "a" {
                    let n = self.a_calls.fetch_add(1, Ordering::SeqCst);
                    if n == 0 {
                        RunnerResult::failure("first attempt fails")
                    } else {
                        RunnerResult {
                            status: "done".to_string(),
                            tokens_in: 1,
                            tokens_out: 1,
                            cost_usd: 0.0,
                            summary: "a retried ok".to_string(),
                            error: None,
                            proofed: None,
                            agent_session_id: None,
                            ghost: None,
                        }
                    }
                } else {
                    self.b_started.store(true, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(400));
                    RunnerResult {
                        status: "done".to_string(),
                        tokens_in: 1,
                        tokens_out: 1,
                        cost_usd: 0.0,
                        summary: "b finished".to_string(),
                        error: None,
                        proofed: None,
                        agent_session_id: None,
                        ghost: None,
                    }
                }
            }
        }

        let (store, id) = store_with(TWO_INDEPENDENT_TASKS);
        let a_calls = Arc::new(AtomicUsize::new(0));
        let b_started = Arc::new(AtomicBool::new(false));
        let runner: Arc<dyn Runner> = Arc::new(ReclaimRunner {
            a_calls: Arc::clone(&a_calls),
            b_started: Arc::clone(&b_started),
        });
        let token = CancelToken::new();

        let worker = {
            let (store, runner, token, id) = (
                Arc::clone(&store),
                Arc::clone(&runner),
                token.clone(),
                id.clone(),
            );
            std::thread::spawn(move || {
                // RAL-213: a fresh, private registry -- these tests only exercise
                // squad-level cancellation via `token`, not a guardian-merge restart.
                execute_squad_with(&store, runner.as_ref(), &id, &token, &Cancellations::new())
            })
        };

        // Wait for task a's first failure to be recorded in the store, and for
        // b to be genuinely in flight (proving the worker is alive and busy
        // elsewhere). A fixed sleep here was flaky under full-suite load.
        while a_calls.load(Ordering::SeqCst) < 1 || !b_started.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(2));
        }
        let mut a_failed = false;
        for _ in 0..500 {
            if matches!(
                store.lock().unwrap().cell_state(&id, 0, 0),
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

        // Restart task a's already-failed cell at the store level — what
        // the HTTP handler does without cancelling the worker once the
        // target is confirmed terminal.
        store.lock().unwrap().restart_cell(&id, 0, 0).unwrap();

        // The still-running worker (not a fresh claim after b finishes) must
        // pick this up and re-run task a within a few dispatcher-loop ticks.
        let mut retried = false;
        for _ in 0..100 {
            if a_calls.load(Ordering::SeqCst) >= 2 {
                retried = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            retried,
            "the still-running worker must re-dispatch the restarted sibling task itself"
        );
        assert!(
            !worker.is_finished(),
            "task a must be retried while the worker is still alive driving task b, \
             not only after a fresh re-claim once the whole squad drains"
        );

        worker.join().unwrap();
    }

    /// Task "a": "work" (one cell-level proof) -> "finalize"
    /// (`depends_on=["work"]`), mirroring ral-171's `work`/`finalize`
    /// cells from squad-000000000148. Task "b" is a lone long-running
    /// cell that keeps the squad's worker thread alive while "a" is
    /// restarted, the same way ral-169/170/172 kept squad-000000000148's
    /// worker alive while ral-171 was restarted.
    const WORK_FINALIZE_WITH_PROOF: &str = "[[task]]\nname=\"a\"\n\
        [[task.cell]]\nid=\"work\"\ncwd=\".\"\ncommand=\"do-work\"\n[[task.cell.proof]]\nid=\"test\"\ncommand=\"check\"\n\
        [[task.cell]]\nid=\"finalize\"\ncwd=\".\"\ncommand=\"do-finalize\"\ndepends_on=[\"work\"]\n\
        [[task]]\nname=\"b\"\n[[task.cell]]\ncwd=\".\"\ncommand=\"do-b\"\n";

    /// Task "b"'s single cell blocks until released (standing in for a
    /// still-in-flight sibling); task "a"'s `test` proof fails on its first
    /// invocation and passes on any later one (standing in for "restarted
    /// after being fixed"); "finalize" and "work" always succeed and record
    /// when they ran.
    struct ProofReclaimRunner {
        proof_calls: Arc<std::sync::atomic::AtomicUsize>,
        finalize_started: Arc<std::sync::atomic::AtomicBool>,
        b_started: Arc<std::sync::atomic::AtomicBool>,
        b_release: Arc<std::sync::atomic::AtomicBool>,
    }

    impl Runner for ProofReclaimRunner {
        fn run(&self, spec: &RunnerSpec) -> RunnerResult {
            use std::sync::atomic::Ordering;
            if spec.task == "b" {
                self.b_started.store(true, Ordering::SeqCst);
                while !self.b_release.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(5));
                }
                return RunnerResult {
                    status: "done".to_string(),
                    tokens_in: 1,
                    tokens_out: 1,
                    cost_usd: 0.0,
                    summary: "b done".to_string(),
                    error: None,
                    proofed: None,
                    agent_session_id: None,
                    ghost: None,
                };
            }
            if spec.cell_id.starts_with("proof-") {
                let n = self.proof_calls.fetch_add(1, Ordering::SeqCst);
                if n == 0 {
                    return RunnerResult::failure("first proof attempt fails");
                }
                return RunnerResult {
                    status: "done".to_string(),
                    tokens_in: 1,
                    tokens_out: 1,
                    cost_usd: 0.0,
                    summary: "proof passed".to_string(),
                    error: None,
                    proofed: None,
                    agent_session_id: None,
                    ghost: None,
                };
            }
            if spec.cell_id == "finalize" {
                self.finalize_started.store(true, Ordering::SeqCst);
            }
            RunnerResult {
                status: "done".to_string(),
                tokens_in: 1,
                tokens_out: 1,
                cost_usd: 0.0,
                summary: "ok".to_string(),
                error: None,
                proofed: None,
                agent_session_id: None,
                ghost: None,
            }
        }
    }

    /// Reproduces squad-000000000148: ral-171's `test` proof failed; the user
    /// restarted it (`restart_cell_proof`) while a sibling task in the
    /// same squad (ral-169/170/172) was still actively running, so this squad's
    /// worker never returned and the restart had to be reconciled by the
    /// dispatcher loop's reclaim step rather than a fresh `execute_squad_inner`
    /// call. The reclaim step (see the previous test) only special-cases
    /// cells whose DB row was reset to `pending` — a proof-only restart
    /// deliberately leaves the cell row `done` (only its proof row is
    /// reset), so it falls through the reclaim filter entirely: "work"'s
    /// stale in-memory `Failed` status is never refreshed, and the dependent
    /// "finalize" is either dispatched (or re-failed) off that stale status
    /// without the proof ever actually being re-run.
    #[test]
    fn restart_cell_proof_with_live_sibling_reruns_proof_before_dispatching_dependent() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

        let (store, id) = store_with(WORK_FINALIZE_WITH_PROOF);
        let proof_calls = Arc::new(AtomicUsize::new(0));
        let finalize_started = Arc::new(AtomicBool::new(false));
        let b_started = Arc::new(AtomicBool::new(false));
        let b_release = Arc::new(AtomicBool::new(false));
        let runner: Arc<dyn Runner> = Arc::new(ProofReclaimRunner {
            proof_calls: Arc::clone(&proof_calls),
            finalize_started: Arc::clone(&finalize_started),
            b_started: Arc::clone(&b_started),
            b_release: Arc::clone(&b_release),
        });
        let token = CancelToken::new();

        let worker = {
            let (store, runner, token, id) = (
                Arc::clone(&store),
                Arc::clone(&runner),
                token.clone(),
                id.clone(),
            );
            std::thread::spawn(move || {
                // RAL-213: a fresh, private registry -- these tests only exercise
                // squad-level cancellation via `token`, not a guardian-merge restart.
                execute_squad_with(&store, runner.as_ref(), &id, &token, &Cancellations::new())
            })
        };

        // Wait for the proof's first (failing) attempt to land and for "b"
        // to be genuinely in flight (the worker is alive and busy elsewhere).
        while proof_calls.load(Ordering::SeqCst) < 1 || !b_started.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(2));
        }
        // Let task "a" actually finalize as Failed before restarting it.
        // Budget is generous (20s) because the worker thread this polls
        // competes with every other scheduler/guardian test's threads when
        // the whole suite runs in parallel -- 1s proved too tight under that
        // load even though the worker itself reacts promptly in isolation.
        let mut a_failed = false;
        for _ in 0..4000 {
            if matches!(
                store.lock().unwrap().task_state(&id, 0),
                Ok(Some(NodeState::Failed))
            ) {
                a_failed = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            a_failed,
            "task a must finalize as Failed before the restart"
        );

        // Restart the failed proof, exactly as the board's right-click
        // "Restart" action on a proof step does.
        store
            .lock()
            .unwrap()
            .restart_cell_proof(&id, 0, 0, 0)
            .unwrap();

        // Give the still-alive worker plenty of dispatcher ticks to react.
        std::thread::sleep(Duration::from_millis(300));

        assert!(
            proof_calls.load(Ordering::SeqCst) >= 2,
            "the still-alive worker must actually re-run the restarted proof, \
             not silently drop it (proof_calls={})",
            proof_calls.load(Ordering::SeqCst)
        );
        assert!(
            !finalize_started.load(Ordering::SeqCst) || proof_calls.load(Ordering::SeqCst) >= 2,
            "finalize must never dispatch before its upstream proof's retry actually completed"
        );

        b_release.store(true, Ordering::SeqCst);
        worker.join().unwrap();
    }

    const CELL_PASSING_PROOF: &str = "[[task]]\nname=\"t\"\n[[task.cell]]\ncwd=\".\"\ncommand=\"do\"\n[[task.cell.proof]]\ncommand=\"exit 0\"\n";
    const TASK_FAILING_PROOF: &str = "[[task]]\nname=\"t\"\n[[task.cell]]\ncwd=\".\"\ncommand=\"do\"\n[[task.proof]]\ncommand=\"exit 1\"\n";

    /// Records the order cells are run in (by command), for ordering tests.
    struct RecordingRunner {
        order: Arc<Mutex<Vec<String>>>,
        fail_on: Option<String>,
    }

    impl Runner for RecordingRunner {
        fn run(&self, spec: &RunnerSpec) -> RunnerResult {
            let cmd = spec.command.clone().unwrap_or_default();
            self.order.lock().unwrap().push(cmd.clone());
            if self.fail_on.as_deref() == Some(cmd.as_str()) || fake_exit_code_fails(&cmd) {
                RunnerResult::failure("intentional failure")
            } else {
                RunnerResult {
                    status: "done".to_string(),
                    tokens_in: 0,
                    tokens_out: 0,
                    cost_usd: 0.0,
                    summary: format!("did {cmd}"),
                    error: None,
                    proofed: None,
                    agent_session_id: None,
                    ghost: None,
                }
            }
        }
    }

    // `b` (listed first) depends on `a`; the scheduler must still run `a` first.
    const OUT_OF_ORDER_DEPS: &str = "[[task]]\nname=\"t\"\n[[task.cell]]\nid=\"b\"\ncwd=\".\"\ncommand=\"cmd-b\"\ndepends_on=[\"a\"]\n[[task.cell]]\nid=\"a\"\ncwd=\".\"\ncommand=\"cmd-a\"\n";

    #[test]
    fn cells_run_in_dependency_order() {
        let (store, id) = store_with(OUT_OF_ORDER_DEPS);
        let order = Arc::new(Mutex::new(Vec::new()));
        let runner: Arc<dyn Runner> = Arc::new(RecordingRunner {
            order: Arc::clone(&order),
            fail_on: None,
        });
        execute_squad(&store, runner.as_ref(), &id);
        assert_eq!(*order.lock().unwrap(), vec!["cmd-a", "cmd-b"]);
        assert_eq!(
            store.lock().unwrap().squad_state(&id).unwrap(),
            SquadState::Done
        );
    }

    #[test]
    fn dependent_cell_is_blocked_when_prerequisite_fails() {
        let (store, id) = store_with(OUT_OF_ORDER_DEPS);
        let order = Arc::new(Mutex::new(Vec::new()));
        let runner: Arc<dyn Runner> = Arc::new(RecordingRunner {
            order: Arc::clone(&order),
            fail_on: Some("cmd-a".to_string()),
        });
        execute_squad(&store, runner.as_ref(), &id);
        // `b` must never run because `a` failed.
        assert_eq!(*order.lock().unwrap(), vec!["cmd-a"]);
        assert_eq!(
            store.lock().unwrap().squad_state(&id).unwrap(),
            SquadState::Failed
        );
        let squad = store.lock().unwrap().get_squad(&id).unwrap();
        let b = squad.tasks[0].cells.iter().find(|s| s.id == "b").unwrap();
        assert_eq!(b.state, "failed");
        assert_eq!(b.error.as_deref(), Some("blocked by a failed dependency"));
    }

    #[test]
    fn failed_cell_enqueues_urgent_mailbox_message() {
        let (store, id) = store_with(ONE_CELL);
        let runner: Arc<dyn Runner> = Arc::new(FakeRunner {
            fail_on: Some("do-thing".to_string()),
        });
        execute_squad(&store, runner.as_ref(), &id);
        assert_eq!(
            store.lock().unwrap().squad_state(&id).unwrap(),
            SquadState::Failed
        );

        let guard = store.lock().unwrap();
        let client_id = guard.register_mailbox_client().unwrap();
        let messages = guard
            .mailbox_messages_for_client(&client_id, true, None)
            .unwrap();
        assert_eq!(messages.len(), 1, "expected exactly one mailbox message");
        assert_eq!(messages[0].priority, "urgent");
        assert!(messages[0].message.contains("failed"));
        assert!(messages[0].message.contains("intentional failure"));
        assert_eq!(messages[0].squad_id.as_deref(), Some(id.as_str()));
        assert_eq!(messages[0].task.as_deref(), Some("build"));
    }

    // `a`'s own body succeeds, but it carries a cell-scope proof that
    // always fails; `b` depends on `a`.
    const CELL_WITH_FAILING_PROOF_HAS_DEPENDENT: &str = "[[task]]\nname=\"t\"\n[[task.cell]]\nid=\"a\"\ncwd=\".\"\ncommand=\"cmd-a\"\n[[task.cell.proof]]\ncommand=\"exit 1\"\n[[task.cell]]\nid=\"b\"\ncwd=\".\"\ncommand=\"cmd-b\"\ndepends_on=[\"a\"]\n";

    #[test]
    fn dependent_cell_is_blocked_when_prerequisites_proof_fails() {
        // Regression: `a`'s body finishes cleanly, but its own cell-scope
        // proof fails. Before the fix, `run_cell_worker` published
        // `CellState::Done` unconditionally once the proof finished
        // (recording the failure only in the task-scoped `prog.failed` set,
        // which the same-squad `depends_on` dispatch loop never reads) — so
        // `b` was dispatched right past a failing prerequisite the instant
        // `a`'s proof verdict landed, even though `a` itself never
        // "succeeded" in any meaningful sense. `b` must never run.
        let (store, id) = store_with(CELL_WITH_FAILING_PROOF_HAS_DEPENDENT);
        let order = Arc::new(Mutex::new(Vec::new()));
        let runner: Arc<dyn Runner> = Arc::new(RecordingRunner {
            order: Arc::clone(&order),
            fail_on: None,
        });
        execute_squad(&store, runner.as_ref(), &id);
        // RAL-151: the "exit 1" proof step now runs through the same
        // `Runner` (and so shows up in `order` too) rather than a direct,
        // unrecorded subprocess — "b" (`cmd-b`) still must never run.
        assert_eq!(*order.lock().unwrap(), vec!["cmd-a", "exit 1"]);
        assert_eq!(
            store.lock().unwrap().squad_state(&id).unwrap(),
            SquadState::Failed
        );
        let squad = store.lock().unwrap().get_squad(&id).unwrap();
        let b = squad.tasks[0].cells.iter().find(|s| s.id == "b").unwrap();
        assert_eq!(b.state, "failed");
        assert_eq!(b.error.as_deref(), Some("blocked by a failed dependency"));
    }

    #[test]
    fn cell_scope_proof_failure_enqueues_urgent_mailbox_message_and_flips_cell_state() {
        // RAL-241 follow-up regression: a cell whose body succeeds but whose
        // attached proof fails previously left the cell's persisted
        // NodeState untouched (still whatever the successful body itself
        // had already recorded) and never enqueued a mailbox message —
        // unlike an outright cell body failure, which does both (see
        // `failed_cell_enqueues_urgent_mailbox_message`). Reuses the same
        // fixture as `dependent_cell_is_blocked_when_prerequisites_proof_fails`.
        let (store, id) = store_with(CELL_WITH_FAILING_PROOF_HAS_DEPENDENT);
        let runner: Arc<dyn Runner> = Arc::new(RecordingRunner {
            order: Arc::new(Mutex::new(Vec::new())),
            fail_on: None,
        });
        execute_squad(&store, runner.as_ref(), &id);

        let squad = store.lock().unwrap().get_squad(&id).unwrap();
        let a = squad.tasks[0].cells.iter().find(|s| s.id == "a").unwrap();
        assert_eq!(
            a.state, "failed",
            "cell body succeeded but its proof failed"
        );

        let guard = store.lock().unwrap();
        let client_id = guard.register_mailbox_client().unwrap();
        let messages = guard
            .mailbox_messages_for_client(&client_id, true, None)
            .unwrap();
        assert_eq!(messages.len(), 1, "expected exactly one mailbox message");
        assert_eq!(messages[0].priority, "urgent");
        assert!(messages[0].message.contains("proof"));
        assert!(messages[0].message.contains("failed"));
        assert_eq!(messages[0].squad_id.as_deref(), Some(id.as_str()));
        assert_eq!(messages[0].cell_id.as_deref(), Some("a"));
    }

    const TASK_LEVEL_DEPENDS_ON_HAS_FAILING_UPSTREAM_PROOF: &str = "[[task]]\nname=\"a\"\n[[task.cell]]\ncwd=\".\"\ncommand=\"cmd-a\"\n[[task.proof]]\ncommand=\"exit 1\"\n\
         [[task]]\nname=\"b\"\ndepends_on=[\"a\"]\n[[task.cell]]\ncwd=\".\"\ncommand=\"cmd-b\"\n";

    #[test]
    fn task_name_dependent_is_blocked_when_upstream_tasks_own_proof_fails() {
        // Regression: task "b" declares `depends_on = ["a"]` at the TASK
        // level (a bare task-name reference, not "a/cell"). "a"'s cell
        // body finishes cleanly, but "a"'s own task-level proof (run by
        // `run_task_finalizer`, separately from and after all of "a"'s
        // cells) fails. Before the `task_deps` fix, the same-squad dispatch
        // loop resolved a task-name reference purely to "a"'s cell
        // positions — satisfied the instant those cells reached
        // `CellState::Done`, before "a"'s task-level proof even started
        // running. "b" must never run (caught live in production on
        // 2026-08-11: RAL-142-project-filter-facet started before
        // RAL-141-project-identifier's own fmt/clippy/test proof finished).
        let (store, id) = store_with(TASK_LEVEL_DEPENDS_ON_HAS_FAILING_UPSTREAM_PROOF);
        let order = Arc::new(Mutex::new(Vec::new()));
        let runner: Arc<dyn Runner> = Arc::new(RecordingRunner {
            order: Arc::clone(&order),
            fail_on: None,
        });
        execute_squad(&store, runner.as_ref(), &id);
        // RAL-151: the "exit 1" task-level proof now runs through the same
        // `Runner` (and so shows up in `order` too) rather than a direct,
        // unrecorded subprocess — "b" (`cmd-b`) still must never run.
        assert_eq!(*order.lock().unwrap(), vec!["cmd-a", "exit 1"]);
        assert_eq!(
            store.lock().unwrap().squad_state(&id).unwrap(),
            SquadState::Failed
        );
        let squad = store.lock().unwrap().get_squad(&id).unwrap();
        let a_task = squad.tasks.iter().find(|t| t.name == "a").unwrap();
        assert_eq!(a_task.state, "failed");
        let b_task = squad.tasks.iter().find(|t| t.name == "b").unwrap();
        assert_eq!(b_task.state, "failed");
        let b_cell = b_task.cells.first().unwrap();
        assert_eq!(b_cell.state, "failed");
        assert_eq!(
            b_cell.error.as_deref(),
            Some("blocked by a failed dependency")
        );
    }

    #[test]
    fn task_scope_proof_failure_enqueues_urgent_mailbox_message() {
        // RAL-241 follow-up regression: a task-level proof failure (run by
        // `run_task_finalizer`, separate from any cell) already flips the
        // task's own NodeState to Failed (asserted above, on `a_task`), but
        // never enqueued a mailbox message. Reuses the same fixture as
        // `task_name_dependent_is_blocked_when_upstream_tasks_own_proof_fails`.
        let (store, id) = store_with(TASK_LEVEL_DEPENDS_ON_HAS_FAILING_UPSTREAM_PROOF);
        let runner: Arc<dyn Runner> = Arc::new(RecordingRunner {
            order: Arc::new(Mutex::new(Vec::new())),
            fail_on: None,
        });
        execute_squad(&store, runner.as_ref(), &id);

        let guard = store.lock().unwrap();
        let client_id = guard.register_mailbox_client().unwrap();
        let messages = guard
            .mailbox_messages_for_client(&client_id, true, None)
            .unwrap();
        assert_eq!(messages.len(), 1, "expected exactly one mailbox message");
        assert_eq!(messages[0].priority, "urgent");
        assert!(messages[0].message.contains("task-level proof"));
        assert!(messages[0].message.contains("failed"));
        assert_eq!(messages[0].squad_id.as_deref(), Some(id.as_str()));
        assert_eq!(messages[0].task.as_deref(), Some("a"));
    }

    #[test]
    fn dependency_cycle_fails_the_squad() {
        let cyclic = "[[task]]\nname=\"t\"\n[[task.cell]]\nid=\"a\"\ncwd=\".\"\ncommand=\"x\"\ndepends_on=[\"b\"]\n[[task.cell]]\nid=\"b\"\ncwd=\".\"\ncommand=\"y\"\ndepends_on=[\"a\"]\n";
        let (store, id) = store_with(cyclic);
        let runner: Arc<dyn Runner> = Arc::new(FakeRunner { fail_on: None });
        execute_squad(&store, runner.as_ref(), &id);
        assert_eq!(
            store.lock().unwrap().squad_state(&id).unwrap(),
            SquadState::Failed
        );
    }

    #[test]
    fn finalize_all_failed_records_reason_to_cartographer() {
        // A precondition failure (here: a dependency cycle, cheap to trigger
        // without touching git) must not just flip the squad to Failed silently
        // -- the reason string finalize_all_failed receives has to actually
        // land somewhere queryable, or a user staring at "squad -> Failed" in
        // the board has no way to find out why.
        let cyclic = "[[task]]\nname=\"t\"\n[[task.cell]]\nid=\"a\"\ncwd=\".\"\ncommand=\"x\"\ndepends_on=[\"b\"]\n[[task.cell]]\nid=\"b\"\ncwd=\".\"\ncommand=\"y\"\ndepends_on=[\"a\"]\n";
        let (store, id) = store_with(cyclic);
        let runner: Arc<dyn Runner> = Arc::new(FakeRunner { fail_on: None });
        execute_squad(&store, runner.as_ref(), &id);

        let guard = store.lock().unwrap();
        let page = guard
            .cartographer_query(&crate::cartographer::CartographerFilter {
                squad_id: Some(id.clone()),
                limit: 100,
                ..Default::default()
            })
            .unwrap();
        assert!(
            page.rows
                .iter()
                .any(|r| r.message == "squad failed before execution"
                    && r.payload["reason"]
                        .as_str()
                        .is_some_and(|reason| !reason.is_empty())),
            "expected a Cartographer entry recording the failure reason: {:?}",
            page.rows
        );
    }

    #[test]
    fn passing_proof_keeps_squad_done() {
        let (store, id) = store_with(CELL_PASSING_PROOF);
        let runner: Arc<dyn Runner> = Arc::new(FakeRunner { fail_on: None });
        execute_squad(&store, runner.as_ref(), &id);
        assert_eq!(
            store.lock().unwrap().squad_state(&id).unwrap(),
            SquadState::Done
        );
    }

    #[test]
    fn failing_proof_fails_squad_even_when_cell_succeeds() {
        let (store, id) = store_with(TASK_FAILING_PROOF);
        let runner: Arc<dyn Runner> = Arc::new(FakeRunner { fail_on: None });
        execute_squad(&store, runner.as_ref(), &id);
        assert_eq!(
            store.lock().unwrap().squad_state(&id).unwrap(),
            SquadState::Failed
        );
    }

    // RAL-152: a daemon-observed proof outcome must be folded onto the
    // owning cell's ghost, independent of the agent's own self-report
    // (`FakeRunner` never sets `ghost`, so any note found here can only have
    // come from `note_proof_outcome`).
    #[test]
    fn passing_cell_proof_notes_validated_outcome_in_ghost() {
        let (store, id) = store_with(CELL_PASSING_PROOF);
        let runner: Arc<dyn Runner> = Arc::new(FakeRunner { fail_on: None });
        execute_squad(&store, runner.as_ref(), &id);
        let uri = crate::ghost::cell_uri(&id, 0, 0);
        let ghost = store.lock().unwrap().get_ghost(&uri).unwrap().unwrap();
        assert!(
            ghost.content.contains("internally validated"),
            "expected a ground-truth validated note in the ghost: {}",
            ghost.content
        );
    }

    // Task-level proof has no ghost of its own -- the outcome must land on
    // the task's representative cell's ghost instead (see
    // `run_task_finalizer`'s call to `note_proof_outcome`).
    #[test]
    fn failing_task_proof_notes_unvalidated_outcome_in_ghost() {
        let (store, id) = store_with(TASK_FAILING_PROOF);
        let runner: Arc<dyn Runner> = Arc::new(FakeRunner { fail_on: None });
        execute_squad(&store, runner.as_ref(), &id);
        let uri = crate::ghost::cell_uri(&id, 0, 0);
        let ghost = store.lock().unwrap().get_ghost(&uri).unwrap().unwrap();
        assert!(
            ghost.content.contains("did NOT all pass"),
            "expected a ground-truth failed note in the ghost: {}",
            ghost.content
        );
    }

    #[test]
    fn no_proof_steps_writes_no_ghost_note() {
        let toml = "[[task]]\nname=\"t\"\n[[task.cell]]\ncwd=\".\"\ncommand=\"do\"\n";
        let (store, id) = store_with(toml);
        let runner: Arc<dyn Runner> = Arc::new(FakeRunner { fail_on: None });
        execute_squad(&store, runner.as_ref(), &id);
        let uri = crate::ghost::cell_uri(&id, 0, 0);
        assert!(
            store.lock().unwrap().get_ghost(&uri).unwrap().is_none(),
            "a scope with no proof steps must never claim its work was validated"
        );
    }

    // Two cell-level proof steps: the second would fail the squad if it
    // were actually executed ("exit 1").
    const CELL_TWO_PROOFS_SECOND_WOULD_FAIL: &str = "[[task]]\nname=\"t\"\n[[task.cell]]\ncwd=\".\"\ncommand=\"do\"\n\
         [[task.cell.proof]]\ncommand=\"exit 0\"\n\
         [[task.cell.proof]]\ncommand=\"exit 1\"\n";

    #[test]
    fn ignored_proof_step_is_never_executed_and_squad_still_passes() {
        // Manually setting a not-yet-run proof step to `ignored` must be a
        // true pass-through — the scheduler must never actually execute it
        // (which would clobber the user's override with a real pass/fail
        // result) and downstream must proceed as if it passed.
        let (store, id) = store_with(CELL_TWO_PROOFS_SECOND_WOULD_FAIL);
        store
            .lock()
            .unwrap()
            .set_proof_state(&id, 0, "cell", 0, 1, NodeState::Ignored)
            .unwrap();

        let runner: Arc<dyn Runner> = Arc::new(FakeRunner { fail_on: None });
        execute_squad(&store, runner.as_ref(), &id);

        let guard = store.lock().unwrap();
        assert_eq!(
            guard.squad_state(&id).unwrap(),
            SquadState::Done,
            "the ignored step must not fail the squad even though its command would fail if run for real"
        );
        let squad = guard.get_squad(&id).unwrap();
        let proofs = &squad.tasks[0].cells[0].proof;
        assert_eq!(proofs[0].state, "done");
        assert_eq!(
            proofs[1].state, "ignored",
            "ignored proof step must stay ignored, not be overwritten by actually running it"
        );
    }

    // Three cell-level proof steps: the second and third are pre-cancelled,
    // mirroring the RAL Queue cascade a manual `set-status ... cancelled` on
    // the second step writes onto every step from that index onward
    // (`stop_cascade_plan`/`apply_stop_cascade` in server.rs) before the
    // scheduler's per-cell proof loop ever reaches them.
    const CELL_THREE_PROOFS_TAIL_CANCELLED: &str = "[[task]]\nname=\"t\"\n[[task.cell]]\ncwd=\".\"\ncommand=\"do\"\n\
         [[task.cell.proof]]\ncommand=\"exit 0\"\n\
         [[task.cell.proof]]\ncommand=\"cargo-test\"\n\
         [[task.cell.proof]]\ncommand=\"lint\"\n";

    #[test]
    fn cancelled_proof_step_blocks_the_next_step_and_fails_the_task() {
        // Regression: cancelling one running proof step must halt the
        // sequence there, the same way a failed one does -- it must not let
        // the next step in that cell's proof list run for real, and the
        // owning task/squad must not be reported as successfully Done.
        let (store, id) = store_with(CELL_THREE_PROOFS_TAIL_CANCELLED);
        store
            .lock()
            .unwrap()
            .set_proof_state(&id, 0, "cell", 0, 1, NodeState::Cancelled)
            .unwrap();
        store
            .lock()
            .unwrap()
            .set_proof_state(&id, 0, "cell", 0, 2, NodeState::Cancelled)
            .unwrap();

        let order = Arc::new(Mutex::new(Vec::new()));
        let runner: Arc<dyn Runner> = Arc::new(RecordingRunner {
            order: Arc::clone(&order),
            fail_on: None,
        });
        execute_squad(&store, runner.as_ref(), &id);

        assert_eq!(
            *order.lock().unwrap(),
            vec!["do", "exit 0"],
            "a cancelled proof step (and anything after it) must never actually execute"
        );

        let guard = store.lock().unwrap();
        assert_eq!(
            guard.squad_state(&id).unwrap(),
            SquadState::Failed,
            "a cancelled proof step must not let the owning cell/task be reported as done"
        );
        let squad = guard.get_squad(&id).unwrap();
        assert_eq!(
            squad.tasks[0].state, "failed",
            "the owning task must not be reported as done"
        );
        let cell = &squad.tasks[0].cells[0];
        assert_eq!(
            cell.state, "done",
            "the cell body itself genuinely finished -- effective_cell_state \
             keeps a cancelled (as opposed to failed) proof from retroactively \
             marking it, same as a whole-squad cancel already does (see \
             store::tests::cancel_flips_nonterminal_nodes_but_preserves_finished_ones)"
        );
        let proofs = &cell.proof;
        assert_eq!(proofs[0].state, "done");
        assert_eq!(
            proofs[1].state, "cancelled",
            "the cancelled step's own state must be preserved, not overwritten"
        );
        assert_eq!(
            proofs[2].state, "cancelled",
            "a step after the cancelled one must never run and get a real verdict"
        );
    }

    // ── prompt-kind proof execution ──────────────────────────────────────

    const TASK_PROMPT_PROOF: &str = "[[task]]\nname=\"t\"\n[[task.cell]]\ncwd=\".\"\ncommand=\"do\"\nagent=\"ollama\"\nmodel=\"qwen3:8b\"\n[[task.proof]]\nid=\"check\"\nprompt=\"check it\"\n";
    const TASK_PROMPT_PROOF_MODEL_OVERRIDE: &str = "[[task]]\nname=\"t\"\n[[task.cell]]\ncwd=\".\"\ncommand=\"do\"\nagent=\"ollama\"\nmodel=\"qwen3:8b\"\n[[task.proof]]\nprompt=\"check it\"\nmodel=\"qwen2:1b\"\n";
    const TASK_BRAIN_PROOF: &str = "[[task]]\nname=\"t\"\n[[task.cell]]\ncwd=\".\"\ncommand=\"do\"\n[[task.proof]]\nbrain=\"check it\"\n";

    #[test]
    fn prompt_proof_pass_keeps_squad_done_and_records_output() {
        let (store, id) = store_with(TASK_PROMPT_PROOF);
        let runner: Arc<dyn Runner> = Arc::new(FakeRunner { fail_on: None });
        execute_squad(&store, runner.as_ref(), &id);
        let guard = store.lock().unwrap();
        assert_eq!(guard.squad_state(&id).unwrap(), SquadState::Done);
        let squad = guard.get_squad(&id).unwrap();
        let v = &squad.tasks[0].proof[0];
        assert_eq!(v.kind, "prompt");
        assert_eq!(v.state, "done");
        assert_eq!(v.output.as_deref(), Some("ok"));
    }

    #[test]
    fn prompt_proof_persists_token_and_cost_usage() {
        // Regression (RAL-185 Phase 0): a `prompt`-kind proof's LLM spend was
        // silently discarded -- `run_proofs` dropped the runner result's
        // token/cost fields on the floor, so the board's per-proof token row
        // showed a permanent `input 0 · output 0 · $0.0000`. Nothing errored
        // and the code compiled clean, so only a test catches a repeat.
        let (store, id) = store_with(TASK_PROMPT_PROOF);
        let runner: Arc<dyn Runner> = Arc::new(FakeRunner { fail_on: None });
        execute_squad(&store, runner.as_ref(), &id);
        let guard = store.lock().unwrap();
        let squad = guard.get_squad(&id).unwrap();
        let v = &squad.tasks[0].proof[0];
        assert_eq!(v.state, "done");
        // FakeRunner's success path reports 1 in / 2 out / $0.5.
        assert_eq!(v.tokens_in, 1, "proof must persist its input tokens");
        assert_eq!(v.tokens_out, 2, "proof must persist its output tokens");
        assert!(
            (v.cost_usd - 0.5).abs() < f64::EPSILON,
            "proof must persist its cost, got {}",
            v.cost_usd
        );
    }

    #[test]
    fn proof_completion_emits_cartographer_event_keyed_for_lifetime_totals() {
        // Regression (RAL-185 Phase 0): the board's "Σ load total" button folds
        // every attempt of one proof step together by querying Cartographer for
        // `"proof completed"` rows under a stable `proof_key`. When that
        // emission went missing the button silently returned nothing, since a
        // zero-match query is indistinguishable from "never ran".
        let (store, id) = store_with(TASK_PROMPT_PROOF);
        let runner: Arc<dyn Runner> = Arc::new(FakeRunner { fail_on: None });
        execute_squad(&store, runner.as_ref(), &id);
        let guard = store.lock().unwrap();
        let page = guard
            .cartographer_query(&crate::cartographer::CartographerFilter {
                squad_id: Some(id.clone()),
                q: Some("proof completed".to_string()),
                ..crate::cartographer::CartographerFilter::recent(50)
            })
            .unwrap();
        let row = page
            .rows
            .iter()
            .find(|r| r.message == "proof completed")
            .expect("a completed proof must emit a 'proof completed' event");
        // Task-scope steps key on step position alone; cell-scope steps key
        // on the owning cell's sid too. TASK_PROMPT_PROOF has one task-scope
        // step at position 0.
        assert_eq!(row.cell_id.as_deref(), Some("task-proof:0"));
        assert_eq!(row.scope.as_deref(), Some("proof"));
        assert_eq!(row.payload["tokens_in"], 1);
        assert_eq!(row.payload["tokens_out"], 2);
        assert_eq!(row.payload["status"], "done");
    }

    #[test]
    fn prompt_proof_fail_fails_squad_and_records_output() {
        let (store, id) = store_with(TASK_PROMPT_PROOF);
        let runner: Arc<dyn Runner> = Arc::new(FakeRunner {
            fail_on: Some("check it".to_string()),
        });
        execute_squad(&store, runner.as_ref(), &id);
        let guard = store.lock().unwrap();
        assert_eq!(guard.squad_state(&id).unwrap(), SquadState::Failed);
        let squad = guard.get_squad(&id).unwrap();
        assert_eq!(squad.tasks[0].proof[0].state, "failed");
    }

    #[test]
    fn brain_proof_kind_stays_pending_and_does_not_block_the_squad() {
        // Regression: brain/approval proofs are still deferred, unlike
        // prompt proofs now that they're implemented.
        let (store, id) = store_with(TASK_BRAIN_PROOF);
        let runner: Arc<dyn Runner> = Arc::new(FakeRunner { fail_on: None });
        execute_squad(&store, runner.as_ref(), &id);
        let guard = store.lock().unwrap();
        assert_eq!(guard.squad_state(&id).unwrap(), SquadState::Done);
        let squad = guard.get_squad(&id).unwrap();
        assert_eq!(squad.tasks[0].proof[0].state, "pending");
    }

    #[test]
    fn proof_with_unrecognized_kind_fails_instead_of_staying_pending_forever() {
        // Regression: a proof row whose `kind` isn't producible by any
        // currently-supported schema path (e.g. stale data from a since-
        // renamed/removed kind) used to fall into the same `continue` as
        // brain/approval and sit at `pending` forever, which
        // `effective_cell_state` folds into a cell that displays
        // "running" indefinitely even though the squad has already finished.
        let (store, id) = store_with(ONE_CELL);
        {
            let guard = store.lock().unwrap();
            guard
                .conn
                .execute(
                    "INSERT INTO proofs(squad_id, task_idx, scope, cell_idx, idx, vid, kind, spec, model, agent, state, timeout_sec, budget_tokens)
                     VALUES(?, 0, 'task', -1, 0, 'stale', 'agent', 'whatever', NULL, 'claude', 'pending', NULL, NULL)",
                    rusqlite::params![id],
                )
                .unwrap();
        }
        let runner: Arc<dyn Runner> = Arc::new(FakeRunner { fail_on: None });
        execute_squad(&store, runner.as_ref(), &id);
        let guard = store.lock().unwrap();
        assert_eq!(guard.squad_state(&id).unwrap(), SquadState::Failed);
        let squad = guard.get_squad(&id).unwrap();
        let v = &squad.tasks[0].proof[0];
        assert_eq!(v.state, "failed");
        assert!(
            v.output
                .as_deref()
                .unwrap_or_default()
                .contains("kind 'agent'")
        );
    }

    /// The `(agent, model)` of one captured proof-kind spec.
    type SeenAgentModel = (String, Option<String>);

    /// Captures the `(agent, model)` of every proof-kind spec it's asked to run.
    struct SpecCapturingRunner {
        seen: Arc<Mutex<Vec<SeenAgentModel>>>,
    }

    impl Runner for SpecCapturingRunner {
        fn run(&self, spec: &RunnerSpec) -> RunnerResult {
            if spec.proof {
                self.seen
                    .lock()
                    .unwrap()
                    .push((spec.agent.clone(), spec.model.clone()));
            }
            RunnerResult {
                status: "done".to_string(),
                tokens_in: 0,
                tokens_out: 0,
                cost_usd: 0.0,
                summary: "ok".to_string(),
                error: None,
                proofed: spec.proof.then_some(true),
                agent_session_id: None,
                ghost: None,
            }
        }
    }

    #[test]
    fn prompt_proof_inherits_cell_agent_and_model_by_default() {
        let (store, id) = store_with(TASK_PROMPT_PROOF);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let runner: Arc<dyn Runner> = Arc::new(SpecCapturingRunner {
            seen: Arc::clone(&seen),
        });
        execute_squad(&store, runner.as_ref(), &id);
        assert_eq!(
            *seen.lock().unwrap(),
            vec![("ollama".to_string(), Some("qwen3:8b".to_string()))]
        );
    }

    #[test]
    fn prompt_proof_own_model_overrides_cell_model() {
        let (store, id) = store_with(TASK_PROMPT_PROOF_MODEL_OVERRIDE);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let runner: Arc<dyn Runner> = Arc::new(SpecCapturingRunner {
            seen: Arc::clone(&seen),
        });
        execute_squad(&store, runner.as_ref(), &id);
        assert_eq!(
            *seen.lock().unwrap(),
            vec![("ollama".to_string(), Some("qwen2:1b".to_string()))]
        );
    }

    // Two independent tasks; task "b" has a task-level prompt proof.
    const TWO_TASKS_B_HAS_PROMPT_PROOF: &str = "[[task]]\nname=\"a\"\n[[task.cell]]\ncwd=\".\"\ncommand=\"cmd-a\"\n\
         [[task]]\nname=\"b\"\n[[task.cell]]\ncwd=\".\"\ncommand=\"cmd-b\"\nagent=\"ollama\"\nmodel=\"qwen3:8b\"\n\
         [[task.proof]]\nprompt=\"proof it\"\n";

    /// Snapshots the DB state of the task named in `spec.task` the moment a
    /// proof spec runs — lets the RAL-64 regression test assert the task is
    /// Running (not Done) while its proof is executing.
    struct ProofStateCapturingRunner {
        store: Arc<Mutex<Store>>,
        captured: Arc<Mutex<Vec<String>>>,
    }

    impl Runner for ProofStateCapturingRunner {
        fn run(&self, spec: &RunnerSpec) -> RunnerResult {
            if spec.proof {
                let guard = self.store.lock().unwrap();
                if let Ok(squad) = guard.get_squad(&spec.squad_id) {
                    if let Some(task) = squad.tasks.iter().find(|t| t.name == spec.task) {
                        self.captured.lock().unwrap().push(task.state.clone());
                    }
                }
            }
            RunnerResult {
                status: "done".to_string(),
                tokens_in: 0,
                tokens_out: 0,
                cost_usd: 0.0,
                summary: "ok".to_string(),
                error: None,
                proofed: spec.proof.then_some(true),
                agent_session_id: None,
                ghost: None,
            }
        }
    }

    #[test]
    fn task_is_running_not_done_while_proof_executes_on_partial_rerun() {
        // Regression for RAL-64: when a task's cells are all already-Done from
        // a prior run (only a different task was restarted), the finalizer must
        // set that task to Running before re-executing its task-level proofs.
        // Without the fix the board shows task=Done + proof=Running.
        let (store, id) = store_with(TWO_TASKS_B_HAS_PROMPT_PROOF);

        // First run: both tasks complete successfully.
        execute_squad(&store, Arc::new(FakeRunner { fail_on: None }).as_ref(), &id);
        {
            let g = store.lock().unwrap();
            assert_eq!(g.squad_state(&id).unwrap(), SquadState::Done);
            let squad = g.get_squad(&id).unwrap();
            assert_eq!(squad.tasks[1].state, "done");
            assert_eq!(squad.tasks[1].proof[0].state, "done");
        }

        // Restart task "a" only (task_idx=0, cell_idx=0).
        // Task "b" (task_idx=1) is intentionally NOT reset: its cell stays Done
        // and its task state stays "done" — the RAL-64 scenario.
        store.lock().unwrap().restart_cell(&id, 0, 0).unwrap();
        {
            let g = store.lock().unwrap();
            let squad = g.get_squad(&id).unwrap();
            assert_eq!(squad.tasks[0].state, "pending", "task 'a' should be reset");
            assert_eq!(squad.tasks[1].state, "done", "task 'b' should be unchanged");
        }

        // Re-run: task "b"'s cell is already Done so only its task-level proof
        // fires.  The runner snapshots task "b"'s DB state when that proof runs.
        let captured: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        execute_squad(
            &store,
            Arc::new(ProofStateCapturingRunner {
                store: Arc::clone(&store),
                captured: Arc::clone(&captured),
            })
            .as_ref(),
            &id,
        );

        let task_states = captured.lock().unwrap().clone();
        assert_eq!(
            task_states.len(),
            1,
            "task 'b' proof should run exactly once on re-run; got: {task_states:?}"
        );
        assert_eq!(
            task_states[0], "running",
            "task 'b' must be Running (not Done) while its proof executes (RAL-64)"
        );

        // Squad ends Done.
        assert_eq!(
            store.lock().unwrap().squad_state(&id).unwrap(),
            SquadState::Done
        );
        let squad = store.lock().unwrap().get_squad(&id).unwrap();
        assert_eq!(squad.tasks[1].state, "done");
    }

    // Two independent tasks; task "a" has a cell-level proof that fails.
    const TWO_TASKS_A_HAS_FAILING_CELL_PROOF: &str = "\
        [[task]]\nname=\"a\"\n[[task.cell]]\ncwd=\".\"\ncommand=\"cmd-a\"\n\
        [[task.cell.proof]]\ncommand=\"exit 1\"\n\
        [[task]]\nname=\"b\"\n[[task.cell]]\ncwd=\".\"\ncommand=\"cmd-b\"\n";

    #[test]
    fn skipped_cell_with_failed_proof_still_fails_task_on_partial_rerun() {
        // Regression for the bug where `progress.failed` started empty on a
        // partial restart, so a cell whose cell-level proof had
        // previously failed but was not restarted (still in `done_cells`)
        // would let its task be marked Done by the finalizer.
        let (store, id) = store_with(TWO_TASKS_A_HAS_FAILING_CELL_PROOF);

        // First run: cell "a" succeeds but its cell-level proof fails.
        // Task "a" → Failed, task "b" → Done, squad → Failed.
        execute_squad(&store, Arc::new(FakeRunner { fail_on: None }).as_ref(), &id);
        {
            let g = store.lock().unwrap();
            assert_eq!(g.squad_state(&id).unwrap(), SquadState::Failed);
            let squad = g.get_squad(&id).unwrap();
            assert_eq!(squad.tasks[0].state, "failed", "task 'a' should be failed");
            assert_eq!(squad.tasks[1].state, "done", "task 'b' should be done");
        }

        // Restart only task "b"'s cell (task_idx=1, cell_idx=0).
        // Task "a"'s cell stays Done and its failing proof stays Failed.
        store.lock().unwrap().restart_cell(&id, 1, 0).unwrap();

        // Re-run: task "a"'s cell is in `already_done` (skipped), but its
        // prior proof failure must seed `progress.failed` so the task finalizer
        // marks it Failed rather than Done.
        execute_squad(&store, Arc::new(FakeRunner { fail_on: None }).as_ref(), &id);

        let guard = store.lock().unwrap();
        assert_eq!(
            guard.squad_state(&id).unwrap(),
            SquadState::Failed,
            "squad must be Failed because task 'a' proof still failed"
        );
        let squad = guard.get_squad(&id).unwrap();
        assert_eq!(
            squad.tasks[0].state, "failed",
            "task 'a' must remain Failed — its cell-level proof was not re-run"
        );
        assert_eq!(
            squad.tasks[1].state, "done",
            "task 'b' re-ran and succeeded"
        );
    }

    /// Like `FakeRunner`, but also records every invoked command/prompt text
    /// (across possibly multiple `execute_squad` calls sharing one instance) —
    /// lets a test assert exactly how many times a given cell actually ran,
    /// not just its final DB state.
    struct CountingRunner {
        fail_on: Option<String>,
        invoked: Arc<Mutex<Vec<String>>>,
    }

    impl Runner for CountingRunner {
        fn run(&self, spec: &RunnerSpec) -> RunnerResult {
            let text = spec
                .command
                .clone()
                .or_else(|| spec.prompt.clone())
                .unwrap_or_default();
            self.invoked.lock().unwrap().push(text.clone());
            if self.fail_on.as_deref() == Some(text.as_str()) {
                RunnerResult::failure("intentional failure")
            } else {
                RunnerResult {
                    status: "done".to_string(),
                    tokens_in: 1,
                    tokens_out: 2,
                    cost_usd: 0.5,
                    summary: "ok".to_string(),
                    error: None,
                    proofed: spec.proof.then_some(true),
                    agent_session_id: None,
                    ghost: None,
                }
            }
        }
    }

    #[test]
    fn restarting_one_cell_does_not_redispatch_an_unrelated_already_failed_sibling() {
        // Regression (board.html "Restart Cell" bug report): `restart_cell`
        // correctly scopes its DB writes to the target + its true downstream via
        // `compute_cell_restart_impact` (matching the dry-squad preview), but it
        // also unconditionally flips the *whole squad* back to Pending so the
        // scheduler reactivates it at all. `execute_squad_inner`'s Progress-seed
        // rebuild used to treat any cell whose DB state wasn't
        // done/ignored/proof-only as Pending -- including one that was already
        // `failed` for a completely unrelated reason -- so restarting ONE
        // cell silently redispatched every other still-failed independent
        // task in the squad too.
        let (store, id) = store_with(TWO_INDEPENDENT_TASKS);
        let invoked: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let runner = CountingRunner {
            fail_on: Some("x".to_string()),
            invoked: Arc::clone(&invoked),
        };

        // First run: task "a" (command "x") fails; independent task "b"
        // (command "y") succeeds.
        execute_squad(&store, &runner, &id);
        {
            let g = store.lock().unwrap();
            assert_eq!(g.squad_state(&id).unwrap(), SquadState::Failed);
            let squad = g.get_squad(&id).unwrap();
            assert_eq!(squad.tasks[0].state, "failed", "task 'a' should be failed");
            assert_eq!(squad.tasks[1].state, "done", "task 'b' should be done");
        }

        // Restart only task "b"'s cell (task_idx=1, cell_idx=0) --
        // completely unrelated to task "a" (no dependency either way).
        store.lock().unwrap().restart_cell(&id, 1, 0).unwrap();

        // Re-run: task "b" re-executes (it was explicitly restarted), but task
        // "a"'s already-failed cell must NOT be redispatched just because
        // the squad itself went back to Running.
        execute_squad(&store, &runner, &id);

        let invocations = invoked.lock().unwrap().clone();
        assert_eq!(
            invocations.iter().filter(|t| t.as_str() == "x").count(),
            1,
            "task 'a's already-failed cell must not be redispatched by an \
             unrelated restart; invocations: {invocations:?}"
        );
        assert_eq!(
            invocations.iter().filter(|t| t.as_str() == "y").count(),
            2,
            "task 'b's cell should run once per execute_squad call \
             (it was explicitly restarted the second time): {invocations:?}"
        );

        let guard = store.lock().unwrap();
        assert_eq!(
            guard.squad_state(&id).unwrap(),
            SquadState::Failed,
            "task 'a' is still failed, so the squad stays failed overall"
        );
        let squad = guard.get_squad(&id).unwrap();
        assert_eq!(
            squad.tasks[0].state, "failed",
            "task 'a' must remain failed, untouched by the unrelated restart"
        );
        assert_eq!(
            squad.tasks[1].state, "done",
            "task 'b' re-ran and succeeded"
        );
    }

    /// Records every invocation like [`CountingRunner`], but parks on one
    /// specific command until the squad's cancel token trips — so a test can
    /// cancel a squad while exactly that cell is genuinely `running`, which is
    /// what leaves it in DB state `cancelled` (RAL-185's live repro).
    struct BlockingCountingRunner {
        block_on: String,
        invoked: Arc<Mutex<Vec<String>>>,
        blocking: Arc<std::sync::atomic::AtomicBool>,
    }

    impl Runner for BlockingCountingRunner {
        fn run(&self, spec: &RunnerSpec) -> RunnerResult {
            self.run_cancellable(spec, &CancelToken::never())
        }
        fn run_cancellable(&self, spec: &RunnerSpec, cancel: &CancelToken) -> RunnerResult {
            let text = spec
                .command
                .clone()
                .or_else(|| spec.prompt.clone())
                .unwrap_or_default();
            self.invoked.lock().unwrap().push(text.clone());
            if text == self.block_on {
                self.blocking
                    .store(true, std::sync::atomic::Ordering::SeqCst);
                for _ in 0..400 {
                    if cancel.is_cancelled() {
                        return RunnerResult::failure("cancelled");
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                return RunnerResult::failure("blocking runner was never cancelled");
            }
            RunnerResult {
                status: "done".to_string(),
                tokens_in: 1,
                tokens_out: 2,
                cost_usd: 0.5,
                summary: "ok".to_string(),
                error: None,
                proofed: spec.proof.then_some(true),
                agent_session_id: None,
                ghost: None,
            }
        }
    }

    /// Block until `f` observes what it wants, or panic after ~4s rather than
    /// hanging the whole test binary.
    fn wait_until(what: &str, mut f: impl FnMut() -> bool) {
        for _ in 0..2000 {
            if f() {
                return;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        panic!("timed out waiting for {what}");
    }

    #[test]
    fn restarting_one_cell_does_not_redispatch_an_unrelated_cancelled_sibling() {
        // RAL-185, the exact live repro from squad-000000000151: a squad-level
        // cancel left one task's cell `cancelled` (it happened to be
        // genuinely `running` at cancel time); restarting a *dependency-
        // unrelated* cell in the other task flipped the whole squad back to
        // Pending, and `execute_squad_inner`'s Progress seed — which recognised
        // only done/proof-only/failed/ignored — silently coerced that
        // `cancelled` sibling to Pending and redispatched it.
        use std::sync::atomic::{AtomicBool, Ordering};
        let (store, id) = store_with(TWO_INDEPENDENT_TASKS);
        let invoked: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let blocking = Arc::new(AtomicBool::new(false));
        let token = CancelToken::new();

        let runner: Arc<dyn Runner> = Arc::new(BlockingCountingRunner {
            block_on: "x".to_string(),
            invoked: Arc::clone(&invoked),
            blocking: Arc::clone(&blocking),
        });
        let worker = {
            let (store, runner, token, id) = (
                Arc::clone(&store),
                Arc::clone(&runner),
                token.clone(),
                id.clone(),
            );
            std::thread::spawn(move || {
                // RAL-213: a fresh, private registry -- these tests only exercise
                // squad-level cancellation via `token`, not a guardian-merge restart.
                execute_squad_with(&store, runner.as_ref(), &id, &token, &Cancellations::new())
            })
        };

        // Wait until task "a"'s cell is genuinely in flight AND independent
        // task "b" has already finished, so the cancel below hits exactly one
        // running cell — mirroring the reported squad.
        wait_until("task 'a' running and task 'b' done", || {
            let b_done = {
                let g = store.lock().unwrap();
                g.get_squad(&id).unwrap().tasks[1].cells[0].state == "done"
            };
            b_done && blocking.load(Ordering::SeqCst)
        });

        store.lock().unwrap().cancel(&id).unwrap();
        token.cancel();
        worker.join().unwrap();
        {
            let g = store.lock().unwrap();
            let squad = g.get_squad(&id).unwrap();
            assert_eq!(squad.tasks[0].cells[0].state, "cancelled");
            assert_eq!(squad.tasks[1].cells[0].state, "done");
        }

        // Restart ONLY task "b"'s cell — no dependency relationship to task
        // "a" in either direction.
        store.lock().unwrap().restart_cell(&id, 1, 0).unwrap();

        // Re-run with a non-blocking runner sharing the same tally, so a
        // regression shows up as an extra recorded "x" instead of a 2s stall.
        let rerun = CountingRunner {
            fail_on: None,
            invoked: Arc::clone(&invoked),
        };
        execute_squad(&store, &rerun, &id);

        let invocations = invoked.lock().unwrap().clone();
        assert_eq!(
            invocations.iter().filter(|t| t.as_str() == "x").count(),
            1,
            "task 'a's cancelled cell must NOT be redispatched by an \
             unrelated restart; invocations: {invocations:?}"
        );
        assert_eq!(
            invocations.iter().filter(|t| t.as_str() == "y").count(),
            2,
            "task 'b' ran once per execute_squad call (it was the restart \
             target the second time): {invocations:?}"
        );

        let guard = store.lock().unwrap();
        let squad = guard.get_squad(&id).unwrap();
        assert_eq!(
            squad.tasks[0].cells[0].state, "cancelled",
            "the cancelled sibling stays cancelled — terminal, not revived"
        );
        assert_eq!(
            squad.tasks[0].state, "cancelled",
            "and its task must not be finalized (to Done via its proofs, or \
             to Failed — the user cancelled it, it did not fail)"
        );
        assert_eq!(
            squad.tasks[1].state, "done",
            "task 'b' re-ran and succeeded"
        );
        assert_eq!(
            guard.squad_state(&id).unwrap(),
            SquadState::Cancelled,
            "a squad still holding a cancelled task did not complete, so it must \
             not report Done"
        );
    }

    #[test]
    fn a_dependent_of_a_cancelled_cell_is_resolved_cancelled_not_left_spinning() {
        // RAL-185 cascade: seeding a cell terminal-but-Cancelled is only
        // safe if its dependents are resolved too. The dispatcher marks any
        // cell whose prerequisites are unsatisfied as `active` and sleeps,
        // so a Pending cell waiting on a Cancelled one — which can never
        // reach Done — would spin the loop forever. This mirrors the existing
        // "blocked by a failed dependency" handling, but records `cancelled`
        // rather than `failed`: nothing here actually failed.
        use std::sync::atomic::{AtomicBool, Ordering};
        // "b" depends on "a", inside one task (ral-171's work/finalize shape).
        let toml = "[[task]]\nname=\"t\"\n\
            [[task.cell]]\nid=\"a\"\ncwd=\".\"\ncommand=\"x\"\n\
            [[task.cell]]\nid=\"b\"\ncwd=\".\"\ncommand=\"y\"\ndepends_on=[\"a\"]\n";
        let (store, id) = store_with(toml);
        let invoked: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let blocking = Arc::new(AtomicBool::new(false));
        let token = CancelToken::new();

        let runner: Arc<dyn Runner> = Arc::new(BlockingCountingRunner {
            block_on: "x".to_string(),
            invoked: Arc::clone(&invoked),
            blocking: Arc::clone(&blocking),
        });
        let worker = {
            let (store, runner, token, id) = (
                Arc::clone(&store),
                Arc::clone(&runner),
                token.clone(),
                id.clone(),
            );
            std::thread::spawn(move || {
                // RAL-213: a fresh, private registry -- these tests only exercise
                // squad-level cancellation via `token`, not a guardian-merge restart.
                execute_squad_with(&store, runner.as_ref(), &id, &token, &Cancellations::new())
            })
        };
        wait_until("cell 'a' running", || blocking.load(Ordering::SeqCst));
        store.lock().unwrap().cancel(&id).unwrap();
        token.cancel();
        worker.join().unwrap();

        // Restart only the *downstream* cell "b". Its upstream "a" is not in
        // the impact set (the BFS runs forward), so "a" stays cancelled while
        // "b" goes back to Pending — the one shape that produces a Pending
        // cell with a Cancelled prerequisite.
        store.lock().unwrap().restart_cell(&id, 0, 1).unwrap();
        {
            let g = store.lock().unwrap();
            let squad = g.get_squad(&id).unwrap();
            assert_eq!(squad.tasks[0].cells[0].state, "cancelled");
            assert_eq!(squad.tasks[0].cells[1].state, "pending");
        }

        let rerun = CountingRunner {
            fail_on: None,
            invoked: Arc::clone(&invoked),
        };
        // The real assertion is that this returns at all — a spinning
        // dispatcher would hang here forever.
        execute_squad(&store, &rerun, &id);

        let invocations = invoked.lock().unwrap().clone();
        assert_eq!(
            invocations.iter().filter(|t| t.as_str() == "y").count(),
            0,
            "'b' must not run while its prerequisite is cancelled: {invocations:?}"
        );
        let guard = store.lock().unwrap();
        let squad = guard.get_squad(&id).unwrap();
        assert_eq!(squad.tasks[0].cells[1].state, "cancelled");
        assert_eq!(
            squad.tasks[0].cells[1].error.as_deref(),
            Some("blocked by a cancelled dependency"),
        );
    }

    #[test]
    fn a_full_restart_squad_still_revives_previously_cancelled_cells() {
        // RAL-185 AC: the new terminal seeds must not shadow `restart_squad`.
        // `reset_squad_to_pending` rewrites every cell/task row back to
        // `pending` before the scheduler reads its seeds, so nothing is left in
        // `cancelled` state and both tasks run again from scratch.
        use std::sync::atomic::{AtomicBool, Ordering};
        let (store, id) = store_with(TWO_INDEPENDENT_TASKS);
        let invoked: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let blocking = Arc::new(AtomicBool::new(false));
        let token = CancelToken::new();

        let runner: Arc<dyn Runner> = Arc::new(BlockingCountingRunner {
            block_on: "x".to_string(),
            invoked: Arc::clone(&invoked),
            blocking: Arc::clone(&blocking),
        });
        let worker = {
            let (store, runner, token, id) = (
                Arc::clone(&store),
                Arc::clone(&runner),
                token.clone(),
                id.clone(),
            );
            std::thread::spawn(move || {
                // RAL-213: a fresh, private registry -- these tests only exercise
                // squad-level cancellation via `token`, not a guardian-merge restart.
                execute_squad_with(&store, runner.as_ref(), &id, &token, &Cancellations::new())
            })
        };
        wait_until("task 'a' running", || blocking.load(Ordering::SeqCst));
        store.lock().unwrap().cancel(&id).unwrap();
        token.cancel();
        worker.join().unwrap();
        assert_eq!(
            store.lock().unwrap().get_squad(&id).unwrap().tasks[0].cells[0].state,
            "cancelled"
        );

        store.lock().unwrap().restart_squad(&id).unwrap();
        let rerun = CountingRunner {
            fail_on: None,
            invoked: Arc::clone(&invoked),
        };
        execute_squad(&store, &rerun, &id);

        let invocations = invoked.lock().unwrap().clone();
        assert!(
            invocations.iter().filter(|t| t.as_str() == "x").count() >= 2,
            "a whole-squad restart must re-run the cancelled cell: {invocations:?}"
        );
        let guard = store.lock().unwrap();
        assert_eq!(guard.squad_state(&id).unwrap(), SquadState::Done);
        let squad = guard.get_squad(&id).unwrap();
        assert_eq!(squad.tasks[0].state, "done");
        assert_eq!(squad.tasks[1].state, "done");
    }

    // ── per-task review readiness ─────────────────────────────────────────

    // `guardian_blocking_tasks` calls `project_root_of` which runs git, so it
    // cannot be unit-tested against fake (non-git) paths. The functional
    // coverage is provided by the integration tests in
    // `daemon/tests/reviews_derive.rs`:
    //   - `non_overlapping_task_does_not_block_readiness`
    //   - `undeclared_overlapping_task_blocks_readiness`

    fn make_cell_row_no_cwd(task_idx: i64) -> crate::store::CellRow {
        crate::store::CellRow {
            task_idx,
            idx: 0,
            task_name: format!("task{task_idx}"),
            cell_id: format!("s{task_idx}"),
            cwd: None,
            subprojects: vec![],
            prompt: None,
            command: Some("x".to_string()),
            agent: "claude".to_string(),
            model: None,
            system_prompt: None,
            system_prompt_position: None,
            depends_on: vec![],
            timeout_sec: None,
            budget_tokens: None,
            maximum_budget_usd: None,
            upstream: None,
            machine: None,
        }
    }

    #[test]
    fn guardian_blocking_tasks_cell_with_no_cwd_skipped() {
        // A cell without a cwd cannot be in any git project, so it must
        // never count as a blocker regardless of the guardian's git_root.
        let row = make_cell_row_no_cwd(0);
        let blocking = guardian_blocking_tasks(&[row], "/any/project");
        assert!(blocking.is_empty(), "cell with no cwd should not block");
    }

    // Sentinel-string parsing itself is covered by
    // `ralphus_core::schema::parse_upstream_task_ref`'s own unit tests.

    #[test]
    fn try_upstream_rebase_noop_when_upstream_is_none() {
        let row = make_cell_row_no_cwd(0);
        // upstream is None — must return None without calling git.
        let result = try_upstream_rebase(&row, &[]);
        assert!(result.is_none(), "no upstream → no rebase attempt");
    }

    #[test]
    fn try_upstream_rebase_noop_when_dep_not_found() {
        let mut row = make_cell_row_no_cwd(0);
        row.upstream = Some("<<task:nonexistent-task>>".to_string());
        // The referenced task doesn't exist in the cell list; returns None
        // (can't rebase something that doesn't exist, graceful skip).
        let result = try_upstream_rebase(&row, &[]);
        assert!(result.is_none(), "missing dep → skip without error");
    }

    #[test]
    fn try_upstream_rebase_skip_when_b_has_no_cwd() {
        let mut row = make_cell_row_no_cwd(1);
        row.upstream = Some("<<task:task0>>".to_string());
        let dep = make_cell_row_no_cwd(0); // cwd is None
        // Neither has a cwd; parse_upstream_task_ref returns Some but the cwd
        // check returns None early — treated as skip (None return).
        let result = try_upstream_rebase(&row, &[dep]);
        assert!(result.is_none(), "no cwd on B → silent skip");
    }

    /// Regression test for the production failure:
    ///   "failed to rebase cell 'work' onto upstream branch
    ///   'RAL-54-reset_downstream_branch_statuses': git rebase ... failed:
    ///   Rebasing (1/1) error: could not apply ... Could not apply ..."
    ///
    /// Root cause: when the upstream rebase has a conflict `try_upstream_rebase`
    /// returned `Some(error)`, permanently failing the cell before the runner
    /// ever executed.  A rebase conflict is ephemeral — `rebase_onto` already
    /// aborted and left the worktree clean — so it must not be treated as a
    /// fatal error.  The fix: log a warning and return `None` so the cell
    /// runs on its current branch.
    #[test]
    fn try_upstream_rebase_warns_and_continues_on_rebase_conflict() {
        use std::process::Command as Cmd;
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("ralphus-up-conflict-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("mkdir temp root");

        let git = |args: &[&str]| {
            let out = Cmd::new("git")
                .args(args)
                .current_dir(&root)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t.com")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t.com")
                .env("GIT_EDITOR", "true")
                .output()
                .expect("run git");
            assert!(
                out.status.success(),
                "git {:?}: {}",
                args,
                String::from_utf8_lossy(&out.stderr)
            );
        };

        // Two branches that both modify the same file — guaranteed conflict
        // when "work" is rebased onto "dep".
        git(&["init", "-b", "main"]);
        std::fs::write(root.join("shared.txt"), "base\n").expect("write");
        git(&["add", "shared.txt"]);
        git(&["commit", "-m", "base"]);
        git(&["checkout", "-b", "dep"]);
        std::fs::write(root.join("shared.txt"), "from dep\n").expect("write dep");
        git(&["commit", "-am", "dep changes"]);
        git(&["checkout", "main"]);
        git(&["checkout", "-b", "work"]);
        std::fs::write(root.join("shared.txt"), "from work\n").expect("write work");
        git(&["commit", "-am", "work changes"]);
        git(&["checkout", "main"]);

        // One worktree per branch, mirroring what the scheduler sets up.
        let dep_wt = root.join("wt-dep");
        let work_wt = root.join("wt-work");
        git(&["worktree", "add", dep_wt.to_str().unwrap(), "dep"]);
        git(&["worktree", "add", work_wt.to_str().unwrap(), "work"]);

        let dep_row = crate::store::CellRow {
            task_idx: 0,
            idx: 0,
            task_name: "dep-task".into(),
            cell_id: "work".into(),
            cwd: Some(dep_wt.to_str().unwrap().into()),
            subprojects: vec![],
            prompt: None,
            command: Some("x".into()),
            agent: "ollama".into(),
            model: None,
            system_prompt: None,
            system_prompt_position: None,
            depends_on: vec![],
            timeout_sec: None,
            budget_tokens: None,
            maximum_budget_usd: None,
            upstream: None,
            machine: None,
        };
        let work_row = crate::store::CellRow {
            task_idx: 1,
            idx: 0,
            task_name: "work-task".into(),
            cell_id: "work".into(),
            cwd: Some(work_wt.to_str().unwrap().into()),
            upstream: Some("<<task:dep-task>>".into()),
            machine: None,
            ..dep_row.clone()
        };
        let cells = [dep_row, work_row];

        // Before the fix this returned Some(error) and permanently failed the
        // cell.  After the fix it must return None so the cell can run.
        let result = try_upstream_rebase(&cells[1], &cells);
        assert!(
            result.is_none(),
            "rebase conflict must not permanently fail the cell; got: {result:?}"
        );

        // The work worktree must be left in a clean state by the failed-rebase
        // abort, ready for the cell runner to use.
        let status_out = Cmd::new("git")
            .args(["status", "--porcelain"])
            .current_dir(&work_wt)
            .output()
            .expect("git status");
        assert!(
            status_out.stdout.is_empty(),
            "work worktree must be clean after rebase conflict abort"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    // ── proof-only restart (restart_cell_proof / restart_task_proof) ──────

    // A cell with a PROMPT proof (not command), so the runner is invoked
    // for both the cell body and the proof — allowing CommandRecorder to
    // distinguish them by checking spec.command vs spec.prompt.
    const CELL_WITH_PROMPT_PROOF: &str = "[[task]]\nname=\"t\"\n[[task.cell]]\ncwd=\".\"\ncommand=\"do-work\"\n\
         agent=\"ollama\"\nmodel=\"test-model\"\n\
         [[task.cell.proof]]\nprompt=\"check-output\"\n";

    /// Records every command/prompt string the runner sees. Because command
    /// proofs bypass the runner (they use run_command_proof_capture), this
    /// only captures cell bodies and prompt proofs.
    struct CommandRecorder {
        calls: Arc<Mutex<Vec<String>>>,
    }

    impl Runner for CommandRecorder {
        fn run(&self, spec: &RunnerSpec) -> RunnerResult {
            let label = spec
                .command
                .clone()
                .or_else(|| spec.prompt.clone())
                .unwrap_or_default();
            self.calls.lock().unwrap().push(label);
            RunnerResult {
                status: "done".to_string(),
                tokens_in: 0,
                tokens_out: 0,
                cost_usd: 0.0,
                summary: "ok".to_string(),
                error: None,
                proofed: spec.proof.then_some(true),
                agent_session_id: None,
                ghost: None,
            }
        }
    }

    #[test]
    fn restart_cell_proof_does_not_re_run_cell_body() {
        let (store, id) = store_with(CELL_WITH_PROMPT_PROOF);

        // First run: cell body ("do-work") and prompt proof ("check-output") both succeed.
        let calls: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        execute_squad(
            &store,
            Arc::new(CommandRecorder {
                calls: Arc::clone(&calls),
            })
            .as_ref(),
            &id,
        );
        assert_eq!(
            store.lock().unwrap().squad_state(&id).unwrap(),
            SquadState::Done
        );
        assert!(
            calls.lock().unwrap().contains(&"do-work".to_string()),
            "cell body must run on first pass"
        );
        assert!(
            calls.lock().unwrap().contains(&"check-output".to_string()),
            "prompt proof must run on first pass"
        );

        // Restart only the cell-level proof.
        store
            .lock()
            .unwrap()
            .restart_cell_proof(&id, 0, 0, 0)
            .unwrap();

        // Re-run: cell body must NOT be called again; only the prompt proof re-runs.
        calls.lock().unwrap().clear();
        execute_squad(
            &store,
            Arc::new(CommandRecorder {
                calls: Arc::clone(&calls),
            })
            .as_ref(),
            &id,
        );

        let recorded = calls.lock().unwrap().clone();
        assert!(
            !recorded.contains(&"do-work".to_string()),
            "cell body must NOT re-run after restart_cell_proof; got: {recorded:?}"
        );
        assert!(
            recorded.contains(&"check-output".to_string()),
            "prompt proof must re-run; got: {recorded:?}"
        );
        assert_eq!(
            store.lock().unwrap().squad_state(&id).unwrap(),
            SquadState::Done
        );
    }

    #[test]
    fn restart_task_proof_does_not_re_run_cell_body() {
        // TASK_PROMPT_PROOF has cell command "do" + task prompt proof "check it".
        // First run: cell succeeds, task proof also succeeds (FakeRunner passes all).
        // We'll use a custom TOML where the cell body can be tracked.
        const TASK_PROMPT_PROOF_WITH_CMD_CELL: &str = "[[task]]\nname=\"t\"\n[[task.cell]]\ncwd=\".\"\ncommand=\"do-work\"\n\
             agent=\"ollama\"\nmodel=\"test-model\"\n\
             [[task.proof]]\nprompt=\"task-check\"\n";

        let (store, id) = store_with(TASK_PROMPT_PROOF_WITH_CMD_CELL);

        // First run: all succeed.
        execute_squad(&store, Arc::new(FakeRunner { fail_on: None }).as_ref(), &id);
        assert_eq!(
            store.lock().unwrap().squad_state(&id).unwrap(),
            SquadState::Done
        );

        // Restart only the task-level proof.
        store.lock().unwrap().restart_task_proof(&id, 0, 0).unwrap();

        // Re-run: cell body "do-work" must NOT be called; only "task-check" runs.
        let calls: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        execute_squad(
            &store,
            Arc::new(CommandRecorder {
                calls: Arc::clone(&calls),
            })
            .as_ref(),
            &id,
        );

        let recorded = calls.lock().unwrap().clone();
        assert!(
            !recorded.contains(&"do-work".to_string()),
            "cell body must NOT re-run after restart_task_proof; got: {recorded:?}"
        );
        assert!(
            recorded.contains(&"task-check".to_string()),
            "task prompt proof must re-run; got: {recorded:?}"
        );
        assert_eq!(
            store.lock().unwrap().squad_state(&id).unwrap(),
            SquadState::Done
        );
    }

    // ── Worktree placeholder cwd (RAL-100) ───────────────────────────────────

    fn wt_test_repo(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ral100-scheduler-{tag}-{}-{}",
            std::process::id(),
            TEST_DIR_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
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

    static TEST_DIR_SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

    #[test]
    fn execute_squad_resolves_worktree_placeholder_cwd() {
        let repo = wt_test_repo("resolve");
        // no_commit_required: this test is about worktree materialization, not
        // the RAL-156 commit guard, and FakeRunner never actually commits.
        let toml = "[[task]]\nname=\"t\"\nproject=\"proj\"\nno_commit_required=true\n[[task.cell]]\ncwd=\"ralphus:new-worktree/feat-a?upstream=main\"\ncommand=\"do-thing\"\n";
        let mut store = Store::open_in_memory().unwrap();
        store
            .register_project("proj", "", &repo.to_string_lossy(), "git")
            .unwrap();
        let file = toml::from_str(toml).unwrap();
        let id = store.insert_squad(&file, None, false).unwrap();
        let store = Arc::new(Mutex::new(store));

        execute_squad(&store, &FakeRunner { fail_on: None }, &id);

        assert_eq!(
            store.lock().unwrap().squad_state(&id).unwrap(),
            SquadState::Done
        );
        let cells = store.lock().unwrap().cells_of(&id).unwrap();
        let cwd = cells[0].cwd.clone().unwrap();
        assert_ne!(
            cwd, "ralphus:new-worktree/feat-a?upstream=main",
            "placeholder must be rewritten"
        );
        let expected = crate::worktrees::worktree_dir(&repo, "feat-a");
        assert_eq!(Path::new(&cwd), expected);
        assert!(expected.join(".git").exists());
    }

    #[test]
    fn execute_squad_dedups_placeholder_across_two_cells() {
        let repo = wt_test_repo("dedupe");
        // no_commit_required: this test is about worktree deduplication, not
        // the RAL-156 commit guard, and FakeRunner never actually commits.
        let toml = "[[task]]\nname=\"t\"\nproject=\"proj\"\nno_commit_required=true\n\
             [[task.cell]]\nid=\"a\"\ncwd=\"ralphus:new-worktree/shared?upstream=main\"\ncommand=\"do-a\"\n\
             [[task.cell]]\nid=\"b\"\ncwd=\"ralphus:new-worktree/shared?upstream=main\"\ncommand=\"do-b\"\n";
        let mut store = Store::open_in_memory().unwrap();
        store
            .register_project("proj", "", &repo.to_string_lossy(), "git")
            .unwrap();
        let file = toml::from_str(toml).unwrap();
        let id = store.insert_squad(&file, None, false).unwrap();
        let store = Arc::new(Mutex::new(store));

        execute_squad(&store, &FakeRunner { fail_on: None }, &id);

        assert_eq!(
            store.lock().unwrap().squad_state(&id).unwrap(),
            SquadState::Done
        );
        let cells = store.lock().unwrap().cells_of(&id).unwrap();
        let cwds: Vec<String> = cells.iter().map(|s| s.cwd.clone().unwrap()).collect();
        assert_eq!(cwds[0], cwds[1], "both cells must resolve to one worktree");
        // Exactly one worktree registered in git for the shared branch.
        let list = crate::guardian_merge::git(&repo, &["worktree", "list", "--porcelain"]).unwrap();
        assert_eq!(
            list.matches("worktree ").count(),
            2, // the main repo itself + the one shared worktree
            "expected exactly one materialized worktree besides the main repo: {list}"
        );
    }

    #[test]
    fn execute_squad_restart_reuses_already_materialized_worktree() {
        let repo = wt_test_repo("restart");
        // no_commit_required: this test is about worktree reuse across a
        // restart, not the RAL-156 commit guard, and FakeRunner never
        // actually commits.
        let toml = "[[task]]\nname=\"t\"\nproject=\"proj\"\nno_commit_required=true\n[[task.cell]]\ncwd=\"ralphus:new-worktree/feat-b?upstream=main\"\ncommand=\"do-thing\"\n";
        let mut store = Store::open_in_memory().unwrap();
        store
            .register_project("proj", "", &repo.to_string_lossy(), "git")
            .unwrap();
        let file = toml::from_str(toml).unwrap();
        let id = store.insert_squad(&file, None, false).unwrap();
        let store = Arc::new(Mutex::new(store));

        execute_squad(&store, &FakeRunner { fail_on: None }, &id);
        let first_cwd = store.lock().unwrap().cells_of(&id).unwrap()[0]
            .cwd
            .clone()
            .unwrap();

        // Simulate a restart: reset the squad back to Pending/cell to pending and
        // re-execute. The already-resolved (real, non-placeholder) cwd must be
        // reused as-is, and re-materializing it must not error or recreate it.
        store.lock().unwrap().restart_squad(&id).unwrap();
        execute_squad(&store, &FakeRunner { fail_on: None }, &id);

        assert_eq!(
            store.lock().unwrap().squad_state(&id).unwrap(),
            SquadState::Done
        );
        let second_cwd = store.lock().unwrap().cells_of(&id).unwrap()[0]
            .cwd
            .clone()
            .unwrap();
        assert_eq!(
            first_cwd, second_cwd,
            "restart must reuse the same worktree path"
        );
        let list = crate::guardian_merge::git(&repo, &["worktree", "list", "--porcelain"]).unwrap();
        assert_eq!(
            list.matches("worktree ").count(),
            2,
            "restart must not duplicate the worktree: {list}"
        );
    }

    struct EnvCapturingRunner {
        seen: Arc<Mutex<Vec<BTreeMap<String, String>>>>,
    }

    impl Runner for EnvCapturingRunner {
        fn run(&self, spec: &RunnerSpec) -> RunnerResult {
            self.seen.lock().unwrap().push(spec.env_overrides.clone());
            RunnerResult {
                status: "done".to_string(),
                tokens_in: 0,
                tokens_out: 0,
                cost_usd: 0.0,
                summary: "ok".to_string(),
                error: None,
                proofed: spec.proof.then_some(true),
                agent_session_id: None,
                ghost: None,
            }
        }
    }

    #[test]
    fn execute_squad_materializes_shared_cwd_and_env_placeholder_once_and_preserves_unknown_text() {
        let repo = wt_test_repo("env-materialize");
        let toml = "[[task]]\nname=\"t\"\nproject=\"proj\"\nno_commit_required=true\n[[task.cell]]\ncwd=\"<<ralphus:new-worktree/feat-env?upstream=main>>\"\ncommand=\"do-thing\"\nenvironment={WT=\"<<ralphus:new-worktree/feat-env?upstream=main>>\", NESTED=\"<<ralphus:new-worktree/feat-env?upstream=main>>/nested\", RAW=\"prefix <<unknown>> suffix\"}\n";
        let mut store = Store::open_in_memory().unwrap();
        store
            .register_project("proj", "", &repo.to_string_lossy(), "git")
            .unwrap();
        let file = toml::from_str(toml).unwrap();
        let id = store.insert_squad(&file, None, false).unwrap();
        let store = Arc::new(Mutex::new(store));
        let seen = Arc::new(Mutex::new(Vec::new()));

        execute_squad(
            &store,
            Arc::new(EnvCapturingRunner {
                seen: Arc::clone(&seen),
            })
            .as_ref(),
            &id,
        );
        store.lock().unwrap().restart_squad(&id).unwrap();
        execute_squad(
            &store,
            Arc::new(EnvCapturingRunner {
                seen: Arc::clone(&seen),
            })
            .as_ref(),
            &id,
        );

        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert_eq!(
            seen[0], seen[1],
            "restart must reuse the first materialized env"
        );
        let wt = seen[0].get("WT").expect("materialized env");
        let persisted_cwd = store.lock().unwrap().get_squad(&id).unwrap().tasks[0].cells[0]
            .cwd
            .clone()
            .expect("persisted cwd");
        assert_eq!(wt, &persisted_cwd, "cwd and env must reuse one worktree");
        assert_eq!(
            Path::new(wt),
            crate::worktrees::worktree_dir(&repo, "feat-env")
        );
        assert_eq!(
            seen[0].get("NESTED").map(Path::new),
            Some(
                crate::worktrees::worktree_dir(&repo, "feat-env")
                    .join("nested")
                    .as_path()
            )
        );
        assert_eq!(
            seen[0].get("RAW").map(String::as_str),
            Some("prefix <<unknown>> suffix"),
            "unknown placeholders must remain byte-for-byte unchanged"
        );
        let snapshot = store
            .lock()
            .unwrap()
            .get_cell_materialized_env_overrides(&id, 0, 0)
            .unwrap()
            .expect("persisted snapshot");
        assert_eq!(snapshot.get("WT"), Some(wt));
        let list = crate::guardian_merge::git(&repo, &["worktree", "list", "--porcelain"]).unwrap();
        assert_eq!(
            list.matches("worktree ").count(),
            2,
            "shared cwd/env placeholder should materialize exactly one extra worktree: {list}"
        );
    }

    // ── RAL-156: no-new-commits-since-baseline guard ─────────────────────────

    /// A `Runner` that actually commits a file change in `spec.cwd`, standing
    /// in for an agent cell that really did the work (unlike `FakeRunner`,
    /// which never touches the filesystem).
    struct CommittingRunner;

    impl Runner for CommittingRunner {
        fn run(&self, spec: &RunnerSpec) -> RunnerResult {
            let git = |args: &[&str]| {
                let status = std::process::Command::new("git")
                    .args(args)
                    .current_dir(&spec.cwd)
                    .env("GIT_AUTHOR_NAME", "t")
                    .env("GIT_AUTHOR_EMAIL", "t@t")
                    .env("GIT_COMMITTER_NAME", "t")
                    .env("GIT_COMMITTER_EMAIL", "t@t")
                    .status()
                    .expect("git");
                assert!(status.success(), "git {args:?} failed in {}", spec.cwd);
            };
            std::fs::write(Path::new(&spec.cwd).join("new.txt"), "x\n").unwrap();
            git(&["add", "."]);
            git(&["commit", "-m", "cell work"]);
            RunnerResult {
                status: "done".to_string(),
                tokens_in: 0,
                tokens_out: 0,
                cost_usd: 0.0,
                summary: "ok".to_string(),
                error: None,
                proofed: spec.proof.then_some(true),
                agent_session_id: None,
                ghost: None,
            }
        }
    }

    #[test]
    fn git_backed_task_with_zero_commits_fails() {
        let repo = wt_test_repo("no-commits");
        let toml = format!(
            "[[task]]\nname=\"t\"\nproject=\"proj\"\n[[task.cell]]\ncwd=\"{}\"\ncommand=\"do-thing\"\n",
            repo.to_string_lossy().replace('\\', "/")
        );
        let mut store = Store::open_in_memory().unwrap();
        store
            .register_project("proj", "", &repo.to_string_lossy(), "git")
            .unwrap();
        let file = toml::from_str(&toml).unwrap();
        let id = store.insert_squad(&file, None, false).unwrap();
        let store = Arc::new(Mutex::new(store));

        // FakeRunner never touches the filesystem, so no commit is made.
        execute_squad(&store, &FakeRunner { fail_on: None }, &id);

        assert_eq!(
            store.lock().unwrap().squad_state(&id).unwrap(),
            SquadState::Failed,
            "a git-backed task with zero new commits must fail"
        );
    }

    #[test]
    fn git_backed_task_with_a_commit_passes() {
        let repo = wt_test_repo("with-commit");
        let toml = format!(
            "[[task]]\nname=\"t\"\nproject=\"proj\"\n[[task.cell]]\ncwd=\"{}\"\ncommand=\"do-thing\"\n",
            repo.to_string_lossy().replace('\\', "/")
        );
        let mut store = Store::open_in_memory().unwrap();
        store
            .register_project("proj", "", &repo.to_string_lossy(), "git")
            .unwrap();
        let file = toml::from_str(&toml).unwrap();
        let id = store.insert_squad(&file, None, false).unwrap();
        let store = Arc::new(Mutex::new(store));

        execute_squad(&store, &CommittingRunner, &id);

        assert_eq!(
            store.lock().unwrap().squad_state(&id).unwrap(),
            SquadState::Done,
            "a cell that actually committed must satisfy the guard"
        );
    }

    #[test]
    fn no_commit_required_opts_out_of_the_guard() {
        let repo = wt_test_repo("opt-out");
        let toml = format!(
            "[[task]]\nname=\"t\"\nproject=\"proj\"\nno_commit_required=true\n[[task.cell]]\ncwd=\"{}\"\ncommand=\"do-thing\"\n",
            repo.to_string_lossy().replace('\\', "/")
        );
        let mut store = Store::open_in_memory().unwrap();
        store
            .register_project("proj", "", &repo.to_string_lossy(), "git")
            .unwrap();
        let file = toml::from_str(&toml).unwrap();
        let id = store.insert_squad(&file, None, false).unwrap();
        let store = Arc::new(Mutex::new(store));

        execute_squad(&store, &FakeRunner { fail_on: None }, &id);

        assert_eq!(
            store.lock().unwrap().squad_state(&id).unwrap(),
            SquadState::Done,
            "no_commit_required must bypass the guard even with zero commits"
        );
    }

    #[test]
    fn non_git_backed_task_with_zero_commits_passes() {
        // No `project` field at all -- the task isn't git-backed (RAL-156 Q1),
        // so the guard must never run, regardless of the plain cwd's contents.
        let toml = "[[task]]\nname=\"t\"\n[[task.cell]]\ncwd=\".\"\ncommand=\"do-thing\"\n";
        let mut store = Store::open_in_memory().unwrap();
        let file = toml::from_str(toml).unwrap();
        let id = store.insert_squad(&file, None, false).unwrap();
        let store = Arc::new(Mutex::new(store));

        execute_squad(&store, &FakeRunner { fail_on: None }, &id);

        assert_eq!(
            store.lock().unwrap().squad_state(&id).unwrap(),
            SquadState::Done,
            "a non-git-backed task must never be subject to the guard"
        );
    }

    #[test]
    fn registered_but_non_git_project_is_not_subject_to_the_guard() {
        let dir = std::env::temp_dir().join(format!(
            "ral156-nonvcs-{}-{}",
            std::process::id(),
            TEST_DIR_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let toml = format!(
            "[[task]]\nname=\"t\"\nproject=\"proj\"\n[[task.cell]]\ncwd=\"{}\"\ncommand=\"do-thing\"\n",
            dir.to_string_lossy().replace('\\', "/")
        );
        let mut store = Store::open_in_memory().unwrap();
        // A project registered with a non-"git" vcs (only "git" is
        // implemented today, but the store doesn't enforce that at
        // registration time) must not be treated as git-backed.
        store
            .register_project("proj", "", &dir.to_string_lossy(), "none")
            .unwrap();
        let file = toml::from_str(&toml).unwrap();
        let id = store.insert_squad(&file, None, false).unwrap();
        let store = Arc::new(Mutex::new(store));

        execute_squad(&store, &FakeRunner { fail_on: None }, &id);

        assert_eq!(
            store.lock().unwrap().squad_state(&id).unwrap(),
            SquadState::Done
        );
    }

    // ── RAL-248 cross-cell session sharing ────────────────────────────────

    #[test]
    fn sharing_blocked_reason_guards_only_claude_code_model_mismatch() {
        // Matching models (both explicit, both None/default) → share.
        assert_eq!(
            sharing_blocked_reason("claude-code", Some("a"), "claude-code", Some("a")),
            None
        );
        assert_eq!(
            sharing_blocked_reason("claude-code", None, "claude-code", None),
            None
        );
        // Two claude-code cells on different models → block.
        assert!(
            sharing_blocked_reason("claude-code", Some("a"), "claude-code", Some("b")).is_some()
        );
        // One side not claude-code → never blocked, regardless of models.
        assert_eq!(
            sharing_blocked_reason("claude", Some("a"), "claude-code", Some("b")),
            None
        );
        assert_eq!(
            sharing_blocked_reason("claude-code", Some("a"), "codex", Some("b")),
            None
        );
        assert_eq!(
            sharing_blocked_reason("claude", None, "codex", Some("b")),
            None
        );
    }

    /// One `(cell_id, resume_seen)` record per cell launch.
    type SessionShareSeen = Vec<(String, Option<String>)>;

    /// RAL-248 test runner: records the `resume_agent_session_id` each cell
    /// saw at launch, and returns a fixed session id for the named `provider`
    /// cell so a dependent cell has something to resume.
    struct SessionShareRunner {
        provider: String,
        session_id: String,
        seen: Arc<Mutex<SessionShareSeen>>,
    }

    impl Runner for SessionShareRunner {
        fn run(&self, spec: &RunnerSpec) -> RunnerResult {
            self.seen
                .lock()
                .unwrap()
                .push((spec.cell_id.clone(), spec.resume_agent_session_id.clone()));
            RunnerResult {
                status: "done".to_string(),
                tokens_in: 1,
                tokens_out: 2,
                cost_usd: 0.5,
                summary: "ok".to_string(),
                error: None,
                proofed: None,
                agent_session_id: if spec.cell_id == self.provider {
                    Some(self.session_id.clone())
                } else {
                    None
                },
                ghost: None,
            }
        }
    }

    fn session_share_recorded(seen: &Arc<Mutex<SessionShareSeen>>, cell: &str) -> Option<String> {
        seen.lock()
            .unwrap()
            .iter()
            .find(|(c, _)| c == cell)
            .and_then(|(_, r)| r.clone())
    }

    #[test]
    fn cell_resumes_a_completed_dependency_session() {
        let toml = "[[task]]\nname=\"t\"\n\
            [[task.cell]]\nid=\"provider\"\ncwd=\".\"\nagent=\"claude-code\"\nmodel=\"claude-sonnet-5\"\ncommand=\"make\"\n\
            [[task.cell]]\nid=\"consumer\"\ncwd=\".\"\nagent=\"claude-code\"\nmodel=\"claude-sonnet-5\"\ncommand=\"verify\"\ndepends_on=[\"provider\"]\n";
        let (store, id) = store_with(toml);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let runner = SessionShareRunner {
            provider: "provider".into(),
            session_id: "sess-1".into(),
            seen: Arc::clone(&seen),
        };
        execute_squad(&store, &runner, &id);
        // The consumer resumes its completed dependency's session; the
        // provider itself (no dependency) starts fresh.
        assert_eq!(
            session_share_recorded(&seen, "consumer"),
            Some("sess-1".to_string())
        );
        assert_eq!(session_share_recorded(&seen, "provider"), None);
    }

    #[test]
    fn claude_code_model_mismatch_starts_fresh() {
        let toml = "[[task]]\nname=\"t\"\n\
            [[task.cell]]\nid=\"provider\"\ncwd=\".\"\nagent=\"claude-code\"\nmodel=\"claude-sonnet-5\"\ncommand=\"make\"\n\
            [[task.cell]]\nid=\"consumer\"\ncwd=\".\"\nagent=\"claude-code\"\nmodel=\"claude-opus-5\"\ncommand=\"verify\"\ndepends_on=[\"provider\"]\n";
        let (store, id) = store_with(toml);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let runner = SessionShareRunner {
            provider: "provider".into(),
            session_id: "sess-1".into(),
            seen: Arc::clone(&seen),
        };
        execute_squad(&store, &runner, &id);
        // Different claude-code models → the session must NOT be shared and
        // the consumer starts fresh.
        assert_eq!(session_share_recorded(&seen, "consumer"), None);
    }

    #[test]
    fn dependency_without_a_session_id_means_fresh_cell() {
        let toml = "[[task]]\nname=\"t\"\n\
            [[task.cell]]\nid=\"provider\"\ncwd=\".\"\nagent=\"claude-code\"\nmodel=\"claude-sonnet-5\"\ncommand=\"make\"\n\
            [[task.cell]]\nid=\"consumer\"\ncwd=\".\"\nagent=\"claude-code\"\nmodel=\"claude-sonnet-5\"\ncommand=\"verify\"\ndepends_on=[\"provider\"]\n";
        let (store, id) = store_with(toml);
        let seen = Arc::new(Mutex::new(Vec::new()));
        // `provider` is never produced, so no cell returns a session id and
        // the consumer has nothing to resume → fresh.
        let runner = SessionShareRunner {
            provider: "never".into(),
            session_id: "sess-1".into(),
            seen: Arc::clone(&seen),
        };
        execute_squad(&store, &runner, &id);
        assert_eq!(session_share_recorded(&seen, "consumer"), None);
    }
}
