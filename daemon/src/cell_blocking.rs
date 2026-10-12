//! RAL-641: tell the user when a cell that keeps failing is holding up other
//! work, and what they can do about it.
//!
//! A cell qualifies when its failure count reaches its threshold
//! ([`CellBlocking::is_blocking`]) and something waits on it. The mailbox
//! message is sent once per cell: a later count of blocked work rewrites the
//! same message in place rather than adding another.
//!
//! The remediation is a list of [`WayOut`]s. A way out is one variant plus one
//! arm in [`WayOut::render`] and one push in [`way_outs`].

use rusqlite::{OptionalExtension, params};

use crate::cartographer::Note;
use crate::events::MailboxNotice;
use crate::logging::LogLevel;
use crate::mailbox::{MailboxPriority, Remediation};
use crate::monitor::NotifiableEventKind;
use crate::store::{CellBlocking, Result, Store};

/// `mailbox_messages.category` of the blocking-cell message.
pub const CATEGORY: &str = "blocking";

/// One way to get a repeatedly failing cell out of the way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WayOut {
    /// Set the cell to `ignored`, which re-pends what it held back and
    /// disables its review branch.
    IgnoreAndContinue {
        /// `squad/task/cell` selector of the failing cell.
        cell_ref: String,
    },
    /// Take the cell's branch out of its review's stack.
    DisableBranch {
        /// `review#branch` selector of the branch.
        branch_ref: String,
    },
    /// Move the cell's branch into a new review of its own.
    ExtractBranch {
        /// `review#branch` selector of the branch.
        branch_ref: String,
    },
    /// Re-run the cell's definition as a new one-task squad.
    CopyToSquad {
        /// `squad/task/cell` selector of the failing cell.
        cell_ref: String,
    },
}

impl WayOut {
    fn render(&self) -> String {
        match self {
            Self::IgnoreAndContinue { cell_ref } => format!(
                "ignore the cell and continue with `ralphus cell set-status {cell_ref} ignored`"
            ),
            Self::DisableBranch { branch_ref } => {
                format!(
                    "disable its review branch with `ralphus review branch disable {branch_ref}`"
                )
            }
            Self::ExtractBranch { branch_ref } => format!(
                "extract its branch to a new review with `ralphus review extract-branch {branch_ref}`"
            ),
            Self::CopyToSquad { cell_ref } => format!(
                "copy it to a new squad with `ralphus cell copy {cell_ref}` (add `--from-branch` to start from its branch)"
            ),
        }
    }
}

/// The ways out for the cell `cell_ref`, in the order they are offered.
#[must_use]
pub fn way_outs(cell_ref: &str, blocking: &CellBlocking) -> Vec<WayOut> {
    let mut out = vec![WayOut::IgnoreAndContinue {
        cell_ref: cell_ref.to_string(),
    }];
    if let Some((review_id, branch)) = &blocking.branch {
        out.push(WayOut::DisableBranch {
            branch_ref: format!("{review_id}#{branch}"),
        });
        out.push(WayOut::ExtractBranch {
            branch_ref: format!("{review_id}#{branch}"),
        });
    }
    out.push(WayOut::CopyToSquad {
        cell_ref: cell_ref.to_string(),
    });
    out
}

/// What the cell holds up, e.g. "4 cells and 2 review branches".
fn holding_up(blocking: &CellBlocking) -> String {
    let plural = |n: i64, noun: &str| format!("{n} {noun}{}", if n == 1 { "" } else { "s" });
    match (blocking.blocked_cells, blocking.blocked_branches) {
        (c, 0) => plural(c, "cell"),
        (0, b) => plural(b, "review branch").replace("branchs", "branches"),
        (c, b) => format!(
            "{} and {}",
            plural(c, "cell"),
            plural(b, "review branch").replace("branchs", "branches")
        ),
    }
}

/// The message body and remediation for a blocking cell.
#[must_use]
pub fn blocking_message(cell_ref: &str, blocking: &CellBlocking) -> (String, Remediation) {
    let times = if blocking.failure_count == 1 {
        "1 time".to_string()
    } else {
        format!("{} times", blocking.failure_count)
    };
    let message = format!(
        "cell '{cell_ref}' has failed {times} and is holding up {}.",
        holding_up(blocking)
    );
    let options: Vec<String> = way_outs(cell_ref, blocking)
        .iter()
        .map(WayOut::render)
        .collect();
    let guidance = format!("to unblock it, {}", options.join(", or "));
    (
        message,
        Remediation::ManualInterventionRequired { guidance },
    )
}

impl Store {
    /// Send, refresh, or skip the blocking-cell message for the cell
    /// `(task_name, cell_sid)` of `squad_id`, after one of its failures was
    /// recorded.
    ///
    /// Nothing is sent below the failure threshold or while nothing waits on
    /// the cell. The first qualifying call enqueues the message (High
    /// priority, tagged like a cell failure so the same watchers receive it);
    /// a later call whose blocked count changed rewrites that message's text,
    /// and an unchanged count does nothing. Best-effort: errors are logged,
    /// not returned, like every other mailbox write on the failure path.
    pub fn report_blocking_cell(&self, squad_id: &str, task_name: &str, cell_sid: &str) {
        if let Err(e) = self.try_report_blocking_cell(squad_id, task_name, cell_sid) {
            Note::new("scheduler")
                .level(LogLevel::WARNING)
                .squad(squad_id)
                .task(task_name)
                .cell(cell_sid)
                .scope("mailbox")
                .emit(
                    self,
                    format!("could not report blocking cell: {e}"),
                    serde_json::json!({"error": e.to_string()}),
                );
        }
    }

    fn try_report_blocking_cell(
        &self,
        squad_id: &str,
        task_name: &str,
        cell_sid: &str,
    ) -> Result<()> {
        let Some(entity_uri) = self.cell_entity_uri(squad_id, task_name, cell_sid) else {
            return Ok(());
        };
        let (task_idx, idx): (i64, i64) = self.conn.query_row(
            "SELECT c.task_idx, c.idx FROM cells c
                 JOIN tasks t ON t.squad_id = c.squad_id AND t.idx = c.task_idx
                 WHERE c.squad_id=?1 AND t.name=?2 AND c.sid=?3",
            params![squad_id, task_name, cell_sid],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let Some(blocking) = self.cell_blocking(squad_id, task_idx, idx)? else {
            return Ok(());
        };
        if !blocking.is_blocking() {
            return Ok(());
        }
        let cell_ref = format!("{squad_id}/{task_name}/{cell_sid}");
        let (message, remediation) = blocking_message(&cell_ref, &blocking);
        let full_message = format!("{message} {}", remediation.render());

        let existing: Option<(String, String)> = self
            .conn
            .query_row(
                "SELECT id, message FROM mailbox_messages
                 WHERE entity_uri=?1 AND category=?2
                 ORDER BY created_at_ms DESC LIMIT 1",
                params![entity_uri, CATEGORY],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let note = || {
            Note::new("scheduler")
                .level(LogLevel::INFO)
                .squad(squad_id)
                .task(task_name)
                .cell(cell_sid)
                .scope("mailbox")
        };
        if let Some((id, current)) = existing {
            if current != full_message {
                self.conn.execute(
                    "UPDATE mailbox_messages SET message=?1 WHERE id=?2",
                    params![full_message, id],
                )?;
                note().emit(
                    self,
                    "blocking-cell mailbox message updated",
                    serde_json::json!({
                        "message_id": id,
                        "blocked_cells": blocking.blocked_cells,
                        "blocked_branches": blocking.blocked_branches,
                    }),
                );
            }
            return Ok(());
        }

        let priority = MailboxPriority::High;
        let id = self.enqueue_error_mailbox_message_deduped(
            priority,
            &message,
            &remediation,
            Some(squad_id),
            Some(task_name),
            Some(cell_sid),
            Some(&entity_uri),
            Some(CATEGORY),
        )?;
        self.conn.execute(
            "UPDATE mailbox_messages SET event_kind=?1 WHERE id=?2",
            params![NotifiableEventKind::SquadFailed.as_str(), id],
        )?;
        self.event_bus().publish_mailbox(MailboxNotice {
            message_id: id.clone(),
            priority: priority.as_str().to_string(),
            message: full_message,
            squad_id: Some(squad_id.to_string()),
            task: Some(task_name.to_string()),
            cell_id: Some(cell_sid.to_string()),
            entity_uri: Some(entity_uri),
            category: Some(CATEGORY.to_string()),
        });
        note().emit(
            self,
            "blocking-cell mailbox message enqueued",
            serde_json::json!({
                "message_id": id,
                "failure_count": blocking.failure_count,
                "blocked_cells": blocking.blocked_cells,
                "blocked_branches": blocking.blocked_branches,
            }),
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blocking(cells: i64, branches: i64) -> CellBlocking {
        CellBlocking {
            failure_count: 4,
            threshold: 3,
            blocked_cells: cells,
            blocked_branches: branches,
            branch: Some(("guardian-1".to_string(), "t-review".to_string())),
        }
    }

    #[test]
    fn message_names_the_counts_and_every_way_out() {
        let (message, remediation) = blocking_message("squad-1/build/compile", &blocking(4, 2));
        assert_eq!(
            message,
            "cell 'squad-1/build/compile' has failed 4 times and is holding up 4 cells and 2 review branches."
        );
        let rendered = remediation.render();
        assert!(rendered.contains("ralphus cell set-status squad-1/build/compile ignored"));
        assert!(rendered.contains("ralphus review branch disable guardian-1#t-review"));
        assert!(rendered.contains("ralphus review extract-branch guardian-1#t-review"));
        assert!(rendered.contains("ralphus cell copy squad-1/build/compile"));
    }

    #[test]
    fn message_with_only_cells_or_only_branches() {
        assert!(
            blocking_message("s/t/c", &blocking(1, 0))
                .0
                .ends_with("holding up 1 cell.")
        );
        assert!(
            blocking_message("s/t/c", &blocking(0, 1))
                .0
                .ends_with("holding up 1 review branch.")
        );
    }

    #[test]
    fn only_cell_ways_out_without_a_review_branch() {
        let mut b = blocking(1, 0);
        b.branch = None;
        assert_eq!(
            way_outs("s/t/c", &b),
            vec![
                WayOut::IgnoreAndContinue {
                    cell_ref: "s/t/c".into()
                },
                WayOut::CopyToSquad {
                    cell_ref: "s/t/c".into()
                },
            ]
        );
    }

    #[test]
    fn report_sends_once_at_the_threshold_and_not_before() {
        use crate::store::{CellOutcome, NodeState, RecordedUsage};
        let file: ralphus_core::schema::TaskFile = toml::from_str(
            "[[task]]\nname=\"t\"\nmax_retries=2\n[[task.cell]]\nid=\"a\"\ncwd=\"/r\"\nprompt=\"p\"\n[[task.cell]]\nid=\"b\"\ncwd=\"/r\"\nprompt=\"p\"\ndepends_on=[\"a\"]\n",
        )
        .unwrap();
        let mut store = Store::open_in_memory().unwrap();
        let squad = store.insert_squad(&file, Some("r"), false).unwrap();
        let fail = |store: &Store| {
            store
                .set_cell_state(&squad, 0, 0, NodeState::Running)
                .unwrap();
            store
                .record_cell_result(
                    &squad,
                    0,
                    0,
                    &CellOutcome {
                        state: NodeState::Failed,
                        usage: RecordedUsage::default(),
                        error: Some("boom".to_string()),
                        agent_session_id: None,
                    },
                )
                .unwrap();
            store.report_blocking_cell(&squad, "t", "a");
        };
        let count = |store: &Store| -> i64 {
            store
                .conn
                .query_row(
                    "SELECT COUNT(*) FROM mailbox_messages WHERE category=?1",
                    params![CATEGORY],
                    |r| r.get(0),
                )
                .unwrap()
        };
        fail(&store);
        assert_eq!(count(&store), 0, "one failure is below max_retries = 2");
        store
            .set_cell_state(&squad, 0, 0, NodeState::Pending)
            .unwrap();
        fail(&store);
        assert_eq!(count(&store), 1);
        store
            .set_cell_state(&squad, 0, 0, NodeState::Pending)
            .unwrap();
        fail(&store);
        assert_eq!(count(&store), 1, "later failures refresh, never duplicate");
    }

    #[test]
    fn is_blocking_needs_threshold_and_waiting_work() {
        assert!(blocking(1, 0).is_blocking());
        assert!(!blocking(0, 0).is_blocking());
        let mut b = blocking(2, 0);
        b.failure_count = 2;
        assert!(!b.is_blocking());
    }
}
