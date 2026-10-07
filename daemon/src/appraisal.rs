//! Appraisal store (RAL-575): the scored verdict a judge-role `prompt` proof
//! step writes through the `RALPHUS_APPRAISAL:` marker -- a 1-10 `score`, a
//! `summary`, and role-defined markdown `sections`.
//!
//! **Not a prophecy** (`docs/prophecy-design.md` §12 keeps scores out of
//! prophecies): an appraisal is one row per proof *attempt* (the latest
//! supersedes, earlier attempts stay as history), is requested rather than
//! volunteered, and must never pass through the model-driven relevance filter
//! that prophecies do.
//!
//! - **Keying**: `entity_uri` is the proof step's [`EntityUri::Proof`] string;
//!   `attempt` is one more than the highest attempt already stored for that
//!   URI, so a proof restart appends a row rather than overwriting.
//! - **Lifetime**: rows are kept until the review's PR is submitted
//!   ([`Store::mark_appraisals_published`] stamps them and prunes the
//!   superseded attempts); afterwards the latest attempt per proof is kept for
//!   as long as the squad exists. [`Store::delete_squad`] and
//!   [`Store::clear_all`] drop them with the squad.
//! - **Emits a Cartographer row on every write**, so the squad timeline shows
//!   it.

use std::collections::HashMap;

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::entity_uri::EntityUri;
use crate::store::{Result, Store, now_ms};

/// One titled markdown section of an appraisal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppraisalSectionView {
    pub title: String,
    pub body: String,
}

/// One row of the `proof_appraisals` table, as returned to API consumers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AppraisalView {
    /// The judged proof step: `proof:<squad>:<task>:<scope>:<cell>:<idx>`.
    pub entity_uri: String,
    /// 1-based; each proof restart that produces an appraisal adds one.
    pub attempt: i64,
    pub score: i64,
    /// The threshold in force when this appraisal was recorded.
    pub pass_score: i64,
    /// `score >= pass_score`.
    pub passed: bool,
    pub summary: String,
    pub sections: Vec<AppraisalSectionView>,
    pub created_at_ms: i64,
    /// Set once this appraisal has been published with its review's PR.
    pub published_at_ms: Option<i64>,
    /// Which PR it was published in.
    pub pr_id: Option<String>,
}

const APPRAISAL_COLUMNS: &str = "entity_uri, attempt, score, pass_score, passed, summary, sections, created_at_ms, published_at_ms, pr_id";

fn map_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<AppraisalView> {
    let sections_json: String = r.get(6)?;
    Ok(AppraisalView {
        entity_uri: r.get(0)?,
        attempt: r.get(1)?,
        score: r.get(2)?,
        pass_score: r.get(3)?,
        passed: r.get(4)?,
        summary: r.get(5)?,
        sections: serde_json::from_str(&sections_json).unwrap_or_default(),
        created_at_ms: r.get(7)?,
        published_at_ms: r.get(8)?,
        pr_id: r.get(9)?,
    })
}

/// The canonical [`EntityUri::Proof`] string for a proof step. A task-scoped
/// step always uses cell index `-1`, whatever the caller held.
#[must_use]
pub fn proof_entity_uri(
    squad_id: &str,
    task_idx: i64,
    scope: &str,
    cell_idx: i64,
    idx: i64,
) -> String {
    EntityUri::Proof {
        squad_id: squad_id.to_string(),
        task_idx,
        proof_scope: scope.to_string(),
        cell_idx: if scope == "task" { -1 } else { cell_idx },
        proof_idx: idx,
    }
    .to_string()
}

/// The latest appraisal of every proof in `squad_id`, keyed by entity URI.
pub(crate) fn latest_appraisals_by_uri(
    conn: &Connection,
    squad_id: &str,
) -> Result<HashMap<String, AppraisalView>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {APPRAISAL_COLUMNS} FROM proof_appraisals a
         WHERE squad_id=? AND attempt=(SELECT MAX(attempt) FROM proof_appraisals WHERE entity_uri=a.entity_uri)"
    ))?;
    let rows = stmt
        .query_map(params![squad_id], map_row)?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows
        .into_iter()
        .map(|a| (a.entity_uri.clone(), a))
        .collect())
}

impl Store {
    /// Append the appraisal one attempt of a scored proof produced, and emit
    /// its Cartographer row. `passed` is derived (`score >= pass_score`).
    #[allow(clippy::too_many_arguments)]
    pub fn record_appraisal(
        &self,
        squad_id: &str,
        entity_uri: &str,
        score: i64,
        pass_score: i64,
        summary: &str,
        sections: &[AppraisalSectionView],
    ) -> Result<AppraisalView> {
        let passed = score >= pass_score;
        let sections_json = serde_json::to_string(sections).unwrap_or_else(|_| "[]".to_string());
        let attempt: i64 = self.conn.query_row(
            "SELECT COALESCE(MAX(attempt), 0) + 1 FROM proof_appraisals WHERE entity_uri=?",
            params![entity_uri],
            |r| r.get(0),
        )?;
        self.conn.execute(
            "INSERT INTO proof_appraisals(entity_uri, squad_id, attempt, score, pass_score, passed, summary, sections, created_at_ms)
             VALUES (?,?,?,?,?,?,?,?,?)",
            params![
                entity_uri,
                squad_id,
                attempt,
                score,
                pass_score,
                passed,
                summary,
                sections_json,
                now_ms()
            ],
        )?;
        let view = self
            .appraisal_attempt(entity_uri, attempt)?
            .expect("just written");
        crate::cartographer::Note::new("appraisal")
            .scope("proof")
            .squad(squad_id)
            .emit(
                self,
                format!(
                    "appraisal recorded for {entity_uri}: {score}/10 (needs >={pass_score}) {}",
                    if passed { "PASS" } else { "FAIL" }
                ),
                serde_json::json!({
                    "entity_uri": entity_uri,
                    "attempt": attempt,
                    "score": score,
                    "pass_score": pass_score,
                    "passed": passed,
                    "sections": sections.len(),
                }),
            );
        Ok(view)
    }

    fn appraisal_attempt(&self, entity_uri: &str, attempt: i64) -> Result<Option<AppraisalView>> {
        Ok(self
            .conn
            .query_row(
                &format!(
                    "SELECT {APPRAISAL_COLUMNS} FROM proof_appraisals WHERE entity_uri=? AND attempt=?"
                ),
                params![entity_uri, attempt],
                map_row,
            )
            .optional()?)
    }

    /// Every stored attempt for one proof, oldest first -- backs
    /// `GET /api/appraisals/{entity_uri}`.
    pub fn list_appraisals_for_entity(&self, entity_uri: &str) -> Result<Vec<AppraisalView>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {APPRAISAL_COLUMNS} FROM proof_appraisals WHERE entity_uri=? ORDER BY attempt ASC"
        ))?;
        let rows = stmt
            .query_map(params![entity_uri], map_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// The latest appraisal of every proof in a squad, ordered by URI.
    pub fn latest_appraisals_for_squad(&self, squad_id: &str) -> Result<Vec<AppraisalView>> {
        let mut rows: Vec<_> = latest_appraisals_by_uri(&self.conn, squad_id)?
            .into_values()
            .collect();
        rows.sort_by(|a, b| a.entity_uri.cmp(&b.entity_uri));
        Ok(rows)
    }

    /// Record that `squad_id`'s latest appraisals were published with `pr_id`,
    /// then drop each proof's superseded attempts (the lifetime rule in the
    /// module doc). Returns how many proofs' latest appraisal was stamped.
    pub fn mark_appraisals_published(&self, squad_id: &str, pr_id: &str) -> Result<usize> {
        let latest = latest_appraisals_by_uri(&self.conn, squad_id)?;
        for a in latest.values() {
            self.mark_appraisal_published(&a.entity_uri, a.attempt, pr_id)?;
        }
        Ok(latest.len())
    }

    /// Stamp one proof's `attempt` as published with `pr_id` and drop its
    /// superseded attempts.
    pub fn mark_appraisal_published(
        &self,
        entity_uri: &str,
        attempt: i64,
        pr_id: &str,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE proof_appraisals SET published_at_ms=?, pr_id=? WHERE entity_uri=? AND attempt=?",
            params![now_ms(), pr_id, entity_uri, attempt],
        )?;
        self.conn.execute(
            "DELETE FROM proof_appraisals WHERE entity_uri=? AND attempt<?",
            params![entity_uri, attempt],
        )?;
        Ok(())
    }

    /// The latest, not-yet-published appraisal of every scored proof that
    /// belongs to review `guardian_id` -- a cell-scoped proof of one of its
    /// cells, or a task-scoped proof of a task that has such a cell -- each
    /// with the label the PR block shows for it (the proof step's `id`, else
    /// `proof N`). Ordered by entity URI so the PR block is stable.
    pub fn list_unpublished_appraisals_for_guardian(
        &self,
        guardian_id: &str,
    ) -> Result<Vec<ReviewAppraisal>> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT squad_id, task_idx, idx FROM cells WHERE review_guardian_id=?",
        )?;
        let cells: Vec<(String, i64, i64)> = stmt
            .query_map(params![guardian_id], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })?
            .collect::<std::result::Result<_, _>>()?;
        self.appraisals_for_cells(&cells, false)
    }

    /// The latest appraisal of every scored proof belonging to review branch
    /// `branch_id` of `guardian_id`, published or not -- the stored source of
    /// truth the board's appraisals tab shows, independent of what the PR body
    /// rendered. A cell belongs to the branch when its `review_branch` is the
    /// branch's name and its review link is this guardian (or, for a row
    /// predating the direct link, unset). A task-scoped proof is included for
    /// every branch holding one of its task's cells. `None` when no such
    /// branch exists.
    pub fn list_appraisals_for_branch(
        &self,
        guardian_id: &str,
        branch_id: &str,
    ) -> Result<Option<Vec<ReviewAppraisal>>> {
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
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT squad_id, task_idx, idx FROM cells
             WHERE review_branch=? AND (review_guardian_id=? OR review_guardian_id IS NULL)",
        )?;
        let cells: Vec<(String, i64, i64)> = stmt
            .query_map(params![branch, guardian_id], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })?
            .collect::<std::result::Result<_, _>>()?;
        Ok(Some(self.appraisals_for_cells(&cells, true)?))
    }

    /// The latest appraisal of every proof reachable from `cells`
    /// (`(squad_id, task_idx, cell idx)`), labelled and ordered by entity URI.
    /// Published ones are kept only when `include_published`.
    fn appraisals_for_cells(
        &self,
        cells: &[(String, i64, i64)],
        include_published: bool,
    ) -> Result<Vec<ReviewAppraisal>> {
        let mut squads: Vec<&str> = cells.iter().map(|(s, _, _)| s.as_str()).collect();
        squads.sort_unstable();
        squads.dedup();
        let mut out = Vec::new();
        for squad_id in squads {
            for (uri, a) in latest_appraisals_by_uri(&self.conn, squad_id)? {
                if !include_published && a.published_at_ms.is_some() {
                    continue;
                }
                let Some(EntityUri::Proof {
                    task_idx,
                    proof_scope,
                    cell_idx,
                    proof_idx,
                    ..
                }) = crate::entity_uri::parse(&uri)
                else {
                    continue;
                };
                let belongs = cells.iter().any(|(s, t, c)| {
                    s == squad_id && *t == task_idx && (proof_scope == "task" || *c == cell_idx)
                });
                if !belongs {
                    continue;
                }
                let vid: Option<String> = self
                    .conn
                    .query_row(
                        "SELECT vid FROM proofs WHERE squad_id=? AND task_idx=? AND scope=? AND cell_idx=? AND idx=?",
                        params![squad_id, task_idx, proof_scope, cell_idx, proof_idx],
                        |r| r.get(0),
                    )
                    .optional()?
                    .flatten();
                let label = vid
                    .filter(|v| !v.trim().is_empty())
                    .unwrap_or_else(|| format!("proof {}", proof_idx + 1));
                out.push(ReviewAppraisal {
                    label,
                    appraisal: a,
                });
            }
        }
        out.sort_by(|a, b| a.appraisal.entity_uri.cmp(&b.appraisal.entity_uri));
        Ok(out)
    }
}

/// An appraisal paired with the label its PR block is headed by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewAppraisal {
    pub label: String,
    pub appraisal: AppraisalView,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sections() -> Vec<AppraisalSectionView> {
        vec![AppraisalSectionView {
            title: "Findings".to_string(),
            body: "- a\n- b".to_string(),
        }]
    }

    #[test]
    fn task_scope_uri_uses_negative_one_cell_idx() {
        assert_eq!(
            proof_entity_uri("squad-1", 0, "task", 3, 2),
            "proof:squad-1:0:task:-1:2"
        );
        assert_eq!(
            proof_entity_uri("squad-1", 0, "cell", 3, 2),
            "proof:squad-1:0:cell:3:2"
        );
    }

    #[test]
    fn restart_appends_an_attempt_and_the_latest_wins() {
        let s = Store::open_in_memory().unwrap();
        let uri = proof_entity_uri("squad-1", 0, "cell", 0, 0);
        let first = s
            .record_appraisal("squad-1", &uri, 4, 7, "bad", &sections())
            .unwrap();
        assert_eq!((first.attempt, first.passed), (1, false));
        let second = s
            .record_appraisal("squad-1", &uri, 7, 7, "ok", &sections())
            .unwrap();
        assert_eq!((second.attempt, second.passed), (2, true));
        assert_eq!(s.list_appraisals_for_entity(&uri).unwrap().len(), 2);
        let latest = s.latest_appraisals_for_squad("squad-1").unwrap();
        assert_eq!(latest.len(), 1);
        assert_eq!(latest[0].attempt, 2);
        assert_eq!(latest[0].sections, sections());
    }

    #[test]
    fn publishing_stamps_the_latest_and_prunes_history() {
        let s = Store::open_in_memory().unwrap();
        let uri = proof_entity_uri("squad-1", 0, "cell", 0, 0);
        s.record_appraisal("squad-1", &uri, 4, 7, "bad", &sections())
            .unwrap();
        s.record_appraisal("squad-1", &uri, 8, 7, "ok", &sections())
            .unwrap();
        assert_eq!(s.mark_appraisals_published("squad-1", "pr-9").unwrap(), 1);
        let rows = s.list_appraisals_for_entity(&uri).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].attempt, 2);
        assert_eq!(rows[0].pr_id.as_deref(), Some("pr-9"));
        assert!(rows[0].published_at_ms.is_some());
    }

    fn seed_cell(s: &Store, squad: &str, task_idx: i64, idx: i64, guardian: &str) {
        s.conn
            .execute(
                "INSERT OR IGNORE INTO squads(id, state, created_at_ms, updated_at_ms) VALUES (?,'pending',0,0)",
                params![squad],
            )
            .unwrap();
        s.conn
            .execute(
                "INSERT INTO cells(squad_id, task_idx, idx, sid, agent, state, review_guardian_id)
                 VALUES (?,?,?,?,?,?,?)",
                params![squad, task_idx, idx, "sid", "claude", "done", guardian],
            )
            .unwrap();
    }

    #[test]
    fn unpublished_listing_is_scoped_to_the_review_and_skips_published() {
        let s = Store::open_in_memory().unwrap();
        seed_cell(&s, "squad-1", 0, 0, "g-1");
        seed_cell(&s, "squad-1", 0, 1, "g-2");
        let mine = proof_entity_uri("squad-1", 0, "cell", 0, 0);
        let task_wide = proof_entity_uri("squad-1", 0, "task", 0, 1);
        let other = proof_entity_uri("squad-1", 0, "cell", 1, 0);
        for uri in [&mine, &task_wide, &other] {
            s.record_appraisal("squad-1", uri, 4, 7, "bad", &sections())
                .unwrap();
        }
        let listed = s.list_unpublished_appraisals_for_guardian("g-1").unwrap();
        let uris: Vec<&str> = listed
            .iter()
            .map(|a| a.appraisal.entity_uri.as_str())
            .collect();
        assert_eq!(uris, vec![mine.as_str(), task_wide.as_str()]);
        assert_eq!(listed[0].label, "proof 1");

        s.mark_appraisal_published(&mine, 1, "pr-3").unwrap();
        let listed = s.list_unpublished_appraisals_for_guardian("g-1").unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].appraisal.entity_uri, task_wide);
        assert!(
            s.list_unpublished_appraisals_for_guardian("nobody")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn branch_listing_keeps_published_and_folds_in_task_proofs() {
        let s = Store::open_in_memory().unwrap();
        let g = s.create_guardian("r", "main", "/repo").unwrap();
        s.add_guardian_branch(&g, "feat-a").unwrap();
        s.add_guardian_branch(&g, "feat-b").unwrap();
        let branches = s.get_guardian(&g).unwrap().branches;
        let (a, b) = (&branches[0], &branches[1]);
        seed_cell(&s, "squad-1", 0, 0, &g);
        seed_cell(&s, "squad-1", 0, 1, &g);
        for (idx, branch) in [(0, "feat-a"), (1, "feat-b")] {
            s.conn
                .execute(
                    "UPDATE cells SET review_branch=? WHERE squad_id='squad-1' AND idx=?",
                    params![branch, idx],
                )
                .unwrap();
        }
        let mine = proof_entity_uri("squad-1", 0, "cell", 0, 0);
        let theirs = proof_entity_uri("squad-1", 0, "cell", 1, 0);
        let task_wide = proof_entity_uri("squad-1", 0, "task", 0, 1);
        for uri in [&mine, &theirs, &task_wide] {
            s.record_appraisal("squad-1", uri, 4, 7, "bad", &sections())
                .unwrap();
        }
        s.mark_appraisal_published(&mine, 1, "pr-3").unwrap();

        let uris = |bid: &str| -> Vec<String> {
            s.list_appraisals_for_branch(&g, bid)
                .unwrap()
                .unwrap()
                .into_iter()
                .map(|a| a.appraisal.entity_uri)
                .collect()
        };
        assert_eq!(uris(&a.id), vec![mine.clone(), task_wide.clone()]);
        assert_eq!(uris(&b.id), vec![theirs, task_wide]);
        assert!(s.list_appraisals_for_branch(&g, "nope").unwrap().is_none());
    }
}
