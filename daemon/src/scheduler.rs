//! The scheduler: turns Pending runs into real work.
//!
//! A single scheduler thread polls the store; each `tick` claims up to the
//! available concurrency slots of ready runs, marks each Running, and hands it
//! to a worker thread. The worker executes the run's sessions via the [`Runner`]
//! (the subprocess wait happens *outside* the store lock, so many runs progress
//! concurrently), records each result, and finalizes task/run states.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use crate::cancel::{CancelToken, Cancellations};
use crate::runner::{Runner, RunnerResult, RunnerSpec, SubprocessRunner};
use crate::store::{NodeState, RunState, SessionOutcome, Store};
use crate::verify;

/// How long a worker holds nothing; the poll interval between ticks.
pub const POLL_INTERVAL: Duration = Duration::from_millis(200);

/// How often to check reviews for a base-branch shift and auto-rebuild them.
pub const REVIEW_MAINT_INTERVAL: Duration = Duration::from_secs(5);

/// A dependency-free counting semaphore that bounds how many sessions execute
/// at once. One instance is shared across every run's worker threads, so the
/// concurrency cap is *global and task-level* — independent tasks (across one
/// run or many) run in parallel up to this many at a time, rather than the old
/// "one run at a time" limit.
#[derive(Debug)]
pub struct Semaphore {
    permits: Mutex<i64>,
    available: Condvar,
}

impl Semaphore {
    /// A semaphore with `permits` slots (clamped to at least 1 so it can never
    /// deadlock at zero).
    #[must_use]
    pub fn new(permits: i64) -> Self {
        Self {
            permits: Mutex::new(permits.max(1)),
            available: Condvar::new(),
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
/// Takes the shared `sem` from [`crate::server::Daemon`] so that sessions,
/// task-level verifies, and guardian review merges all count against the same
/// global concurrency cap.
pub fn run_loop(
    store: Arc<Mutex<Store>>,
    runner: Arc<dyn Runner>,
    _max_concurrent: i64,
    cancellations: Cancellations,
    sem: Arc<Semaphore>,
) {
    let mut last_maintenance = std::time::Instant::now();
    // Recovery: start collecting guardians whose contributing sessions are all
    // Done. This handles the case where the daemon was restarted after the run
    // completed but before the guardian auto-started.
    {
        let ids = store
            .lock()
            .expect("store mutex poisoned")
            .collecting_guardians_ready()
            .unwrap_or_default();
        start_reviews(&store, ids, &sem);
    }
    loop {
        tick(&store, &runner, &sem, &cancellations);
        if last_maintenance.elapsed() >= REVIEW_MAINT_INTERVAL {
            crate::guardian_merge::review_maintenance(&store, &sem);
            last_maintenance = std::time::Instant::now();
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// One scheduling pass: start a worker for every ready run. Runs are no longer
/// rationed by a run-count limit — the shared `sem` bounds concurrency at the
/// session (task) level instead, so every ready run gets a worker and its
/// sessions compete for the global slots.
pub fn tick(
    store: &Arc<Mutex<Store>>,
    runner: &Arc<dyn Runner>,
    sem: &Arc<Semaphore>,
    cancellations: &Cancellations,
) {
    let to_start = claim_ready(store);
    for run_id in to_start {
        let store = Arc::clone(store);
        let runner = Arc::clone(runner);
        let sem = Arc::clone(sem);
        let cancellations = cancellations.clone();
        std::thread::spawn(move || {
            // Register a cancel token so a user `cancel` can stop this worker
            // (and its subprocess); drop it once the run is done.
            let token = cancellations.register(&run_id);
            execute_run_inner(&store, runner.as_ref(), &run_id, &token, &sem);
            cancellations.remove(&run_id);
        });
    }
}

/// Under the lock: mark every ready run Running (so the next tick won't re-claim
/// it) and return the claimed ids. Readiness — Pending with cross-run deps Done
/// — is decided by [`Store::list_ready`]; the concurrency cap is enforced later,
/// per session, by the shared [`Semaphore`].
fn claim_ready(store: &Arc<Mutex<Store>>) -> Vec<String> {
    let guard = store.lock().expect("store mutex poisoned");
    let ready = guard.list_ready().unwrap_or_default();
    let mut claimed = Vec::new();
    for run_id in ready {
        if guard.set_run_state(&run_id, RunState::Running).is_ok() {
            claimed.push(run_id);
        }
    }
    claimed
}

/// Execute one already-claimed (Running) run to completion, honouring the
/// dependency order of its sessions. Uses a never-firing cancel token — the
/// cancellable path goes through [`execute_run_with`].
///
/// This single-run entry point owns a private [`Semaphore`], so its independent
/// sessions still run concurrently up to the default limit.
pub fn execute_run(store: &Arc<Mutex<Store>>, runner: &dyn Runner, run_id: &str) {
    let sem = Arc::new(Semaphore::new(crate::DEFAULT_MAX_CONCURRENT));
    execute_run_inner(store, runner, run_id, &CancelToken::never(), &sem);
}

/// Execute one already-claimed (Running) run to completion, stopping early if
/// `cancel` trips. When cancelled mid-flight the worker abandons the run
/// without finalizing it: [`Store::cancel`] has already flipped the run and its
/// non-terminal nodes to `cancelled`, and the runner has killed any subprocess.
///
/// Owns a private [`Semaphore`]; the scheduler's [`tick`] uses the concurrent
/// path directly with the *shared* global semaphore instead.
pub fn execute_run_with(
    store: &Arc<Mutex<Store>>,
    runner: &dyn Runner,
    run_id: &str,
    cancel: &CancelToken,
) {
    let sem = Arc::new(Semaphore::new(crate::DEFAULT_MAX_CONCURRENT));
    execute_run_inner(store, runner, run_id, cancel, &sem);
}

/// Per-session execution state, shared across a run's concurrent workers.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SessState {
    Pending,
    Running,
    Done,
    Failed,
}

/// Shared progress for a run's sessions, behind one mutex.
///
/// `failed` lives here (rather than in a separate mutex) so a session's status
/// transition and its task's failure flag are published together under one
/// lock. The dispatcher decides when a task's sessions have all finished *and*
/// whether that task failed from the same locked snapshot, so a per-task
/// finalizer can never launch in the window between "status is terminal" and
/// "failure recorded".
struct Progress {
    status: Vec<SessState>,
    summaries: Vec<Option<String>>,
    /// Task indices with at least one failed session, session-verify, or
    /// task-verify — i.e. tasks that must finalize as `Failed`.
    failed: HashSet<i64>,
}

/// Execute a run's sessions concurrently: a session starts the moment all its
/// dependencies are `Done`, and each worker holds a permit from the shared
/// `sem` only while doing real work — so the number of sessions running at once
/// is bounded globally (the task-level concurrency cap). After every session is
/// terminal, task-level verifies and finalization run exactly as before.
fn execute_run_inner(
    store: &Arc<Mutex<Store>>,
    runner: &dyn Runner,
    run_id: &str,
    cancel: &CancelToken,
    sem: &Arc<Semaphore>,
) {
    let (sessions, tasks, already_done) = {
        let guard = store.lock().expect("store mutex poisoned");
        // Mark Running up front so the finalization guard (which leaves an
        // edit-reset run Pending) has a Running baseline to compare against.
        let _ = guard.set_run_state(run_id, RunState::Running);
        (
            guard.sessions_of(run_id).unwrap_or_default(),
            guard.tasks_of(run_id).unwrap_or_default(),
            // Sessions already Done are skipped, so a restarted run only re-runs
            // its dirty (reset-to-pending) subset instead of redoing them (RAL-19).
            guard.done_sessions(run_id).unwrap_or_default(),
        )
    };

    let plan = match crate::plan::plan(&sessions, &tasks) {
        Ok(p) => p,
        Err(msg) => {
            finalize_all_failed(store, run_id, &tasks, &msg);
            return;
        }
    };

    let n = sessions.len();
    // An already-Done session (restarted run's untouched upstream) starts
    // satisfied so its downstream can proceed (RAL-19); the rest start Pending.
    let progress = Mutex::new(Progress {
        status: (0..n)
            .map(|i| {
                if already_done.contains(&(sessions[i].task_idx, sessions[i].idx)) {
                    SessState::Done
                } else {
                    SessState::Pending
                }
            })
            .collect(),
        summaries: vec![None; n],
        failed: HashSet::new(),
    });

    // Each task's session indices, so the dispatcher can tell when a task's own
    // sessions have all finished and finalize *that* task on its own — rather
    // than holding every finished task at `running` until the slowest sibling
    // task in the run completes (the "all sessions done but task still running"
    // bug).
    let mut task_sessions: HashMap<i64, Vec<usize>> = HashMap::new();
    for (i, s) in sessions.iter().enumerate() {
        task_sessions.entry(s.task_idx).or_default().push(i);
    }
    let task_indices: Vec<i64> = tasks.iter().map(|t| t.idx).collect();
    // Tasks whose finalizer has already been launched (dispatcher-thread-local,
    // so no lock needed).
    let mut finalized: HashSet<i64> = HashSet::new();

    // Dispatcher loop: each pass launches every session whose prerequisites are
    // all Done and fails every session whose prerequisite failed, and launches a
    // per-task finalizer the moment a task's sessions are all terminal — until
    // no session is left Pending or Running.
    std::thread::scope(|scope| {
        loop {
            // Stop launching the moment the run is cancelled; in-flight workers
            // observe the same token and unwind.
            if cancel.is_cancelled() {
                return;
            }
            let mut to_dispatch: Vec<usize> = Vec::new();
            let mut blocked: Vec<usize> = Vec::new();
            let mut to_finalize: Vec<i64> = Vec::new();
            let mut active = false;
            {
                let mut prog = progress.lock().expect("progress mutex poisoned");
                // Indexes several parallel collections (status, deps, sessions),
                // so a range loop is the natural form here.
                #[allow(clippy::needless_range_loop)]
                for i in 0..n {
                    match prog.status[i] {
                        SessState::Done | SessState::Failed => {}
                        SessState::Running => active = true,
                        SessState::Pending => {
                            let deps = &plan.deps[i];
                            if deps.iter().any(|&d| prog.status[d] == SessState::Failed) {
                                prog.status[i] = SessState::Failed;
                                prog.failed.insert(sessions[i].task_idx);
                                blocked.push(i);
                            } else if deps.iter().all(|&d| prog.status[d] == SessState::Done) {
                                prog.status[i] = SessState::Running;
                                to_dispatch.push(i);
                                active = true;
                            } else {
                                active = true;
                            }
                        }
                    }
                }
                // A task is ready to finalize once every one of its sessions is
                // terminal. Read this from the same snapshot as `prog.failed` so
                // the finalizer sees a consistent (done, failed?) pair.
                for &t in &task_indices {
                    if finalized.contains(&t) {
                        continue;
                    }
                    let idxs = task_sessions.get(&t);
                    let all_terminal = idxs.is_some_and(|idxs| {
                        !idxs.is_empty()
                            && idxs.iter().all(|&i| {
                                matches!(prog.status[i], SessState::Done | SessState::Failed)
                            })
                    });
                    if all_terminal {
                        to_finalize.push(t);
                    }
                }
            }

            // Record sessions blocked by a failed prerequisite (store writes
            // happen without the progress lock held; the failure flag was set
            // above under the lock).
            for &i in &blocked {
                let row = &sessions[i];
                let outcome = SessionOutcome {
                    state: NodeState::Failed,
                    tokens_in: 0,
                    tokens_out: 0,
                    cost_usd: 0.0,
                    error: Some("blocked by a failed dependency".to_string()),
                    claude_session_id: None,
                };
                let guard = store.lock().expect("store mutex poisoned");
                let _ = guard.set_session_state(run_id, row.task_idx, row.idx, NodeState::Failed);
                let _ = guard.record_session_result(run_id, row.task_idx, row.idx, &outcome);
            }

            // Launch the ready sessions concurrently. Borrow the owned locals by
            // reference so each worker shares (not moves) them.
            let progress_ref = &progress;
            let plan_ref = &plan;
            let sessions_ref = &sessions;
            for i in to_dispatch {
                scope.spawn(move || {
                    run_session_worker(
                        store,
                        runner,
                        run_id,
                        cancel,
                        sem,
                        sessions_ref,
                        plan_ref,
                        progress_ref,
                        i,
                    );
                });
            }

            // Launch a finalizer for each task that just finished all its
            // sessions: it runs the task-level verifies and flips the task to
            // Done/Failed immediately, concurrently with the rest of the run.
            for t in to_finalize {
                finalized.insert(t);
                scope.spawn(move || {
                    run_task_finalizer(
                        store,
                        runner,
                        run_id,
                        cancel,
                        sessions_ref,
                        progress_ref,
                        t,
                        sem,
                    );
                });
            }

            if !active {
                return;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    });

    // A cancellation abandons the run without finalizing: the store already set
    // the run + its non-terminal nodes to `cancelled`.
    if cancel.is_cancelled() {
        return;
    }
    // All session workers and per-task finalizers have joined at the end of the
    // scope, so `failed` is final and every task's state has been written.
    let failed_tasks = progress
        .into_inner()
        .expect("progress mutex poisoned")
        .failed;

    let guard = store.lock().expect("store mutex poisoned");
    // If an edit reset this run to Pending mid-flight, don't clobber it with a
    // terminal state — leave it Pending so it re-runs with the new values.
    if !matches!(guard.run_state(run_id), Ok(RunState::Running)) {
        return;
    }
    let run_state = if failed_tasks.is_empty() {
        RunState::Done
    } else {
        RunState::Failed
    };
    let _ = guard.set_run_state(run_id, run_state);
    // Per-task review triggers fire from `run_task_finalizer` as each task
    // completes, so no run-level sweep is needed here.
    drop(guard);
}

/// Parse the task/session ref from an `upstream = "<<task:...>>"` sentinel.
/// Returns the inner ref string (e.g. `"task-a"` or `"task-a/session-1"`)
/// when the sentinel matches, `None` otherwise.
fn parse_upstream_task_ref(upstream: &str) -> Option<&str> {
    upstream
        .strip_prefix(ralphus_core::schema::UPSTREAM_TASK_REF_PREFIX)
        .and_then(|s| s.strip_suffix(">>"))
}

/// Before running `row`'s session, check whether it declares an
/// `upstream = "<<task:...>>"` sentinel. If so, rebase `row`'s branch onto
/// the named dependency's current branch tip.
///
/// Same-repo worktrees get a real `git rebase`; cross-repo upstreams are
/// skipped with a logged warning (rebase cannot happen without shared history).
///
/// Returns `Some(error_message)` when the rebase was attempted but failed
/// (the caller must fail the session), or `None` when either no rebase was
/// needed or the rebase succeeded.
fn try_upstream_rebase(
    row: &crate::store::SessionRow,
    sessions: &[crate::store::SessionRow],
) -> Option<String> {
    let upstream = row.upstream.as_deref()?;
    let ref_str = parse_upstream_task_ref(upstream)?;

    // Split "task-name" or "task-name/session-id".
    let (task_name, session_id_filter) = ref_str
        .split_once('/')
        .map_or((ref_str, None), |(t, s)| (t, Some(s)));

    let dep = sessions.iter().find(|s| {
        s.task_name == task_name && session_id_filter.is_none_or(|sid| s.session_id == sid)
    })?;

    let b_cwd = std::path::Path::new(row.cwd.as_deref()?);
    let a_cwd = std::path::Path::new(dep.cwd.as_deref()?);

    if !crate::reviews::same_git_repo(a_cwd, b_cwd) {
        eprintln!(
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
        return Some(format!(
            "failed to rebase session '{}' onto upstream branch '{a_branch}': {e}",
            row.session_id
        ));
    }

    eprintln!(
        "ralphus: rebased session '{}' onto upstream branch '{a_branch}'",
        row.session_id
    );
    None
}

/// Run one session — and, on success, its session-level verifies — while
/// holding a permit from the shared semaphore, then publish the result into the
/// shared `progress` (status + failure flag). Dependency-waiting is the
/// dispatcher's job, so a permit is only ever held during real work, never
/// while parked.
#[allow(clippy::too_many_arguments)]
fn run_session_worker(
    store: &Arc<Mutex<Store>>,
    runner: &dyn Runner,
    run_id: &str,
    cancel: &CancelToken,
    sem: &Semaphore,
    sessions: &[crate::store::SessionRow],
    plan: &crate::plan::ExecutionPlan,
    progress: &Mutex<Progress>,
    i: usize,
) {
    let row = &sessions[i];
    if cancel.is_cancelled() {
        return;
    }
    // Acquire a global slot; released when `_permit` drops at function end.
    let _permit = sem.acquire();
    if cancel.is_cancelled() {
        return;
    }
    {
        let guard = store.lock().expect("store mutex poisoned");
        let _ = guard.set_session_state(run_id, row.task_idx, row.idx, NodeState::Running);
        let _ = guard.set_task_state(run_id, row.task_idx, NodeState::Running);
    }

    // Resolve handoff placeholders against completed upstream summaries.
    let summaries = progress
        .lock()
        .expect("progress mutex poisoned")
        .summaries
        .clone();
    let mut spec = RunnerSpec::from_row(run_id, row);
    spec.prompt = spec
        .prompt
        .map(|p| resolve_handoffs(&p, &plan.deps[i], &summaries));

    // If this session declares an upstream task ref, rebase its branch onto
    // the dependency's current branch tip before handing off to the runner.
    if let Some(rebase_err) = try_upstream_rebase(row, sessions) {
        let outcome = crate::store::SessionOutcome {
            state: crate::store::NodeState::Failed,
            tokens_in: 0,
            tokens_out: 0,
            cost_usd: 0.0,
            error: Some(rebase_err),
            claude_session_id: None,
        };
        {
            let guard = store.lock().expect("store mutex poisoned");
            let _ = guard.record_session_result(run_id, row.task_idx, row.idx, &outcome);
        }
        let mut prog = progress.lock().expect("progress mutex poisoned");
        prog.status[i] = SessState::Failed;
        prog.failed.insert(row.task_idx);
        return;
    }

    let result = runner.run_cancellable(&spec, cancel);
    // A cancellation that landed while the runner was working: leave the
    // (already `cancelled`) node as the store set it and abandon this session.
    if cancel.is_cancelled() {
        return;
    }
    let outcome = SessionOutcome {
        state: result.node_state(),
        tokens_in: result.tokens_in,
        tokens_out: result.tokens_out,
        cost_usd: result.cost_usd,
        error: result.error.clone(),
        claude_session_id: result.claude_session_id.clone(),
    };
    {
        let guard = store.lock().expect("store mutex poisoned");
        let _ = guard.record_session_result(run_id, row.task_idx, row.idx, &outcome);
    }

    if !result.is_done() {
        let mut prog = progress.lock().expect("progress mutex poisoned");
        prog.status[i] = SessState::Failed;
        prog.failed.insert(row.task_idx);
        return;
    }

    // Session-level verify steps run in the session's own working directory,
    // with `prompt`-kind steps using this session's resolved backend
    // (agent/model) unless the step overrides `model` itself. They run *before*
    // the session is published `Done`, so a dependant (e.g. finalize) still
    // waits for the verifies to finish — matching the old sequential order.
    let cwd = row.cwd.clone().unwrap_or_default();
    let verified = run_verifies(
        store,
        runner,
        run_id,
        row.task_idx,
        &row.task_name,
        "session",
        row.idx,
        &cwd,
        &row.agent,
        row.model.as_deref(),
        cancel,
    );
    // Publish the terminal status and the failure flag together, so the
    // dispatcher never sees this session Done before its verify verdict is
    // recorded — otherwise a task finalizer could launch and mark the task Done
    // while a failing session-verify is still in flight.
    let mut prog = progress.lock().expect("progress mutex poisoned");
    prog.summaries[i] = Some(result.summary.clone());
    prog.status[i] = SessState::Done;
    if !verified {
        prog.failed.insert(row.task_idx);
    }
}

/// Finalize a single task once all its sessions are terminal: run its
/// task-level verify steps (skipped if any session already failed) in the
/// task's first session's working directory and resolved backend, then flip the
/// task to `Done`/`Failed` in the store. Runs concurrently with the rest of the
/// run, so a task that finishes early no longer waits at `running` for its
/// slower siblings.
///
/// Task-level verifies acquire a slot from `sem` so they count against the same
/// global concurrency cap as sessions and guardian review merges.
#[allow(clippy::too_many_arguments)]
fn run_task_finalizer(
    store: &Arc<Mutex<Store>>,
    runner: &dyn Runner,
    run_id: &str,
    cancel: &CancelToken,
    sessions: &[crate::store::SessionRow],
    progress: &Mutex<Progress>,
    task_idx: i64,
    sem: &Arc<Semaphore>,
) {
    if cancel.is_cancelled() {
        return;
    }
    // A session (or session-verify) failure already condemns the task; skip the
    // task-level verifies in that case, matching the old batch behaviour.
    let already_failed = progress
        .lock()
        .expect("progress mutex poisoned")
        .failed
        .contains(&task_idx);
    let mut failed = already_failed;
    if !failed {
        let task_session = sessions.iter().find(|s| s.task_idx == task_idx);
        let cwd = task_session.and_then(|s| s.cwd.clone()).unwrap_or_default();
        let task_name = task_session
            .map(|s| s.task_name.clone())
            .unwrap_or_default();
        let agent = task_session
            .map(|s| s.agent.clone())
            .unwrap_or_else(|| "claude".to_string());
        let model = task_session.and_then(|s| s.model.clone());
        // Acquire a slot so task-level verifies count against max_concurrent.
        // Released before calling try_start_ready_reviews_for_task so the review
        // worker can acquire a slot of its own.
        let _permit = sem.acquire();
        if cancel.is_cancelled() {
            return;
        }
        // Mark the task Running before executing its verify steps so the board
        // never shows a task as Done while verifies are in-flight.  In normal
        // first-run execution this is a no-op (the task is already Running), but
        // when all of a task's sessions were already Done from a prior run (e.g.
        // a partial restart) the task can be Done in the DB while its finalizer
        // re-runs the verifies — this guard closes that window.
        {
            let guard = store.lock().expect("store mutex poisoned");
            if matches!(guard.run_state(run_id), Ok(RunState::Running)) {
                let _ = guard.set_task_state(run_id, task_idx, NodeState::Running);
            }
        }
        if !run_verifies(
            store,
            runner,
            run_id,
            task_idx,
            &task_name,
            "task",
            -1,
            &cwd,
            &agent,
            model.as_deref(),
            cancel,
        ) {
            failed = true;
            progress
                .lock()
                .expect("progress mutex poisoned")
                .failed
                .insert(task_idx);
        }
        drop(_permit);
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
    let did_write = {
        let guard = store.lock().expect("store mutex poisoned");
        // Don't clobber a run an edit reset to Pending mid-flight (mirrors the
        // run-level guard); leave the task for the re-run.
        if matches!(guard.run_state(run_id), Ok(RunState::Running)) {
            let _ = guard.set_task_state(run_id, task_idx, state);
            true
        } else {
            false
        }
    };
    // Guard dropped above; now check per-task review readiness without holding
    // the lock.
    if did_write && state == NodeState::Done {
        try_start_ready_reviews_for_task(store, run_id, sessions, task_idx, sem);
    }
}

/// Spawn a background merge for each guardian that is still `collecting`.
/// Uses an atomic DB transition (`collecting → merging`) so that concurrent
/// calls for the same guardian — possible when multiple task finalizers finish
/// simultaneously — never both proceed to `run_merge`. A user can still start
/// one earlier via the merge endpoint ("allow review"); that path uses the same
/// guard inside `run_merge`. Conflict resolution uses a fresh subprocess
/// runner, independent of the session runner.
///
/// Each spawned merge worker acquires a slot from `sem` before calling
/// `run_merge`, so review merges count against the same global concurrency cap
/// as sessions and task-level verifies.
fn start_reviews(store: &Arc<Mutex<Store>>, guardian_ids: Vec<String>, sem: &Arc<Semaphore>) {
    for gid in guardian_ids {
        let store = Arc::clone(store);
        let sem = Arc::clone(sem);
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
                let runner: Arc<dyn Runner> = Arc::new(SubprocessRunner::from_env());
                crate::guardian_merge::run_merge(&store, runner.as_ref(), &gid);
            }
        });
    }
}

/// Return the set of task indices that have at least one session whose git
/// project root matches `git_root`. These are the tasks that must be `Done`
/// before the guardian rooted at `git_root` may begin rebasing.
///
/// Attribution is via [`crate::reviews::project_root_of`]: a session's cwd is
/// resolved to its git project root (the parent of its shared git-common-dir),
/// then compared against the guardian's stored `git_root`. This correctly
/// handles linked worktrees, which are siblings of the main repo rather than
/// subdirectories of it.
fn guardian_blocking_tasks(sessions: &[crate::store::SessionRow], git_root: &str) -> HashSet<i64> {
    let root_path = Path::new(git_root);
    sessions
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
/// from `run_id`: if all of a guardian's blocking tasks (those with sessions
/// under the guardian's `git_root`) are now Done, kick off that guardian's merge
/// immediately — without waiting for the whole run to finish.
fn try_start_ready_reviews_for_task(
    store: &Arc<Mutex<Store>>,
    run_id: &str,
    sessions: &[crate::store::SessionRow],
    completed_task_idx: i64,
    sem: &Arc<Semaphore>,
) {
    let guardian_ids = {
        let guard = store.lock().expect("store mutex poisoned");
        // Combine guardians tagged with this run_id AND collecting guardians
        // that have branches contributed by this run's sessions. The latter
        // handles linked reviews whose guardian was created by an earlier
        // submission and therefore carries a different run_id.
        let mut ids = guard.guardians_for_run(run_id).unwrap_or_default();
        for gid in guard
            .collecting_guardians_for_sessions(run_id)
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
        let blocking = guardian_blocking_tasks(sessions, &git_root);
        // Only proceed when the just-completed task is one of the blockers; skip
        // guardians that are unrelated to this task.
        if blocking.is_empty() || !blocking.contains(&completed_task_idx) {
            continue;
        }
        let all_done = {
            let guard = store.lock().expect("store mutex poisoned");
            guard.all_tasks_done(run_id, &blocking).unwrap_or(false)
        };
        if all_done {
            ready.push(gid.clone());
        }
    }
    start_reviews(store, ready, sem);
}

/// Mark every task and the run failed — used when the run cannot even be planned
/// (a dependency cycle).
fn finalize_all_failed(
    store: &Arc<Mutex<Store>>,
    run_id: &str,
    tasks: &[crate::store::TaskRow],
    _reason: &str,
) {
    let guard = store.lock().expect("store mutex poisoned");
    for task in tasks {
        let _ = guard.set_task_state(run_id, task.idx, NodeState::Failed);
    }
    let _ = guard.set_run_state(run_id, RunState::Failed);
}

/// Substitute `{handoff:<task>}` placeholders in a prompt with the summary of a
/// completed upstream session in that task (a dependency of this session).
fn resolve_handoffs(prompt: &str, dep_positions: &[usize], summaries: &[Option<String>]) -> String {
    if !prompt.contains("{handoff:") {
        return prompt.to_string();
    }
    // Join all available upstream summaries for a generic {handoff:*} and also
    // support {handoff:<taskname>} — but since we only have positions here, a
    // simple, predictable rule: replace every {handoff:...} with the summaries
    // of this session's completed dependencies, in order.
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

/// Run the `command` and `prompt` verify steps of one scope, updating each
/// step's state. Returns false if any of them fails. Other verifier kinds
/// (`brain` / `approval`) are still deferred and left pending.
///
/// `agent` verifiers run through the same [`Runner`] as sessions do — a
/// [`RunnerSpec`] is built with `verify: true`, `prompt` set to the step's
/// instruction text, and `agent`/`model` taken from the owning session's
/// resolved backend (`session_agent`/`session_model`), with the step's own
/// `model` (from `verify_specs`) overriding it when set.
#[allow(clippy::too_many_arguments)]
fn run_verifies(
    store: &Arc<Mutex<Store>>,
    runner: &dyn Runner,
    run_id: &str,
    task_idx: i64,
    task_name: &str,
    scope: &str,
    session_idx: i64,
    cwd: &str,
    session_agent: &str,
    session_model: Option<&str>,
    cancel: &CancelToken,
) -> bool {
    let specs = {
        let guard = store.lock().expect("store mutex poisoned");
        guard
            .verify_specs(run_id, task_idx, scope, session_idx)
            .unwrap_or_default()
    };
    let mut all_ok = true;
    for (idx, kind, spec, verify_model, verify_timeout, verify_budget) in specs {
        if cancel.is_cancelled() {
            return all_ok;
        }
        let (passed, output) = match kind.as_str() {
            "command" => {
                set_verify_running(store, run_id, task_idx, scope, session_idx, idx);
                verify::run_command_verify_capture(cwd, &spec)
            }
            "prompt" => {
                set_verify_running(store, run_id, task_idx, scope, session_idx, idx);
                let model = verify_model.as_deref().or(session_model);
                let runner_spec = RunnerSpec::for_verify(
                    run_id,
                    task_name,
                    &format!("verify-{scope}-{idx}"),
                    cwd,
                    &spec,
                    session_agent,
                    model,
                    verify_timeout.and_then(|s| u64::try_from(s).ok()),
                    verify_budget.and_then(|b| u64::try_from(b).ok()),
                );
                let result: RunnerResult = runner.run_cancellable(&runner_spec, cancel);
                let passed = result.verify_passed();
                let output = match &result.error {
                    Some(err) if result.summary.is_empty() => err.clone(),
                    Some(err) => format!("{}\n{err}", result.summary),
                    None => result.summary.clone(),
                };
                (passed, output)
            }
            _ => continue, // brain / approval / unknown stay pending (deferred)
        };
        let state = if passed {
            NodeState::Done
        } else {
            NodeState::Failed
        };
        {
            let guard = store.lock().expect("store mutex poisoned");
            let _ =
                guard.set_verify_result(run_id, task_idx, scope, session_idx, idx, state, &output);
        }
        if !passed {
            all_ok = false;
        }
    }
    all_ok
}

/// Mark one verify step `Running` before executing it.
fn set_verify_running(
    store: &Arc<Mutex<Store>>,
    run_id: &str,
    task_idx: i64,
    scope: &str,
    session_idx: i64,
    idx: i64,
) {
    let guard = store.lock().expect("store mutex poisoned");
    let _ = guard.set_verify_state(
        run_id,
        task_idx,
        scope,
        session_idx,
        idx,
        NodeState::Running,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::{RunnerResult, RunnerSpec};

    /// A runner that succeeds or fails based on the session command (or, for
    /// an agent verify spec, its prompt), without any subprocess — lets us
    /// test the scheduler deterministically. Agent verify specs pass unless
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
                    verified: spec.verify.then_some(true),
                    claude_session_id: None,
                }
            }
        }
    }

    fn store_with(toml: &str) -> (Arc<Mutex<Store>>, String) {
        let mut store = Store::open_in_memory().unwrap();
        let file = toml::from_str(toml).unwrap();
        let id = store.insert_run(&file, None, false).unwrap();
        (Arc::new(Mutex::new(store)), id)
    }

    const ONE_SESSION: &str =
        "[[task]]\nname=\"build\"\n[[task.session]]\ncwd=\".\"\ncommand=\"do-thing\"\n";

    /// Two tasks with no dependency between them — they must run in parallel.
    const TWO_INDEPENDENT_TASKS: &str = "[[task]]\nname=\"a\"\n[[task.session]]\ncwd=\".\"\ncommand=\"x\"\n[[task]]\nname=\"b\"\n[[task.session]]\ncwd=\".\"\ncommand=\"y\"\n";

    /// Records the peak number of sessions executing at the same instant.
    struct ConcurrencyRunner {
        current: Arc<std::sync::atomic::AtomicI64>,
        peak: Arc<std::sync::atomic::AtomicI64>,
    }

    impl Runner for ConcurrencyRunner {
        fn run(&self, _spec: &RunnerSpec) -> RunnerResult {
            use std::sync::atomic::Ordering;
            let now = self.current.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(now, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(60));
            self.current.fetch_sub(1, Ordering::SeqCst);
            RunnerResult {
                status: "done".to_string(),
                tokens_in: 0,
                tokens_out: 0,
                cost_usd: 0.0,
                summary: "ok".to_string(),
                error: None,
                verified: None,
                claude_session_id: None,
            }
        }
    }

    // The whole point of the refactor: two independent tasks in one run execute
    // concurrently (peak concurrency 2), not one-at-a-time.
    #[test]
    fn independent_sessions_run_concurrently() {
        use std::sync::atomic::{AtomicI64, Ordering};
        let (store, id) = store_with(TWO_INDEPENDENT_TASKS);
        let current = Arc::new(AtomicI64::new(0));
        let peak = Arc::new(AtomicI64::new(0));
        let runner: Arc<dyn Runner> = Arc::new(ConcurrencyRunner {
            current: Arc::clone(&current),
            peak: Arc::clone(&peak),
        });
        execute_run(&store, runner.as_ref(), &id);

        assert_eq!(
            store.lock().unwrap().run_state(&id).unwrap(),
            RunState::Done
        );
        assert_eq!(
            peak.load(Ordering::SeqCst),
            2,
            "independent sessions should run in parallel"
        );
    }

    #[test]
    fn successful_run_reaches_done() {
        let (store, id) = store_with(ONE_SESSION);
        let runner: Arc<dyn Runner> = Arc::new(FakeRunner { fail_on: None });
        execute_run(&store, runner.as_ref(), &id);

        let guard = store.lock().unwrap();
        assert_eq!(guard.run_state(&id).unwrap(), RunState::Done);
        let run = guard.get_run(&id).unwrap();
        assert_eq!(run.tasks[0].state, "done");
        assert_eq!(run.tasks[0].sessions[0].state, "done");
        assert_eq!(run.tasks[0].sessions[0].tokens_in, 1);
        assert_eq!(run.tasks[0].sessions[0].cost_usd, 0.5);
    }

    #[test]
    fn failing_session_fails_run() {
        let (store, id) = store_with(ONE_SESSION);
        let runner: Arc<dyn Runner> = Arc::new(FakeRunner {
            fail_on: Some("do-thing".to_string()),
        });
        execute_run(&store, runner.as_ref(), &id);

        let guard = store.lock().unwrap();
        assert_eq!(guard.run_state(&id).unwrap(), RunState::Failed);
        assert_eq!(guard.get_run(&id).unwrap().tasks[0].state, "failed");
    }

    #[test]
    fn tick_claims_and_runs_pending() {
        let (store, id) = store_with(ONE_SESSION);
        let runner: Arc<dyn Runner> = Arc::new(FakeRunner { fail_on: None });

        // Tick spawns a worker thread; poll until it finishes.
        let sem = Arc::new(Semaphore::new(4));
        tick(&store, &runner, &sem, &Cancellations::new());
        let mut waited = 0;
        loop {
            let state = store.lock().unwrap().run_state(&id).unwrap();
            if state.is_terminal() || waited > 200 {
                assert_eq!(state, RunState::Done);
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
            waited += 1;
        }
    }

    #[test]
    fn claim_ready_claims_all_pending() {
        let (store, id) = store_with(ONE_SESSION);
        // Claiming is no longer rationed by a run-count limit; every ready run
        // is claimed and the semaphore bounds concurrency per session instead.
        assert_eq!(claim_ready(&store), vec![id]);
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

    /// Stands in for a long-running session: blocks in `run_cancellable` until
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

    // Cancelling a run mid-session must (a) stop the worker and (b) leave the
    // run + its nodes `cancelled`, not clobbered to done/failed by the worker.
    #[test]
    fn cancelling_mid_run_stops_and_leaves_cancelled() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let (store, id) = store_with(ONE_SESSION);
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
            std::thread::spawn(move || execute_run_with(&store, runner.as_ref(), &id, &token))
        };

        // Wait until the session is in flight, then cancel as the API does.
        while !started.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(2));
        }
        store.lock().unwrap().cancel(&id).unwrap();
        token.cancel();
        worker.join().unwrap();

        let guard = store.lock().unwrap();
        assert_eq!(guard.run_state(&id).unwrap(), RunState::Cancelled);
        let run = guard.get_run(&id).unwrap();
        assert_eq!(run.tasks[0].state, "cancelled");
        assert_eq!(run.tasks[0].sessions[0].state, "cancelled");
    }

    const SESSION_PASSING_VERIFY: &str = "[[task]]\nname=\"t\"\n[[task.session]]\ncwd=\".\"\ncommand=\"do\"\n[[task.session.verify]]\ncommand=\"exit 0\"\n";
    const TASK_FAILING_VERIFY: &str = "[[task]]\nname=\"t\"\n[[task.session]]\ncwd=\".\"\ncommand=\"do\"\n[[task.verify]]\ncommand=\"exit 1\"\n";

    /// Records the order sessions are run in (by command), for ordering tests.
    struct RecordingRunner {
        order: Arc<Mutex<Vec<String>>>,
        fail_on: Option<String>,
    }

    impl Runner for RecordingRunner {
        fn run(&self, spec: &RunnerSpec) -> RunnerResult {
            let cmd = spec.command.clone().unwrap_or_default();
            self.order.lock().unwrap().push(cmd.clone());
            if self.fail_on.as_deref() == Some(cmd.as_str()) {
                RunnerResult::failure("intentional failure")
            } else {
                RunnerResult {
                    status: "done".to_string(),
                    tokens_in: 0,
                    tokens_out: 0,
                    cost_usd: 0.0,
                    summary: format!("did {cmd}"),
                    error: None,
                    verified: None,
                    claude_session_id: None,
                }
            }
        }
    }

    // `b` (listed first) depends on `a`; the scheduler must still run `a` first.
    const OUT_OF_ORDER_DEPS: &str = "[[task]]\nname=\"t\"\n[[task.session]]\nid=\"b\"\ncwd=\".\"\ncommand=\"cmd-b\"\ndepends_on=[\"a\"]\n[[task.session]]\nid=\"a\"\ncwd=\".\"\ncommand=\"cmd-a\"\n";

    #[test]
    fn sessions_run_in_dependency_order() {
        let (store, id) = store_with(OUT_OF_ORDER_DEPS);
        let order = Arc::new(Mutex::new(Vec::new()));
        let runner: Arc<dyn Runner> = Arc::new(RecordingRunner {
            order: Arc::clone(&order),
            fail_on: None,
        });
        execute_run(&store, runner.as_ref(), &id);
        assert_eq!(*order.lock().unwrap(), vec!["cmd-a", "cmd-b"]);
        assert_eq!(
            store.lock().unwrap().run_state(&id).unwrap(),
            RunState::Done
        );
    }

    #[test]
    fn dependent_session_is_blocked_when_prerequisite_fails() {
        let (store, id) = store_with(OUT_OF_ORDER_DEPS);
        let order = Arc::new(Mutex::new(Vec::new()));
        let runner: Arc<dyn Runner> = Arc::new(RecordingRunner {
            order: Arc::clone(&order),
            fail_on: Some("cmd-a".to_string()),
        });
        execute_run(&store, runner.as_ref(), &id);
        // `b` must never run because `a` failed.
        assert_eq!(*order.lock().unwrap(), vec!["cmd-a"]);
        assert_eq!(
            store.lock().unwrap().run_state(&id).unwrap(),
            RunState::Failed
        );
        let run = store.lock().unwrap().get_run(&id).unwrap();
        let b = run.tasks[0].sessions.iter().find(|s| s.id == "b").unwrap();
        assert_eq!(b.state, "failed");
        assert_eq!(b.error.as_deref(), Some("blocked by a failed dependency"));
    }

    #[test]
    fn dependency_cycle_fails_the_run() {
        let cyclic = "[[task]]\nname=\"t\"\n[[task.session]]\nid=\"a\"\ncwd=\".\"\ncommand=\"x\"\ndepends_on=[\"b\"]\n[[task.session]]\nid=\"b\"\ncwd=\".\"\ncommand=\"y\"\ndepends_on=[\"a\"]\n";
        let (store, id) = store_with(cyclic);
        let runner: Arc<dyn Runner> = Arc::new(FakeRunner { fail_on: None });
        execute_run(&store, runner.as_ref(), &id);
        assert_eq!(
            store.lock().unwrap().run_state(&id).unwrap(),
            RunState::Failed
        );
    }

    #[test]
    fn passing_verify_keeps_run_done() {
        let (store, id) = store_with(SESSION_PASSING_VERIFY);
        let runner: Arc<dyn Runner> = Arc::new(FakeRunner { fail_on: None });
        execute_run(&store, runner.as_ref(), &id);
        assert_eq!(
            store.lock().unwrap().run_state(&id).unwrap(),
            RunState::Done
        );
    }

    #[test]
    fn failing_verify_fails_run_even_when_session_succeeds() {
        let (store, id) = store_with(TASK_FAILING_VERIFY);
        let runner: Arc<dyn Runner> = Arc::new(FakeRunner { fail_on: None });
        execute_run(&store, runner.as_ref(), &id);
        assert_eq!(
            store.lock().unwrap().run_state(&id).unwrap(),
            RunState::Failed
        );
    }

    // ── prompt-kind verify execution ──────────────────────────────────────

    const TASK_PROMPT_VERIFY: &str = "[[task]]\nname=\"t\"\n[[task.session]]\ncwd=\".\"\ncommand=\"do\"\nagent=\"ollama\"\nmodel=\"qwen3:8b\"\n[[task.verify]]\nid=\"check\"\nprompt=\"check it\"\n";
    const TASK_PROMPT_VERIFY_MODEL_OVERRIDE: &str = "[[task]]\nname=\"t\"\n[[task.session]]\ncwd=\".\"\ncommand=\"do\"\nagent=\"ollama\"\nmodel=\"qwen3:8b\"\n[[task.verify]]\nprompt=\"check it\"\nmodel=\"qwen2:1b\"\n";
    const TASK_BRAIN_VERIFY: &str = "[[task]]\nname=\"t\"\n[[task.session]]\ncwd=\".\"\ncommand=\"do\"\n[[task.verify]]\nbrain=\"check it\"\n";

    #[test]
    fn prompt_verify_pass_keeps_run_done_and_records_output() {
        let (store, id) = store_with(TASK_PROMPT_VERIFY);
        let runner: Arc<dyn Runner> = Arc::new(FakeRunner { fail_on: None });
        execute_run(&store, runner.as_ref(), &id);
        let guard = store.lock().unwrap();
        assert_eq!(guard.run_state(&id).unwrap(), RunState::Done);
        let run = guard.get_run(&id).unwrap();
        let v = &run.tasks[0].verify[0];
        assert_eq!(v.kind, "prompt");
        assert_eq!(v.state, "done");
        assert_eq!(v.output.as_deref(), Some("ok"));
    }

    #[test]
    fn prompt_verify_fail_fails_run_and_records_output() {
        let (store, id) = store_with(TASK_PROMPT_VERIFY);
        let runner: Arc<dyn Runner> = Arc::new(FakeRunner {
            fail_on: Some("check it".to_string()),
        });
        execute_run(&store, runner.as_ref(), &id);
        let guard = store.lock().unwrap();
        assert_eq!(guard.run_state(&id).unwrap(), RunState::Failed);
        let run = guard.get_run(&id).unwrap();
        assert_eq!(run.tasks[0].verify[0].state, "failed");
    }

    #[test]
    fn brain_verify_kind_stays_pending_and_does_not_block_the_run() {
        // Regression: brain/approval verifiers are still deferred, unlike
        // prompt verifiers now that they're implemented.
        let (store, id) = store_with(TASK_BRAIN_VERIFY);
        let runner: Arc<dyn Runner> = Arc::new(FakeRunner { fail_on: None });
        execute_run(&store, runner.as_ref(), &id);
        let guard = store.lock().unwrap();
        assert_eq!(guard.run_state(&id).unwrap(), RunState::Done);
        let run = guard.get_run(&id).unwrap();
        assert_eq!(run.tasks[0].verify[0].state, "pending");
    }

    /// The `(agent, model)` of one captured verify-kind spec.
    type SeenAgentModel = (String, Option<String>);

    /// Captures the `(agent, model)` of every verify-kind spec it's asked to run.
    struct SpecCapturingRunner {
        seen: Arc<Mutex<Vec<SeenAgentModel>>>,
    }

    impl Runner for SpecCapturingRunner {
        fn run(&self, spec: &RunnerSpec) -> RunnerResult {
            if spec.verify {
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
                verified: spec.verify.then_some(true),
                claude_session_id: None,
            }
        }
    }

    #[test]
    fn prompt_verify_inherits_session_agent_and_model_by_default() {
        let (store, id) = store_with(TASK_PROMPT_VERIFY);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let runner: Arc<dyn Runner> = Arc::new(SpecCapturingRunner {
            seen: Arc::clone(&seen),
        });
        execute_run(&store, runner.as_ref(), &id);
        assert_eq!(
            *seen.lock().unwrap(),
            vec![("ollama".to_string(), Some("qwen3:8b".to_string()))]
        );
    }

    #[test]
    fn prompt_verify_own_model_overrides_session_model() {
        let (store, id) = store_with(TASK_PROMPT_VERIFY_MODEL_OVERRIDE);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let runner: Arc<dyn Runner> = Arc::new(SpecCapturingRunner {
            seen: Arc::clone(&seen),
        });
        execute_run(&store, runner.as_ref(), &id);
        assert_eq!(
            *seen.lock().unwrap(),
            vec![("ollama".to_string(), Some("qwen2:1b".to_string()))]
        );
    }

    // Two independent tasks; task "b" has a task-level prompt verify.
    const TWO_TASKS_B_HAS_PROMPT_VERIFY: &str = "[[task]]\nname=\"a\"\n[[task.session]]\ncwd=\".\"\ncommand=\"cmd-a\"\n\
         [[task]]\nname=\"b\"\n[[task.session]]\ncwd=\".\"\ncommand=\"cmd-b\"\nagent=\"ollama\"\nmodel=\"qwen3:8b\"\n\
         [[task.verify]]\nprompt=\"verify it\"\n";

    /// Snapshots the DB state of the task named in `spec.task` the moment a
    /// verify spec runs — lets the RAL-64 regression test assert the task is
    /// Running (not Done) while its verify is executing.
    struct VerifyStateCapturingRunner {
        store: Arc<Mutex<Store>>,
        captured: Arc<Mutex<Vec<String>>>,
    }

    impl Runner for VerifyStateCapturingRunner {
        fn run(&self, spec: &RunnerSpec) -> RunnerResult {
            if spec.verify {
                let guard = self.store.lock().unwrap();
                if let Ok(run) = guard.get_run(&spec.run_id) {
                    if let Some(task) = run.tasks.iter().find(|t| t.name == spec.task) {
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
                verified: spec.verify.then_some(true),
                claude_session_id: None,
            }
        }
    }

    #[test]
    fn task_is_running_not_done_while_verify_executes_on_partial_rerun() {
        // Regression for RAL-64: when a task's sessions are all already-Done from
        // a prior run (only a different task was restarted), the finalizer must
        // set that task to Running before re-executing its task-level verifies.
        // Without the fix the board shows task=Done + verify=Running.
        let (store, id) = store_with(TWO_TASKS_B_HAS_PROMPT_VERIFY);

        // First run: both tasks complete successfully.
        execute_run(&store, Arc::new(FakeRunner { fail_on: None }).as_ref(), &id);
        {
            let g = store.lock().unwrap();
            assert_eq!(g.run_state(&id).unwrap(), RunState::Done);
            let run = g.get_run(&id).unwrap();
            assert_eq!(run.tasks[1].state, "done");
            assert_eq!(run.tasks[1].verify[0].state, "done");
        }

        // Restart task "a" only (task_idx=0, session_idx=0).
        // Task "b" (task_idx=1) is intentionally NOT reset: its session stays Done
        // and its task state stays "done" — the RAL-64 scenario.
        store.lock().unwrap().restart_session(&id, 0, 0).unwrap();
        {
            let g = store.lock().unwrap();
            let run = g.get_run(&id).unwrap();
            assert_eq!(run.tasks[0].state, "pending", "task 'a' should be reset");
            assert_eq!(run.tasks[1].state, "done", "task 'b' should be unchanged");
        }

        // Re-run: task "b"'s session is already Done so only its task-level verify
        // fires.  The runner snapshots task "b"'s DB state when that verify runs.
        let captured: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        execute_run(
            &store,
            Arc::new(VerifyStateCapturingRunner {
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
            "task 'b' verify should run exactly once on re-run; got: {task_states:?}"
        );
        assert_eq!(
            task_states[0], "running",
            "task 'b' must be Running (not Done) while its verify executes (RAL-64)"
        );

        // Run ends Done.
        assert_eq!(
            store.lock().unwrap().run_state(&id).unwrap(),
            RunState::Done
        );
        let run = store.lock().unwrap().get_run(&id).unwrap();
        assert_eq!(run.tasks[1].state, "done");
    }

    // ── per-task review readiness ─────────────────────────────────────────

    // `guardian_blocking_tasks` calls `project_root_of` which runs git, so it
    // cannot be unit-tested against fake (non-git) paths. The functional
    // coverage is provided by the integration tests in
    // `daemon/tests/reviews_derive.rs`:
    //   - `non_overlapping_task_does_not_block_readiness`
    //   - `undeclared_overlapping_task_blocks_readiness`

    fn make_session_row_no_cwd(task_idx: i64) -> crate::store::SessionRow {
        crate::store::SessionRow {
            task_idx,
            idx: 0,
            task_name: format!("task{task_idx}"),
            session_id: format!("s{task_idx}"),
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
            upstream: None,
        }
    }

    #[test]
    fn guardian_blocking_tasks_session_with_no_cwd_skipped() {
        // A session without a cwd cannot be in any git project, so it must
        // never count as a blocker regardless of the guardian's git_root.
        let row = make_session_row_no_cwd(0);
        let blocking = guardian_blocking_tasks(&[row], "/any/project");
        assert!(blocking.is_empty(), "session with no cwd should not block");
    }

    // ── upstream rebase sentinel parsing ─────────────────────────────────────

    #[test]
    fn parse_upstream_task_ref_matches_simple_task() {
        assert_eq!(parse_upstream_task_ref("<<task:my-task>>"), Some("my-task"));
    }

    #[test]
    fn parse_upstream_task_ref_matches_task_session() {
        assert_eq!(
            parse_upstream_task_ref("<<task:my-task/session-1>>"),
            Some("my-task/session-1")
        );
    }

    #[test]
    fn parse_upstream_task_ref_none_for_plain_branch() {
        assert_eq!(parse_upstream_task_ref("main"), None);
        assert_eq!(parse_upstream_task_ref("<<upstream>>"), None);
        assert_eq!(parse_upstream_task_ref(""), None);
    }

    #[test]
    fn try_upstream_rebase_noop_when_upstream_is_none() {
        let row = make_session_row_no_cwd(0);
        // upstream is None — must return None without calling git.
        let result = try_upstream_rebase(&row, &[]);
        assert!(result.is_none(), "no upstream → no rebase attempt");
    }

    #[test]
    fn try_upstream_rebase_noop_when_dep_not_found() {
        let mut row = make_session_row_no_cwd(0);
        row.upstream = Some("<<task:nonexistent-task>>".to_string());
        // The referenced task doesn't exist in the session list; returns None
        // (can't rebase something that doesn't exist, graceful skip).
        let result = try_upstream_rebase(&row, &[]);
        assert!(result.is_none(), "missing dep → skip without error");
    }

    #[test]
    fn try_upstream_rebase_skip_when_b_has_no_cwd() {
        let mut row = make_session_row_no_cwd(1);
        row.upstream = Some("<<task:task0>>".to_string());
        let dep = make_session_row_no_cwd(0); // cwd is None
        // Neither has a cwd; parse_upstream_task_ref returns Some but the cwd
        // check returns None early — treated as skip (None return).
        let result = try_upstream_rebase(&row, &[dep]);
        assert!(result.is_none(), "no cwd on B → silent skip");
    }
}
