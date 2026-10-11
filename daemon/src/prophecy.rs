//! Prophecy store: a durable, append-only record of what an agent learned
//! while it worked (`docs/prophecy-design.md`) -- written *during* the work,
//! carried across attempts and into review, and landing in the pull request
//! as prose a human reads.
//!
//! **Not a bigger ghost** (see the design doc's §3): a ghost is one row per
//! owner, merged on rewrite, cascade-deleted with its squad/guardian, and
//! read by the next cell's prompt. A prophecy is append-only (one row per
//! insight, attempts stay distinct), outlives its owning squad/guardian (the
//! PR is the terminal home -- §11.2), and is read by a human, not fed back
//! into a prompt (§11.3, deferred to a later phase).
//!
//! - **Keying**: `entity_uri` reuses [`crate::entity_uri::EntityUri`]'s
//!   string form (`cell:…`/`guardian:…`) -- no new addressing scheme.
//!   `attempt` keeps a cell's restarts distinct rather than folding them
//!   together the way a ghost does.
//! - **`kind` is a closed enum** ([`ProphecyKind`]), per §11.1's resolution:
//!   an open string column becomes forty synonyms for "note" within a month
//!   and nothing stays filterable. `discovery`/`decision`/`hazard`/
//!   `deferred`/`unconfirmed` covers the cases in the design doc's examples
//!   (auto-fix/conflict-resolution decisions, a hazard left behind in a
//!   rebase); revisit if a real write site needs a fifth.
//! - **Survives its squad/guardian's deletion** (§11.2): `squad_id`/
//!   `guardian_id` are informational cross-references, not `ON DELETE
//!   CASCADE` targets -- [`Store::delete_squad`]/[`Store::delete_guardian`]/
//!   [`Store::clear_all`] null them out (orphan-retention) rather than
//!   deleting the row, since deleting a squad must never silently strip the
//!   reasoning out of an already-open PR.
//! - **Emits a Cartographer row on every write** (§3): unlike Cartographer
//!   itself (30-day/50k-row retention), a prophecy must survive until it
//!   reaches a PR, so it gets its own table -- but still shows up in the
//!   squad timeline for free via the Cartographer row [`Store::add_prophecy`]
//!   emits alongside the durable write.

use rusqlite::params;
use serde::Serialize;

use crate::store::{Result, Store, now_ms};

/// A prophecy's kind (§11.1): closed set, not an open string -- see the
/// module doc comment for why.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProphecyKind {
    /// Something learned that the diff alone couldn't show.
    Discovery,
    /// A choice made among alternatives, and why.
    Decision,
    /// A risk noticed but not (yet) fixed.
    Hazard,
    /// Follow-up work someone should do later, suggested as a task. Never a
    /// "could not run or check this" note -- that is [`Self::Unconfirmed`].
    Deferred,
    /// Something the agent could not verify or does not know (a test that
    /// could not run, behavior not checked live, an untested assumption).
    Unconfirmed,
}

impl ProphecyKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Discovery => "discovery",
            Self::Decision => "decision",
            Self::Hazard => "hazard",
            Self::Deferred => "deferred",
            Self::Unconfirmed => "unconfirmed",
        }
    }
}

impl std::str::FromStr for ProphecyKind {
    type Err = ();

    fn from_str(s: &str) -> std::result::Result<Self, ()> {
        match s {
            "discovery" => Ok(Self::Discovery),
            "decision" => Ok(Self::Decision),
            "hazard" => Ok(Self::Hazard),
            "deferred" => Ok(Self::Deferred),
            "unconfirmed" => Ok(Self::Unconfirmed),
            _ => Err(()),
        }
    }
}

/// Where a cell's prophecies are filed: its position, working directory (for
/// the revision stamp) and current attempt (RAL-595).
#[derive(Debug, Clone)]
pub struct CellProphecyTarget {
    pub task_idx: i64,
    pub idx: i64,
    pub cwd: Option<String>,
    pub attempt: i64,
}

/// One row of the `prophecies` table, as returned to API/internal consumers.
#[derive(Debug, Clone, Serialize)]
pub struct ProphecyView {
    pub id: i64,
    /// Owner: `cell:…` / `guardian:…` (see [`crate::entity_uri::EntityUri`]).
    pub entity_uri: String,
    /// Keeps a cell's restarts distinct; `0` for a daemon-side (non-cell)
    /// writer, which has no attempt concept of its own.
    pub attempt: i64,
    pub kind: String,
    pub body: String,
    /// Opaque, VCS-agnostic revision marker, best-effort (mirrors
    /// [`crate::ghost::current_revision`]). `None` when unavailable.
    pub revision: Option<String>,
    /// Owning squad id, if any. Cross-reference only -- see the module doc
    /// comment on why this is never a cascade-delete target.
    pub squad_id: Option<String>,
    /// Owning guardian (review) id, if any. Same non-cascading note as
    /// `squad_id`.
    pub guardian_id: Option<String>,
    pub created_at_ms: i64,
    /// Set once this prophecy has been folded into a PR body (phase 4).
    pub published_at_ms: Option<i64>,
    /// Which PR it landed in, once published.
    pub pr_id: Option<String>,
}

struct ProphecyRow {
    id: i64,
    entity_uri: String,
    attempt: i64,
    kind: String,
    body: String,
    revision: Option<String>,
    squad_id: Option<String>,
    guardian_id: Option<String>,
    created_at_ms: i64,
    published_at_ms: Option<i64>,
    pr_id: Option<String>,
}

impl From<ProphecyRow> for ProphecyView {
    fn from(r: ProphecyRow) -> Self {
        Self {
            id: r.id,
            entity_uri: r.entity_uri,
            attempt: r.attempt,
            kind: r.kind,
            body: r.body,
            revision: r.revision,
            squad_id: r.squad_id,
            guardian_id: r.guardian_id,
            created_at_ms: r.created_at_ms,
            published_at_ms: r.published_at_ms,
            pr_id: r.pr_id,
        }
    }
}

const PROPHECY_COLUMNS: &str = "id, entity_uri, attempt, kind, body, revision, squad_id, guardian_id, created_at_ms, published_at_ms, pr_id";

fn map_prophecy_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<ProphecyRow> {
    Ok(ProphecyRow {
        id: r.get(0)?,
        entity_uri: r.get(1)?,
        attempt: r.get(2)?,
        kind: r.get(3)?,
        body: r.get(4)?,
        revision: r.get(5)?,
        squad_id: r.get(6)?,
        guardian_id: r.get(7)?,
        created_at_ms: r.get(8)?,
        published_at_ms: r.get(9)?,
        pr_id: r.get(10)?,
    })
}

/// A filtered, paginated query against the `prophecies` table (mirrors
/// `crate::cartographer::CartographerFilter`'s shape, scaled down to what a
/// prophecy actually needs filtering by).
#[derive(Debug, Clone)]
pub struct ProphecyFilter {
    pub entity_uri: Option<String>,
    pub squad_id: Option<String>,
    pub guardian_id: Option<String>,
    pub limit: i64,
    pub offset: i64,
}

impl Default for ProphecyFilter {
    fn default() -> Self {
        Self {
            entity_uri: None,
            squad_id: None,
            guardian_id: None,
            limit: 100,
            offset: 0,
        }
    }
}

/// The key a prophecy is deduplicated on: its body with all whitespace runs
/// collapsed, so a marker re-wrapped or re-indented on restatement still
/// matches the first write.
fn dedupe_key(body: &str) -> String {
    body.split_whitespace().collect::<Vec<_>>().join(" ")
}

impl Store {
    /// Append one prophecy. Never merges/overwrites an existing row for the
    /// same `entity_uri` -- unlike [`Store::upsert_ghost`], this table is
    /// append-only by design (§3: "Attempt 1…N stay distinct"). The one
    /// exception is an exact repeat (same entity, attempt, kind and
    /// whitespace-normalized body): it returns the existing row unchanged.
    ///
    /// Also emits a Cartographer row (module doc comment) so the write shows
    /// up in the squad timeline for free, without needing every call site to
    /// remember to log it separately.
    #[allow(clippy::too_many_arguments)]
    pub fn add_prophecy(
        &self,
        entity_uri: &str,
        attempt: i64,
        kind: ProphecyKind,
        body: &str,
        revision: Option<&str>,
        squad_id: Option<&str>,
        guardian_id: Option<&str>,
    ) -> Result<ProphecyView> {
        let now = now_ms();
        let body_key = dedupe_key(body);
        // RAL-595: the same insight reaches the daemon more than once (an
        // inline marker restated in the closing message, the live event
        // plus the at-exit backstop). `INSERT OR IGNORE` against the unique
        // (entity, attempt, kind, body_key) index collapses those into the
        // first row.
        let inserted = self.conn.execute(
            "INSERT OR IGNORE INTO prophecies(entity_uri, attempt, kind, body, revision, squad_id, guardian_id, created_at_ms, body_key)
             VALUES (?,?,?,?,?,?,?,?,?)",
            params![
                entity_uri,
                attempt,
                kind.as_str(),
                body,
                revision,
                squad_id,
                guardian_id,
                now,
                body_key
            ],
        )?;
        if inserted == 0 {
            let id: i64 = self.conn.query_row(
                "SELECT id FROM prophecies WHERE entity_uri=? AND attempt=? AND kind=? AND body_key=?",
                params![entity_uri, attempt, kind.as_str(), body_key],
                |r| r.get(0),
            )?;
            return Ok(self.get_prophecy(id)?.expect("conflicting row exists"));
        }
        let id = self.conn.last_insert_rowid();
        let view = self.get_prophecy(id)?.expect("just written");
        let mut note = crate::cartographer::Note::new("prophecy").scope("prophecy");
        if let Some(s) = squad_id {
            note = note.squad(s);
        }
        if let Some(g) = guardian_id {
            note = note.guardian(g);
        }
        note.emit(
            self,
            format!("prophecy recorded for {entity_uri}"),
            serde_json::json!({"kind": kind.as_str(), "attempt": attempt, "len": body.len()}),
        );
        Ok(view)
    }

    /// Fetch one prophecy by its row id.
    pub fn get_prophecy(&self, id: i64) -> Result<Option<ProphecyView>> {
        use rusqlite::OptionalExtension;
        Ok(self
            .conn
            .query_row(
                &format!("SELECT {PROPHECY_COLUMNS} FROM prophecies WHERE id=?"),
                params![id],
                map_prophecy_row,
            )
            .optional()?
            .map(ProphecyView::from))
    }

    /// All prophecies recorded for one owner URI, oldest first (append
    /// order) -- backs `ralphus prophecy show <entity-uri>`.
    pub fn list_prophecies_for_entity(&self, entity_uri: &str) -> Result<Vec<ProphecyView>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {PROPHECY_COLUMNS} FROM prophecies WHERE entity_uri=? ORDER BY id ASC"
        ))?;
        let rows = stmt
            .query_map(params![entity_uri], map_prophecy_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows.into_iter().map(ProphecyView::from).collect())
    }

    /// Resolve the cell a runner event belongs to (`squad_id` + the owning
    /// task's name + the cell's `sid`) into what a prophecy write needs.
    /// `None` when the triple is not a cell row (a proof step or Guardian
    /// resolver invocation shares the event-forwarding path).
    pub fn cell_prophecy_target(
        &self,
        squad_id: &str,
        task_name: &str,
        cell_sid: &str,
    ) -> Option<CellProphecyTarget> {
        self.conn
            .query_row(
                "SELECT task_idx, idx, cwd, restart_count FROM cells
                 WHERE squad_id=? AND sid=? AND task_idx=(SELECT idx FROM tasks WHERE squad_id=? AND name=?)",
                params![squad_id, cell_sid, squad_id, task_name],
                |r| {
                    Ok(CellProphecyTarget {
                        task_idx: r.get(0)?,
                        idx: r.get(1)?,
                        cwd: r.get(2)?,
                        attempt: r.get(3)?,
                    })
                },
            )
            .ok()
    }

    /// Record one runner-reported `RALPHUS_PROPHECY:` marker against a cell.
    /// Shared by the live `prophecy` event and the at-exit
    /// `RunnerResult.prophecies` backstop so both apply the same
    /// unknown-kind policy (a Cartographer warning, not a failure) and the
    /// same dedupe.
    #[allow(clippy::too_many_arguments)]
    pub fn record_cell_prophecy_marker(
        &self,
        squad_id: &str,
        task_name: &str,
        cell_id: &str,
        target: &CellProphecyTarget,
        revision: Option<&str>,
        kind: &str,
        body: &str,
    ) -> Result<()> {
        let Ok(parsed) = kind.parse::<ProphecyKind>() else {
            crate::cartographer::Note::new("prophecy")
                .level(crate::logging::LogLevel::WARNING)
                .scope("cell")
                .squad(squad_id)
                .cell(cell_id)
                .task(task_name)
                .emit(
                    self,
                    format!(
                        "prophecy marker dropped: unknown kind {kind:?} cell={squad_id}/{cell_id}"
                    ),
                    serde_json::json!({ "kind": kind, "body_len": body.len() }),
                );
            return Ok(());
        };
        let uri = crate::ghost::cell_uri(squad_id, target.task_idx, target.idx);
        self.add_prophecy(
            &uri,
            target.attempt,
            parsed,
            body,
            revision,
            Some(squad_id),
            None,
        )?;
        Ok(())
    }

    /// The prophecies of the cell `entity_uri`'s current attempt only, oldest
    /// first (RAL-595): a re-run supersedes the earlier attempt's insights.
    /// The current attempt is the cell's `restart_count`; for a cell whose
    /// row is gone (its squad was deleted) it falls back to the highest
    /// attempt recorded for the URI.
    pub fn list_latest_attempt_prophecies_for_cell(
        &self,
        entity_uri: &str,
    ) -> Result<Vec<ProphecyView>> {
        let mut rows = self.list_prophecies_for_entity(entity_uri)?;
        let current = entity_uri
            .strip_prefix("cell:")
            .and_then(|rest| {
                let mut parts = rest.rsplitn(3, ':');
                let idx = parts.next()?.parse::<i64>().ok()?;
                let task_idx = parts.next()?.parse::<i64>().ok()?;
                let squad_id = parts.next()?;
                self.conn
                    .query_row(
                        "SELECT restart_count FROM cells WHERE squad_id=? AND task_idx=? AND idx=?",
                        params![squad_id, task_idx, idx],
                        |r| r.get::<_, i64>(0),
                    )
                    .ok()
            })
            .or_else(|| rows.iter().map(|p| p.attempt).max());
        if let Some(current) = current {
            rows.retain(|p| p.attempt == current);
        }
        Ok(rows)
    }

    /// Filtered, paginated, newest-first query -- backs
    /// `ralphus prophecy list`.
    pub fn list_prophecies(&self, filter: &ProphecyFilter) -> Result<Vec<ProphecyView>> {
        let mut clauses: Vec<String> = Vec::new();
        let mut args: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        if let Some(v) = &filter.entity_uri {
            clauses.push("entity_uri=?".to_string());
            args.push(Box::new(v.clone()));
        }
        if let Some(v) = &filter.squad_id {
            clauses.push("squad_id=?".to_string());
            args.push(Box::new(v.clone()));
        }
        if let Some(v) = &filter.guardian_id {
            clauses.push("guardian_id=?".to_string());
            args.push(Box::new(v.clone()));
        }
        let where_sql = if clauses.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", clauses.join(" AND "))
        };
        args.push(Box::new(filter.limit));
        args.push(Box::new(filter.offset));
        let sql = format!(
            "SELECT {PROPHECY_COLUMNS} FROM prophecies {where_sql} ORDER BY id DESC LIMIT ? OFFSET ?"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let params_ref: Vec<&dyn rusqlite::ToSql> = args.iter().map(AsRef::as_ref).collect();
        let rows = stmt
            .query_map(params_ref.as_slice(), map_prophecy_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows.into_iter().map(ProphecyView::from).collect())
    }

    /// Every not-yet-published prophecy that belongs to `guardian_id`'s
    /// review, oldest first -- backs phase 4's PR-body fold-in (§8.1 of
    /// docs/prophecy-design.md). "Belongs to" is the union of two sources,
    /// matching §3's "whole review stack" reach:
    ///
    /// - Daemon-side writes stamped directly with this guardian id (e.g.
    ///   `guardian_merge.rs`'s conflict-resolution decisions).
    /// - Cell-authored prophecies, resolved via `cells.review_guardian_id`
    ///   (RAL-314: recorded at submit time) -- the same join `crate::store`
    ///   already uses to resolve which cells belong to which review.
    ///
    /// Filtered to `published_at_ms IS NULL` so a resubmit's PR body doesn't
    /// repeat a prophecy already folded into an earlier open PR.
    pub fn list_unpublished_prophecies_for_guardian(
        &self,
        guardian_id: &str,
    ) -> Result<Vec<ProphecyView>> {
        let mut direct = self.list_all_prophecies_for_guardian(guardian_id)?;
        direct.retain(|p| p.published_at_ms.is_none());
        Ok(direct)
    }

    /// Every prophecy that belongs to `guardian_id`'s review (the same two
    /// sources as [`Self::list_unpublished_prophecies_for_guardian`]),
    /// published or not, oldest first.
    pub fn list_all_prophecies_for_guardian(&self, guardian_id: &str) -> Result<Vec<ProphecyView>> {
        let mut direct = self.list_prophecies(&ProphecyFilter {
            guardian_id: Some(guardian_id.to_string()),
            limit: 10000,
            ..ProphecyFilter::default()
        })?;
        let mut cell_uri_stmt = self
            .conn
            .prepare("SELECT squad_id, task_idx, idx FROM cells WHERE review_guardian_id=?")?;
        let cell_uris: Vec<String> = cell_uri_stmt
            .query_map(params![guardian_id], |r| {
                let squad_id: String = r.get(0)?;
                let task_idx: i64 = r.get(1)?;
                let idx: i64 = r.get(2)?;
                Ok(crate::ghost::cell_uri(&squad_id, task_idx, idx))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        for uri in cell_uris {
            direct.extend(self.list_latest_attempt_prophecies_for_cell(&uri)?);
        }
        direct.sort_by_key(|p| p.created_at_ms);
        Ok(direct)
    }

    /// Stamp every prophecy in `ids` as published into `pr_id`, so a later
    /// resubmit's [`Self::list_unpublished_prophecies_for_guardian`] call
    /// does not repeat them (§8.1). Best-effort per row -- a single bad id
    /// (there should never be one) does not abort the rest.
    pub fn mark_prophecies_published(&self, ids: &[i64], pr_id: &str) -> Result<()> {
        let now = now_ms();
        for id in ids {
            self.conn.execute(
                "UPDATE prophecies SET published_at_ms=?, pr_id=? WHERE id=?",
                params![now, pr_id, id],
            )?;
        }
        if !ids.is_empty() {
            crate::cartographer::Note::new("prophecy")
                .scope("prophecy")
                .emit(
                    self,
                    format!("{} prophecy(ies) marked published into {pr_id}", ids.len()),
                    serde_json::json!({ "count": ids.len(), "pr_id": pr_id }),
                );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> Store {
        Store::open_in_memory().unwrap()
    }

    fn seed_guardian(s: &Store, id: &str) {
        s.conn
            .execute(
                "INSERT INTO guardians(id, name, base_branch, git_root, status, created_at_ms, updated_at_ms)
                 VALUES (?,'g','main','/repo','collecting',0,0)",
                params![id],
            )
            .unwrap();
    }

    fn seed_squad(s: &Store, id: &str) {
        s.conn
            .execute(
                "INSERT INTO squads(id, state, created_at_ms, updated_at_ms) VALUES (?,'pending',0,0)",
                params![id],
            )
            .unwrap();
    }

    #[test]
    fn kind_round_trips_through_str() {
        for k in [
            ProphecyKind::Discovery,
            ProphecyKind::Decision,
            ProphecyKind::Hazard,
            ProphecyKind::Deferred,
            ProphecyKind::Unconfirmed,
        ] {
            assert_eq!(k.as_str().parse::<ProphecyKind>().unwrap(), k);
        }
        assert!("bogus".parse::<ProphecyKind>().is_err());
    }

    #[test]
    fn add_then_get_round_trips() {
        let s = store();
        seed_squad(&s, "squad-1");
        let p = s
            .add_prophecy(
                "cell:squad-1:0:0",
                0,
                ProphecyKind::Discovery,
                "learned something",
                Some("abc123"),
                Some("squad-1"),
                None,
            )
            .unwrap();
        assert_eq!(p.body, "learned something");
        assert_eq!(p.revision.as_deref(), Some("abc123"));
        let fetched = s.get_prophecy(p.id).unwrap().unwrap();
        assert_eq!(fetched.entity_uri, "cell:squad-1:0:0");
    }

    #[test]
    fn a_repeated_marker_in_one_attempt_yields_one_row() {
        let s = store();
        seed_squad(&s, "squad-1");
        let add = |attempt: i64, body: &str| {
            s.add_prophecy(
                "cell:squad-1:0:0",
                attempt,
                ProphecyKind::Hazard,
                body,
                None,
                Some("squad-1"),
                None,
            )
            .unwrap()
        };
        let first = add(0, "leaves a race");
        let again = add(0, "leaves   a race");
        assert_eq!(
            first.id, again.id,
            "the live copy and the at-exit copy collapse"
        );
        let later_attempt = add(1, "leaves a race");
        assert_ne!(first.id, later_attempt.id, "a restart files its own row");
        assert_eq!(
            s.list_prophecies_for_entity("cell:squad-1:0:0")
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn views_show_only_the_latest_attempts_prophecies() {
        let s = store();
        seed_squad(&s, "squad-1");
        for (attempt, body) in [(0, "stale"), (1, "current")] {
            s.add_prophecy(
                "cell:squad-1:0:0",
                attempt,
                ProphecyKind::Discovery,
                body,
                None,
                Some("squad-1"),
                None,
            )
            .unwrap();
        }
        let shown = s
            .list_latest_attempt_prophecies_for_cell("cell:squad-1:0:0")
            .unwrap();
        assert_eq!(shown.len(), 1);
        assert_eq!(shown[0].body, "current");
    }

    #[test]
    fn add_prophecy_is_append_only_not_merged() {
        let s = store();
        seed_squad(&s, "squad-1");
        s.add_prophecy(
            "cell:squad-1:0:0",
            0,
            ProphecyKind::Discovery,
            "first",
            None,
            Some("squad-1"),
            None,
        )
        .unwrap();
        s.add_prophecy(
            "cell:squad-1:0:0",
            1,
            ProphecyKind::Hazard,
            "second",
            None,
            Some("squad-1"),
            None,
        )
        .unwrap();
        let all = s.list_prophecies_for_entity("cell:squad-1:0:0").unwrap();
        assert_eq!(all.len(), 2, "each write must be its own row");
        assert_eq!(all[0].body, "first");
        assert_eq!(all[1].body, "second");
        assert_eq!(all[0].attempt, 0);
        assert_eq!(all[1].attempt, 1);
    }

    #[test]
    fn add_prophecy_emits_a_cartographer_row() {
        let s = store();
        seed_guardian(&s, "guardian-1");
        s.add_prophecy(
            "guardian:guardian-1",
            0,
            ProphecyKind::Hazard,
            "left a conflict unresolved",
            None,
            None,
            Some("guardian-1"),
        )
        .unwrap();
        let page = s
            .cartographer_query(&crate::cartographer::CartographerFilter {
                source: Some("prophecy".to_string()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(page.rows.len(), 1);
        assert_eq!(page.rows[0].guardian_id.as_deref(), Some("guardian-1"));
    }

    #[test]
    fn list_prophecies_filters_by_guardian_id() {
        let s = store();
        seed_guardian(&s, "guardian-1");
        seed_guardian(&s, "guardian-2");
        s.add_prophecy(
            "guardian:guardian-1",
            0,
            ProphecyKind::Decision,
            "took ours",
            None,
            None,
            Some("guardian-1"),
        )
        .unwrap();
        s.add_prophecy(
            "guardian:guardian-2",
            0,
            ProphecyKind::Decision,
            "took theirs",
            None,
            None,
            Some("guardian-2"),
        )
        .unwrap();
        let filtered = s
            .list_prophecies(&ProphecyFilter {
                guardian_id: Some("guardian-1".to_string()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].body, "took ours");
    }

    #[test]
    fn list_unpublished_prophecies_for_guardian_unions_direct_and_cell_authored() {
        let s = store();
        seed_squad(&s, "squad-1");
        seed_guardian(&s, "guardian-1");
        // Daemon-side write, stamped directly with the guardian id.
        s.add_prophecy(
            "guardian:guardian-1",
            0,
            ProphecyKind::Decision,
            "took ours",
            None,
            None,
            Some("guardian-1"),
        )
        .unwrap();
        // Cell-authored write: the cell's review membership (RAL-314)
        // resolves to this guardian.
        s.add_prophecy(
            "cell:squad-1:0:0",
            0,
            ProphecyKind::Hazard,
            "left something behind",
            None,
            Some("squad-1"),
            None,
        )
        .unwrap();
        s.conn
            .execute(
                "INSERT INTO cells(squad_id, task_idx, idx, sid, agent, state, review_guardian_id)
                 VALUES (?,?,?,?,?,?,?)",
                params![
                    "squad-1",
                    0i64,
                    0i64,
                    "sid-1",
                    "claude",
                    "done",
                    "guardian-1"
                ],
            )
            .unwrap();

        let unioned = s
            .list_unpublished_prophecies_for_guardian("guardian-1")
            .unwrap();
        assert_eq!(unioned.len(), 2);
        let bodies: Vec<&str> = unioned.iter().map(|p| p.body.as_str()).collect();
        assert!(bodies.contains(&"took ours"));
        assert!(bodies.contains(&"left something behind"));
    }

    #[test]
    fn list_unpublished_prophecies_for_guardian_excludes_already_published() {
        let s = store();
        seed_guardian(&s, "guardian-1");
        let p = s
            .add_prophecy(
                "guardian:guardian-1",
                0,
                ProphecyKind::Decision,
                "took ours",
                None,
                None,
                Some("guardian-1"),
            )
            .unwrap();
        s.mark_prophecies_published(&[p.id], "pr-1").unwrap();
        let unioned = s
            .list_unpublished_prophecies_for_guardian("guardian-1")
            .unwrap();
        assert!(
            unioned.is_empty(),
            "a resubmit must not repeat an already-published prophecy"
        );
    }

    #[test]
    fn mark_prophecies_published_stamps_pr_id_and_timestamp() {
        let s = store();
        seed_guardian(&s, "guardian-1");
        let p = s
            .add_prophecy(
                "guardian:guardian-1",
                0,
                ProphecyKind::Decision,
                "took ours",
                None,
                None,
                Some("guardian-1"),
            )
            .unwrap();
        s.mark_prophecies_published(&[p.id], "pr-42").unwrap();
        let fetched = s.get_prophecy(p.id).unwrap().unwrap();
        assert_eq!(fetched.pr_id.as_deref(), Some("pr-42"));
        assert!(fetched.published_at_ms.is_some());
    }

    #[test]
    fn list_prophecies_orders_newest_first_and_paginates() {
        let s = store();
        seed_squad(&s, "squad-1");
        for i in 0..3 {
            s.add_prophecy(
                "cell:squad-1:0:0",
                i,
                ProphecyKind::Discovery,
                &format!("note {i}"),
                None,
                Some("squad-1"),
                None,
            )
            .unwrap();
        }
        let page = s
            .list_prophecies(&ProphecyFilter {
                limit: 2,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(page.len(), 2);
        assert_eq!(page[0].body, "note 2", "newest first");
        assert_eq!(page[1].body, "note 1");
    }

    #[test]
    fn delete_squad_orphans_rather_than_deletes_prophecies() {
        let mut s = store();
        seed_squad(&s, "squad-1");
        let p = s
            .add_prophecy(
                "cell:squad-1:0:0",
                0,
                ProphecyKind::Discovery,
                "survives squad deletion",
                None,
                Some("squad-1"),
                None,
            )
            .unwrap();
        s.delete_squad("squad-1").unwrap();
        let still_there = s.get_prophecy(p.id).unwrap().expect("must survive (§11.2)");
        assert_eq!(still_there.squad_id, None, "orphaned, not deleted");
        assert_eq!(still_there.body, "survives squad deletion");
    }

    #[test]
    fn delete_guardian_orphans_rather_than_deletes_prophecies() {
        let s = store();
        seed_guardian(&s, "guardian-1");
        let p = s
            .add_prophecy(
                "guardian:guardian-1",
                0,
                ProphecyKind::Hazard,
                "survives guardian deletion",
                None,
                None,
                Some("guardian-1"),
            )
            .unwrap();
        s.delete_guardian("guardian-1").unwrap();
        let still_there = s.get_prophecy(p.id).unwrap().expect("must survive (§11.2)");
        assert_eq!(still_there.guardian_id, None, "orphaned, not deleted");
    }

    #[test]
    fn clear_all_orphans_rather_than_deletes_prophecies() {
        let mut s = store();
        seed_squad(&s, "squad-1");
        let p = s
            .add_prophecy(
                "cell:squad-1:0:0",
                0,
                ProphecyKind::Discovery,
                "survives clear",
                None,
                Some("squad-1"),
                None,
            )
            .unwrap();
        s.clear_all(&[]).unwrap();
        let still_there = s.get_prophecy(p.id).unwrap().expect("must survive (§11.2)");
        assert_eq!(still_there.squad_id, None);
    }
}
