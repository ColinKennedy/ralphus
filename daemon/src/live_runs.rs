//! Live actions: the run queue, the one automatic run, and manual runs.
//!
//! Every execution of a live action (or of a user action whose `prompt` makes
//! it agent-run) is a **live run**, one row in `guardian_live_runs`. Runs of one
//! review execute one at a time, in queue order, on a worker thread that owns
//! the review's slot in [`crate::store_memory::StoreMemory`]; each run is
//! executed by [`crate::live_actions::execute_live_run`].
//!
//! A review opted into live actions (`live_actions`, resolved per review >
//! project > global > off) gets exactly one automatic run: the first time its
//! build has been prepared while every member cell has finished. That claim is
//! written once (`live_actions_auto_ran_at_ms`), so a restart, a rebase, a
//! feedback pass or an auto-fix never runs them again. After that, runs are
//! manual (Run all / Run). Live runs never wait for PR CI.

use std::sync::Arc;

use crate::cancel::CancelToken;
use crate::guardian::GuardianView;
use crate::live_actions::{LiveRunEnd, LiveRunRequest};
use crate::runner::Runner;

/// Builds the runner a live-run worker uses.
pub type LiveRunnerFactory =
    Arc<dyn Fn(&crate::store_lock::StoreHandle) -> Arc<dyn Runner> + Send + Sync>;

/// The production runner: the machine-routing subprocess runner.
#[must_use]
pub fn default_runner_factory() -> LiveRunnerFactory {
    Arc::new(|store: &crate::store_lock::StoreHandle| {
        Arc::new(crate::remote_runner::MachineRouter::from_env(Arc::clone(
            store,
        ))) as Arc<dyn Runner>
    })
}

/// Author name on the review messages a live run posts.
pub const LIVE_ACTION_AUTHOR: &str = "Live action";

/// What a run executes: one of the review's live actions, or a user action
/// (`[[review.action]]`) whose `prompt` makes it agent-run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiveRunKind {
    Live,
    Action,
}

impl LiveRunKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Action => "action",
        }
    }

    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "live" => Some(Self::Live),
            "action" => Some(Self::Action),
            _ => None,
        }
    }
}

/// One row of `guardian_live_runs`.
#[derive(Debug, Clone, serde::Serialize, PartialEq)]
pub struct LiveRunRow {
    pub run_id: String,
    pub guardian_id: String,
    pub kind: String,
    pub action_idx: i64,
    pub label: String,
    /// `auto` or `manual`.
    pub trigger: String,
    /// `queued`, `running`, then one of `finished`, `failed`, `cancelled`,
    /// `over_budget`, or `interrupted` (the daemon stopped mid-run).
    pub state: String,
    pub cwd: Option<String>,
    pub base_tip: Option<String>,
    pub live_ref: Option<String>,
    pub commits: Vec<String>,
    pub report: Option<String>,
    pub error: Option<String>,
    pub cost_usd: Option<f64>,
    /// The cell id the agent's session ran under (Live View / transcript key).
    pub cell_id: Option<String>,
    /// What happened to the run's commits: `none` (nothing to carry),
    /// `pending`, `integrated`, `failed` or `skipped` -- see
    /// [`Self::integration_detail`].
    pub integration: String,
    pub integration_detail: Option<String>,
    pub queued_at_ms: i64,
    pub started_at_ms: Option<i64>,
    pub finished_at_ms: Option<i64>,
}

const RUN_COLUMNS: &str = "run_id, guardian_id, kind, action_idx, label, trigger, state, cwd, \
     base_tip, live_ref, commits, report, error, cost_usd, cell_id, integration, \
     integration_detail, queued_at_ms, started_at_ms, finished_at_ms";

fn run_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<LiveRunRow> {
    let commits: Option<String> = r.get(10)?;
    Ok(LiveRunRow {
        run_id: r.get(0)?,
        guardian_id: r.get(1)?,
        kind: r.get(2)?,
        action_idx: r.get(3)?,
        label: r.get(4)?,
        trigger: r.get(5)?,
        state: r.get(6)?,
        cwd: r.get(7)?,
        base_tip: r.get(8)?,
        live_ref: r.get(9)?,
        commits: commits
            .and_then(|c| serde_json::from_str(&c).ok())
            .unwrap_or_default(),
        report: r.get(11)?,
        error: r.get(12)?,
        cost_usd: r.get(13)?,
        cell_id: r.get(14)?,
        integration: r.get(15)?,
        integration_detail: r.get(16)?,
        queued_at_ms: r.get(17)?,
        started_at_ms: r.get(18)?,
        finished_at_ms: r.get(19)?,
    })
}

/// A review's live runs, newest first.
///
/// # Errors
///
/// On a database failure.
pub fn guardian_live_runs_conn(
    conn: &rusqlite::Connection,
    guardian_id: &str,
) -> crate::store::Result<Vec<LiveRunRow>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {RUN_COLUMNS} FROM guardian_live_runs WHERE guardian_id=?
         ORDER BY queued_at_ms DESC, run_id DESC LIMIT 200"
    ))?;
    let rows = stmt.query_map(rusqlite::params![guardian_id], run_from_row)?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

/// One live run.
///
/// # Errors
///
/// [`crate::store::StoreError::NotFound`] when there is no such run.
pub fn live_run_conn(
    conn: &rusqlite::Connection,
    run_id: &str,
) -> crate::store::Result<LiveRunRow> {
    use rusqlite::OptionalExtension as _;
    conn.query_row(
        &format!("SELECT {RUN_COLUMNS} FROM guardian_live_runs WHERE run_id=?"),
        rusqlite::params![run_id],
        run_from_row,
    )
    .optional()?
    .ok_or(crate::store::StoreError::NotFound)
}

/// Where a review's live runs execute: the checkout its latest preparation
/// produced, and that preparation's failure detail when it failed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LiveCheckout {
    pub path: Option<String>,
    pub preparation_failure: Option<String>,
}

fn new_run_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    format!(
        "lr{}-{}",
        crate::store::now_ms(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    )
}

impl crate::store::Store {
    /// One live run.
    ///
    /// # Errors
    ///
    /// [`crate::store::StoreError::NotFound`] when there is no such run.
    pub fn live_run(&self, run_id: &str) -> crate::store::Result<LiveRunRow> {
        live_run_conn(&self.conn, run_id)
    }

    /// Queue one live run. Returns its id.
    ///
    /// # Errors
    ///
    /// On a database failure.
    pub fn queue_live_run(
        &self,
        guardian_id: &str,
        kind: LiveRunKind,
        action_idx: i64,
        label: &str,
        trigger: &str,
    ) -> crate::store::Result<String> {
        let run_id = new_run_id();
        self.conn.execute(
            "INSERT INTO guardian_live_runs
                (run_id, guardian_id, kind, action_idx, label, trigger, state, integration, queued_at_ms)
             VALUES (?, ?, ?, ?, ?, ?, 'queued', 'none', ?)",
            rusqlite::params![
                run_id,
                guardian_id,
                kind.as_str(),
                action_idx,
                label,
                trigger,
                crate::store::now_ms()
            ],
        )?;
        Ok(run_id)
    }

    /// The oldest queued run of a review, if any.
    ///
    /// # Errors
    ///
    /// On a database failure.
    pub fn next_queued_live_run(
        &self,
        guardian_id: &str,
    ) -> crate::store::Result<Option<LiveRunRow>> {
        use rusqlite::OptionalExtension as _;
        Ok(self
            .conn
            .query_row(
                &format!(
                    "SELECT {RUN_COLUMNS} FROM guardian_live_runs
                     WHERE guardian_id=? AND state='queued'
                     ORDER BY queued_at_ms, run_id LIMIT 1"
                ),
                rusqlite::params![guardian_id],
                run_from_row,
            )
            .optional()?)
    }

    /// Mark a queued run as running.
    ///
    /// # Errors
    ///
    /// On a database failure.
    pub fn start_live_run(
        &self,
        run_id: &str,
        cwd: &str,
        cell_id: &str,
    ) -> crate::store::Result<()> {
        self.conn.execute(
            "UPDATE guardian_live_runs SET state='running', cwd=?, cell_id=?, started_at_ms=?
             WHERE run_id=?",
            rusqlite::params![cwd, cell_id, crate::store::now_ms(), run_id],
        )?;
        Ok(())
    }

    /// Record how a run ended.
    ///
    /// # Errors
    ///
    /// On a database failure.
    pub fn finish_live_run(
        &self,
        run_id: &str,
        outcome: &crate::live_actions::LiveRunOutcome,
        integration: &str,
    ) -> crate::store::Result<()> {
        self.conn.execute(
            "UPDATE guardian_live_runs SET state=?, base_tip=?, live_ref=?, commits=?, report=?,
                 error=?, cost_usd=?, cell_id=?, integration=?, finished_at_ms=?
             WHERE run_id=?",
            rusqlite::params![
                outcome.end.as_str(),
                outcome.base_tip,
                outcome.live_ref,
                serde_json::to_string(&outcome.commits).unwrap_or_else(|_| "[]".into()),
                outcome.report,
                outcome.error,
                outcome.cost_usd,
                outcome.cell_id,
                integration,
                crate::store::now_ms(),
                run_id
            ],
        )?;
        Ok(())
    }

    /// Fail a queued run that could not start (no checkout, no such action).
    ///
    /// # Errors
    ///
    /// On a database failure.
    pub fn fail_queued_live_run(&self, run_id: &str, error: &str) -> crate::store::Result<()> {
        self.conn.execute(
            "UPDATE guardian_live_runs SET state='failed', error=?, finished_at_ms=?
             WHERE run_id=? AND state='queued'",
            rusqlite::params![error, crate::store::now_ms(), run_id],
        )?;
        Ok(())
    }

    /// Cancel a run that is still queued. `true` when it was.
    ///
    /// # Errors
    ///
    /// On a database failure.
    pub fn cancel_queued_live_run(&self, run_id: &str) -> crate::store::Result<bool> {
        let n = self.conn.execute(
            "UPDATE guardian_live_runs SET state='cancelled', finished_at_ms=?
             WHERE run_id=? AND state='queued'",
            rusqlite::params![crate::store::now_ms(), run_id],
        )?;
        Ok(n == 1)
    }

    /// Forget a run's live ref once it has been deleted.
    ///
    /// # Errors
    ///
    /// On a database failure.
    pub fn clear_live_run_ref(&self, run_id: &str) -> crate::store::Result<()> {
        self.conn.execute(
            "UPDATE guardian_live_runs SET live_ref=NULL WHERE run_id=?",
            rusqlite::params![run_id],
        )?;
        Ok(())
    }

    /// Record what happened to a run's commits.
    ///
    /// # Errors
    ///
    /// On a database failure.
    pub fn set_live_run_integration(
        &self,
        run_id: &str,
        integration: &str,
        detail: Option<&str>,
    ) -> crate::store::Result<()> {
        self.conn.execute(
            "UPDATE guardian_live_runs SET integration=?, integration_detail=? WHERE run_id=?",
            rusqlite::params![integration, detail, run_id],
        )?;
        Ok(())
    }

    /// At daemon start: no run can still be executing in this process, so a
    /// row left `running` was interrupted. Returns how many were.
    ///
    /// # Errors
    ///
    /// On a database failure.
    pub fn interrupt_running_live_runs(&self) -> crate::store::Result<usize> {
        Ok(self.conn.execute(
            "UPDATE guardian_live_runs SET state='interrupted', finished_at_ms=?,
                 error=COALESCE(error, 'the daemon stopped while this run was going; rerun it')
             WHERE state='running'",
            rusqlite::params![crate::store::now_ms()],
        )?)
    }

    /// Record the session cell of the agent generating a review's live actions
    /// (the deduplication pass), so its session can be followed and read back.
    ///
    /// # Errors
    ///
    /// On a database failure.
    pub fn set_live_actions_generation_cell(
        &self,
        guardian_id: &str,
        cell_id: &str,
    ) -> crate::store::Result<()> {
        self.conn.execute(
            "UPDATE guardians SET live_actions_generation_cell=? WHERE id=?",
            rusqlite::params![cell_id, guardian_id],
        )?;
        Ok(())
    }

    /// Record the checkout a review's latest preparation produced.
    ///
    /// # Errors
    ///
    /// On a database failure.
    pub fn set_live_checkout(
        &self,
        guardian_id: &str,
        path: &str,
        preparation_failure: Option<&str>,
    ) -> crate::store::Result<()> {
        self.conn.execute(
            "UPDATE guardians SET live_actions_checkout=?, live_actions_checkout_note=? WHERE id=?",
            rusqlite::params![path, preparation_failure, guardian_id],
        )?;
        Ok(())
    }

    /// Where a review's live runs execute.
    ///
    /// # Errors
    ///
    /// On a database failure.
    pub fn live_checkout(&self, guardian_id: &str) -> crate::store::Result<LiveCheckout> {
        live_checkout_conn(&self.conn, guardian_id)
    }

    /// Reviews that have a prepared checkout but have not had their automatic
    /// live-action run: the candidates [`sweep_automatic_runs`] checks.
    ///
    /// # Errors
    ///
    /// On a database failure.
    pub fn live_action_auto_run_candidates(&self) -> crate::store::Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT id FROM guardians
             WHERE live_actions_checkout IS NOT NULL AND live_actions_auto_ran_at_ms IS NULL
               AND status NOT IN ('deployed', 'cancelled')",
        )?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }
}

/// The session cell of the agent that generated a review's live actions, if one
/// ran (a single handoff needs no generation agent).
///
/// # Errors
///
/// On a database failure.
pub fn live_actions_generation_cell_conn(
    conn: &rusqlite::Connection,
    guardian_id: &str,
) -> crate::store::Result<Option<String>> {
    use rusqlite::OptionalExtension as _;
    Ok(conn
        .query_row(
            "SELECT live_actions_generation_cell FROM guardians WHERE id=?",
            rusqlite::params![guardian_id],
            |r| r.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten())
}

/// [`crate::store::Store::live_checkout`] over a bare connection.
///
/// # Errors
///
/// On a database failure.
pub fn live_checkout_conn(
    conn: &rusqlite::Connection,
    guardian_id: &str,
) -> crate::store::Result<LiveCheckout> {
    use rusqlite::OptionalExtension as _;
    Ok(conn
        .query_row(
            "SELECT live_actions_checkout, live_actions_checkout_note FROM guardians WHERE id=?",
            rusqlite::params![guardian_id],
            |r| {
                Ok(LiveCheckout {
                    path: r.get(0)?,
                    preparation_failure: r.get(1)?,
                })
            },
        )
        .optional()?
        .unwrap_or_default())
}

/// Review states in which nothing more is run automatically.
fn review_is_settled_out(g: &GuardianView) -> bool {
    matches!(
        g.status.as_str(),
        "merge_stopped" | "deployed" | "cancelled"
    )
}

/// Why the automatic run did not start (for logs and tests).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AutoRunDecision {
    /// Every condition holds; the run may start.
    Due,
    /// This call claimed and started the run.
    Started,
    NotOptedIn,
    AlreadyRan,
    ReviewStopped,
    NoCheckout,
    NoMemberCells,
    CellsStillRunning,
    OverBudget,
}

/// Start review `guardian_id`'s one automatic live-action run when it is due:
/// opted in, never run, a prepared checkout exists, the review is not stopped
/// or finished, it has member cells and every one has finished, and its budget
/// is not spent. Claims the run (write-once) before doing anything else, then
/// deduplicates the handoffs and queues every resulting live action on a
/// worker thread.
pub fn maybe_start_automatic_run(
    store: &crate::store_lock::StoreHandle,
    runners: &LiveRunnerFactory,
    guardian_id: &str,
) -> AutoRunDecision {
    let decision = automatic_run_decision(store, guardian_id);
    if decision != AutoRunDecision::Due {
        if decision == AutoRunDecision::OverBudget {
            notify_over_budget(store, guardian_id);
        }
        return decision;
    }
    if !store
        .lock()
        .claim_guardian_live_actions_auto_run(guardian_id)
        .unwrap_or(false)
    {
        return AutoRunDecision::AlreadyRan;
    }
    crate::cartographer::Note::new("guardian")
        .guardian(guardian_id)
        .scope("live_action")
        .emit(
            &store.lock(),
            "live actions automatic run claimed",
            serde_json::json!({}),
        );
    let store = Arc::clone(store);
    let runners = Arc::clone(runners);
    let gid = guardian_id.to_string();
    std::thread::Builder::new()
        .name(format!("live-actions-{gid}"))
        .spawn(move || {
            let count = generate_and_queue_all(&store, &runners, &gid, "auto");
            crate::rlog!(
                INFO,
                "ralphus [guardian] review {gid} automatic live-action run queued {count} action(s)"
            );
        })
        .ok();
    AutoRunDecision::Started
}

/// The checks behind [`maybe_start_automatic_run`], without side effects.
#[must_use]
pub fn automatic_run_decision(
    store: &crate::store_lock::StoreHandle,
    guardian_id: &str,
) -> AutoRunDecision {
    let guard = store.lock();
    let Ok(g) = guard.get_guardian(guardian_id) else {
        return AutoRunDecision::ReviewStopped;
    };
    if !g.effective_live_actions {
        return AutoRunDecision::NotOptedIn;
    }
    if g.live_actions_auto_ran_at_ms.is_some() {
        return AutoRunDecision::AlreadyRan;
    }
    if review_is_settled_out(&g) {
        return AutoRunDecision::ReviewStopped;
    }
    if guard
        .live_checkout(guardian_id)
        .map(|c| c.path.is_none())
        .unwrap_or(true)
    {
        return AutoRunDecision::NoCheckout;
    }
    let members =
        crate::live_actions::review_member_cells_conn(&guard.conn, guardian_id).unwrap_or_default();
    if members.is_empty() {
        return AutoRunDecision::NoMemberCells;
    }
    if !members
        .iter()
        .all(crate::live_actions::MemberCell::is_finished)
    {
        return AutoRunDecision::CellsStillRunning;
    }
    if over_budget(&guard, guardian_id) {
        return AutoRunDecision::OverBudget;
    }
    AutoRunDecision::Due
}

fn over_budget(store: &crate::store::Store, guardian_id: &str) -> bool {
    let Ok(Some(cap)) = store.guardian_maximum_budget_usd(guardian_id) else {
        return false;
    };
    store
        .guardian_cost_total(guardian_id)
        .is_ok_and(|(_, _, spent)| spent >= cap)
}

fn notify_over_budget(store: &crate::store_lock::StoreHandle, guardian_id: &str) {
    let guard = store.lock();
    let _ = guard.enqueue_error_mailbox_message(
        crate::mailbox::MailboxPriority::Normal,
        &format!(
            "Review {guardian_id}'s live actions did not run: the review has spent its maximum_budget_usd cap."
        ),
        &crate::mailbox::Remediation::ManualInterventionRequired {
            guidance: format!(
                "raise the cap with `ralphus review settings {guardian_id}` (or the review's \
                 settings on the board); the live actions then run on the next maintenance pass"
            ),
        },
        None,
        None,
        None,
        Some(&format!("guardian:{guardian_id}")),
        Some("live_actions"),
    );
}

/// Make sure the review has its live actions (collecting and deduplicating
/// the handoffs the first time), then queue a run of every one. Returns how
/// many were queued. Blocks for the dedupe agent call, so callers run it on a
/// worker thread.
pub fn generate_and_queue_all(
    store: &crate::store_lock::StoreHandle,
    runners: &LiveRunnerFactory,
    guardian_id: &str,
    trigger: &str,
) -> usize {
    let actions = ensure_live_actions(store, runners, guardian_id);
    let mut queued = 0;
    for action in &actions {
        if store
            .lock()
            .queue_live_run(
                guardian_id,
                LiveRunKind::Live,
                action.idx,
                &action.label,
                trigger,
            )
            .is_ok()
        {
            queued += 1;
        }
    }
    if queued > 0 {
        start_worker(store, runners, guardian_id);
    }
    queued
}

/// The review's live actions, generating them from its member cells'
/// handoffs when it has none yet. The set is written once and then fixed.
pub fn ensure_live_actions(
    store: &crate::store_lock::StoreHandle,
    runners: &LiveRunnerFactory,
    guardian_id: &str,
) -> Vec<crate::live_actions::LiveAction> {
    let existing = store
        .lock()
        .guardian_live_actions(guardian_id)
        .unwrap_or_default();
    if !existing.is_empty() {
        return existing;
    }
    let (guardian, handoffs, checkout) = {
        let guard = store.lock();
        let Ok(g) = guard.get_guardian(guardian_id) else {
            return Vec::new();
        };
        let handoffs = crate::live_actions::collect_review_handoffs_conn(&guard.conn, guardian_id)
            .unwrap_or_default();
        let checkout = guard.live_checkout(guardian_id).unwrap_or_default();
        (g, handoffs, checkout)
    };
    if handoffs.is_empty() {
        crate::cartographer::Note::new("guardian")
            .guardian(guardian_id)
            .scope("live_action")
            .emit(
                &store.lock(),
                "no live handoffs to run",
                serde_json::json!({}),
            );
        return Vec::new();
    }
    let cwd = checkout
        .path
        .clone()
        .or_else(|| guardian.combined_worktree.clone())
        .unwrap_or_else(|| guardian.git_root.clone());
    let runner = runners(store);
    let new_actions = crate::live_actions::deduplicate_handoffs(
        store,
        runner.as_ref(),
        &guardian,
        &cwd,
        &handoffs,
        &CancelToken::never(),
    );
    let guard = store.lock();
    let _ = guard.insert_guardian_live_actions(guardian_id, &new_actions);
    guard.guardian_live_actions(guardian_id).unwrap_or_default()
}

/// Start draining a review's queue on a worker thread, unless one already is.
pub fn start_worker(
    store: &crate::store_lock::StoreHandle,
    runners: &LiveRunnerFactory,
    guardian_id: &str,
) {
    let memory = store.lock_free_memory();
    let Some(claim) = memory.try_claim_live_run_worker(guardian_id) else {
        return;
    };
    let store = Arc::clone(store);
    let runners = Arc::clone(runners);
    let gid = guardian_id.to_string();
    std::thread::Builder::new()
        .name(format!("live-runs-{gid}"))
        .spawn(move || {
            drain_queue(&store, &runners, &gid, claim);
        })
        .ok();
}

/// Run every queued run of the review, one at a time. Re-checks the queue
/// after giving up the slot, so a run queued in that instant is not stranded.
pub fn drain_queue(
    store: &crate::store_lock::StoreHandle,
    runners: &LiveRunnerFactory,
    guardian_id: &str,
    claim: crate::store_memory::LiveRunWorkerClaim,
) {
    let mut claim = Some(claim);
    loop {
        // Commits are carried only once the queue is empty: carrying one
        // run's fix rebuilds the review, which would move the checkout out
        // from under the next queued run. Every queued run tests the same tip.
        let mut to_integrate = Vec::new();
        loop {
            // Bound first: a `while let` scrutinee would keep the store lock
            // held for the whole run.
            let next = store.lock().next_queued_live_run(guardian_id);
            let Ok(Some(run)) = next else {
                break;
            };
            if execute_queued_run(store, runners, &run) {
                to_integrate.push(run.run_id.clone());
            }
        }
        for run_id in to_integrate {
            let runner = runners(store);
            integrate_run(store, runner.as_ref(), &run_id);
        }
        drop(claim.take());
        let more = store
            .lock()
            .next_queued_live_run(guardian_id)
            .ok()
            .flatten()
            .is_some();
        if !more {
            return;
        }
        match store
            .lock_free_memory()
            .try_claim_live_run_worker(guardian_id)
        {
            Some(again) => claim = Some(again),
            // Another worker took over.
            None => return,
        }
    }
}

/// What a queued run should execute.
struct RunTarget {
    label: String,
    prompt: String,
    allow_commits: bool,
}

fn resolve_target(
    store: &crate::store::Store,
    g: &GuardianView,
    run: &LiveRunRow,
) -> Result<RunTarget, String> {
    let idx = usize::try_from(run.action_idx).map_err(|_| "bad action index".to_string())?;
    match LiveRunKind::parse(&run.kind) {
        Some(LiveRunKind::Live) => {
            let actions = store
                .guardian_live_actions(&g.id)
                .map_err(|e| e.to_string())?;
            let action = actions
                .into_iter()
                .find(|a| a.idx == run.action_idx)
                .ok_or_else(|| format!("review has no live action {idx}"))?;
            Ok(RunTarget {
                label: action.label,
                prompt: action.prompt,
                allow_commits: true,
            })
        }
        Some(LiveRunKind::Action) => {
            let hint = g
                .action_hints
                .get(idx)
                .ok_or_else(|| format!("review has no action {idx}"))?;
            let prompt = hint
                .prompt
                .clone()
                .filter(|p| !p.trim().is_empty())
                .ok_or_else(|| format!("action {idx} has no prompt"))?;
            Ok(RunTarget {
                label: hint
                    .label
                    .clone()
                    .unwrap_or_else(|| format!("action {idx}")),
                prompt,
                allow_commits: true,
            })
        }
        None => Err(format!("unknown live-run kind {:?}", run.kind)),
    }
}

/// Execute one queued run to completion and record it. `true` when its
/// commits are waiting to be carried into the review.
fn execute_queued_run(
    store: &crate::store_lock::StoreHandle,
    runners: &LiveRunnerFactory,
    run: &LiveRunRow,
) -> bool {
    let (guardian, target, checkout) = {
        let guard = store.lock();
        let Ok(g) = guard.get_guardian(&run.guardian_id) else {
            let _ = guard.fail_queued_live_run(&run.run_id, "the review no longer exists");
            return false;
        };
        let target = match resolve_target(&guard, &g, run) {
            Ok(t) => t,
            Err(e) => {
                let _ = guard.fail_queued_live_run(&run.run_id, &e);
                return false;
            }
        };
        let checkout = guard.live_checkout(&g.id).unwrap_or_default();
        (g, target, checkout)
    };
    let cwd = match checkout.path.clone().or_else(|| {
        // A user prompt action may run before the review records a live
        // checkout: use the directory it was prepared in.
        usize::try_from(run.action_idx)
            .ok()
            .and_then(|i| guardian.action_hints.get(i))
            .and_then(|h| h.prepared_cwd.clone())
    }) {
        Some(cwd) => cwd,
        None => {
            let _ = store.lock().fail_queued_live_run(
                &run.run_id,
                "the review's build has not been prepared yet; run it after preparation finishes",
            );
            return false;
        }
    };
    let prompt = match &checkout.preparation_failure {
        Some(failure) => format!(
            "{}\n\nNote: ralphus's preparation of this checkout reported a failure, so the build \
             may be incomplete -- that may be the first thing to look at:\n{failure}",
            target.prompt
        ),
        None => target.prompt.clone(),
    };
    let action_index = usize::try_from(run.action_idx).unwrap_or(0);
    let cell_id = crate::live_actions::live_run_cell_id(action_index, &run.run_id);
    let _ = store.lock().start_live_run(&run.run_id, &cwd, &cell_id);
    let cancel = CancelToken::new();
    let memory = store.lock_free_memory();
    memory.register_live_run_cancel(&run.run_id, cancel.clone());
    let runner = runners(store);
    let outcome = crate::live_actions::execute_live_run(
        store,
        runner.as_ref(),
        &LiveRunRequest {
            guardian: &guardian,
            run_id: &run.run_id,
            action_index,
            label: &target.label,
            prompt: &prompt,
            cwd: &cwd,
            allow_commits: target.allow_commits,
        },
        &cancel,
    );
    memory.clear_live_run_cancel(&run.run_id);
    let integration = if outcome.commits.is_empty() || !target.allow_commits {
        "none"
    } else {
        "pending"
    };
    let _ = store
        .lock()
        .finish_live_run(&run.run_id, &outcome, integration);
    post_report(store, &guardian, &target.label, &outcome);
    integration == "pending"
}

/// Most patch text handed to the resolver when a live run's commits do not
/// cherry-pick cleanly.
const INTEGRATION_PATCH_CHARS: usize = 60_000;

/// The topmost enabled branch with a worktree: where a live run's commits
/// land.
fn top_branch(g: &GuardianView) -> Option<&crate::guardian::BranchView> {
    g.branches
        .iter()
        .filter(|b| b.enabled && b.worktree.is_some())
        .max_by_key(|b| b.position)
}

/// Whether every commit between `base` and `live_ref` has an equivalent
/// (same patch-id) on `head` -- i.e. the live run's change really landed.
fn commits_landed(
    ws: &crate::workspace::Workspace,
    head: &str,
    live_ref: &str,
    base: &str,
) -> bool {
    ws.git(&["cherry", head, live_ref, base])
        .map(|out| out.lines().all(|l| !l.trim_start().starts_with('+')))
        .unwrap_or(false)
}

/// Carry a finished live run's commits onto the review's top branch, through
/// the same pass feedback uses (lease, proof gate, PR push, restack, rebuild):
/// a clean cherry-pick when they apply, otherwise the review's resolver
/// applies the patch. The live ref is deleted only once its commits are
/// confirmed on the branch; when they cannot be carried, the ref is kept and
/// the review's watchers get a notice with what to do.
pub fn integrate_run(store: &crate::store_lock::StoreHandle, runner: &dyn Runner, run_id: &str) {
    let loaded = {
        let guard = store.lock();
        let run = live_run_conn(&guard.conn, run_id);
        let g = run
            .as_ref()
            .ok()
            .map(|r| guard.get_guardian(&r.guardian_id));
        (run, g)
    };
    let (Ok(run), Some(Ok(g))) = loaded else {
        return;
    };
    let (Some(live_ref), Some(base)) = (run.live_ref.clone(), run.base_tip.clone()) else {
        let _ = store
            .lock()
            .set_live_run_integration(run_id, "none", Some("no commits to carry"));
        return;
    };
    if g.status.as_str() == "merge_stopped" {
        // Picked up again by the maintenance sweep once the review resumes.
        let _ = store.lock().set_live_run_integration(
            run_id,
            "pending",
            Some("waiting: the review is stopped; its commits are carried when it resumes"),
        );
        return;
    }
    if matches!(g.status.as_str(), "deployed" | "cancelled") {
        let _ = store.lock().set_live_run_integration(
            run_id,
            "skipped",
            Some("the review finished before this run's commits could be carried"),
        );
        return;
    }
    let Some(branch) = top_branch(&g) else {
        fail_integration(
            store,
            &g,
            &run,
            "the review has no enabled branch with a worktree to carry the commits onto",
        );
        return;
    };
    let branch_id = branch.id.clone();
    let branch_name = branch.branch.clone();
    let wt_path = branch.worktree.clone().unwrap_or_default();
    let ws = crate::workspace::Workspace::for_guardian(store, &g.id, wt_path.as_str());
    let current_head = ws
        .git(&["rev-parse", "HEAD"])
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    if !current_head.is_empty() && commits_landed(&ws, &current_head, &live_ref, &base) {
        // Feedback or an auto-fix already made the same change.
        let _ = ws.git(&["update-ref", "-d", &live_ref]);
        let _ = store.lock().clear_live_run_ref(run_id);
        let _ = store.lock().set_live_run_integration(
            run_id,
            "integrated",
            Some("already on the top branch (the same change landed another way)"),
        );
        return;
    }
    let patch = ws
        .git(&["format-patch", "--stdout", &format!("{base}..{live_ref}")])
        .unwrap_or_default();
    let patch: String = patch.chars().take(INTEGRATION_PATCH_CHARS).collect();
    let feedback = format!(
        "A live action (\"{label}\") tested this review's tip live and committed a fix while \
         doing so. Apply the same change to this branch -- it was made against an older tip, so \
         adapt it to the current code rather than discarding it. Keep its intent; do not widen \
         it.\n\nWhat the live test found:\n{report}\n\nThe change, as a patch:\n{patch}",
        label = run.label,
        report = run.report.as_deref().unwrap_or("(no report)"),
    );
    let _ = store.lock().set_live_run_integration(
        run_id,
        "integrating",
        Some(&format!("onto {branch_name}")),
    );
    let outcome = crate::guardian_merge::run_live_action_integration(
        store,
        runner,
        &g.id,
        &branch_id,
        &feedback,
        &run.commits,
        &CancelToken::never(),
    );
    if !outcome.committed {
        fail_integration(
            store,
            &g,
            &run,
            "the commits could not be applied to the top branch (the cherry-pick conflicted and \
             the resolver made no change)",
        );
        return;
    }
    let head = ws
        .git(&["rev-parse", "HEAD"])
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    let exact = commits_landed(&ws, &head, &live_ref, &base);
    let detail = match (exact, outcome.pushed) {
        (true, true) => "cherry-picked onto the top branch and pushed".to_string(),
        (true, false) => "cherry-picked onto the top branch (not pushed: no PR yet)".to_string(),
        (false, _) => "applied by the review's resolver after a conflict".to_string(),
    };
    if exact {
        // Confirmed on the branch: the live ref has done its job.
        let _ = ws.git(&["update-ref", "-d", &live_ref]);
        let _ = store.lock().clear_live_run_ref(run_id);
    }
    let _ = store
        .lock()
        .set_live_run_integration(run_id, "integrated", Some(&detail));
    crate::cartographer::Note::new("guardian")
        .guardian(&g.id)
        .scope("live_action")
        .emit(
            &store.lock(),
            "live run integrated",
            serde_json::json!({
                "run_id": run_id,
                "branch_id": branch_id,
                "commits": run.commits,
                "new_head": head,
                "exact": exact,
                "pushed": outcome.pushed,
                "live_ref_deleted": exact,
            }),
        );
    let _ = store.lock().add_guardian_message(
        &g.id,
        "guardian",
        &format!(
            "Live action \"{}\": its {} commit(s) were {detail}.",
            run.label,
            run.commits.len()
        ),
        None,
        Some(&branch_id),
        Some(LIVE_ACTION_AUTHOR),
        None,
    );
}

fn fail_integration(
    store: &crate::store_lock::StoreHandle,
    g: &GuardianView,
    run: &LiveRunRow,
    reason: &str,
) {
    let guard = store.lock();
    let _ = guard.set_live_run_integration(&run.run_id, "failed", Some(reason));
    let live_ref = run.live_ref.as_deref().unwrap_or("(none)");
    let _ = guard.enqueue_error_mailbox_message(
        crate::mailbox::MailboxPriority::High,
        &format!(
            "Live action \"{}\" on review {} committed a fix that could not be carried into the \
             review: {reason}",
            run.label, g.id
        ),
        &crate::mailbox::Remediation::ManualInterventionRequired {
            guidance: format!(
                "the commits are kept on {live_ref} in the review's repository; inspect them with \
                 `git log {live_ref}` and cherry-pick or re-apply them onto the review's top \
                 branch by hand, or send them as feedback with `ralphus review feedback {}`",
                g.id
            ),
        },
        None,
        None,
        None,
        Some(&format!("guardian:{}", g.id)),
        Some("live_actions"),
    );
}

/// Retry integrations left pending (the review was stopped when the run
/// finished, or the daemon restarted mid-integration).
pub fn retry_pending_integrations(
    store: &crate::store_lock::StoreHandle,
    runners: &LiveRunnerFactory,
) {
    let pending: Vec<(String, String)> = {
        let guard = store.lock();
        let Ok(mut stmt) = guard.conn.prepare(
            "SELECT r.run_id, r.guardian_id FROM guardian_live_runs r
             JOIN guardians g ON g.id = r.guardian_id
             WHERE r.integration IN ('pending', 'integrating') AND g.status != 'merge_stopped'",
        ) else {
            return;
        };
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .map(|rows| rows.flatten().collect())
            .unwrap_or_default()
    };
    for (run_id, gid) in pending {
        if store.lock_free_memory().has_live_run_worker(&gid) {
            // That review's worker integrates its own runs.
            continue;
        }
        let runner = runners(store);
        integrate_run(store, runner.as_ref(), &run_id);
    }
}

/// Post the run's report into the review's conversation on its top branch,
/// so what the live test found is part of the review, not only a log row.
fn post_report(
    store: &crate::store_lock::StoreHandle,
    guardian: &GuardianView,
    label: &str,
    outcome: &crate::live_actions::LiveRunOutcome,
) {
    let top = guardian
        .branches
        .iter()
        .filter(|b| b.enabled && b.worktree.is_some())
        .max_by_key(|b| b.position)
        .map(|b| b.id.clone());
    let status = match outcome.end {
        LiveRunEnd::Finished => "finished",
        LiveRunEnd::Failed => "failed",
        LiveRunEnd::Cancelled => "was cancelled",
        LiveRunEnd::OverBudget => "did not run (budget reached)",
    };
    let mut text = format!("Live action \"{label}\" {status}");
    if !outcome.commits.is_empty() {
        text.push_str(&format!(
            " with {} commit(s); ralphus carries them onto the top branch",
            outcome.commits.len()
        ));
    }
    text.push('.');
    if !outcome.report.is_empty() {
        text.push_str("\n\n");
        text.push_str(&outcome.report);
    }
    if let Some(error) = &outcome.error {
        if !error.is_empty() {
            text.push_str(&format!("\n\nError: {error}"));
        }
    }
    let _ = store.lock().add_guardian_message(
        &guardian.id,
        "guardian",
        &text,
        None,
        top.as_deref(),
        Some(LIVE_ACTION_AUTHOR),
        None,
    );
}

/// Called when a review's preparation finishes (successfully or not): record
/// the checkout live runs execute in, then start the automatic run if it is
/// due.
pub fn on_preparation_finished(
    store: &crate::store_lock::StoreHandle,
    runners: &LiveRunnerFactory,
    guardian_id: &str,
    checkout: &str,
    preparation_failure: Option<&str>,
) {
    let _ = store
        .lock()
        .set_live_checkout(guardian_id, checkout, preparation_failure);
    let decision = maybe_start_automatic_run(store, runners, guardian_id);
    crate::rlog!(
        DEBUG,
        "ralphus [guardian] review {guardian_id} live actions after preparation: {decision:?}"
    );
}

/// Periodic check for reviews whose automatic run became due without a new
/// preparation -- the last member cell finished after the stack had already
/// settled, the opt-in was turned on later, or the budget cap was raised.
pub fn sweep_automatic_runs(store: &crate::store_lock::StoreHandle, runners: &LiveRunnerFactory) {
    let candidates = store
        .lock()
        .live_action_auto_run_candidates()
        .unwrap_or_default();
    for gid in candidates {
        let _ = maybe_start_automatic_run(store, runners, &gid);
    }
    retry_pending_integrations(store, runners);
}

/// Daily cleanup of live refs (called with the worktree retirement sweep).
///
/// For a review that has finished (`deployed`, `cancelled`, `merge_failed`):
/// a live ref whose commits were carried (or that never had any) is deleted.
/// One whose commits never landed is kept -- it may be the only copy of a fix
/// -- and the review's watchers are told once, with what to do.
///
/// Live refs of a review that no longer exists at all are deleted: deleting
/// the review discarded its work.
pub fn retire_live_refs(store: &crate::store_lock::StoreHandle) {
    let finished: Vec<(String, String, String, String, String)> = {
        let guard = store.lock();
        let Ok(mut stmt) = guard.conn.prepare(
            "SELECT r.run_id, r.live_ref, r.integration, g.id, g.git_root
             FROM guardian_live_runs r JOIN guardians g ON g.id = r.guardian_id
             WHERE r.live_ref IS NOT NULL
               AND g.status IN ('deployed', 'cancelled', 'merge_failed')",
        ) else {
            return;
        };
        stmt.query_map([], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })
        .map(|rows| rows.flatten().collect())
        .unwrap_or_default()
    };
    for (run_id, live_ref, integration, gid, git_root) in finished {
        let ws = crate::workspace::Workspace::for_guardian(store, &gid, git_root.as_str());
        match integration.as_str() {
            "integrated" | "none" | "skipped" => {
                let _ = ws.git(&["update-ref", "-d", &live_ref]);
                let _ = store.lock().clear_live_run_ref(&run_id);
                crate::cartographer::Note::new("guardian")
                    .guardian(&gid)
                    .scope("live_action")
                    .emit(
                        &store.lock(),
                        "live ref retired",
                        serde_json::json!({"run_id": run_id, "live_ref": live_ref}),
                    );
            }
            "kept" => {}
            _ => {
                // Commits that never landed: keep them, say so once.
                let guard = store.lock();
                let _ = guard.set_live_run_integration(
                    &run_id,
                    "kept",
                    Some("the review finished with this run's commits not carried; kept on its live ref"),
                );
                let _ = guard.enqueue_error_mailbox_message(
                    crate::mailbox::MailboxPriority::Normal,
                    &format!(
                        "Review {gid} finished, but a live action's commits were never carried                          into it; they are kept on {live_ref}."
                    ),
                    &crate::mailbox::Remediation::ManualInterventionRequired {
                        guidance: format!(
                            "inspect them with `git log {live_ref}` in {git_root}; cherry-pick                              what is worth keeping, then delete the ref with                              `git update-ref -d {live_ref}`"
                        ),
                    },
                    None,
                    None,
                    None,
                    Some(&format!("guardian:{gid}")),
                    Some("live_actions"),
                );
            }
        }
    }
    retire_orphan_live_refs(store);
}

/// Delete live refs whose review no longer exists, in every repository a
/// review or registered project lives in.
fn retire_orphan_live_refs(store: &crate::store_lock::StoreHandle) {
    let (roots, known): (Vec<String>, std::collections::HashSet<String>) = {
        let guard = store.lock();
        let mut roots: Vec<String> = Vec::new();
        if let Ok(mut stmt) = guard
            .conn
            .prepare("SELECT DISTINCT git_root FROM guardians")
        {
            if let Ok(rows) = stmt.query_map([], |r| r.get::<_, String>(0)) {
                roots.extend(rows.flatten());
            }
        }
        if let Ok(projects) = guard.list_projects() {
            roots.extend(projects.into_iter().map(|p| p.path));
        }
        roots.sort();
        roots.dedup();
        let mut known = std::collections::HashSet::new();
        if let Ok(mut stmt) = guard.conn.prepare("SELECT id FROM guardians") {
            if let Ok(rows) = stmt.query_map([], |r| r.get::<_, String>(0)) {
                known.extend(rows.flatten());
            }
        }
        (roots, known)
    };
    for root in roots {
        if !std::path::Path::new(&root).is_dir() {
            continue;
        }
        let ws = crate::workspace::Workspace::local(&root);
        let Ok(refs) = ws.git(&[
            "for-each-ref",
            "--format=%(refname)",
            crate::live_actions::LIVE_REF_PREFIX,
        ]) else {
            continue;
        };
        for name in refs.lines().map(str::trim).filter(|l| !l.is_empty()) {
            let owner = name
                .strip_prefix(crate::live_actions::LIVE_REF_PREFIX)
                .and_then(|rest| rest.trim_start_matches('/').split('/').next())
                .unwrap_or_default();
            if owner.is_empty() || known.contains(owner) {
                continue;
            }
            let _ = ws.git(&["update-ref", "-d", name]);
            crate::rlog!(
                INFO,
                "ralphus [guardian] deleted orphan live ref {name} in {root} (its review no longer exists)"
            );
            crate::cartographer::Note::new("guardian")
                .scope("live_action")
                .emit(
                    &store.lock(),
                    "orphan live ref retired",
                    serde_json::json!({"live_ref": name, "repository": root, "review": owner}),
                );
        }
    }
}

/// Queue manual runs: one action (`Some(idx)`) or all of them. Generates the
/// review's live actions first when it has none yet. Returns the queued run
/// ids.
pub fn queue_manual_runs(
    store: &crate::store_lock::StoreHandle,
    runners: &LiveRunnerFactory,
    guardian_id: &str,
    index: Option<i64>,
) -> Result<Vec<String>, String> {
    let actions = ensure_live_actions(store, runners, guardian_id);
    if actions.is_empty() {
        return Err(
            "this review has no live actions: none of its finished cells posted a \
             RALPHUS_LIVE_ACTION handoff"
                .to_string(),
        );
    }
    let chosen: Vec<_> = match index {
        Some(i) => actions.iter().filter(|a| a.idx == i).collect(),
        None => actions.iter().collect(),
    };
    if chosen.is_empty() {
        return Err(format!(
            "this review has no live action {}",
            index.unwrap_or(-1)
        ));
    }
    let mut ids = Vec::new();
    for action in chosen {
        let id = store
            .lock()
            .queue_live_run(
                guardian_id,
                LiveRunKind::Live,
                action.idx,
                &action.label,
                "manual",
            )
            .map_err(|e| e.to_string())?;
        ids.push(id);
    }
    start_worker(store, runners, guardian_id);
    Ok(ids)
}

/// Queue a run of a user action whose `prompt` makes it agent-run.
pub fn queue_action_prompt_run(
    store: &crate::store_lock::StoreHandle,
    runners: &LiveRunnerFactory,
    guardian_id: &str,
    index: usize,
    trigger: &str,
) -> Result<String, String> {
    let label = {
        let guard = store.lock();
        let g = guard.get_guardian(guardian_id).map_err(|e| e.to_string())?;
        let hint = g
            .action_hints
            .get(index)
            .ok_or_else(|| format!("review has no action {index}"))?;
        if hint.prompt.as_deref().is_none_or(|p| p.trim().is_empty()) {
            return Err(format!("action {index} has no prompt"));
        }
        hint.label
            .clone()
            .unwrap_or_else(|| format!("action {index}"))
    };
    let id = store
        .lock()
        .queue_live_run(
            guardian_id,
            LiveRunKind::Action,
            i64::try_from(index).unwrap_or(i64::MAX),
            &label,
            trigger,
        )
        .map_err(|e| e.to_string())?;
    start_worker(store, runners, guardian_id);
    Ok(id)
}

/// Cancel a run: drop it from the queue, or stop it while it runs.
pub fn cancel_run(store: &crate::store_lock::StoreHandle, run_id: &str) -> Result<(), String> {
    if store
        .lock()
        .cancel_queued_live_run(run_id)
        .map_err(|e| e.to_string())?
    {
        return Ok(());
    }
    if store.lock_free_memory().cancel_live_run(run_id) {
        return Ok(());
    }
    Err("that live run is not queued or running".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// Stands in for the agent: records every prompt and answers `reply`.
    struct Agent {
        reply: String,
        prompts: parking_lot::Mutex<Vec<String>>,
    }

    impl Runner for Agent {
        fn run(&self, spec: &crate::runner::RunnerSpec) -> crate::runner::RunnerResult {
            self.prompts
                .lock()
                .push(spec.prompt.clone().unwrap_or_default());
            let mut result = crate::runner::RunnerResult::failure("");
            result.status = "done".to_string();
            result.error = None;
            result.summary.clone_from(&self.reply);
            result
        }
    }

    fn git(dir: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
    }

    struct Fixture {
        store: crate::store_lock::StoreHandle,
        gid: String,
        squad: String,
        dir: std::path::PathBuf,
        agent: Arc<Agent>,
        runners: LiveRunnerFactory,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// A review with one finished member cell that posted two handoffs, a
    /// prepared checkout (a real git repo), and live actions opted in.
    fn fixture(tag: &str, reply: &str) -> Fixture {
        let dir = std::env::temp_dir().join(format!(
            "ralphus-live-runs-{tag}-{}-{}",
            std::process::id(),
            crate::store::now_ms()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        git(&dir, &["init", "-q", "-b", "main"]);
        git(&dir, &["config", "user.name", "t"]);
        git(&dir, &["config", "user.email", "t@example.com"]);
        std::fs::write(dir.join("app.txt"), "v1\n").unwrap();
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-q", "-m", "init"]);

        let mut raw = crate::store::Store::open_in_memory().unwrap();
        let file: ralphus_core::schema::TaskFile = toml::from_str(
            r#"
[[task]]
name = "build"
[[task.cell]]
id = "one"
cwd = "/repo"
prompt = "first"
"#,
        )
        .unwrap();
        let squad = raw.insert_squad(&file, None, false).unwrap();
        let store: crate::store_lock::StoreHandle =
            Arc::new(crate::store_lock::StoreMutex::new(raw));
        let gid = store
            .lock()
            .create_guardian("live", "main", &dir.to_string_lossy())
            .unwrap();
        {
            let guard = store.lock();
            guard
                .conn
                .execute(
                    "UPDATE cells SET state='done', finished_at_ms=1, review_guardian_id=?
                     WHERE squad_id=?",
                    rusqlite::params![gid, squad],
                )
                .unwrap();
            guard
                .replace_cell_live_handoffs(
                    &squad,
                    0,
                    0,
                    &crate::live_actions::LiveHandoffs::Tests(vec![
                        "Start the app and check the greeting.".to_string(),
                    ]),
                )
                .unwrap();
            guard.set_guardian_live_actions(&gid, Some(true)).unwrap();
        }
        let agent = Arc::new(Agent {
            reply: reply.to_string(),
            prompts: parking_lot::Mutex::new(Vec::new()),
        });
        let shared = Arc::clone(&agent);
        let runners: LiveRunnerFactory = Arc::new(move |_| Arc::clone(&shared) as Arc<dyn Runner>);
        Fixture {
            store,
            gid,
            squad,
            dir,
            agent,
            runners,
        }
    }

    /// Wait until no run of the review is queued or running.
    fn settle(f: &Fixture) -> Vec<LiveRunRow> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        loop {
            let runs = guardian_live_runs_conn(&f.store.lock().conn, &f.gid).unwrap();
            let busy = runs
                .iter()
                .any(|r| r.state == "queued" || r.state == "running")
                || f.store.lock_free_memory().has_live_run_worker(&f.gid);
            if !runs.is_empty() && !busy {
                return runs;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "runs never settled: {runs:?}"
            );
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    #[test]
    fn the_automatic_run_waits_for_every_condition() {
        let f = fixture("gates", "ok");
        assert_eq!(
            automatic_run_decision(&f.store, &f.gid),
            AutoRunDecision::NoCheckout
        );
        f.store
            .lock()
            .set_live_checkout(&f.gid, &f.dir.to_string_lossy(), None)
            .unwrap();
        assert_eq!(
            automatic_run_decision(&f.store, &f.gid),
            AutoRunDecision::Due
        );

        f.store
            .lock()
            .conn
            .execute(
                "UPDATE cells SET state='running' WHERE squad_id=?",
                rusqlite::params![f.squad],
            )
            .unwrap();
        assert_eq!(
            automatic_run_decision(&f.store, &f.gid),
            AutoRunDecision::CellsStillRunning
        );
        f.store
            .lock()
            .conn
            .execute(
                "UPDATE cells SET state='done' WHERE squad_id=?",
                rusqlite::params![f.squad],
            )
            .unwrap();

        f.store
            .lock()
            .set_guardian_live_actions(&f.gid, Some(false))
            .unwrap();
        assert_eq!(
            automatic_run_decision(&f.store, &f.gid),
            AutoRunDecision::NotOptedIn
        );
        f.store
            .lock()
            .set_guardian_live_actions(&f.gid, Some(true))
            .unwrap();

        f.store
            .lock()
            .set_guardian_maximum_budget_usd(&f.gid, Some(0.0))
            .unwrap();
        assert_eq!(
            automatic_run_decision(&f.store, &f.gid),
            AutoRunDecision::OverBudget
        );
    }

    #[test]
    fn the_automatic_run_happens_once_and_reports_into_the_review() {
        let f = fixture("once", "Started it: the greeting renders.");
        f.store
            .lock()
            .set_live_checkout(&f.gid, &f.dir.to_string_lossy(), None)
            .unwrap();

        assert_eq!(
            maybe_start_automatic_run(&f.store, &f.runners, &f.gid),
            AutoRunDecision::Started
        );
        let runs = settle(&f);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].state, "finished");
        assert_eq!(runs[0].trigger, "auto");
        assert_eq!(
            runs[0].report.as_deref(),
            Some("Started it: the greeting renders.")
        );
        assert!(f.agent.prompts.lock()[0].contains("check the greeting"));

        let messages: Vec<(Option<String>, String)> = {
            let guard = f.store.lock();
            let mut stmt = guard
                .conn
                .prepare("SELECT author, text FROM guardian_messages WHERE guardian_id=?")
                .unwrap();
            stmt.query_map(rusqlite::params![f.gid], |r| Ok((r.get(0)?, r.get(1)?)))
                .unwrap()
                .collect::<std::result::Result<_, _>>()
                .unwrap()
        };
        assert!(
            messages.iter().any(
                |(author, text)| author.as_deref() == Some(LIVE_ACTION_AUTHOR)
                    && text.contains("the greeting renders")
            ),
            "the report lands in the review's conversation: {messages:?}"
        );

        // A later preparation, a restart's sweep, a settings change: never again.
        assert_eq!(
            maybe_start_automatic_run(&f.store, &f.runners, &f.gid),
            AutoRunDecision::AlreadyRan
        );
        sweep_automatic_runs(&f.store, &f.runners);
        assert_eq!(
            guardian_live_runs_conn(&f.store.lock().conn, &f.gid)
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn a_preparation_failure_is_handed_to_the_live_run() {
        let f = fixture("prep-failed", "found it");
        on_preparation_finished(
            &f.store,
            &f.runners,
            &f.gid,
            &f.dir.to_string_lossy(),
            Some("cargo build failed: missing crate `foo`"),
        );
        settle(&f);
        let prompt = f.agent.prompts.lock()[0].clone();
        assert!(prompt.contains("missing crate `foo`"), "{prompt}");
    }

    #[test]
    fn manual_runs_rerun_one_or_all_actions() {
        let f = fixture("manual", "ok");
        f.store
            .lock()
            .set_live_checkout(&f.gid, &f.dir.to_string_lossy(), None)
            .unwrap();
        let ids = queue_manual_runs(&f.store, &f.runners, &f.gid, None).unwrap();
        assert_eq!(ids.len(), 1);
        settle(&f);
        let again = queue_manual_runs(&f.store, &f.runners, &f.gid, Some(0)).unwrap();
        assert_eq!(again.len(), 1);
        let runs = settle(&f);
        assert_eq!(runs.len(), 2);
        assert!(
            runs.iter()
                .all(|r| r.trigger == "manual" && r.state == "finished")
        );
        assert!(queue_manual_runs(&f.store, &f.runners, &f.gid, Some(9)).is_err());
    }

    #[test]
    fn a_user_prompt_action_runs_as_an_agent() {
        let f = fixture("prompt-action", "the dashboard loads");
        f.store
            .lock()
            .set_live_checkout(&f.gid, &f.dir.to_string_lossy(), None)
            .unwrap();
        let action = crate::guardian::GuardianCheck {
            label: Some("Dashboard".to_string()),
            prompt: Some("Open the dashboard and check it loads".to_string()),
            ..crate::guardian::GuardianCheck::default()
        };
        f.store
            .lock()
            .set_guardian_action_hints(&f.gid, &[action])
            .unwrap();
        queue_action_prompt_run(&f.store, &f.runners, &f.gid, 0, "manual").unwrap();
        let runs = settle(&f);
        assert_eq!(runs[0].kind, "action");
        assert_eq!(runs[0].label, "Dashboard");
        assert!(f.agent.prompts.lock()[0].contains("Open the dashboard"));
        assert!(
            queue_action_prompt_run(&f.store, &f.runners, &f.gid, 3, "manual").is_err(),
            "no such action"
        );
    }

    #[test]
    fn a_queued_run_can_be_cancelled() {
        let f = fixture("cancel", "ok");
        let id = f
            .store
            .lock()
            .queue_live_run(&f.gid, LiveRunKind::Live, 0, "x", "manual")
            .unwrap();
        cancel_run(&f.store, &id).unwrap();
        let run = live_run_conn(&f.store.lock().conn, &id).unwrap();
        assert_eq!(run.state, "cancelled");
        assert!(cancel_run(&f.store, &id).is_err(), "nothing left to cancel");
    }

    fn ref_exists(dir: &Path, name: &str) -> bool {
        std::process::Command::new("git")
            .args(["rev-parse", "--verify", "--quiet", name])
            .current_dir(dir)
            .status()
            .unwrap()
            .success()
    }

    /// A finished run of `f`'s review with its commits pinned on a live ref.
    fn run_with_ref(f: &Fixture, integration: &str) -> (String, String) {
        let id = f
            .store
            .lock()
            .queue_live_run(&f.gid, LiveRunKind::Live, 0, "x", "auto")
            .unwrap();
        let live_ref = crate::live_actions::live_ref_name(&f.gid, &id);
        git(&f.dir, &["update-ref", &live_ref, "HEAD"]);
        f.store
            .lock()
            .conn
            .execute(
                "UPDATE guardian_live_runs SET state='finished', live_ref=?, integration=?
                 WHERE run_id=?",
                rusqlite::params![live_ref, integration, id],
            )
            .unwrap();
        (id, live_ref)
    }

    #[test]
    fn a_finished_reviews_spent_live_refs_are_retired_and_unlanded_ones_kept() {
        let f = fixture("retire", "ok");
        let (done_id, done_ref) = run_with_ref(&f, "integrated");
        let (kept_id, kept_ref) = run_with_ref(&f, "failed");

        // Still in review: nothing is touched.
        retire_live_refs(&f.store);
        assert!(ref_exists(&f.dir, &done_ref) && ref_exists(&f.dir, &kept_ref));

        f.store
            .lock()
            .conn
            .execute(
                "UPDATE guardians SET status='deployed' WHERE id=?",
                rusqlite::params![f.gid],
            )
            .unwrap();
        retire_live_refs(&f.store);
        assert!(
            !ref_exists(&f.dir, &done_ref),
            "carried commits: ref retired"
        );
        assert!(
            f.store
                .lock()
                .live_run(&done_id)
                .unwrap()
                .live_ref
                .is_none()
        );
        assert!(
            ref_exists(&f.dir, &kept_ref),
            "commits that never landed are kept"
        );
        assert_eq!(
            f.store.lock().live_run(&kept_id).unwrap().integration,
            "kept"
        );

        // Kept refs are reported once, not on every sweep.
        let notices = |f: &Fixture| -> i64 {
            f.store
                .lock()
                .conn
                .query_row(
                    "SELECT COUNT(*) FROM mailbox_messages WHERE category='live_actions'",
                    [],
                    |r| r.get(0),
                )
                .unwrap()
        };
        let after_first = notices(&f);
        assert_eq!(after_first, 1, "the kept ref is reported");
        retire_live_refs(&f.store);
        assert_eq!(notices(&f), after_first);
        assert!(ref_exists(&f.dir, &kept_ref));
    }

    #[test]
    fn live_refs_of_a_deleted_review_are_removed() {
        let f = fixture("orphan", "ok");
        let orphan = format!("{}/guardian-gone/r1", crate::live_actions::LIVE_REF_PREFIX);
        git(&f.dir, &["update-ref", &orphan, "HEAD"]);
        let (_, mine) = run_with_ref(&f, "pending");
        retire_live_refs(&f.store);
        assert!(!ref_exists(&f.dir, &orphan), "no review owns it any more");
        assert!(
            ref_exists(&f.dir, &mine),
            "a live review's ref is left alone"
        );
    }

    #[test]
    fn a_restart_marks_running_runs_interrupted() {
        let f = fixture("restart", "ok");
        let id = f
            .store
            .lock()
            .queue_live_run(&f.gid, LiveRunKind::Live, 0, "x", "auto")
            .unwrap();
        f.store
            .lock()
            .start_live_run(&id, "/somewhere", "live-0-x")
            .unwrap();
        assert_eq!(f.store.lock().interrupt_running_live_runs().unwrap(), 1);
        let run = live_run_conn(&f.store.lock().conn, &id).unwrap();
        assert_eq!(run.state, "interrupted");
        assert!(run.error.unwrap().contains("rerun"));
    }
}
