//! Prophecies: a durable, append-only record of what an agent (or ralphus
//! itself) learned mid-work, surfaced in the PR a human reads (see
//! `docs/prophecy-design.md`).
//!
//! Distinct from both `crate::ghost` (one row per owner, merge-on-write,
//! cascade-deleted with its squad) and `crate::cartographer` (a
//! cross-system event log, pruned by `[cartographer] retention_days`/
//! `max_rows`). A prophecy is one row per write, keyed by `entity_uri` +
//! `attempt` so a retried cell's notes never collide with a prior attempt's,
//! and it deliberately outlives the squad that produced it (see the
//! `prophecies` table comment in `store::init_schema` and design doc §11.2).
//!
//! Every write also emits a `crate::cartographer::Note` (see
//! `Store::record_prophecy`), per `.agent/logging-policy.md` — Cartographer
//! is not this subsystem's durable home (it's pruned), but every notable
//! event still needs a structured log row.

use rusqlite::{OptionalExtension, params};
use serde::Serialize;

use crate::store::{Result, Store, now_ms};

/// One row of the prophecy log, as returned to API/CLI consumers.
#[derive(Debug, Clone, Serialize)]
pub struct ProphecyRow {
    /// Monotonically increasing row id.
    pub id: i64,
    /// The entity this prophecy is about, e.g. `cell:<squad_id>:<task_idx>:<cell_idx>`
    /// or `guardian:<guardian_id>` (see `crate::entity_uri`).
    pub entity_uri: String,
    /// Which attempt of that entity this prophecy was written during
    /// (1-based; callers vary in what they count as an "attempt" -- e.g.
    /// `daemon/src/scheduler.rs` uses the command-remediation attempt
    /// number), so a retried cell's notes don't collide with a prior
    /// attempt's.
    pub attempt: i64,
    /// Freeform category, e.g. `"conflict-resolution"`, `"note"`. Not
    /// validated against a closed set today.
    //
    // TODO(prophecy-kind-enum): open question per design doc §11.1 -- whether
    // `kind` should become a closed enum once real usage exists to draw the
    // boundaries from. Left as an open string deliberately; do not guess a
    // closed set here.
    pub kind: String,
    /// The prophecy's text.
    pub body: String,
    /// Optional revision/commit the prophecy was written against.
    pub revision: Option<String>,
    /// When it was recorded (Unix epoch milliseconds).
    pub created_at_ms: i64,
    /// When it was folded into a PR description (Phase 4), if it has been.
    pub published_at_ms: Option<i64>,
    /// The PR/MR it was published into, if any.
    pub pr_id: Option<String>,
}

/// A filtered, paginated query against the prophecy log.
#[derive(Debug, Clone, Default)]
pub struct ProphecyFilter {
    /// Exact-match `entity_uri`.
    pub entity_uri: Option<String>,
    /// Exact-match `kind`.
    pub kind: Option<String>,
    /// Substring match against `body` (case-insensitive).
    pub q: Option<String>,
    /// Max rows to return (clamped to a sane ceiling server-side).
    pub limit: i64,
    /// Rows to skip, for pagination.
    pub offset: i64,
    /// Oldest-first when true; newest-first (the default) when false.
    pub ascending: bool,
}

impl ProphecyFilter {
    /// A filter that returns the most recent `limit` rows for one entity.
    #[must_use]
    pub fn for_entity(entity_uri: impl Into<String>, limit: i64) -> Self {
        Self {
            entity_uri: Some(entity_uri.into()),
            limit,
            ..Self::default()
        }
    }
}

/// One page of prophecy results plus the total row count matching the
/// filter (ignoring `limit`/`offset`), so callers can render pagination.
#[derive(Debug, Clone, Serialize)]
pub struct ProphecyPage {
    /// The rows in this page.
    pub rows: Vec<ProphecyRow>,
    /// Total rows matching the filter, across all pages.
    pub total: i64,
}

/// The fields needed to persist one prophecy (used by
/// [`Store::record_prophecy`]).
pub struct ProphecyEntry<'a> {
    pub entity_uri: &'a str,
    pub attempt: i64,
    pub kind: &'a str,
    pub body: &'a str,
    pub revision: Option<&'a str>,
}

impl Store {
    /// Persist one prophecy row and emit the matching Cartographer note
    /// (`.agent/logging-policy.md`). `source` is the Cartographer source
    /// location (e.g. `"scheduler"`, `"guardian_merge"`); `squad_id`/
    /// `guardian_id`/`cell_id`/`task` are the same optional entity
    /// attribution `crate::cartographer::Note` takes, so the emitted row
    /// shows up alongside the rest of that entity's log.
    #[allow(clippy::too_many_arguments)]
    pub fn record_prophecy(
        &self,
        entry: ProphecyEntry<'_>,
        source: &str,
        squad_id: Option<&str>,
        guardian_id: Option<&str>,
        cell_id: Option<&str>,
        task: Option<&str>,
    ) -> Result<i64> {
        let at_ms = now_ms();
        self.conn.execute(
            "INSERT INTO prophecies(entity_uri, attempt, kind, body, revision, created_at_ms)
             VALUES(?,?,?,?,?,?)",
            params![
                entry.entity_uri,
                entry.attempt,
                entry.kind,
                entry.body,
                entry.revision,
                at_ms,
            ],
        )?;
        let id = self.conn.last_insert_rowid();
        let mut note = crate::cartographer::Note::new(source).scope("prophecy");
        if let Some(v) = squad_id {
            note = note.squad(v);
        }
        if let Some(v) = guardian_id {
            note = note.guardian(v);
        }
        if let Some(v) = cell_id {
            note = note.cell(v);
        }
        if let Some(v) = task {
            note = note.task(v);
        }
        note.emit(
            self,
            format!("prophecy recorded for {}", entry.entity_uri),
            serde_json::json!({
                "prophecy_id": id,
                "attempt": entry.attempt,
                "kind": entry.kind,
                "len": entry.body.len(),
            }),
        );
        Ok(id)
    }

    /// Query prophecies with filters and pagination.
    pub fn prophecy_query(&self, filter: &ProphecyFilter) -> Result<ProphecyPage> {
        let mut clauses: Vec<String> = Vec::new();
        let mut values: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

        if let Some(v) = filter.entity_uri.as_ref() {
            clauses.push("entity_uri = ?".to_string());
            values.push(Box::new(v.clone()));
        }
        if let Some(v) = filter.kind.as_ref() {
            clauses.push("kind = ?".to_string());
            values.push(Box::new(v.clone()));
        }
        if let Some(q) = filter.q.as_ref() {
            clauses.push("body LIKE ? ESCAPE '\\'".to_string());
            let escaped = q
                .replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_");
            values.push(Box::new(format!("%{escaped}%")));
        }

        let where_sql = if clauses.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", clauses.join(" AND "))
        };

        let count_sql = format!("SELECT COUNT(*) FROM prophecies {where_sql}");
        let param_refs: Vec<&dyn rusqlite::ToSql> = values.iter().map(AsRef::as_ref).collect();
        let total: i64 = self
            .conn
            .query_row(&count_sql, param_refs.as_slice(), |r| r.get(0))?;

        let order = if filter.ascending { "ASC" } else { "DESC" };
        let limit = filter.limit.clamp(1, 1000);
        let offset = filter.offset.max(0);
        let sql = format!(
            "SELECT id, entity_uri, attempt, kind, body, revision, created_at_ms, published_at_ms, pr_id
             FROM prophecies {where_sql}
             ORDER BY created_at_ms {order}, id {order}
             LIMIT {limit} OFFSET {offset}"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let param_refs: Vec<&dyn rusqlite::ToSql> = values.iter().map(AsRef::as_ref).collect();
        let rows = stmt
            .query_map(param_refs.as_slice(), map_prophecy_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        Ok(ProphecyPage { rows, total })
    }

    /// Fetch one prophecy by row id.
    pub fn prophecy_get(&self, id: i64) -> Result<Option<ProphecyRow>> {
        self.conn
            .query_row(
                "SELECT id, entity_uri, attempt, kind, body, revision, created_at_ms, published_at_ms, pr_id
                 FROM prophecies WHERE id = ?",
                params![id],
                map_prophecy_row,
            )
            .optional()
            .map_err(Into::into)
    }
}

fn map_prophecy_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<ProphecyRow> {
    Ok(ProphecyRow {
        id: r.get(0)?,
        entity_uri: r.get(1)?,
        attempt: r.get(2)?,
        kind: r.get(3)?,
        body: r.get(4)?,
        revision: r.get(5)?,
        created_at_ms: r.get(6)?,
        published_at_ms: r.get(7)?,
        pr_id: r.get(8)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_and_query_roundtrip() {
        let store = Store::open_in_memory().unwrap();
        let id = store
            .record_prophecy(
                ProphecyEntry {
                    entity_uri: "cell:squad-1:0:0",
                    attempt: 0,
                    kind: "note",
                    body: "the flaky test needs a retry guard",
                    revision: Some("abc123"),
                },
                "scheduler",
                Some("squad-1"),
                None,
                Some("cell-1"),
                Some("build"),
            )
            .unwrap();
        assert!(id > 0);

        let page = store
            .prophecy_query(&ProphecyFilter::for_entity("cell:squad-1:0:0", 10))
            .unwrap();
        assert_eq!(page.total, 1);
        assert_eq!(page.rows.len(), 1);
        assert_eq!(page.rows[0].body, "the flaky test needs a retry guard");
        assert_eq!(page.rows[0].attempt, 0);
        assert_eq!(page.rows[0].revision.as_deref(), Some("abc123"));

        let fetched = store.prophecy_get(id).unwrap().unwrap();
        assert_eq!(fetched.id, id);
        assert_eq!(fetched.entity_uri, "cell:squad-1:0:0");

        // A different entity_uri must not match.
        let empty = store
            .prophecy_query(&ProphecyFilter::for_entity("cell:squad-1:0:1", 10))
            .unwrap();
        assert_eq!(empty.total, 0);
    }
}
