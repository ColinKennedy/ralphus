//! The agent runs a review branch has had -- rebase passes, final proofs and
//! feedback revisions -- derived from the review's Cartographer rows.
//!
//! A run is the span between its start row and its end row. Branch attribution
//! is not uniform across emitters: status transitions key on `payload.ref` (a
//! branch id), the rebase and proof passes on `payload.branch` (a branch
//! name), PR and feedback-posting rows on `payload.branch_id`, and the
//! feedback pass itself on `payload.position` (a stack ordinal, which a reorder
//! can move). Rows that predate a stable id still attribute through the name
//! and ordinal matches.
//!
//! Pure: rows in, runs out. The route in `server.rs` does the paging.

use serde::Serialize;

use crate::cartographer::CartographerRow;

/// One agent pass a branch ran.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BranchRun {
    /// Id stable for the run, derived from its start row.
    pub id: String,
    /// `rebase`, `proof` or `feedback`.
    pub kind: &'static str,
    /// What the pass was doing.
    pub label: String,
    /// When the run started (Unix epoch milliseconds).
    pub at_ms: i64,
    /// `running` until an end row closes it, then `resolved`, `passed`, ...
    pub outcome: String,
    /// Measured duration; `0` when no emitter reported one. Never estimated.
    pub elapsed_ms: i64,
    /// The agent that ran it, when the row named one.
    pub who: Option<String>,
    /// The daemon task that owns the pass's transcript.
    pub task: String,
    /// The unique cell that owns the pass's transcript.
    pub cell_id: Option<String>,
    /// The dispatched instruction, when the daemon retained it.
    pub prompt: Option<String>,
}

/// Which branch to derive runs for.
pub struct BranchKey<'a> {
    /// Stable branch id.
    pub id: &'a str,
    /// Feature branch name.
    pub branch: &'a str,
    /// Current stack ordinal.
    pub position: i64,
}

fn belongs(row: &CartographerRow, key: &BranchKey<'_>) -> bool {
    let p = &row.payload;
    let s = |k: &str| p.get(k).and_then(|v| v.as_str());
    s("ref") == Some(key.id)
        || s("branch") == Some(key.branch)
        || s("branch_id") == Some(key.id)
        || p.get("position").and_then(|v| v.as_i64()) == Some(key.position)
}

fn close(runs: &mut [BranchRun], kind: &str, outcome: &str) {
    if let Some(open) = runs
        .iter_mut()
        .rev()
        .find(|r| r.kind == kind && r.outcome == "running")
    {
        open.outcome = outcome.to_string();
    }
}

fn start(
    row: &CartographerRow,
    kind: &'static str,
    id_prefix: &str,
    label: String,
    default_task: &str,
) -> BranchRun {
    let p = &row.payload;
    let text = |k: &str| p.get(k).and_then(|v| v.as_str()).map(str::to_string);
    BranchRun {
        id: format!("{id_prefix}-{}", row.id),
        kind,
        label,
        at_ms: row.at_ms,
        outcome: "running".to_string(),
        elapsed_ms: 0,
        who: text("agent"),
        task: row
            .task
            .clone()
            .or_else(|| text("task"))
            .unwrap_or_else(|| default_task.to_string()),
        cell_id: row.cell_id.clone().or_else(|| text("cell_id")),
        prompt: text("prompt"),
    }
}

/// Every run `key`'s branch had, oldest first. `rows` may be in any order and
/// may include rows for other branches or sources.
pub fn derive(rows: &[CartographerRow], key: &BranchKey<'_>, base_branch: &str) -> Vec<BranchRun> {
    let mut mine: Vec<&CartographerRow> = rows.iter().filter(|r| belongs(r, key)).collect();
    mine.sort_by_key(|r| (r.at_ms, r.id));
    let mut runs: Vec<BranchRun> = Vec::new();
    for row in mine {
        let msg = row.message.as_str();
        let flag = |k: &str| {
            row.payload
                .get(k)
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
        };
        if msg.starts_with("conflicts starting") {
            let found = row
                .payload
                .get("found")
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            let suffix = match found {
                0 => String::new(),
                1 => " · 1 conflict".to_string(),
                n => format!(" · {n} conflicts"),
            };
            let onto = if base_branch.is_empty() {
                "upstream"
            } else {
                base_branch
            };
            let label = format!("rebase onto {onto}{suffix}");
            runs.push(start(row, "rebase", "rebase", label, "resolver"));
        } else if msg.starts_with("conflicts resolved") {
            let outcome = if flag("committed") {
                "resolved"
            } else {
                "resolved, nothing to commit"
            };
            close(&mut runs, "rebase", outcome);
        } else if msg.starts_with("conflicts failed") {
            close(&mut runs, "rebase", "failed");
        } else if msg.starts_with("final proof starting") {
            let label = "final proof".to_string();
            runs.push(start(row, "proof", "proof", label, "resolver-proof"));
        } else if msg.starts_with("final proof done") {
            close(
                &mut runs,
                "proof",
                if flag("passed") { "passed" } else { "failed" },
            );
        } else if msg.starts_with("feedback applying") {
            let label = "feedback revision".to_string();
            runs.push(start(row, "feedback", "fb", label, "feedback"));
        } else if msg.starts_with("feedback done") {
            let outcome = if flag("committed") {
                "committed"
            } else {
                "no change committed"
            };
            close(&mut runs, "feedback", outcome);
        }
    }
    runs
}

#[cfg(test)]
mod tests {
    use super::*;

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
            cell_id: Some(format!("cell-{id}")),
            task: None,
            log_path: None,
            payload,
            admin_only: false,
        }
    }

    const KEY: BranchKey<'static> = BranchKey {
        id: "branch-1",
        branch: "feat-a",
        position: 2,
    };

    #[test]
    fn pairs_start_and_end_rows_oldest_first() {
        let rows = vec![
            row(
                3,
                30,
                "conflicts resolved",
                serde_json::json!({"branch":"feat-a","committed":true}),
            ),
            row(
                1,
                10,
                "conflicts starting",
                serde_json::json!({"branch":"feat-a","found":2}),
            ),
            row(
                2,
                20,
                "conflicts starting",
                serde_json::json!({"branch":"other"}),
            ),
            row(
                4,
                40,
                "final proof starting",
                serde_json::json!({"branch":"feat-a"}),
            ),
            row(
                5,
                50,
                "final proof done",
                serde_json::json!({"branch":"feat-a","passed":false}),
            ),
        ];
        let runs = derive(&rows, &KEY, "staging");
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].label, "rebase onto staging · 2 conflicts");
        assert_eq!(runs[0].outcome, "resolved");
        assert_eq!(runs[0].cell_id.as_deref(), Some("cell-1"));
        assert_eq!(
            (runs[1].kind, runs[1].outcome.as_str()),
            ("proof", "failed")
        );
    }

    #[test]
    fn open_run_stays_running_and_orphan_end_row_opens_nothing() {
        let rows = vec![
            row(
                1,
                10,
                "conflicts resolved",
                serde_json::json!({"branch":"feat-a"}),
            ),
            row(
                2,
                20,
                "feedback applying",
                serde_json::json!({"branch_id":"branch-1","position":2}),
            ),
        ];
        let runs = derive(&rows, &KEY, "");
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].kind, "feedback");
        assert_eq!(runs[0].outcome, "running");
    }

    #[test]
    fn feedback_done_with_only_a_position_still_closes_the_run() {
        let rows = vec![
            row(
                1,
                10,
                "feedback applying",
                serde_json::json!({"branch_id":"branch-1","position":2}),
            ),
            row(
                2,
                20,
                "feedback done",
                serde_json::json!({"position":2,"committed":true}),
            ),
        ];
        assert_eq!(derive(&rows, &KEY, "")[0].outcome, "committed");
    }

    #[test]
    fn a_branch_with_no_lifecycle_rows_has_no_runs() {
        let rows = vec![row(
            1,
            10,
            "poll healthy",
            serde_json::json!({"branch":"feat-a"}),
        )];
        assert!(derive(&rows, &KEY, "").is_empty());
    }
}
