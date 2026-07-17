//! Cartographer: the unified, structured, cross-system event log (RAL-98).
//!
//! Every notable thing that happens in the daemon — task/session lifecycle,
//! verify starts/results, status transitions, Guardian review lifecycle
//! events — is recorded here as one row with a timestamp, a human-readable
//! message, the source location that emitted it, optional entity references
//! (run/guardian/session/task), and an arbitrary JSON payload.
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
    /// Entity scope this event concerns, e.g. `"run"` / `"session"` /
    /// `"guardian"` / `"verify"`, if any.
    pub scope: Option<String>,
    /// Owning run id, if any.
    pub run_id: Option<String>,
    /// Owning guardian (review) id, if any.
    pub guardian_id: Option<String>,
    /// Owning session id, if any.
    pub session_id: Option<String>,
    /// Owning task name, if any.
    pub task: Option<String>,
    /// Arbitrary structured detail, as a raw JSON string (already validated
    /// JSON — parsed lazily by consumers, not re-parsed here).
    pub payload: serde_json::Value,
}

/// A filtered, paginated, sorted query against the Cartographer log.
#[derive(Debug, Clone, Default)]
pub struct CartographerFilter {
    /// Exact-match source, e.g. `"scheduler"`.
    pub source: Option<String>,
    /// Exact-match scope, e.g. `"run"`.
    pub scope: Option<String>,
    /// Exact-match level, e.g. `"error"`.
    pub level: Option<String>,
    /// Exact-match run id.
    pub run_id: Option<String>,
    /// Exact-match guardian id.
    pub guardian_id: Option<String>,
    /// Exact-match session id.
    pub session_id: Option<String>,
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
///     .run(&run_id)
///     .scope("run")
///     .emit(&store, format!("run {run_id} claimed → running"), serde_json::json!({}));
/// ```
pub struct Note<'a> {
    source: &'a str,
    level: LogLevel,
    scope: Option<&'a str>,
    run_id: Option<&'a str>,
    guardian_id: Option<&'a str>,
    session_id: Option<&'a str>,
    task: Option<&'a str>,
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
            run_id: None,
            guardian_id: None,
            session_id: None,
            task: None,
        }
    }

    /// Set the severity level.
    #[must_use]
    pub fn level(mut self, level: LogLevel) -> Self {
        self.level = level;
        self
    }

    /// Set the entity scope, e.g. `"run"` / `"session"` / `"guardian"`.
    #[must_use]
    pub fn scope(mut self, scope: &'a str) -> Self {
        self.scope = Some(scope);
        self
    }

    /// Attach an owning run id.
    #[must_use]
    pub fn run(mut self, run_id: &'a str) -> Self {
        self.run_id = Some(run_id);
        self
    }

    /// Attach an owning guardian (review) id.
    #[must_use]
    pub fn guardian(mut self, guardian_id: &'a str) -> Self {
        self.guardian_id = Some(guardian_id);
        self
    }

    /// Attach an owning session id.
    #[must_use]
    pub fn session(mut self, session_id: &'a str) -> Self {
        self.session_id = Some(session_id);
        self
    }

    /// Attach an owning task name.
    #[must_use]
    pub fn task(mut self, task: &'a str) -> Self {
        self.task = Some(task);
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
            run_id: self.run_id,
            guardian_id: self.guardian_id,
            session_id: self.session_id,
            task: self.task,
            payload,
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
    pub run_id: Option<&'a str>,
    pub guardian_id: Option<&'a str>,
    pub session_id: Option<&'a str>,
    pub task: Option<&'a str>,
    pub payload: serde_json::Value,
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
    pub fn cartographer_log(&self, entry: CartographerEntry<'_>) -> Result<()> {
        self.conn.execute(
            "INSERT INTO cartographer_events(at_ms, level, source, message, scope, run_id, guardian_id, session_id, task, payload)
             VALUES(?,?,?,?,?,?,?,?,?,?)",
            params![
                now_ms(),
                level_str(entry.level),
                entry.source,
                entry.message,
                entry.scope,
                entry.run_id,
                entry.guardian_id,
                entry.session_id,
                entry.task,
                entry.payload.to_string(),
            ],
        )?;
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
        eq_clause!("run_id", filter.run_id);
        eq_clause!("guardian_id", filter.guardian_id);
        eq_clause!("session_id", filter.session_id);
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
            "SELECT id, at_ms, level, source, message, scope, run_id, guardian_id, session_id, task, payload
             FROM cartographer_events {where_sql}
             ORDER BY at_ms {order}, id {order}
             LIMIT {limit} OFFSET {offset}"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let param_refs: Vec<&dyn rusqlite::ToSql> = values.iter().map(AsRef::as_ref).collect();
        let rows = stmt
            .query_map(param_refs.as_slice(), |r| {
                let payload_str: String = r.get(10)?;
                Ok(CartographerRow {
                    id: r.get(0)?,
                    at_ms: r.get(1)?,
                    level: r.get(2)?,
                    source: r.get(3)?,
                    message: r.get(4)?,
                    scope: r.get(5)?,
                    run_id: r.get(6)?,
                    guardian_id: r.get(7)?,
                    session_id: r.get(8)?,
                    task: r.get(9)?,
                    payload: serde_json::from_str(&payload_str).unwrap_or(serde_json::Value::Null),
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
    /// target's full payload without re-issuing a filtered query).
    pub fn cartographer_get(&self, id: i64) -> Result<Option<CartographerRow>> {
        self.conn
            .query_row(
                "SELECT id, at_ms, level, source, message, scope, run_id, guardian_id, session_id, task, payload
                 FROM cartographer_events WHERE id = ?",
                params![id],
                |r| {
                    let payload_str: String = r.get(10)?;
                    Ok(CartographerRow {
                        id: r.get(0)?,
                        at_ms: r.get(1)?,
                        level: r.get(2)?,
                        source: r.get(3)?,
                        message: r.get(4)?,
                        scope: r.get(5)?,
                        run_id: r.get(6)?,
                        guardian_id: r.get(7)?,
                        session_id: r.get(8)?,
                        task: r.get(9)?,
                        payload: serde_json::from_str(&payload_str)
                            .unwrap_or(serde_json::Value::Null),
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
        Note::new("scheduler").run("run-1").scope("run").emit(
            &store,
            "run run-1 claimed",
            serde_json::json!({"foo": "bar"}),
        );

        let page = store
            .cartographer_query(&CartographerFilter::recent(10))
            .unwrap();
        assert_eq!(page.total, 1);
        assert_eq!(page.rows.len(), 1);
        assert_eq!(page.rows[0].source, "scheduler");
        assert_eq!(page.rows[0].run_id.as_deref(), Some("run-1"));
        assert_eq!(page.rows[0].payload, serde_json::json!({"foo": "bar"}));
    }

    #[test]
    fn filters_by_run_id() {
        let store = Store::open_in_memory().unwrap();
        Note::new("scheduler")
            .run("run-1")
            .emit(&store, "a", serde_json::json!({}));
        Note::new("scheduler")
            .run("run-2")
            .emit(&store, "b", serde_json::json!({}));

        let filter = CartographerFilter {
            run_id: Some("run-1".to_string()),
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
        Note::new("verify").emit(&store, "100% complete", serde_json::json!({}));
        Note::new("verify").emit(&store, "something else", serde_json::json!({}));

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
