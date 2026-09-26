//! Per-user hidden squads, reviews, and (RAL-365) individual tasks.
//!
//! A hidden item changes only what one user chooses to see. It does not alter
//! the squad/review/task itself, and deleting the owning entity removes the
//! preference so a later reuse of its sequential id cannot inherit it.
//!
//! A hidden task and its owning hidden squad are independent rows, unioned
//! at read time by callers (the board's Tasks tab) -- unhiding the squad
//! leaves an explicitly-hidden task of it still hidden, and hiding a squad
//! never writes a task row.

use rusqlite::params;
use serde::Serialize;

use crate::store::{Result as StoreResult, Store, StoreError, now_ms};

/// One per-user hidden item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HiddenItem {
    /// `"squad"`, `"review"`, or `"task"`.
    pub kind: String,
    /// Set for `kind == "squad"` or `kind == "task"` (a task's owning squad).
    pub squad_id: Option<String>,
    /// Internal guardian id, set only when `kind == "review"`.
    pub guardian_id: Option<String>,
    /// Set only when `kind == "task"`.
    pub task_idx: Option<i64>,
    /// Unix epoch milliseconds when the item was first hidden.
    pub hidden_at_ms: i64,
}

impl Store {
    /// Hide a squad for `user_name`. Repeated calls preserve the original
    /// timestamp and otherwise do nothing.
    pub fn hide_squad(&self, user_name: &str, squad_id: &str) -> StoreResult<()> {
        self.get_squad(squad_id)?;
        self.conn.execute(
            "INSERT INTO hidden_items(kind, squad_id, guardian_id, task_idx, user_name, hidden_at_ms)
             VALUES('squad', ?, NULL, NULL, ?, ?)
             ON CONFLICT(user_name, squad_id) WHERE kind='squad' DO NOTHING",
            params![squad_id, user_name, now_ms()],
        )?;
        Ok(())
    }

    /// Hide a review for `user_name`. Repeated calls preserve the original
    /// timestamp and otherwise do nothing.
    pub fn hide_review(&self, user_name: &str, guardian_id: &str) -> StoreResult<()> {
        self.get_guardian(guardian_id)?;
        self.conn.execute(
            "INSERT INTO hidden_items(kind, squad_id, guardian_id, task_idx, user_name, hidden_at_ms)
             VALUES('review', NULL, ?, NULL, ?, ?)
             ON CONFLICT(user_name, guardian_id) DO NOTHING",
            params![guardian_id, user_name, now_ms()],
        )?;
        Ok(())
    }

    /// Hide one task for `user_name`, independent of its owning squad's own
    /// hidden state. Repeated calls preserve the original timestamp and
    /// otherwise do nothing.
    pub fn hide_task(&self, user_name: &str, squad_id: &str, task_idx: i64) -> StoreResult<()> {
        self.get_task_name(squad_id, task_idx)?;
        self.conn.execute(
            "INSERT INTO hidden_items(kind, squad_id, guardian_id, task_idx, user_name, hidden_at_ms)
             VALUES('task', ?, NULL, ?, ?, ?)
             ON CONFLICT(user_name, squad_id, task_idx) WHERE kind='task' DO NOTHING",
            params![squad_id, task_idx, user_name, now_ms()],
        )?;
        Ok(())
    }

    /// Re-enable a squad for `user_name`. Missing preferences are a no-op.
    /// Leaves any explicitly-hidden task of this squad untouched (the
    /// squad/task union rule).
    pub fn unhide_squad(&self, user_name: &str, squad_id: &str) -> StoreResult<()> {
        self.conn.execute(
            "DELETE FROM hidden_items WHERE user_name=? AND kind='squad' AND squad_id=?",
            params![user_name, squad_id],
        )?;
        Ok(())
    }

    /// Re-enable a review for `user_name`. Missing preferences are a no-op.
    pub fn unhide_review(&self, user_name: &str, guardian_id: &str) -> StoreResult<()> {
        self.conn.execute(
            "DELETE FROM hidden_items WHERE user_name=? AND kind='review' AND guardian_id=?",
            params![user_name, guardian_id],
        )?;
        Ok(())
    }

    /// Re-enable one task for `user_name`. Missing preferences are a no-op.
    /// Does not touch the owning squad's own hidden state.
    pub fn unhide_task(&self, user_name: &str, squad_id: &str, task_idx: i64) -> StoreResult<()> {
        self.conn.execute(
            "DELETE FROM hidden_items WHERE user_name=? AND kind='task' AND squad_id=? AND task_idx=?",
            params![user_name, squad_id, task_idx],
        )?;
        Ok(())
    }

    /// Batch form of [`Self::hide_squad`]/[`Self::unhide_squad`] -- applies to
    /// every id under the one `Store` lock the caller already holds, instead
    /// of one HTTP round trip per id (the board's multi-select Hide/Unhide
    /// menu). Best-effort per id: a failure (e.g. the squad was deleted
    /// concurrently) is reported back rather than aborting the rest of the
    /// batch.
    pub fn set_squads_hidden(
        &self,
        user_name: &str,
        squad_ids: &[String],
        hidden: bool,
    ) -> Vec<(String, StoreError)> {
        squad_ids
            .iter()
            .filter_map(|squad_id| {
                let result = if hidden {
                    self.hide_squad(user_name, squad_id)
                } else {
                    self.unhide_squad(user_name, squad_id)
                };
                result.err().map(|e| (squad_id.clone(), e))
            })
            .collect()
    }

    /// Batch form of [`Self::hide_task`]/[`Self::unhide_task`] -- applies to
    /// every `(squad_id, task_idx)` pair under the one `Store` lock the
    /// caller already holds, instead of one HTTP round trip per task (the
    /// board's multi-select Hide/Unhide menu). Best-effort per pair: a
    /// failure (e.g. the task was deleted concurrently) is reported back
    /// rather than aborting the rest of the batch.
    pub fn set_tasks_hidden(
        &self,
        user_name: &str,
        tasks: &[(String, i64)],
        hidden: bool,
    ) -> Vec<((String, i64), StoreError)> {
        tasks
            .iter()
            .filter_map(|(squad_id, task_idx)| {
                let result = if hidden {
                    self.hide_task(user_name, squad_id, *task_idx)
                } else {
                    self.unhide_task(user_name, squad_id, *task_idx)
                };
                result.err().map(|e| ((squad_id.clone(), *task_idx), e))
            })
            .collect()
    }

    /// List all items hidden by one user, newest first.
    pub fn list_hidden(&self, user_name: &str) -> StoreResult<Vec<HiddenItem>> {
        Self::list_hidden_conn(&self.conn, user_name)
    }

    /// [`Self::list_hidden`] against an explicit connection (WS-E.2) -- the
    /// board reads this to apply its hidden-item filters.
    pub(crate) fn list_hidden_conn(
        conn: &rusqlite::Connection,
        user_name: &str,
    ) -> StoreResult<Vec<HiddenItem>> {
        let mut stmt = conn.prepare(
            "SELECT kind, squad_id, guardian_id, task_idx, hidden_at_ms
             FROM hidden_items WHERE user_name=?
             ORDER BY hidden_at_ms DESC, kind, COALESCE(squad_id, guardian_id), task_idx",
        )?;
        let rows = stmt
            .query_map(params![user_name], |row| {
                Ok(HiddenItem {
                    kind: row.get(0)?,
                    squad_id: row.get(1)?,
                    guardian_id: row.get(2)?,
                    task_idx: row.get(3)?,
                    hidden_at_ms: row.get(4)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use ralphus_core::schema::TaskFile;

    use super::*;
    use crate::store::StoreError;

    fn fixture() -> (Store, String, String) {
        let mut store = Store::open_in_memory().unwrap();
        store.create_user("alice").unwrap();
        store.create_user("bob").unwrap();
        let file: TaskFile =
            toml::from_str("[[task]]\nname='build'\n[[task.cell]]\ncwd='/repo'\nprompt='go'\n")
                .unwrap();
        let squad_id = store.insert_squad(&file, None, false).unwrap();
        let guardian_id = store.create_guardian("review", "main", "/repo").unwrap();
        (store, squad_id, guardian_id)
    }

    #[test]
    fn hidden_items_are_per_user_and_idempotent() {
        let (store, squad_id, guardian_id) = fixture();
        store.hide_squad("alice", &squad_id).unwrap();
        let first = store.list_hidden("alice").unwrap()[0].hidden_at_ms;
        store.hide_squad("alice", &squad_id).unwrap();
        store.hide_review("alice", &guardian_id).unwrap();

        let alice = store.list_hidden("alice").unwrap();
        assert_eq!(alice.len(), 2);
        assert_eq!(
            alice
                .iter()
                .find(|item| item.kind == "squad")
                .unwrap()
                .hidden_at_ms,
            first
        );
        assert!(store.list_hidden("bob").unwrap().is_empty());

        store.unhide_squad("alice", &squad_id).unwrap();
        store.unhide_squad("alice", &squad_id).unwrap();
        assert_eq!(store.list_hidden("alice").unwrap().len(), 1);
    }

    #[test]
    fn deleting_entities_removes_hidden_preferences() {
        let (mut store, squad_id, guardian_id) = fixture();
        store.hide_squad("alice", &squad_id).unwrap();
        store.hide_review("alice", &guardian_id).unwrap();

        store.delete_squad(&squad_id).unwrap();
        store.delete_guardian(&guardian_id).unwrap();
        assert!(store.list_hidden("alice").unwrap().is_empty());
    }

    #[test]
    fn hiding_unknown_entity_fails() {
        let (store, _, _) = fixture();
        assert!(matches!(
            store.hide_squad("alice", "squad-missing"),
            Err(StoreError::NotFound)
        ));
        assert!(matches!(
            store.hide_review("alice", "guardian-missing"),
            Err(StoreError::NotFound)
        ));
    }

    #[test]
    fn set_squads_hidden_applies_the_whole_batch_and_reports_partial_failure() {
        let (mut store, squad_id, _) = fixture();
        let file: TaskFile =
            toml::from_str("[[task]]\nname='build'\n[[task.cell]]\ncwd='/repo'\nprompt='go'\n")
                .unwrap();
        let squad_id_2 = store.insert_squad(&file, None, false).unwrap();

        let ids = vec![
            squad_id.clone(),
            squad_id_2.clone(),
            "squad-missing".to_string(),
        ];
        let failed = store.set_squads_hidden("alice", &ids, true);
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].0, "squad-missing");
        assert!(matches!(failed[0].1, StoreError::NotFound));

        let alice = store.list_hidden("alice").unwrap();
        assert_eq!(alice.len(), 2);
        assert!(alice.iter().all(|item| item.kind == "squad"));

        // unhide_squad has no existence check (it's a plain idempotent
        // delete), so unhiding an unknown id is not a failure the way
        // hiding one is.
        let failed = store.set_squads_hidden("alice", &ids, false);
        assert!(failed.is_empty());
        assert!(store.list_hidden("alice").unwrap().is_empty());
    }

    #[test]
    fn hide_task_and_unhide_task_are_per_user_and_idempotent() {
        let (store, squad_id, _) = fixture();
        store.hide_task("alice", &squad_id, 0).unwrap();
        let first = store.list_hidden("alice").unwrap()[0].hidden_at_ms;
        store.hide_task("alice", &squad_id, 0).unwrap();

        let alice = store.list_hidden("alice").unwrap();
        assert_eq!(alice.len(), 1);
        assert_eq!(alice[0].kind, "task");
        assert_eq!(alice[0].squad_id.as_deref(), Some(squad_id.as_str()));
        assert_eq!(alice[0].task_idx, Some(0));
        assert_eq!(alice[0].hidden_at_ms, first);
        assert!(store.list_hidden("bob").unwrap().is_empty());

        store.unhide_task("alice", &squad_id, 0).unwrap();
        store.unhide_task("alice", &squad_id, 0).unwrap();
        assert!(store.list_hidden("alice").unwrap().is_empty());
    }

    #[test]
    fn hiding_task_and_squad_are_independent_rows_unioned_at_read_time() {
        // The interview-decision union rule: a hidden task and its owning
        // hidden squad are separate rows. Hiding the squad must not write a
        // task row, and unhiding the squad must leave an explicitly-hidden
        // task of it still hidden.
        let (store, squad_id, _) = fixture();
        store.hide_task("alice", &squad_id, 0).unwrap();
        store.hide_squad("alice", &squad_id).unwrap();

        let alice = store.list_hidden("alice").unwrap();
        assert_eq!(
            alice.len(),
            2,
            "squad and task must coexist as separate rows"
        );
        assert!(
            alice
                .iter()
                .any(|i| i.kind == "task" && i.task_idx == Some(0))
        );
        assert!(
            alice
                .iter()
                .any(|i| i.kind == "squad" && i.task_idx.is_none())
        );

        // Hiding the squad did not implicitly write/touch the task row.
        store.unhide_squad("alice", &squad_id).unwrap();
        let alice = store.list_hidden("alice").unwrap();
        assert_eq!(
            alice.len(),
            1,
            "unhiding the squad must leave the explicitly-hidden task alone"
        );
        assert_eq!(alice[0].kind, "task");
        assert_eq!(alice[0].task_idx, Some(0));

        store.unhide_task("alice", &squad_id, 0).unwrap();
        assert!(store.list_hidden("alice").unwrap().is_empty());
    }

    #[test]
    fn hiding_unknown_task_fails() {
        let (store, squad_id, _) = fixture();
        assert!(matches!(
            store.hide_task("alice", &squad_id, 99),
            Err(StoreError::NotFound)
        ));
        assert!(matches!(
            store.hide_task("alice", "squad-missing", 0),
            Err(StoreError::NotFound)
        ));
    }

    #[test]
    fn deleting_squad_removes_hidden_task_preferences() {
        let (mut store, squad_id, _) = fixture();
        store.hide_task("alice", &squad_id, 0).unwrap();
        store.hide_squad("alice", &squad_id).unwrap();
        assert_eq!(store.list_hidden("alice").unwrap().len(), 2);

        store.delete_squad(&squad_id).unwrap();
        assert!(
            store.list_hidden("alice").unwrap().is_empty(),
            "deleting a squad must purge both its hidden-squad row and any hidden-task rows"
        );
    }

    #[test]
    fn set_tasks_hidden_applies_the_whole_batch_and_reports_partial_failure() {
        let (mut store, squad_id, _) = fixture();
        let file: TaskFile = toml::from_str(
            "[[task]]\nname='build'\n[[task.cell]]\ncwd='/repo'\nprompt='go'\n\
             [[task]]\nname='test'\n[[task.cell]]\ncwd='/repo'\nprompt='go'\n",
        )
        .unwrap();
        let squad_id_2 = store.insert_squad(&file, None, false).unwrap();

        let pairs = vec![
            (squad_id.clone(), 0),
            (squad_id_2.clone(), 1),
            (squad_id_2.clone(), 99),
        ];
        let failed = store.set_tasks_hidden("alice", &pairs, true);
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].0, (squad_id_2.clone(), 99));
        assert!(matches!(failed[0].1, StoreError::NotFound));

        let alice = store.list_hidden("alice").unwrap();
        assert_eq!(alice.len(), 2);
        assert!(alice.iter().all(|item| item.kind == "task"));

        let failed = store.set_tasks_hidden("alice", &pairs, false);
        assert!(failed.is_empty());
        assert!(store.list_hidden("alice").unwrap().is_empty());
    }
}
