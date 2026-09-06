//! Cartographer: the unified, structured, cross-system event log (RAL-98).
//!
//! Every notable thing that happens in the daemon — task/cell lifecycle,
//! proof starts/results, status transitions, Guardian review lifecycle
//! events — is recorded here as one row with a timestamp, a human-readable
//! message, the source location that emitted it, optional entity references
//! (squad/guardian/cell/task), and an arbitrary JSON payload.
//!
//! Cartographer formally replaces `rlog!` as the primary logging mechanism
//! (see `AGENTS.md`'s Logging Policy). A human-readable line is still written
//! to the log file/stderr alongside every structured record, so `tail`-based
//! debugging keeps working — [`Note::emit`] does both in one call.
//!
//! Retention is enforced by two independently configurable caps (time window
//! and max row count; see [`crate::config::CartographerConfig`]) — either
//! condition triggers pruning via [`Store::cartographer_prune`].

use rusqlite::{OptionalExtension, params};
use serde::Serialize;

use crate::logging::LogLevel;
use crate::store::{Result, Store, now_ms};

/// One row of the Cartographer log, as returned to API/UI consumers.
#[derive(Debug, Clone, Serialize)]
pub struct CartographerRow {
    /// Monotonically increasing row id.
    pub id: i64,
    /// When it happened (Unix epoch milliseconds).
    pub at_ms: i64,
    /// Severity: `"trace"` / `"debug"` / `"info"` / `"warning"` / `"error"`.
    pub level: String,
    /// The source location that emitted this event, e.g. `"daemon/scheduler"`,
    /// `"runner/execute"`.
    pub source: String,
    /// Human-readable description of what happened.
    pub message: String,
    /// Entity scope this event concerns, e.g. `"squad"` / `"cell"` /
    /// `"guardian"` / `"proof"`, if any.
    pub scope: Option<String>,
    /// Owning squad id, if any.
    pub squad_id: Option<String>,
    /// Owning guardian (review) id, if any.
    pub guardian_id: Option<String>,
    /// Owning cell id, if any.
    pub cell_id: Option<String>,
    /// Owning task name, if any.
    pub task: Option<String>,
    /// Path to an on-disk log file this event references (RAL-155), e.g. a
    /// durable terminal-log attempt file (`crate::terminal_log`). The row
    /// carries the *path*, never the file's content — a consumer (like
    /// `crate::timeline`) reads it separately when it needs to inline the
    /// content.
    pub log_path: Option<String>,
    /// Arbitrary structured detail, as a raw JSON string (already validated
    /// JSON — parsed lazily by consumers, not re-parsed here).
    pub payload: serde_json::Value,
    /// RAL-332: restricts this row to admin viewers only (see
    /// [`Note::admin_only`]) -- e.g. RAL-328's hide/unhide rows and this
    /// ticket's "Edit Profile" rows. `false` (the default) is every
    /// pre-RAL-332 row, unrestricted exactly as before.
    pub admin_only: bool,
}

/// A filtered, paginated, sorted query against the Cartographer log.
#[derive(Debug, Clone, Default)]
pub struct CartographerFilter {
    /// Exact-match source, e.g. `"scheduler"`.
    pub source: Option<String>,
    /// Exact-match scope, e.g. `"squad"`.
    pub scope: Option<String>,
    /// Exact-match level, e.g. `"error"`.
    pub level: Option<String>,
    /// Exact-match squad id.
    pub squad_id: Option<String>,
    /// Exact-match guardian id.
    pub guardian_id: Option<String>,
    /// Exact-match cell id.
    pub cell_id: Option<String>,
    /// Exact-match task name (RAL-155 Q2). Populated on rows whose emitter
    /// knew the owning task's name at emit time (most cell/proof-scoped
    /// events do; squad-level `Store::log_event`-derived transitions do not).
    pub task: Option<String>,
    /// Substring match against the message (case-insensitive).
    pub q: Option<String>,
    /// Only rows at or after this time (Unix epoch milliseconds).
    pub since_ms: Option<i64>,
    /// Only rows at or before this time (Unix epoch milliseconds).
    pub until_ms: Option<i64>,
    /// Max rows to return (clamped to a sane ceiling server-side).
    pub limit: i64,
    /// Rows to skip, for pagination.
    pub offset: i64,
    /// Oldest-first when true; newest-first (the default) when false.
    pub ascending: bool,
    /// RAL-332: include rows marked [`Note::admin_only`]. `false` (the
    /// default) is the safe choice for every existing internal consumer
    /// (scheduler reconciliation checks, `crate::timeline`, ...) -- only the
    /// `/api/cartographer` HTTP handler sets this, and only once it has
    /// confirmed the caller is an admin.
    pub include_admin_only: bool,
}

impl CartographerFilter {
    /// A filter that returns the most recent `limit` rows, unfiltered.
    #[must_use]
    pub fn recent(limit: i64) -> Self {
        Self {
            limit,
            ..Self::default()
        }
    }
}

/// One page of Cartographer results plus the total row count matching the
/// filter (ignoring `limit`/`offset`), so callers can render pagination.
#[derive(Debug, Clone, Serialize)]
pub struct CartographerPage {
    /// The rows in this page.
    pub rows: Vec<CartographerRow>,
    /// Total rows matching the filter, across all pages.
    pub total: i64,
}

/// Builder for a Cartographer entry: pick a source, attach whichever entity
/// references apply, then [`emit`](Note::emit) to write both the human-readable
/// log line (via `rlog!`'s underlying sink) and the structured record.
///
/// ```ignore
/// Note::new("scheduler")
///     .squad(&squad_id)
///     .scope("squad")
///     .emit(&store, format!("squad {squad_id} claimed → running"), serde_json::json!({}));
/// ```
pub struct Note<'a> {
    source: &'a str,
    level: LogLevel,
    scope: Option<&'a str>,
    squad_id: Option<&'a str>,
    guardian_id: Option<&'a str>,
    cell_id: Option<&'a str>,
    task: Option<&'a str>,
    log_path: Option<&'a str>,
    admin_only: bool,
}

impl<'a> Note<'a> {
    /// Start a new note from the given source location, e.g. `"scheduler"`.
    /// Defaults to `INFO` level and no entity references.
    #[must_use]
    pub fn new(source: &'a str) -> Self {
        Self {
            source,
            level: LogLevel::INFO,
            scope: None,
            squad_id: None,
            guardian_id: None,
            cell_id: None,
            task: None,
            log_path: None,
            admin_only: false,
        }
    }

    /// Set the severity level.
    #[must_use]
    pub fn level(mut self, level: LogLevel) -> Self {
        self.level = level;
        self
    }

    /// Set the entity scope, e.g. `"squad"` / `"cell"` / `"guardian"`.
    #[must_use]
    pub fn scope(mut self, scope: &'a str) -> Self {
        self.scope = Some(scope);
        self
    }

    /// Attach an owning squad id.
    #[must_use]
    pub fn squad(mut self, squad_id: &'a str) -> Self {
        self.squad_id = Some(squad_id);
        self
    }

    /// Attach an owning guardian (review) id.
    #[must_use]
    pub fn guardian(mut self, guardian_id: &'a str) -> Self {
        self.guardian_id = Some(guardian_id);
        self
    }

    /// Attach an owning cell id.
    #[must_use]
    pub fn cell(mut self, cell_id: &'a str) -> Self {
        self.cell_id = Some(cell_id);
        self
    }

    /// Attach an owning task name.
    #[must_use]
    pub fn task(mut self, task: &'a str) -> Self {
        self.task = Some(task);
        self
    }

    /// Attach the path to an on-disk log file this event references (RAL-155),
    /// e.g. a durable terminal-log attempt file. Carries the path only, never
    /// the file's content — see [`CartographerRow::log_path`].
    #[must_use]
    pub fn log_path(mut self, log_path: &'a str) -> Self {
        self.log_path = Some(log_path);
        self
    }

    /// Restrict this row to admin viewers (RAL-332) -- excluded from the
    /// Logs tab, and from `crate::cartographer::CartographerFilter`-based
    /// queries generally, for anyone whose `is_admin` flag isn't set. Use
    /// for rows that reveal one user's personal preference to every other
    /// user (RAL-328's hide/unhide) or an admin's action on someone else's
    /// account (RAL-332's "Edit Profile"/visit-as).
    #[must_use]
    pub fn admin_only(mut self) -> Self {
        self.admin_only = true;
        self
    }

    /// Write the human-readable log line (`ralphus [source] message`) to the
    /// active log sink and persist the structured record to Cartographer.
    /// Failures to persist are swallowed (a missing structured record must
    /// never break the caller), matching the existing `log_event` contract.
    pub fn emit(self, store: &Store, message: impl AsRef<str>, payload: serde_json::Value) {
        let message = message.as_ref();
        crate::logging::write_line(self.level, &format!("ralphus [{}] {message}", self.source));
        let _ = store.cartographer_log(CartographerEntry {
            level: self.level,
            source: self.source,
            message,
            scope: self.scope,
            squad_id: self.squad_id,
            guardian_id: self.guardian_id,
            cell_id: self.cell_id,
            task: self.task,
            log_path: self.log_path,
            payload,
            admin_only: self.admin_only,
        });
    }
}

/// The raw fields persisted for one Cartographer row (used internally by
/// [`Store::cartographer_log`]; build one via [`Note`] rather than directly).
pub struct CartographerEntry<'a> {
    pub level: LogLevel,
    pub source: &'a str,
    pub message: &'a str,
    pub scope: Option<&'a str>,
    pub squad_id: Option<&'a str>,
    pub guardian_id: Option<&'a str>,
    pub cell_id: Option<&'a str>,
    pub task: Option<&'a str>,
    pub log_path: Option<&'a str>,
    pub payload: serde_json::Value,
    /// RAL-332: see [`CartographerRow::admin_only`]. Direct `CartographerEntry`
    /// construction sites (as opposed to [`Note`]) all set this to `false` --
    /// only [`Note::admin_only`] ever sets it `true`.
    pub admin_only: bool,
}

fn level_str(level: LogLevel) -> &'static str {
    match level {
        LogLevel::TRACE => "trace",
        LogLevel::DEBUG => "debug",
        LogLevel::INFO => "info",
        LogLevel::WARNING => "warning",
        LogLevel::ERROR => "error",
    }
}

impl Store {
    /// Persist one Cartographer record. Does *not* write the human-readable
    /// log line — use [`Note::emit`] for the combined write, or call
    /// `crate::logging::write_line` yourself first for lower-level call sites
    /// (e.g. forwarded runner-subprocess events that already carry their own
    /// text line).
    ///
    /// Also broadcasts the row to every connected SSE subscriber via
    /// [`Store::event_bus`] (RAL-167) — this is the single choke point every
    /// Cartographer-instrumented state change already passes through, so it
    /// gives push coverage for squad/task/cell/guardian/queue/cartographer
    /// events without a second, parallel set of instrumentation call sites.
    pub fn cartographer_log(&self, entry: CartographerEntry<'_>) -> Result<()> {
        let at_ms = now_ms();
        self.conn.execute(
            "INSERT INTO cartographer_events(at_ms, level, source, message, scope, squad_id, guardian_id, cell_id, task, log_path, payload, admin_only)
             VALUES(?,?,?,?,?,?,?,?,?,?,?,?)",
            params![
                at_ms,
                level_str(entry.level),
                entry.source,
                entry.message,
                entry.scope,
                entry.squad_id,
                entry.guardian_id,
                entry.cell_id,
                entry.task,
                entry.log_path,
                entry.payload.to_string(),
                entry.admin_only,
            ],
        )?;
        // RAL-332: the SSE broadcast below is not viewer-scoped (`EventBus`
        // has no per-connection identity), so an admin_only row's full
        // payload still reaches every connected browser over the wire even
        // though the Logs tab's own `GET /api/cartographer`/`GET
        // /api/cartographer/{id}` calls filter it out of what's ever
        // rendered. Acceptable for a UI-level convenience gate (see
        // `crate::users`'s module doc comment) -- closing this would need a
        // per-connection-aware `EventBus`, out of scope here.
        self.event_bus().publish(CartographerRow {
            id: self.conn.last_insert_rowid(),
            at_ms,
            level: level_str(entry.level).to_string(),
            source: entry.source.to_string(),
            message: entry.message.to_string(),
            scope: entry.scope.map(str::to_string),
            squad_id: entry.squad_id.map(str::to_string),
            guardian_id: entry.guardian_id.map(str::to_string),
            cell_id: entry.cell_id.map(str::to_string),
            task: entry.task.map(str::to_string),
            log_path: entry.log_path.map(str::to_string),
            payload: entry.payload,
            admin_only: entry.admin_only,
        });
        Ok(())
    }

    /// Query Cartographer with filters, sorting, and pagination.
    pub fn cartographer_query(&self, filter: &CartographerFilter) -> Result<CartographerPage> {
        let mut clauses: Vec<String> = Vec::new();
        let mut values: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

        macro_rules! eq_clause {
            ($col:literal, $field:expr) => {
                if let Some(v) = $field.as_ref() {
                    clauses.push(format!("{} = ?", $col));
                    values.push(Box::new(v.clone()));
                }
            };
        }
        eq_clause!("source", filter.source);
        eq_clause!("scope", filter.scope);
        eq_clause!("level", filter.level);
        eq_clause!("squad_id", filter.squad_id);
        eq_clause!("guardian_id", filter.guardian_id);
        eq_clause!("cell_id", filter.cell_id);
        eq_clause!("task", filter.task);
        if !filter.include_admin_only {
            clauses.push("admin_only = 0".to_string());
        }
        if let Some(q) = filter.q.as_ref() {
            clauses.push("message LIKE ? ESCAPE '\\'".to_string());
            let escaped = q
                .replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_");
            values.push(Box::new(format!("%{escaped}%")));
        }
        if let Some(since) = filter.since_ms {
            clauses.push("at_ms >= ?".to_string());
            values.push(Box::new(since));
        }
        if let Some(until) = filter.until_ms {
            clauses.push("at_ms <= ?".to_string());
            values.push(Box::new(until));
        }

        let where_sql = if clauses.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", clauses.join(" AND "))
        };

        let count_sql = format!("SELECT COUNT(*) FROM cartographer_events {where_sql}");
        let param_refs: Vec<&dyn rusqlite::ToSql> = values.iter().map(AsRef::as_ref).collect();
        let total: i64 = self
            .conn
            .query_row(&count_sql, param_refs.as_slice(), |r| r.get(0))?;

        let order = if filter.ascending { "ASC" } else { "DESC" };
        let limit = filter.limit.clamp(1, 1000);
        let offset = filter.offset.max(0);
        let sql = format!(
            "SELECT id, at_ms, level, source, message, scope, squad_id, guardian_id, cell_id, task, log_path, payload, admin_only
             FROM cartographer_events {where_sql}
             ORDER BY at_ms {order}, id {order}
             LIMIT {limit} OFFSET {offset}"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let param_refs: Vec<&dyn rusqlite::ToSql> = values.iter().map(AsRef::as_ref).collect();
        let rows = stmt
            .query_map(param_refs.as_slice(), |r| {
                let payload_str: String = r.get(11)?;
                Ok(CartographerRow {
                    id: r.get(0)?,
                    at_ms: r.get(1)?,
                    level: r.get(2)?,
                    source: r.get(3)?,
                    message: r.get(4)?,
                    scope: r.get(5)?,
                    squad_id: r.get(6)?,
                    guardian_id: r.get(7)?,
                    cell_id: r.get(8)?,
                    task: r.get(9)?,
                    log_path: r.get(10)?,
                    payload: serde_json::from_str(&payload_str).unwrap_or(serde_json::Value::Null),
                    admin_only: r.get::<_, i64>(12)? != 0,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        Ok(CartographerPage { rows, total })
    }

    /// Prune Cartographer rows older than `retention_days`, then — if the
    /// table still exceeds `max_rows` — delete the oldest excess rows.
    /// Either cap alone can trigger a delete. Returns the number of rows
    /// removed.
    pub fn cartographer_prune(&self, retention_days: i64, max_rows: i64) -> Result<usize> {
        let mut deleted = 0usize;
        if retention_days > 0 {
            let cutoff = now_ms() - retention_days * 86_400_000;
            deleted += self.conn.execute(
                "DELETE FROM cartographer_events WHERE at_ms < ?",
                params![cutoff],
            )?;
        }
        if max_rows > 0 {
            let total: i64 =
                self.conn
                    .query_row("SELECT COUNT(*) FROM cartographer_events", [], |r| r.get(0))?;
            if total > max_rows {
                let excess = total - max_rows;
                deleted += self.conn.execute(
                    "DELETE FROM cartographer_events WHERE id IN (
                        SELECT id FROM cartographer_events ORDER BY at_ms ASC, id ASC LIMIT ?
                    )",
                    params![excess],
                )?;
            }
        }
        Ok(deleted)
    }

    /// Fetch one Cartographer row by id (used to resolve a click-through
    /// target's full payload without re-issuing a filtered query). Returns
    /// the row regardless of [`CartographerRow::admin_only`] -- callers that
    /// need to hide admin-only rows from a non-admin viewer (RAL-332) check
    /// that field themselves, the same way `cartographer_query`'s HTTP
    /// handler does.
    pub fn cartographer_get(&self, id: i64) -> Result<Option<CartographerRow>> {
        self.conn
            .query_row(
                "SELECT id, at_ms, level, source, message, scope, squad_id, guardian_id, cell_id, task, log_path, payload, admin_only
                 FROM cartographer_events WHERE id = ?",
                params![id],
                |r| {
                    let payload_str: String = r.get(11)?;
                    Ok(CartographerRow {
                        id: r.get(0)?,
                        at_ms: r.get(1)?,
                        level: r.get(2)?,
                        source: r.get(3)?,
                        message: r.get(4)?,
                        scope: r.get(5)?,
                        squad_id: r.get(6)?,
                        guardian_id: r.get(7)?,
                        cell_id: r.get(8)?,
                        task: r.get(9)?,
                        log_path: r.get(10)?,
                        payload: serde_json::from_str(&payload_str)
                            .unwrap_or(serde_json::Value::Null),
                        admin_only: r.get::<_, i64>(12)? != 0,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logs_and_queries_a_note() {
        let store = Store::open_in_memory().unwrap();
        Note::new("scheduler").squad("squad-1").scope("squad").emit(
            &store,
            "squad squad-1 claimed",
            serde_json::json!({"foo": "bar"}),
        );

        let page = store
            .cartographer_query(&CartographerFilter::recent(10))
            .unwrap();
        assert_eq!(page.total, 1);
        assert_eq!(page.rows.len(), 1);
        assert_eq!(page.rows[0].source, "scheduler");
        assert_eq!(page.rows[0].squad_id.as_deref(), Some("squad-1"));
        assert_eq!(page.rows[0].payload, serde_json::json!({"foo": "bar"}));
    }

    #[test]
    fn log_path_round_trips() {
        let store = Store::open_in_memory().unwrap();
        Note::new("runner")
            .squad("squad-1")
            .log_path("/tmp/terminal_logs/sess-a/0000.log")
            .emit(
                &store,
                "terminal log attempt written",
                serde_json::json!({}),
            );

        let page = store
            .cartographer_query(&CartographerFilter::recent(10))
            .unwrap();
        assert_eq!(
            page.rows[0].log_path.as_deref(),
            Some("/tmp/terminal_logs/sess-a/0000.log")
        );
    }

    #[test]
    fn filters_by_task() {
        let store = Store::open_in_memory().unwrap();
        Note::new("scheduler").squad("squad-1").task("build").emit(
            &store,
            "a",
            serde_json::json!({}),
        );
        Note::new("scheduler").squad("squad-1").task("test").emit(
            &store,
            "b",
            serde_json::json!({}),
        );

        let filter = CartographerFilter {
            task: Some("build".to_string()),
            limit: 10,
            ..Default::default()
        };
        let page = store.cartographer_query(&filter).unwrap();
        assert_eq!(page.total, 1);
        assert_eq!(page.rows[0].message, "a");
    }

    #[test]
    fn filters_by_squad_id() {
        let store = Store::open_in_memory().unwrap();
        Note::new("scheduler")
            .squad("squad-1")
            .emit(&store, "a", serde_json::json!({}));
        Note::new("scheduler")
            .squad("squad-2")
            .emit(&store, "b", serde_json::json!({}));

        let filter = CartographerFilter {
            squad_id: Some("squad-1".to_string()),
            limit: 10,
            ..Default::default()
        };
        let page = store.cartographer_query(&filter).unwrap();
        assert_eq!(page.total, 1);
        assert_eq!(page.rows[0].message, "a");
    }

    #[test]
    fn filters_by_text_query_case_and_wildcard_safe() {
        let store = Store::open_in_memory().unwrap();
        Note::new("proof").emit(&store, "100% complete", serde_json::json!({}));
        Note::new("proof").emit(&store, "something else", serde_json::json!({}));

        let filter = CartographerFilter {
            q: Some("100%".to_string()),
            limit: 10,
            ..Default::default()
        };
        let page = store.cartographer_query(&filter).unwrap();
        assert_eq!(page.total, 1);
        assert_eq!(page.rows[0].message, "100% complete");
    }

    #[test]
    fn pagination_limits_and_offsets() {
        let store = Store::open_in_memory().unwrap();
        for i in 0..5 {
            Note::new("scheduler").emit(&store, format!("event {i}"), serde_json::json!({}));
        }
        let page1 = store
            .cartographer_query(&CartographerFilter {
                limit: 2,
                offset: 0,
                ascending: true,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(page1.total, 5);
        assert_eq!(page1.rows.len(), 2);
        assert_eq!(page1.rows[0].message, "event 0");

        let page2 = store
            .cartographer_query(&CartographerFilter {
                limit: 2,
                offset: 2,
                ascending: true,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(page2.rows[0].message, "event 2");
    }

    #[test]
    fn sorts_newest_first_by_default() {
        let store = Store::open_in_memory().unwrap();
        Note::new("scheduler").emit(&store, "first", serde_json::json!({}));
        Note::new("scheduler").emit(&store, "second", serde_json::json!({}));
        let page = store
            .cartographer_query(&CartographerFilter::recent(10))
            .unwrap();
        assert_eq!(page.rows[0].message, "second");
        assert_eq!(page.rows[1].message, "first");
    }

    #[test]
    fn prune_enforces_row_cap() {
        let store = Store::open_in_memory().unwrap();
        for i in 0..10 {
            Note::new("scheduler").emit(&store, format!("event {i}"), serde_json::json!({}));
        }
        let deleted = store.cartographer_prune(0, 4).unwrap();
        assert_eq!(deleted, 6);
        let page = store
            .cartographer_query(&CartographerFilter::recent(100))
            .unwrap();
        assert_eq!(page.total, 4);
        // The newest 4 survive.
        assert_eq!(page.rows[0].message, "event 9");
        assert_eq!(page.rows[3].message, "event 6");
    }

    #[test]
    fn prune_enforces_time_window() {
        let store = Store::open_in_memory().unwrap();
        Note::new("scheduler").emit(&store, "old", serde_json::json!({}));
        // Force the row to look old by rewriting at_ms directly.
        store
            .conn
            .execute("UPDATE cartographer_events SET at_ms = 0", [])
            .unwrap();
        Note::new("scheduler").emit(&store, "new", serde_json::json!({}));

        let deleted = store.cartographer_prune(1, 0).unwrap();
        assert_eq!(deleted, 1);
        let page = store
            .cartographer_query(&CartographerFilter::recent(100))
            .unwrap();
        assert_eq!(page.total, 1);
        assert_eq!(page.rows[0].message, "new");
    }

    #[test]
    fn get_by_id_returns_full_row() {
        let store = Store::open_in_memory().unwrap();
        Note::new("scheduler").emit(&store, "hello", serde_json::json!({"k": 1}));
        let page = store
            .cartographer_query(&CartographerFilter::recent(1))
            .unwrap();
        let id = page.rows[0].id;
        let row = store.cartographer_get(id).unwrap().unwrap();
        assert_eq!(row.message, "hello");
        assert!(store.cartographer_get(id + 999).unwrap().is_none());
    }
}
