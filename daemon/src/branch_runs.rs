//! Per-branch agent runs (RAL-587): a review branch's rebase, final-proof and
//! feedback runs, paired and attributed from the guardian lifecycle rows in
//! the Cartographer log so the board never derives them itself.
//!
//! Each run is bracketed by a `starting`/`applying` row and an end row. The
//! end row closes the most recent open run of the same [`RunKind`]; a retry
//! inside one resolve pass emits no new start row, so it stays one run. A
//! start row that arrives while an older run of the same kind is still open
//! supersedes it: the older run is reported as `interrupted`. An end row with
//! no open run of its kind becomes a run flagged `start_recorded: false`,
//! stamped with the end row's own time.

use serde::Serialize;

use crate::cartographer::CartographerRow;
use crate::guardian::{BranchView, GuardianView};
use crate::store::{Result, Store};

/// Lifecycle messages that bracket a run, as the guardian emitters write them.
const LIFECYCLE_MESSAGES: [&str; 7] = [
    "conflicts starting",
    "conflicts resolved",
    "conflicts failed",
    "final proof starting",
    "final proof done",
    "feedback applying",
    "feedback done",
];

/// The lifecycle query minus its `message IN (...)` and ordering tail. The
/// literal `source`/`scope` terms let SQLite use `idx_carto_branch_lifecycle`.
const LIFECYCLE_SELECT: &str = "SELECT id, at_ms, message, cell_id, task, payload \
     FROM cartographer_events \
     WHERE guardian_id = ? AND source = 'guardian' AND scope = 'branch'";

/// Which kind of agent run a lifecycle row belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunKind {
    /// Rebasing the branch onto its base, resolving conflicts.
    Rebase,
    /// The final proof over the rebased branch.
    Proof,
    /// A feedback revision.
    Feedback,
}

/// One paired, attributed run.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct BranchRun {
    /// Stable id derived from the opening row, e.g. `rebase-42`.
    pub id: String,
    /// Run kind.
    pub kind: RunKind,
    /// Human label, e.g. `rebase onto main · 2 conflicts`.
    pub label: String,
    /// When the run started (epoch ms); the end row's time when
    /// `start_recorded` is false.
    pub start_at_ms: i64,
    /// When the run ended; `None` while it is still open.
    pub end_at_ms: Option<i64>,
    /// `end_at_ms - start_at_ms`; `None` while open.
    pub elapsed_ms: Option<i64>,
    /// `running`, `interrupted`, or the kind-specific result.
    pub outcome: String,
    /// The agent that ran it, when the row recorded one.
    pub agent: Option<String>,
    /// Owning task name.
    pub task: String,
    /// Cell id, which keys the run's transcript.
    pub cell_id: Option<String>,
    /// The prompt the agent was given, when recorded.
    pub prompt: Option<String>,
    /// False when only the end row exists.
    pub start_recorded: bool,
}

/// How a branch is recognised on rows that predate `branch_id`.
struct BranchKey<'a> {
    id: &'a str,
    name: &'a str,
    position: i64,
}

fn classify(message: &str) -> Option<(RunKind, bool)> {
    match message {
        "conflicts starting" => Some((RunKind::Rebase, true)),
        "conflicts resolved" | "conflicts failed" => Some((RunKind::Rebase, false)),
        "final proof starting" => Some((RunKind::Proof, true)),
        "final proof done" => Some((RunKind::Proof, false)),
        "feedback applying" => Some((RunKind::Feedback, true)),
        "feedback done" => Some((RunKind::Feedback, false)),
        _ => None,
    }
}

/// `branch_id` (or `ref`) is authoritative and excludes every other branch;
/// rows without one fall back to the branch name, then to the position.
fn belongs(row: &CartographerRow, key: &BranchKey<'_>) -> bool {
    let p = &row.payload;
    if let Some(id) = p
        .get("branch_id")
        .or_else(|| p.get("ref"))
        .and_then(|v| v.as_str())
    {
        return id == key.id;
    }
    if let Some(name) = p.get("branch").and_then(|v| v.as_str()) {
        return name == key.name;
    }
    p.get("position").and_then(serde_json::Value::as_i64) == Some(key.position)
}

fn payload_str(row: &CartographerRow, field: &str) -> Option<String> {
    row.payload
        .get(field)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn default_task(kind: RunKind) -> &'static str {
    match kind {
        RunKind::Rebase => crate::guardian_merge::RESOLVER_TASK,
        RunKind::Proof => crate::guardian_merge::RESOLVER_PROOF_TASK,
        RunKind::Feedback => crate::guardian_merge::FEEDBACK_TASK,
    }
}

fn kind_prefix(kind: RunKind) -> &'static str {
    match kind {
        RunKind::Rebase => "rebase",
        RunKind::Proof => "proof",
        RunKind::Feedback => "fb",
    }
}

fn label_for(kind: RunKind, row: &CartographerRow, base_branch: &str) -> String {
    match kind {
        RunKind::Rebase => {
            let found = row
                .payload
                .get("found")
                .and_then(serde_json::Value::as_i64)
                .unwrap_or(0);
            let base = if base_branch.is_empty() {
                "upstream"
            } else {
                base_branch
            };
            if found > 0 {
                let plural = if found == 1 { "" } else { "s" };
                format!("rebase onto {base} · {found} conflict{plural}")
            } else {
                format!("rebase onto {base}")
            }
        }
        RunKind::Proof => "final proof".to_string(),
        RunKind::Feedback => "feedback revision".to_string(),
    }
}

fn end_outcome(row: &CartographerRow) -> &'static str {
    let flag = |f: &str| {
        row.payload
            .get(f)
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
    };
    match row.message.as_str() {
        "conflicts resolved" if flag("committed") => "resolved",
        "conflicts resolved" => "resolved, nothing to commit",
        "conflicts failed" => "failed",
        "final proof done" if flag("passed") => "passed",
        "final proof done" => "failed",
        "feedback done" if flag("committed") => "committed",
        _ => "no change committed",
    }
}

fn open_run(row: &CartographerRow, kind: RunKind, label: String) -> BranchRun {
    BranchRun {
        id: format!("{}-{}", kind_prefix(kind), row.id),
        kind,
        label,
        start_at_ms: row.at_ms,
        end_at_ms: None,
        elapsed_ms: None,
        outcome: "running".to_string(),
        agent: payload_str(row, "agent"),
        task: row
            .task
            .clone()
            .or_else(|| payload_str(row, "task"))
            .unwrap_or_else(|| default_task(kind).to_string()),
        cell_id: row.cell_id.clone().or_else(|| payload_str(row, "cell_id")),
        prompt: payload_str(row, "prompt"),
        start_recorded: true,
    }
}

fn finish(run: &mut BranchRun, at_ms: i64, outcome: &str) {
    run.end_at_ms = Some(at_ms);
    run.elapsed_ms = Some((at_ms - run.start_at_ms).max(0));
    run.outcome = outcome.to_string();
}

/// Pair `rows` (oldest first) into the runs of one branch.
fn pair_runs(rows: &[CartographerRow], key: &BranchKey<'_>, base_branch: &str) -> Vec<BranchRun> {
    let mut runs: Vec<BranchRun> = Vec::new();
    for row in rows {
        let Some((kind, is_start)) = classify(&row.message) else {
            continue;
        };
        if !belongs(row, key) {
            continue;
        }
        if is_start {
            for older in runs
                .iter_mut()
                .filter(|r| r.kind == kind && r.end_at_ms.is_none())
            {
                finish(older, row.at_ms, "interrupted");
            }
            runs.push(open_run(row, kind, label_for(kind, row, base_branch)));
        } else if let Some(open) = runs
            .iter_mut()
            .rev()
            .find(|r| r.kind == kind && r.end_at_ms.is_none())
        {
            finish(open, row.at_ms, end_outcome(row));
        } else {
            let label = format!("{} (start not recorded)", label_for(kind, row, base_branch));
            let mut run = open_run(row, kind, label);
            run.start_recorded = false;
            finish(&mut run, row.at_ms, end_outcome(row));
            runs.push(run);
        }
    }
    runs
}

/// Derive `branch`'s runs from `rows`, oldest first.
#[must_use]
pub fn runs_for(
    guardian: &GuardianView,
    branch: &BranchView,
    rows: &[CartographerRow],
) -> Vec<BranchRun> {
    let key = BranchKey {
        id: &branch.id,
        name: &branch.branch,
        position: branch.position,
    };
    pair_runs(rows, &key, &guardian.base_branch)
}

impl Store {
    /// The runs of `branch_id` in `guardian_id`, oldest first. Reads every
    /// lifecycle row of the review (no page cap) through the partial
    /// lifecycle index.
    ///
    /// # Errors
    /// Returns `StoreError::NotFound` for an unknown review or branch, or a
    /// database error.
    pub fn guardian_branch_runs(
        &self,
        guardian_id: &str,
        branch_id: &str,
    ) -> Result<Vec<BranchRun>> {
        let guardian = self.get_guardian(guardian_id)?;
        let branch = guardian
            .branches
            .iter()
            .find(|b| b.id == branch_id)
            .ok_or(crate::store::StoreError::NotFound)?;
        let rows = self.guardian_lifecycle_rows(guardian_id)?;
        Ok(runs_for(&guardian, branch, &rows))
    }

    fn guardian_lifecycle_rows(&self, guardian_id: &str) -> Result<Vec<CartographerRow>> {
        let marks = vec!["?"; LIFECYCLE_MESSAGES.len()].join(",");
        let sql = format!("{LIFECYCLE_SELECT} AND message IN ({marks}) ORDER BY at_ms, id");
        let mut args: Vec<&dyn rusqlite::ToSql> = vec![&guardian_id];
        for m in &LIFECYCLE_MESSAGES {
            args.push(m);
        }
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt
            .query_map(args.as_slice(), |r| {
                let payload: String = r.get(5)?;
                Ok(CartographerRow {
                    id: r.get(0)?,
                    at_ms: r.get(1)?,
                    level: String::new(),
                    source: "guardian".to_string(),
                    message: r.get(2)?,
                    scope: Some("branch".to_string()),
                    squad_id: None,
                    guardian_id: Some(guardian_id.to_string()),
                    cell_id: r.get(3)?,
                    task: r.get(4)?,
                    log_path: None,
                    payload: serde_json::from_str(&payload).unwrap_or(serde_json::Value::Null),
                    admin_only: false,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn row(id: i64, at_ms: i64, message: &str, payload: serde_json::Value) -> CartographerRow {
        CartographerRow {
            id,
            at_ms,
            level: "info".into(),
            source: "guardian".into(),
            message: message.into(),
            scope: Some("branch".into()),
            squad_id: None,
            guardian_id: Some("g".into()),
            cell_id: None,
            task: None,
            log_path: None,
            payload,
            admin_only: false,
        }
    }

    fn key() -> BranchKey<'static> {
        BranchKey {
            id: "branch-1",
            name: "feature/a",
            position: 0,
        }
    }

    #[test]
    fn open_run_has_no_end_or_elapsed() {
        let rows = vec![row(
            1,
            100,
            "conflicts starting",
            json!({"branch_id":"branch-1","found":2,"agent":"claude-code","cell_id":"c1","prompt":"p"}),
        )];
        let runs = pair_runs(&rows, &key(), "main");
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].outcome, "running");
        assert_eq!(runs[0].end_at_ms, None);
        assert_eq!(runs[0].elapsed_ms, None);
        assert_eq!(runs[0].label, "rebase onto main · 2 conflicts");
        assert_eq!(runs[0].cell_id.as_deref(), Some("c1"));
        assert_eq!(runs[0].agent.as_deref(), Some("claude-code"));
        assert_eq!(runs[0].task, "resolve");
    }

    #[test]
    fn closed_run_reports_elapsed_and_outcome() {
        let rows = vec![
            row(
                1,
                100,
                "final proof starting",
                json!({"branch_id":"branch-1"}),
            ),
            row(
                2,
                350,
                "final proof done",
                json!({"branch_id":"branch-1","passed":true}),
            ),
        ];
        let runs = pair_runs(&rows, &key(), "main");
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].outcome, "passed");
        assert_eq!(runs[0].elapsed_ms, Some(250));
        assert_eq!(runs[0].end_at_ms, Some(350));
    }

    #[test]
    fn lone_end_row_is_a_visible_run_without_a_start() {
        let mut end = row(
            5,
            900,
            "conflicts failed",
            json!({"branch_id":"branch-1","cell_id":"c9"}),
        );
        end.cell_id = None;
        let runs = pair_runs(&[end], &key(), "main");
        assert_eq!(runs.len(), 1);
        assert!(!runs[0].start_recorded);
        assert_eq!(runs[0].outcome, "failed");
        assert_eq!(runs[0].start_at_ms, 900);
        assert_eq!(runs[0].elapsed_ms, Some(0));
        assert_eq!(runs[0].cell_id.as_deref(), Some("c9"));
        assert!(runs[0].label.ends_with("(start not recorded)"));
    }

    #[test]
    fn rows_attribute_by_branch_id_before_name_or_position() {
        // Renamed + reordered: the row's name and position are stale, the id
        // is the truth.
        let rows = vec![
            row(
                1,
                10,
                "feedback applying",
                json!({"branch_id":"branch-1","position":7,"branch":"old/name"}),
            ),
            row(
                2,
                20,
                "feedback applying",
                json!({"branch_id":"branch-2","position":0,"branch":"feature/a"}),
            ),
        ];
        let runs = pair_runs(&rows, &key(), "main");
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].id, "fb-1");
    }

    #[test]
    fn legacy_rows_fall_back_to_name_then_position() {
        let rows = vec![
            row(1, 10, "conflicts starting", json!({"branch":"feature/a"})),
            row(
                2,
                20,
                "conflicts resolved",
                json!({"branch":"feature/a","committed":false}),
            ),
            row(
                3,
                30,
                "conflicts starting",
                json!({"branch":"feature/other"}),
            ),
            row(4, 40, "feedback applying", json!({"position":0})),
            row(5, 50, "feedback applying", json!({"position":1})),
        ];
        let runs = pair_runs(&rows, &key(), "main");
        let ids: Vec<_> = runs.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, ["rebase-1", "fb-4"]);
        assert_eq!(runs[0].outcome, "resolved, nothing to commit");
    }

    #[test]
    fn retry_inside_one_pass_is_one_run() {
        // A retry emits no second start row, so the pass yields one run.
        let rows = vec![
            row(1, 10, "conflicts starting", json!({"branch_id":"branch-1"})),
            row(
                2,
                90,
                "conflicts resolved",
                json!({"branch_id":"branch-1","committed":true}),
            ),
        ];
        let runs = pair_runs(&rows, &key(), "main");
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].outcome, "resolved");
    }

    #[test]
    fn a_new_start_interrupts_an_unclosed_run_of_the_same_kind() {
        let rows = vec![
            row(
                1,
                10,
                "final proof starting",
                json!({"branch_id":"branch-1"}),
            ),
            row(
                2,
                40,
                "final proof starting",
                json!({"branch_id":"branch-1"}),
            ),
            row(
                3,
                70,
                "final proof done",
                json!({"branch_id":"branch-1","passed":false}),
            ),
        ];
        let runs = pair_runs(&rows, &key(), "main");
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].outcome, "interrupted");
        assert_eq!(runs[0].elapsed_ms, Some(30));
        assert_eq!(runs[1].outcome, "failed");
    }

    #[test]
    fn kinds_pair_independently() {
        let rows = vec![
            row(1, 10, "conflicts starting", json!({"branch_id":"branch-1"})),
            row(
                2,
                20,
                "final proof starting",
                json!({"branch_id":"branch-1"}),
            ),
            row(
                3,
                30,
                "conflicts resolved",
                json!({"branch_id":"branch-1","committed":true}),
            ),
        ];
        let runs = pair_runs(&rows, &key(), "main");
        assert_eq!(runs[0].outcome, "resolved");
        assert_eq!(runs[1].outcome, "running");
    }

    #[test]
    fn lifecycle_query_uses_the_partial_index() {
        let store = Store::open_in_memory().unwrap();
        let marks = vec!["?"; LIFECYCLE_MESSAGES.len()].join(",");
        let sql = format!(
            "EXPLAIN QUERY PLAN {LIFECYCLE_SELECT} AND message IN ({marks}) ORDER BY at_ms, id"
        );
        let mut stmt = store.conn.prepare(&sql).unwrap();
        let mut args: Vec<&dyn rusqlite::ToSql> = vec![&"g"];
        for m in &LIFECYCLE_MESSAGES {
            args.push(m);
        }
        let plan: Vec<String> = stmt
            .query_map(args.as_slice(), |r| r.get::<_, String>(3))
            .unwrap()
            .map(std::result::Result::unwrap)
            .collect();
        let plan = plan.join("\n");
        assert!(plan.contains("idx_carto_branch_lifecycle"), "{plan}");
        assert!(!plan.contains("SCAN"), "{plan}");
    }
}
