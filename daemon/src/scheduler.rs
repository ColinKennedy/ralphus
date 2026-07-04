//! The scheduler: turns Pending runs into real work.
//!
//! A single scheduler thread polls the store; each `tick` claims up to the
//! available concurrency slots of ready runs, marks each Running, and hands it
//! to a worker thread. The worker executes the run's sessions via the [`Runner`]
//! (the subprocess wait happens *outside* the store lock, so many runs progress
//! concurrently), records each result, and finalizes task/run states.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::runner::{Runner, RunnerSpec, SubprocessRunner};
use crate::store::{NodeState, RunState, SessionOutcome, Store};
use crate::verify;

/// How long a worker holds nothing; the poll interval between ticks.
pub const POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Run the scheduler forever, ticking every [`POLL_INTERVAL`].
pub fn run_loop(store: Arc<Mutex<Store>>, runner: Arc<dyn Runner>, max_concurrent: i64) {
    loop {
        tick(&store, &runner, max_concurrent);
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// One scheduling pass: start workers for as many ready runs as free slots allow.
pub fn tick(store: &Arc<Mutex<Store>>, runner: &Arc<dyn Runner>, max_concurrent: i64) {
    let to_start = claim_ready(store, max_concurrent);
    for run_id in to_start {
        let store = Arc::clone(store);
        let runner = Arc::clone(runner);
        std::thread::spawn(move || execute_run(&store, runner.as_ref(), &run_id));
    }
}

/// Under the lock: compute free slots, take that many ready runs, and mark each
/// Running so the next tick will not re-claim them.
fn claim_ready(store: &Arc<Mutex<Store>>, max_concurrent: i64) -> Vec<String> {
    let guard = store.lock().expect("store mutex poisoned");
    let running = guard.running_count().unwrap_or(0);
    let slots = (max_concurrent - running).max(0);
    if slots == 0 {
        return Vec::new();
    }
    let ready = guard.list_ready().unwrap_or_default();
    let mut claimed = Vec::new();
    for run_id in ready.into_iter().take(usize::try_from(slots).unwrap_or(0)) {
        if guard.set_run_state(&run_id, RunState::Running).is_ok() {
            claimed.push(run_id);
        }
    }
    claimed
}

/// Execute one already-claimed (Running) run to completion, honouring the
/// dependency order of its sessions.
pub fn execute_run(store: &Arc<Mutex<Store>>, runner: &dyn Runner, run_id: &str) {
    let (sessions, tasks) = {
        let guard = store.lock().expect("store mutex poisoned");
        // Mark Running up front so the finalization guard (which leaves an
        // edit-reset run Pending) has a Running baseline to compare against.
        let _ = guard.set_run_state(run_id, RunState::Running);
        (
            guard.sessions_of(run_id).unwrap_or_default(),
            guard.tasks_of(run_id).unwrap_or_default(),
        )
    };

    let plan = match crate::plan::plan(&sessions, &tasks) {
        Ok(p) => p,
        Err(msg) => {
            finalize_all_failed(store, run_id, &tasks, &msg);
            return;
        }
    };

    // Resolve handoff placeholders against upstream sessions' summaries.
    let mut summaries: Vec<Option<String>> = vec![None; sessions.len()];
    let mut session_done = vec![false; sessions.len()];
    let mut failed_tasks: HashSet<i64> = HashSet::new();

    for &i in &plan.order {
        let row = &sessions[i];

        // Block a session whose prerequisites did not complete.
        if plan.deps[i].iter().any(|&d| !session_done[d]) {
            let outcome = SessionOutcome {
                state: NodeState::Failed,
                tokens_in: 0,
                tokens_out: 0,
                cost_usd: 0.0,
                error: Some("blocked by a failed dependency".to_string()),
            };
            let guard = store.lock().expect("store mutex poisoned");
            let _ = guard.set_session_state(run_id, row.task_idx, row.idx, NodeState::Failed);
            let _ = guard.record_session_result(run_id, row.task_idx, row.idx, &outcome);
            failed_tasks.insert(row.task_idx);
            continue;
        }

        {
            let guard = store.lock().expect("store mutex poisoned");
            let _ = guard.set_session_state(run_id, row.task_idx, row.idx, NodeState::Running);
            let _ = guard.set_task_state(run_id, row.task_idx, NodeState::Running);
        }

        // Subprocess wait happens without the lock held.
        let mut spec = RunnerSpec::from_row(run_id, row);
        spec.prompt = spec
            .prompt
            .map(|p| resolve_handoffs(&p, &plan.deps[i], &summaries));
        let result = runner.run(&spec);
        let outcome = SessionOutcome {
            state: result.node_state(),
            tokens_in: result.tokens_in,
            tokens_out: result.tokens_out,
            cost_usd: result.cost_usd,
            error: result.error.clone(),
        };
        {
            let guard = store.lock().expect("store mutex poisoned");
            let _ = guard.record_session_result(run_id, row.task_idx, row.idx, &outcome);
        }

        if !result.is_done() {
            failed_tasks.insert(row.task_idx);
            continue;
        }
        session_done[i] = true;
        summaries[i] = Some(result.summary.clone());

        // Session-level verify steps run in the session's own working directory.
        let cwd = row.cwd.clone().unwrap_or_default();
        if !run_verifies(store, run_id, row.task_idx, "session", row.idx, &cwd) {
            failed_tasks.insert(row.task_idx);
        }
    }

    // Task-level verify steps run after all sessions, in the task's first
    // session's working directory.
    let task_indices: Vec<i64> = tasks.iter().map(|t| t.idx).collect();
    for task_idx in &task_indices {
        if failed_tasks.contains(task_idx) {
            continue;
        }
        let cwd = sessions
            .iter()
            .find(|s| s.task_idx == *task_idx)
            .and_then(|s| s.cwd.clone())
            .unwrap_or_default();
        if !run_verifies(store, run_id, *task_idx, "task", -1, &cwd) {
            failed_tasks.insert(*task_idx);
        }
    }

    let guard = store.lock().expect("store mutex poisoned");
    // If an edit reset this run to Pending mid-flight, don't clobber it with a
    // terminal state — leave it Pending so it re-runs with the new values.
    if !matches!(guard.run_state(run_id), Ok(RunState::Running)) {
        return;
    }
    for task_idx in &task_indices {
        let state = if failed_tasks.contains(task_idx) {
            NodeState::Failed
        } else {
            NodeState::Done
        };
        let _ = guard.set_task_state(run_id, *task_idx, state);
    }
    let run_state = if failed_tasks.is_empty() {
        RunState::Done
    } else {
        RunState::Failed
    };
    let _ = guard.set_run_state(run_id, run_state);
    // On success, kick off any reviews (guardians) derived from this run. Read
    // the ids under the lock, then release it before spawning the merges.
    let review_guardians = if run_state == RunState::Done {
        guard.guardians_for_run(run_id).unwrap_or_default()
    } else {
        Vec::new()
    };
    drop(guard);
    start_reviews(store, review_guardians);
}

/// Spawn a background merge for each guardian that is still `collecting` (the
/// review's "start gate": it fires when its run finishes). A user can start one
/// earlier via the merge endpoint ("allow review"). Conflict resolution uses a
/// fresh subprocess runner, independent of the session runner.
fn start_reviews(store: &Arc<Mutex<Store>>, guardian_ids: Vec<String>) {
    for gid in guardian_ids {
        let store = Arc::clone(store);
        std::thread::spawn(move || {
            let collecting = matches!(
                store.lock().expect("store mutex poisoned").get_guardian(&gid),
                Ok(g) if g.status == "collecting"
            );
            if collecting {
                let runner: Arc<dyn Runner> = Arc::new(SubprocessRunner::from_env());
                crate::guardian_merge::run_merge(&store, runner.as_ref(), &gid);
            }
        });
    }
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

/// Run the `command` verify steps of one scope, updating each step's state.
/// Returns false if any command verify fails. Non-command verifiers (agent /
/// brain / approval) are deferred and left pending.
fn run_verifies(
    store: &Arc<Mutex<Store>>,
    run_id: &str,
    task_idx: i64,
    scope: &str,
    session_idx: i64,
    cwd: &str,
) -> bool {
    let specs = {
        let guard = store.lock().expect("store mutex poisoned");
        guard
            .verify_specs(run_id, task_idx, scope, session_idx)
            .unwrap_or_default()
    };
    let mut all_ok = true;
    for (idx, kind, spec) in specs {
        if kind != "command" {
            continue; // deferred verifier kinds stay pending
        }
        {
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
        let passed = verify::run_command_verify(cwd, &spec);
        let state = if passed {
            NodeState::Done
        } else {
            NodeState::Failed
        };
        {
            let guard = store.lock().expect("store mutex poisoned");
            let _ = guard.set_verify_state(run_id, task_idx, scope, session_idx, idx, state);
        }
        if !passed {
            all_ok = false;
        }
    }
    all_ok
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::{RunnerResult, RunnerSpec};

    /// A runner that succeeds or fails based on the session command, without any
    /// subprocess — lets us test the scheduler deterministically.
    struct FakeRunner {
        fail_on: Option<String>,
    }

    impl Runner for FakeRunner {
        fn run(&self, spec: &RunnerSpec) -> RunnerResult {
            let cmd = spec.command.clone().unwrap_or_default();
            if self.fail_on.as_deref() == Some(cmd.as_str()) {
                RunnerResult::failure("intentional failure")
            } else {
                RunnerResult {
                    status: "done".to_string(),
                    tokens_in: 1,
                    tokens_out: 2,
                    cost_usd: 0.5,
                    summary: "ok".to_string(),
                    error: None,
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
        tick(&store, &runner, 4);
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
    fn concurrency_limit_zero_claims_nothing() {
        let (store, _id) = store_with(ONE_SESSION);
        assert!(claim_ready(&store, 0).is_empty());
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
}
