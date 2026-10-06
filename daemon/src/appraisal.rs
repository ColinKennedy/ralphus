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
        let now = now_ms();
        for a in latest.values() {
            self.conn.execute(
                "UPDATE proof_appraisals SET published_at_ms=?, pr_id=? WHERE entity_uri=? AND attempt=?",
                params![now, pr_id, a.entity_uri, a.attempt],
            )?;
            self.conn.execute(
                "DELETE FROM proof_appraisals WHERE entity_uri=? AND attempt<?",
                params![a.entity_uri, a.attempt],
            )?;
        }
        Ok(latest.len())
    }
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
}
