//! Dependency planning: order a run's sessions so each runs after the sessions
//! it depends on.
//!
//! Edges come from two places (both resolved to session positions here):
//! - a session's own `depends_on` — a within-task session id (`"a"`) or a
//!   cross-task `"task/session"` reference;
//! - its task's `depends_on` — a task name (all that task's sessions) or a
//!   cross-task `"task/session"` reference — applied to every session in the task.
//!
//! Unresolvable references produce no edge (best-effort; the validator is the
//! place that rejects bad refs). A cycle is a hard error.

use std::collections::HashMap;

use crate::store::{SessionRow, TaskRow};

/// A concrete execution plan for a run's sessions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutionPlan {
    /// Session positions (indices into the `sessions` slice) in execution order.
    pub order: Vec<usize>,
    /// `deps[i]` is the set of prerequisite session positions for session `i`.
    pub deps: Vec<Vec<usize>>,
}

/// Build an execution plan, or return an error message if the dependencies form
/// a cycle.
///
/// # Errors
/// Returns `Err` with a human-readable message when a dependency cycle exists.
pub fn plan(sessions: &[SessionRow], tasks: &[TaskRow]) -> Result<ExecutionPlan, String> {
    let n = sessions.len();
    let task_name_to_idx: HashMap<&str, i64> =
        tasks.iter().map(|t| (t.name.as_str(), t.idx)).collect();
    let task_deps: HashMap<i64, &[String]> = tasks
        .iter()
        .map(|t| (t.idx, t.depends_on.as_slice()))
        .collect();

    let mut sid_to_pos: HashMap<(i64, &str), usize> = HashMap::new();
    let mut task_sessions: HashMap<i64, Vec<usize>> = HashMap::new();
    for (i, s) in sessions.iter().enumerate() {
        sid_to_pos.insert((s.task_idx, s.session_id.as_str()), i);
        task_sessions.entry(s.task_idx).or_default().push(i);
    }

    let mut deps: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (i, s) in sessions.iter().enumerate() {
        let mut prereqs: Vec<usize> = Vec::new();

        // Session-level references.
        for dep in &s.depends_on {
            if let Some((tname, sid)) = dep.split_once('/') {
                if let Some(&pos) = task_name_to_idx
                    .get(tname)
                    .and_then(|t| sid_to_pos.get(&(*t, sid)))
                {
                    prereqs.push(pos);
                }
            } else if let Some(&pos) = sid_to_pos.get(&(s.task_idx, dep.as_str())) {
                prereqs.push(pos);
            }
        }

        // Task-level references apply to every session in the task.
        if let Some(tdeps) = task_deps.get(&s.task_idx) {
            for dep in *tdeps {
                if let Some((tname, sid)) = dep.split_once('/') {
                    if let Some(&pos) = task_name_to_idx
                        .get(tname)
                        .and_then(|t| sid_to_pos.get(&(*t, sid)))
                    {
                        prereqs.push(pos);
                    }
                } else if let Some(&tidx) = task_name_to_idx.get(dep.as_str()) {
                    if let Some(positions) = task_sessions.get(&tidx) {
                        prereqs.extend(positions.iter().copied());
                    }
                }
            }
        }

        prereqs.retain(|&p| p != i);
        prereqs.sort_unstable();
        prereqs.dedup();
        deps[i] = prereqs;
    }

    let order = topo_order(&deps)?;
    Ok(ExecutionPlan { order, deps })
}

/// Deterministic topological sort (lowest index first). `deps[i]` lists the
/// prerequisites of `i`. Returns an error if no valid order exists (a cycle).
fn topo_order(deps: &[Vec<usize>]) -> Result<Vec<usize>, String> {
    let n = deps.len();
    let mut order = Vec::with_capacity(n);
    let mut done = vec![false; n];
    while order.len() < n {
        // Pick the lowest-index not-yet-emitted node whose prerequisites are all emitted.
        let next = (0..n).find(|&i| !done[i] && deps[i].iter().all(|&d| done[d]));
        match next {
            Some(i) => {
                done[i] = true;
                order.push(i);
            }
            None => return Err("dependency cycle among sessions".to_string()),
        }
    }
    Ok(order)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(task_idx: i64, idx: i64, id: &str, deps: &[&str]) -> SessionRow {
        SessionRow {
            task_idx,
            idx,
            task_name: format!("task{task_idx}"),
            session_id: id.to_string(),
            cwd: Some(".".to_string()),
            subprojects: vec![],
            prompt: None,
            command: Some("do".to_string()),
            agent: "claude".to_string(),
            model: None,
            system_prompt: None,
            system_prompt_position: None,
            depends_on: deps.iter().map(|s| (*s).to_string()).collect(),
            timeout_sec: None,
            budget_tokens: None,
            upstream: None,
        }
    }

    fn task(idx: i64, deps: &[&str]) -> TaskRow {
        TaskRow {
            idx,
            name: format!("task{idx}"),
            depends_on: deps.iter().map(|s| (*s).to_string()).collect(),
        }
    }

    #[test]
    fn independent_sessions_keep_order() {
        let sessions = vec![session(0, 0, "a", &[]), session(0, 1, "b", &[])];
        let tasks = vec![task(0, &[])];
        let p = plan(&sessions, &tasks).unwrap();
        assert_eq!(p.order, vec![0, 1]);
    }

    #[test]
    fn within_task_dependency_orders_after() {
        // b depends on a, but is listed first -> a must still run before b.
        let sessions = vec![session(0, 0, "b", &["a"]), session(0, 1, "a", &[])];
        let tasks = vec![task(0, &[])];
        let p = plan(&sessions, &tasks).unwrap();
        assert_eq!(p.order, vec![1, 0]);
        assert_eq!(p.deps[0], vec![1]);
    }

    #[test]
    fn cross_task_dependency() {
        // task1/w depends on task0/x via "task0/x".
        let sessions = vec![session(0, 0, "x", &[]), session(1, 0, "w", &["task0/x"])];
        let tasks = vec![task(0, &[]), task(1, &[])];
        let p = plan(&sessions, &tasks).unwrap();
        assert_eq!(p.order, vec![0, 1]);
        assert_eq!(p.deps[1], vec![0]);
    }

    #[test]
    fn task_level_dependency_applies_to_all_sessions() {
        // task1 depends on task0 -> every task1 session waits for every task0 session.
        let sessions = vec![session(0, 0, "x", &[]), session(1, 0, "w", &[])];
        let tasks = vec![task(0, &[]), task(1, &["task0"])];
        let p = plan(&sessions, &tasks).unwrap();
        assert_eq!(p.order, vec![0, 1]);
        assert_eq!(p.deps[1], vec![0]);
    }

    #[test]
    fn cycle_is_an_error() {
        let sessions = vec![session(0, 0, "a", &["b"]), session(0, 1, "b", &["a"])];
        let tasks = vec![task(0, &[])];
        assert!(plan(&sessions, &tasks).is_err());
    }

    #[test]
    fn unresolvable_ref_is_ignored() {
        let sessions = vec![session(0, 0, "a", &["ghost"])];
        let tasks = vec![task(0, &[])];
        let p = plan(&sessions, &tasks).unwrap();
        assert_eq!(p.order, vec![0]);
        assert!(p.deps[0].is_empty());
    }
}
