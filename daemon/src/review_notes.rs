//! Review notes (RAL-670): the one list the board's Notes tab shows for a
//! review branch, merging three kinds of note that already live in separate
//! stores:
//!
//! - **appraisal** -- the latest scored verdict of each proof on the branch
//!   (`appraisal.rs`);
//! - **live_action** -- a review live action attributed to the branch, with the
//!   latest run's verdict (`live_actions.rs`, `live_results.rs`). An action
//!   spanning several branches appears on every branch it is attributed to;
//! - **ghost** -- the most recently written handoff among the ghosts of the
//!   branch's cells and the branch's review-worktree ghost. A ghost
//!   accumulates its own history, so only the latest one is listed.
//!
//! Every note carries `type`, a stable `id`, a `title` and `at_ms`; the rest of
//! its fields depend on the type. Notes are ordered newest first.

use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};

use crate::followup::PendingFollowupItem;
use crate::ghost::{GhostView, cell_uri, review_uri};
use crate::live_actions::guardian_live_actions_conn;
use crate::live_runs::guardian_live_runs_conn;
use crate::store::{Result, Store};

/// The ghost to list: the most recently updated one with content, `None` when
/// there is none.
fn latest_ghost(ghosts: Vec<GhostView>) -> Option<GhostView> {
    ghosts
        .into_iter()
        .filter(|g| !g.content.trim().is_empty())
        .max_by_key(|g| g.updated_at_ms)
}

/// The ghost note JSON for `ghost`.
fn ghost_note(ghost: &GhostView) -> Value {
    json!({
        "type": "ghost",
        "id": ghost.owner_uri,
        "title": "Ghost",
        "at_ms": ghost.updated_at_ms,
        "owner_uri": ghost.owner_uri,
        "content": ghost.content,
    })
}

/// The deferral note JSON for `item`. `ignored` and `squad_id` let the board
/// badge an ignored deferral and link an accepted one to its follow-up squad.
fn deferral_note(item: &PendingFollowupItem) -> Value {
    json!({
        "type": "deferral",
        "id": format!("deferral:{}", item.item.prophecy_id),
        "title": item.task_name.clone().unwrap_or_else(|| "Deferral".to_string()),
        "at_ms": item.created_at_ms,
        "owner_uri": item.item.entity_uri,
        "content": item.item.body,
        "ignored": item.ignored,
        "accepted": item.accepted,
        "squad_id": item.squad_id,
    })
}

/// Sort notes newest first; ties keep their relative order.
fn sort_newest_first(notes: &mut [Value]) {
    notes.sort_by_key(|n| std::cmp::Reverse(n["at_ms"].as_i64().unwrap_or(0)));
}

impl Store {
    /// Every note of review branch `branch_id` of `guardian_id`, newest first.
    /// `None` when no such branch exists.
    ///
    /// # Errors
    ///
    /// On a database failure.
    pub fn list_review_notes(
        &self,
        guardian_id: &str,
        branch_id: &str,
    ) -> Result<Option<Vec<Value>>> {
        let branch: Option<String> = self
            .conn
            .query_row(
                "SELECT branch FROM guardian_branches WHERE guardian_id=? AND id=?",
                params![guardian_id, branch_id],
                |r| r.get(0),
            )
            .optional()?;
        let Some(branch) = branch else {
            return Ok(None);
        };
        let mut notes: Vec<Value> = Vec::new();

        for r in self
            .list_appraisals_for_branch(guardian_id, branch_id)?
            .unwrap_or_default()
        {
            let mut v = serde_json::to_value(&r.appraisal).unwrap_or_default();
            v["type"] = json!("appraisal");
            v["id"] = json!(format!(
                "{}#{}",
                r.appraisal.entity_uri, r.appraisal.attempt
            ));
            v["title"] = json!(r.label);
            v["at_ms"] = json!(r.appraisal.created_at_ms);
            notes.push(v);
        }

        let runs = guardian_live_runs_conn(&self.conn, guardian_id)?;
        for action in guardian_live_actions_conn(&self.conn, guardian_id)? {
            if !self
                .live_action_branch_ids(guardian_id, action.idx)?
                .iter()
                .any(|b| b == branch_id)
            {
                continue;
            }
            // `runs` is newest first.
            let latest = runs
                .iter()
                .filter(|r| r.action_idx == action.idx && r.state == "finished")
                .find_map(|r| {
                    let evidence = self.live_run_evidence(&r.run_id).ok()?;
                    Some((r, evidence))
                });
            let (verdict, verdict_summary, report, run_at, unbacked) = match &latest {
                Some((run, evidence)) => (
                    evidence.verdict.clone(),
                    evidence.verdict_summary.clone(),
                    run.report.clone(),
                    run.finished_at_ms,
                    crate::live_results::verdict_unbacked(evidence),
                ),
                None => (None, None, None, None, false),
            };
            notes.push(json!({
                "type": "live_action",
                "id": format!("live-action:{guardian_id}:{}", action.idx),
                "title": action.label,
                "at_ms": run_at.unwrap_or(action.created_at_ms),
                "prompt": action.prompt,
                "verdict": verdict,
                "verdict_summary": verdict_summary,
                "verdict_unbacked": unbacked,
                "report": report,
            }));
        }

        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT squad_id, task_idx, idx FROM cells
             WHERE review_branch=? AND (review_guardian_id=? OR review_guardian_id IS NULL)",
        )?;
        let cells: Vec<(String, i64, i64)> = stmt
            .query_map(params![branch, guardian_id], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })?
            .collect::<std::result::Result<_, _>>()?;
        let mut uris: Vec<String> = cells
            .iter()
            .map(|(squad, task, cell)| cell_uri(squad, *task, *cell))
            .collect();
        uris.push(review_uri(guardian_id, Some(branch_id)));
        let mut ghosts = Vec::new();
        for uri in &uris {
            if let Some(g) = self.get_ghost(uri)? {
                ghosts.push(g);
            }
        }
        if let Some(g) = latest_ghost(ghosts) {
            notes.push(ghost_note(&g));
        }

        // Deferrals whose writing cell has no review branch belong to no
        // branch and stay in the follow-up list only.
        for d in self.pending_followup_items(guardian_id)? {
            if d.branch.as_deref() != Some(branch.as_str()) {
                continue;
            }
            notes.push(deferral_note(&d));
        }

        sort_newest_first(&mut notes);
        Ok(Some(notes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ghost(uri: &str, content: &str, updated: i64) -> GhostView {
        GhostView {
            owner_uri: uri.to_string(),
            kind: "cell".to_string(),
            squad_id: None,
            guardian_id: None,
            content: content.to_string(),
            user_note: None,
            revision: None,
            created_at_ms: 1,
            updated_at_ms: updated,
        }
    }

    #[test]
    fn latest_ghost_is_the_most_recently_updated() {
        let picked = latest_ghost(vec![
            ghost("cell:s:0:0", "old", 10),
            ghost("cell:s:0:1", "new", 30),
            ghost("review:g:b", "mid", 20),
        ])
        .expect("a ghost");
        assert_eq!(picked.owner_uri, "cell:s:0:1");
    }

    #[test]
    fn latest_ghost_skips_blank_content_and_handles_none() {
        assert!(latest_ghost(vec![]).is_none());
        let picked = latest_ghost(vec![
            ghost("cell:s:0:0", "kept", 10),
            ghost("cell:s:0:1", "  \n", 99),
        ])
        .expect("a ghost");
        assert_eq!(picked.owner_uri, "cell:s:0:0");
    }

    #[test]
    fn ghost_note_has_the_common_fields() {
        let note = ghost_note(&ghost("cell:s:0:0", "body", 7));
        assert_eq!(note["type"], "ghost");
        assert_eq!(note["id"], "cell:s:0:0");
        assert_eq!(note["at_ms"], 7);
        assert_eq!(note["content"], "body");
    }

    #[test]
    fn deferral_note_carries_ignored_and_squad() {
        let item = PendingFollowupItem {
            item: crate::followup::FollowupItem {
                prophecy_id: 4,
                entity_uri: "cell:s:0:0".to_string(),
                body: "later".to_string(),
                prompt: None,
                agent: None,
                model: None,
            },
            attempt: 1,
            ignored: true,
            created_at_ms: 9,
            branch: Some("b".to_string()),
            task_name: Some("t".to_string()),
            squad_id: Some("squad-x".to_string()),
            accepted: true,
        };
        let note = deferral_note(&item);
        assert_eq!(note["type"], "deferral");
        assert_eq!(note["id"], "deferral:4");
        assert_eq!(note["title"], "t");
        assert_eq!(note["at_ms"], 9);
        assert_eq!(note["content"], "later");
        assert_eq!(note["ignored"], true);
        assert_eq!(note["squad_id"], "squad-x");
    }

    #[test]
    fn notes_sort_newest_first() {
        let mut notes = vec![
            json!({"id": "a", "at_ms": 1}),
            json!({"id": "b", "at_ms": 3}),
            json!({"id": "c", "at_ms": 2}),
        ];
        sort_newest_first(&mut notes);
        let ids: Vec<&str> = notes.iter().map(|n| n["id"].as_str().unwrap()).collect();
        assert_eq!(ids, ["b", "c", "a"]);
    }

    #[test]
    fn unknown_branch_has_no_notes() {
        let store = Store::open_in_memory().expect("store");
        assert!(store.list_review_notes("g", "branch-1").unwrap().is_none());
    }
}
