//! Triage type registry and pooling (RAL-318) -- the user-facing surface of
//! the daemon's Arbiter subsystem.
//!
//! A cell opts into Triage via `[[task.cell]] triage = true` (see
//! `ralphus_core::schema::CellDef::triage`) instead of naming an explicit
//! `[[review]]`. This module owns:
//!
//! - the Triage **type registry**: store-backed, CLI-mutable, mirroring
//!   `crate::machines`' register/list/get/deregister pattern (deliberately
//!   NOT `crate::agent_profiles`' config-file-only pattern -- a type is meant
//!   to be added/removed at runtime, not redeployed). The built-in
//!   [`UNCLASSIFIED_TYPE`] always exists and can never be deregistered.
//! - the **pool**: cells opted into Triage with a resolved type are pooled by
//!   `(project, triage_type)` until that pool's count threshold or one of its
//!   cron schedule entries fires (see `crate::scheduler`'s Triage tick),
//!   which drains the pool and hands the caller its cells to build a review
//!   from. A cell can resolve to more than one type at once (e.g. both
//!   "bug" and "investigation") -- it is pooled into every one of those
//!   types' pools independently, and draining one never removes it from the
//!   others.
//!
//! Classification itself (asking the Arbiter's agent/model to pick a
//! type -- possibly more than one) lives in `crate::arbiter`, which is a
//! separate concern from this module's pure storage/registry role.

use rusqlite::{OptionalExtension, params};
use serde::Serialize;

use ralphus_core::schema::TaskFile;
use ralphus_core::validate::{ErrorKind, ValidationError};

use crate::store::{Result as StoreResult, Store, now_ms};

/// The built-in Triage type every daemon always has, and the type a
/// classification failure (timeout, error, ambiguous result) permanently
/// assigns -- single-attempt, no retry (RAL-318).
pub const UNCLASSIFIED_TYPE: &str = "unclassified";

/// Starter Triage types seeded once, the first time the `triage_types` table
/// is created (`Store::init_schema`, mirroring `secret_env_names`'
/// `DEFAULT_SECRET_ENV_NAMES` seeding pattern) -- never re-seeded afterward,
/// so deregistering one of these is honored across every later restart.
/// `(name, label, description)`.
pub const DEFAULT_TRIAGE_TYPES: &[(&str, &str, &str)] = &[
    (
        "bug",
        "Bug",
        "An error or flaw in a program that causes unexpected results. This could be UX issues or technical issues.",
    ),
    (
        "feature",
        "Feature",
        "A new capability or improvement that did not exist before",
    ),
    (
        "investigation",
        "Investigation",
        "Research into a topic or thought process. The final result may be a simple report or code changes.",
    ),
];

/// A registered Triage type.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TriageTypeView {
    pub name: String,
    pub label: String,
    pub description: String,
    pub created_at_ms: i64,
}

/// The outcome of attempting to deregister a Triage type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeregisterOutcome {
    Removed,
    NotFound,
    /// [`UNCLASSIFIED_TYPE`] can never be deregistered.
    BuiltIn,
}

/// One cell drained from a pool once it fires -- enough to attach the cell's
/// worktree branch to a freshly created review guardian (see
/// `crate::reviews::derive_triage_pools`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TriagePoolCellRow {
    pub squad_id: String,
    pub task_idx: i64,
    pub idx: i64,
    pub branch: String,
    pub upstream: String,
}

/// One configured cron-style schedule entry for a `(project, triage_type)` pool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TriageScheduleRow {
    pub id: i64,
    pub project: String,
    pub triage_type: String,
    pub cron_expr: String,
    pub anchor_date_ms: i64,
    pub every_n: i64,
    pub occurrence_count: i64,
    pub last_checked_ms: Option<i64>,
}

// ── Type registry ───────────────────────────────────────────────────────────

impl Store {
    /// Register (or update) a Triage type. Upserts on `name`, matching
    /// [`Store::register_machine_provider`]'s behavior.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn register_triage_type(
        &self,
        name: &str,
        label: &str,
        description: &str,
    ) -> StoreResult<()> {
        self.conn.execute(
            "INSERT INTO triage_types(name, label, description, created_at_ms)
             VALUES(?,?,?,?)
             ON CONFLICT(name) DO UPDATE SET label=excluded.label, description=excluded.description",
            params![name.trim(), label, description, now_ms()],
        )?;
        crate::rlog!(INFO, "ralphus [store] triage type {name:?} registered");
        let _ = self.cartographer_log(crate::cartographer::CartographerEntry {
            level: crate::logging::LogLevel::INFO,
            source: "store",
            message: "triage type registered",
            scope: Some("triage"),
            squad_id: None,
            guardian_id: None,
            cell_id: None,
            task: None,
            log_path: None,
            payload: serde_json::json!({ "name": name }),
            admin_only: false,
        });
        Ok(())
    }

    /// Every registered Triage type, alphabetical by name.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn list_triage_types(&self) -> StoreResult<Vec<TriageTypeView>> {
        let mut stmt = self.conn.prepare(
            "SELECT name, label, description, created_at_ms FROM triage_types ORDER BY name",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(TriageTypeView {
                    name: r.get(0)?,
                    label: r.get(1)?,
                    description: r.get(2)?,
                    created_at_ms: r.get(3)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// One Triage type by exact name, or `None`.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn get_triage_type(&self, name: &str) -> StoreResult<Option<TriageTypeView>> {
        Ok(self
            .list_triage_types()?
            .into_iter()
            .find(|t| t.name == name.trim()))
    }

    /// Remove a Triage type. [`UNCLASSIFIED_TYPE`] is refused outright.
    ///
    /// Deliberately does not check whether any historical cell still
    /// references this type -- same rationale as
    /// [`Store::deregister_machine_provider`]: a past squad already resolved
    /// its type, and a historical record should not block cleaning up the
    /// registry.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn deregister_triage_type(&self, name: &str) -> StoreResult<DeregisterOutcome> {
        let name = name.trim();
        if name.eq_ignore_ascii_case(UNCLASSIFIED_TYPE) {
            return Ok(DeregisterOutcome::BuiltIn);
        }
        let n = self
            .conn
            .execute("DELETE FROM triage_types WHERE name = ?", params![name])?;
        if n == 0 {
            return Ok(DeregisterOutcome::NotFound);
        }
        crate::rlog!(INFO, "ralphus [store] triage type {name:?} removed");
        let _ = self.cartographer_log(crate::cartographer::CartographerEntry {
            level: crate::logging::LogLevel::INFO,
            source: "store",
            message: "triage type removed",
            scope: Some("triage"),
            squad_id: None,
            guardian_id: None,
            cell_id: None,
            task: None,
            log_path: None,
            payload: serde_json::json!({ "name": name }),
            admin_only: false,
        });
        Ok(DeregisterOutcome::Removed)
    }
}

// ── Per-cell resolved type (classification result) ──────────────────────────

impl Store {
    /// Persist the resolved Triage type(s) for one cell -- either the cell's
    /// own inline `triage_type` list, or the Arbiter's classification
    /// result(s). Called exactly once per cell, at `ralphus submit` time.
    /// Replaces (delete-then-insert) any prior rows for this cell so a
    /// re-submission of the same squad id (should that ever happen) does not
    /// error or accumulate duplicates, though in practice this is
    /// write-once. A cell can resolve to more than one type -- see this
    /// module's doc comment -- so each is its own row.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn set_cell_triage_types(
        &self,
        squad_id: &str,
        task_idx: i64,
        idx: i64,
        triage_types: &[String],
    ) -> StoreResult<()> {
        self.conn.execute(
            "DELETE FROM triage_cell_types WHERE squad_id=? AND task_idx=? AND idx=?",
            params![squad_id, task_idx, idx],
        )?;
        for triage_type in triage_types {
            self.conn.execute(
                "INSERT INTO triage_cell_types(squad_id, task_idx, idx, triage_type, created_at_ms)
                 VALUES(?,?,?,?,?)",
                params![squad_id, task_idx, idx, triage_type, now_ms()],
            )?;
        }
        Ok(())
    }

    /// The resolved Triage type(s) for one cell, alphabetical by name. Empty
    /// means the cell was never classified (not a Triage cell, or
    /// classification hasn't run yet).
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn get_cell_triage_types(
        &self,
        squad_id: &str,
        task_idx: i64,
        idx: i64,
    ) -> StoreResult<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT triage_type FROM triage_cell_types WHERE squad_id=? AND task_idx=? AND idx=? ORDER BY triage_type",
        )?;
        let rows = stmt
            .query_map(params![squad_id, task_idx, idx], |r| r.get(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }
}

// ── Pool ─────────────────────────────────────────────────────────────────────

impl Store {
    /// Add one cell to the `(project, triage_type)` pool.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    #[allow(clippy::too_many_arguments)]
    pub fn record_triage_pool_cell(
        &self,
        project: &str,
        triage_type: &str,
        squad_id: &str,
        task_idx: i64,
        idx: i64,
        branch: &str,
        upstream: &str,
    ) -> StoreResult<()> {
        self.conn.execute(
            "INSERT INTO triage_pool_cells(project, triage_type, squad_id, task_idx, idx, branch, upstream, created_at_ms)
             VALUES(?,?,?,?,?,?,?,?)",
            params![project, triage_type, squad_id, task_idx, idx, branch, upstream, now_ms()],
        )?;
        Ok(())
    }

    /// Current pool size for `(project, triage_type)`.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn triage_pool_count(&self, project: &str, triage_type: &str) -> StoreResult<i64> {
        self.conn
            .query_row(
                "SELECT COUNT(*) FROM triage_pool_cells WHERE project=? AND triage_type=?",
                params![project, triage_type],
                |r| r.get(0),
            )
            .map_err(Into::into)
    }

    /// Every distinct `(project, triage_type)` key with at least one pooled
    /// cell -- what the scheduler's Triage tick iterates over, and what the
    /// board's Triage tab lists as "current pool state".
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn triage_pool_keys(&self) -> StoreResult<Vec<(String, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT project, triage_type FROM triage_pool_cells ORDER BY project, triage_type",
        )?;
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Every distinct `(project, triage_type)` key with a configured count
    /// threshold, regardless of whether it currently has any pooled cells --
    /// unlike [`Store::triage_pool_keys`], which only sees keys with at least
    /// one pooled cell. The board's Triage tab unions this with
    /// `triage_pool_keys` so a threshold set ahead of the first cell (e.g.
    /// "fire every 4 bug fixes for this project" configured before any bug
    /// fix has landed yet) is visible and editable immediately, the same way
    /// a cron schedule already is.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn triage_threshold_keys(&self) -> StoreResult<Vec<(String, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT project, triage_type FROM triage_pool_thresholds ORDER BY project, triage_type",
        )?;
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Atomically remove and return every cell currently pooled for
    /// `(project, triage_type)`. Race-safety note: every `Store` method is
    /// called through the daemon's single `Arc<Mutex<Store>>` (see
    /// `crate::scheduler`/`crate::server`), so this single-statement
    /// `DELETE ... RETURNING` already can't race a concurrent
    /// insert/drain from another thread -- the same reliance every other
    /// cumulative-then-act sequence in this codebase makes (e.g.
    /// `guardian_merge.rs`'s cost-cap check). No cell can be double-drained:
    /// once removed here, it is gone from the pool for every other caller.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn drain_triage_pool(
        &self,
        project: &str,
        triage_type: &str,
    ) -> StoreResult<Vec<TriagePoolCellRow>> {
        let mut stmt = self.conn.prepare(
            "DELETE FROM triage_pool_cells WHERE project=? AND triage_type=?
             RETURNING squad_id, task_idx, idx, branch, upstream",
        )?;
        let rows = stmt
            .query_map(params![project, triage_type], |r| {
                Ok(TriagePoolCellRow {
                    squad_id: r.get(0)?,
                    task_idx: r.get(1)?,
                    idx: r.get(2)?,
                    branch: r.get(3)?,
                    upstream: r.get(4)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Set (or clear, with `None`) the count threshold for `(project,
    /// triage_type)`. A pool with no threshold configured never fires on
    /// count alone -- see this module's doc comment.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn set_triage_pool_threshold(
        &self,
        project: &str,
        triage_type: &str,
        threshold: Option<i64>,
    ) -> StoreResult<()> {
        match threshold {
            Some(n) => {
                self.conn.execute(
                    "INSERT INTO triage_pool_thresholds(project, triage_type, threshold_count, updated_at_ms)
                     VALUES(?,?,?,?)
                     ON CONFLICT(project, triage_type) DO UPDATE SET threshold_count=excluded.threshold_count, updated_at_ms=excluded.updated_at_ms",
                    params![project, triage_type, n, now_ms()],
                )?;
            }
            None => {
                self.conn.execute(
                    "DELETE FROM triage_pool_thresholds WHERE project=? AND triage_type=?",
                    params![project, triage_type],
                )?;
            }
        }
        Ok(())
    }

    /// The configured count threshold for `(project, triage_type)`, or `None`
    /// when unconfigured.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn get_triage_pool_threshold(
        &self,
        project: &str,
        triage_type: &str,
    ) -> StoreResult<Option<i64>> {
        self.conn
            .query_row(
                "SELECT threshold_count FROM triage_pool_thresholds WHERE project=? AND triage_type=?",
                params![project, triage_type],
                |r| r.get(0),
            )
            .optional()
            .map_err(Into::into)
    }

    /// Register a new cron schedule entry for `(project, triage_type)`.
    /// Validates `cron_expr` parses before storing it.
    ///
    /// # Errors
    /// Returns a message when `cron_expr` fails to parse, or propagates any
    /// SQLite failure.
    pub fn add_triage_schedule(
        &self,
        project: &str,
        triage_type: &str,
        cron_expr: &str,
        anchor_date_ms: i64,
        every_n: i64,
    ) -> std::result::Result<i64, String> {
        use std::str::FromStr as _;
        cron::Schedule::from_str(cron_expr)
            .map_err(|e| format!("invalid cron expression {cron_expr:?}: {e}"))?;
        if every_n < 1 {
            return Err("every_n must be at least 1".to_string());
        }
        self.conn
            .execute(
                "INSERT INTO triage_schedules(project, triage_type, cron_expr, anchor_date_ms, every_n, occurrence_count, last_checked_ms, created_at_ms)
                 VALUES(?,?,?,?,?,0,NULL,?)",
                params![project, triage_type, cron_expr, anchor_date_ms, every_n, now_ms()],
            )
            .map_err(|e| e.to_string())?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Every configured schedule entry, optionally filtered by `(project,
    /// triage_type)`.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn list_triage_schedules(
        &self,
        filter: Option<(&str, &str)>,
    ) -> StoreResult<Vec<TriageScheduleRow>> {
        let sql = "SELECT id, project, triage_type, cron_expr, anchor_date_ms, every_n, occurrence_count, last_checked_ms
                   FROM triage_schedules";
        let to_row = |r: &rusqlite::Row| {
            Ok(TriageScheduleRow {
                id: r.get(0)?,
                project: r.get(1)?,
                triage_type: r.get(2)?,
                cron_expr: r.get(3)?,
                anchor_date_ms: r.get(4)?,
                every_n: r.get(5)?,
                occurrence_count: r.get(6)?,
                last_checked_ms: r.get(7)?,
            })
        };
        let rows = if let Some((project, triage_type)) = filter {
            let mut stmt = self.conn.prepare(&format!(
                "{sql} WHERE project=? AND triage_type=? ORDER BY id"
            ))?;
            stmt.query_map(params![project, triage_type], to_row)?
                .collect::<std::result::Result<Vec<_>, _>>()?
        } else {
            let mut stmt = self.conn.prepare(&format!("{sql} ORDER BY id"))?;
            stmt.query_map([], to_row)?
                .collect::<std::result::Result<Vec<_>, _>>()?
        };
        Ok(rows)
    }

    /// Remove a schedule entry. Returns whether a row was deleted.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn remove_triage_schedule(&self, id: i64) -> StoreResult<bool> {
        let n = self
            .conn
            .execute("DELETE FROM triage_schedules WHERE id=?", params![id])?;
        Ok(n > 0)
    }

    /// Advance a schedule's cursor by one cron occurrence (RAL-318's
    /// scheduler tick calls this each time it steps a schedule past an
    /// occurrence, whether or not that occurrence was the qualifying
    /// `every_n`th one).
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn advance_triage_schedule(
        &self,
        id: i64,
        occurrence_count: i64,
        last_checked_ms: i64,
    ) -> StoreResult<()> {
        self.conn.execute(
            "UPDATE triage_schedules SET occurrence_count=?, last_checked_ms=? WHERE id=?",
            params![occurrence_count, last_checked_ms, id],
        )?;
        Ok(())
    }
}

/// Whether occurrence number `occurrence_count` (1-based count of cron
/// occurrences passed since the schedule's anchor) is the qualifying
/// `every_n`th one. `every_n <= 1` means every occurrence qualifies.
///
/// Pure function (no store access) so the parity math is unit-testable
/// without a scheduler or a database.
#[must_use]
pub fn schedule_occurrence_fires(occurrence_count: i64, every_n: i64) -> bool {
    every_n <= 1 || occurrence_count.rem_euclid(every_n.max(1)) == 0
}

/// Given a schedule's `cron_expr` and its cursor (`after`, defaulting to the
/// anchor when `None`), how many occurrences have passed by `now` -- advanced
/// one occurrence at a time rather than replaying full history, so a
/// long-lived schedule never pays for its own age. Returns the new
/// `(occurrence_count_delta, last_occurrence_ms, fired)` where `fired` is
/// true if any of the newly-passed occurrences was a qualifying `every_n`th
/// one. Returns `None` if `cron_expr` fails to parse (should not happen for a
/// schedule that passed [`Store::add_triage_schedule`]'s validation) or no
/// occurrence has passed yet.
#[must_use]
pub fn advance_schedule(
    cron_expr: &str,
    anchor_date_ms: i64,
    after_ms: Option<i64>,
    every_n: i64,
    starting_occurrence_count: i64,
    now_ms: i64,
) -> Option<(i64, i64, bool)> {
    use std::str::FromStr as _;
    let schedule = cron::Schedule::from_str(cron_expr).ok()?;
    let cursor = after_ms.unwrap_or(anchor_date_ms);
    let cursor_dt = chrono::DateTime::from_timestamp_millis(cursor)?;
    let now_dt = chrono::DateTime::from_timestamp_millis(now_ms)?;
    let mut occurrence_count = starting_occurrence_count;
    let mut last_occurrence_ms = cursor;
    let mut fired = false;
    // Bounded: a misconfigured very-frequent cron on a long-idle daemon
    // should not spin forever catching up. 100_000 occurrences is far beyond
    // any realistic poll gap for a schedule meant to fire monthly/weekly.
    for occurrence in schedule.after(&cursor_dt).take(100_000) {
        if occurrence > now_dt {
            break;
        }
        occurrence_count += 1;
        last_occurrence_ms = occurrence.timestamp_millis();
        if schedule_occurrence_fires(occurrence_count, every_n) {
            fired = true;
        }
    }
    if last_occurrence_ms == cursor {
        return None;
    }
    Some((occurrence_count, last_occurrence_ms, fired))
}

// ── Scheduler tick ───────────────────────────────────────────────────────────

/// The scheduler's Triage hook (RAL-318), called on its own interval
/// (`crate::scheduler::TRIAGE_SCHEDULE_INTERVAL`) alongside the loop's other
/// `Duration`-based maintenance blocks. Steps every configured schedule
/// entry forward by whatever cron occurrences have passed since it was last
/// checked (see [`advance_schedule`]) and, for any that just crossed a
/// qualifying `every_n`th occurrence, drains that `(project, triage_type)`
/// pool into a fresh review -- racing safely against a concurrent
/// submission's own count-threshold check (both ultimately call
/// [`crate::reviews::create_review_from_triage_pool`], which no-ops on an
/// already-empty pool).
pub fn run_schedule_tick(store: &std::sync::Arc<std::sync::Mutex<Store>>) {
    let now = now_ms();
    let schedules = {
        let guard = store.lock().expect("store mutex poisoned");
        guard.list_triage_schedules(None).unwrap_or_default()
    };
    for sched in schedules {
        let Some((count, last, fired)) = advance_schedule(
            &sched.cron_expr,
            sched.anchor_date_ms,
            sched.last_checked_ms,
            sched.every_n,
            sched.occurrence_count,
            now,
        ) else {
            continue;
        };
        let guard = store.lock().expect("store mutex poisoned");
        let _ = guard.advance_triage_schedule(sched.id, count, last);
        if fired {
            match crate::reviews::create_review_from_triage_pool(
                &guard,
                &sched.project,
                &sched.triage_type,
            ) {
                Ok(Some(gid)) => crate::cartographer::Note::new("scheduler")
                    .guardian(&gid)
                    .scope("guardian")
                    .emit(
                        &guard,
                        format!(
                            "triage schedule {} ({}, {}) fired -> review {gid}",
                            sched.id, sched.project, sched.triage_type
                        ),
                        serde_json::json!({
                            "schedule_id": sched.id,
                            "project": sched.project,
                            "triage_type": sched.triage_type,
                            "guardian_id": gid,
                        }),
                    ),
                Ok(None) => {}
                Err(e) => crate::cartographer::Note::new("scheduler")
                    .level(crate::logging::LogLevel::WARNING)
                    .emit(
                        &guard,
                        format!("triage schedule {} fire failed: {e}", sched.id),
                        serde_json::json!({"schedule_id": sched.id, "error": e.to_string()}),
                    ),
            }
        }
        drop(guard);
    }
}

// ── Submit-time registry validation ──────────────────────────────────────────

/// Best-effort 1-based line number of the `triage_type = "<value>"` occurrence
/// for the `n`th cell (in file order) declaring this exact value, scanning
/// forward from `search_from_line` so repeated identical values in different
/// cells each resolve to their own occurrence rather than all pointing at the
/// first one. `core::validate`'s own `HeaderIndex` line-finder isn't
/// reachable from here (it's private to that crate's `validate` module), so
/// this is a simpler, purely textual re-scan -- good enough for pointing a
/// user at "roughly which line", not a byte-exact guarantee.
fn find_triage_type_line(raw_toml: &str, value: &str, search_from_line: usize) -> Option<u32> {
    let needle = format!("\"{value}\"");
    for (i, line) in raw_toml.lines().enumerate().skip(search_from_line) {
        let trimmed = line.trim_start();
        if trimmed.starts_with("triage_type") && line.contains(&needle) {
            return Some((i + 1) as u32);
        }
    }
    None
}

/// Validate every cell's inline `triage_type` (RAL-318) against the daemon's
/// Triage type registry -- `core::validate::check_triage` only checked
/// structure (non-empty, requires `triage = true`), since `core` has no store
/// access. A submission naming an unregistered type fails here, listing the
/// currently registered types plus the offending line number (best-effort;
/// see [`find_triage_type_line`]).
///
/// Only inline-declared types are checked here -- a cell left to Arbiter
/// classification (`triage = true` with no `triage_type`) always resolves to
/// either a real registered type or [`UNCLASSIFIED_TYPE`], both of which are
/// registered by construction, so there is nothing to validate for it.
#[must_use]
pub fn validate_task_file_triage_types(
    store: &Store,
    raw_toml: &str,
    file: &TaskFile,
) -> Vec<ValidationError> {
    let mut errors = Vec::new();
    let registered = store.list_triage_types().unwrap_or_default();
    let known_names: Vec<&str> = registered.iter().map(|t| t.name.as_str()).collect();
    let mut search_from_line = 0usize;
    for (task_idx, task) in file.task.iter().enumerate() {
        for (cell_idx, cell) in task.cell.iter().enumerate() {
            let Some(declared_types) = cell.triage_type.as_deref() else {
                continue;
            };
            for declared in declared_types {
                let declared = declared.trim();
                if declared.is_empty() {
                    continue; // reported separately by core::validate::check_triage
                }
                if known_names.iter().any(|n| n.eq_ignore_ascii_case(declared)) {
                    continue;
                }
                let line = find_triage_type_line(raw_toml, declared, search_from_line);
                if let Some(l) = line {
                    search_from_line = l as usize;
                }
                errors.push(ValidationError {
                    path: format!("task[{task_idx}].cell[{cell_idx}].triage_type"),
                    kind: ErrorKind::InvalidValue,
                    message: format!(
                        "triage_type \"{declared}\" is not a registered Triage type (registered: {})",
                        if known_names.is_empty() {
                            "none".to_string()
                        } else {
                            known_names.join(", ")
                        }
                    ),
                    line,
                });
            }
        }
    }
    errors
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> Store {
        Store::open_in_memory().expect("in-memory store")
    }

    #[test]
    fn unclassified_type_always_exists_and_cannot_be_removed() {
        let s = store();
        assert!(s.get_triage_type(UNCLASSIFIED_TYPE).unwrap().is_some());
        assert_eq!(
            s.deregister_triage_type(UNCLASSIFIED_TYPE).unwrap(),
            DeregisterOutcome::BuiltIn
        );
        assert!(s.get_triage_type(UNCLASSIFIED_TYPE).unwrap().is_some());
    }

    #[test]
    fn default_triage_types_are_seeded_on_a_fresh_store() {
        let s = store();
        for (name, ..) in DEFAULT_TRIAGE_TYPES {
            assert!(
                s.get_triage_type(name).unwrap().is_some(),
                "{name:?} should be seeded by default alongside {UNCLASSIFIED_TYPE:?}"
            );
        }
        // Unlike `unclassified`, these are ordinary (removable) types.
        assert_eq!(
            s.deregister_triage_type(DEFAULT_TRIAGE_TYPES[0].0).unwrap(),
            DeregisterOutcome::Removed
        );
    }

    #[test]
    fn register_upserts_and_deregister_removes() {
        let s = store();
        s.register_triage_type("security", "Security", "Security-sensitive changes")
            .unwrap();
        s.register_triage_type("security", "Security", "updated description")
            .unwrap();
        let all = s.list_triage_types().unwrap();
        assert_eq!(
            all.iter().filter(|t| t.name == "security").count(),
            1,
            "re-registering must upsert, not duplicate"
        );
        assert_eq!(
            s.get_triage_type("security").unwrap().unwrap().description,
            "updated description"
        );
        assert_eq!(
            s.deregister_triage_type("security").unwrap(),
            DeregisterOutcome::Removed
        );
        assert!(s.get_triage_type("security").unwrap().is_none());
    }

    #[test]
    fn deregister_unknown_type_reports_not_found() {
        let s = store();
        assert_eq!(
            s.deregister_triage_type("nope").unwrap(),
            DeregisterOutcome::NotFound
        );
    }

    #[test]
    fn cell_triage_type_round_trips_and_replaces() {
        let s = store();
        assert!(s.get_cell_triage_types("squad-1", 0, 0).unwrap().is_empty());
        s.set_cell_triage_types("squad-1", 0, 0, &["security".to_string()])
            .unwrap();
        assert_eq!(
            s.get_cell_triage_types("squad-1", 0, 0).unwrap(),
            vec!["security".to_string()]
        );
        s.set_cell_triage_types("squad-1", 0, 0, &[UNCLASSIFIED_TYPE.to_string()])
            .unwrap();
        assert_eq!(
            s.get_cell_triage_types("squad-1", 0, 0).unwrap(),
            vec![UNCLASSIFIED_TYPE.to_string()]
        );
    }

    #[test]
    fn cell_can_resolve_to_more_than_one_triage_type() {
        let s = store();
        s.set_cell_triage_types(
            "squad-1",
            0,
            0,
            &["bug".to_string(), "investigation".to_string()],
        )
        .unwrap();
        assert_eq!(
            s.get_cell_triage_types("squad-1", 0, 0).unwrap(),
            vec!["bug".to_string(), "investigation".to_string()]
        );
    }

    #[test]
    fn pool_accumulates_and_drains_atomically() {
        let s = store();
        assert_eq!(s.triage_pool_count("proj", "security").unwrap(), 0);
        s.record_triage_pool_cell("proj", "security", "squad-1", 0, 0, "b1", "main")
            .unwrap();
        s.record_triage_pool_cell("proj", "security", "squad-2", 0, 0, "b2", "main")
            .unwrap();
        s.record_triage_pool_cell("proj", "perf", "squad-3", 0, 0, "b3", "main")
            .unwrap();
        assert_eq!(s.triage_pool_count("proj", "security").unwrap(), 2);
        assert_eq!(
            s.triage_pool_keys().unwrap(),
            vec![
                ("proj".to_string(), "perf".to_string()),
                ("proj".to_string(), "security".to_string())
            ]
        );
        let drained = s.drain_triage_pool("proj", "security").unwrap();
        assert_eq!(drained.len(), 2);
        assert_eq!(s.triage_pool_count("proj", "security").unwrap(), 0);
        // The "perf" pool is untouched by draining "security".
        assert_eq!(s.triage_pool_count("proj", "perf").unwrap(), 1);
    }

    #[test]
    fn pool_threshold_round_trips_and_clears() {
        let s = store();
        assert!(
            s.get_triage_pool_threshold("proj", "security")
                .unwrap()
                .is_none()
        );
        s.set_triage_pool_threshold("proj", "security", Some(5))
            .unwrap();
        assert_eq!(
            s.get_triage_pool_threshold("proj", "security").unwrap(),
            Some(5)
        );
        s.set_triage_pool_threshold("proj", "security", None)
            .unwrap();
        assert!(
            s.get_triage_pool_threshold("proj", "security")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn add_schedule_rejects_invalid_cron() {
        let s = store();
        assert!(
            s.add_triage_schedule("proj", "security", "not a cron", 0, 1)
                .is_err()
        );
    }

    #[test]
    fn schedule_lifecycle() {
        let s = store();
        // 6-field cron (seconds field first), as the `cron` crate expects.
        let id = s
            .add_triage_schedule("proj", "security", "0 0 0 * * *", 0, 2)
            .unwrap();
        let all = s.list_triage_schedules(Some(("proj", "security"))).unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].id, id);
        assert_eq!(all[0].every_n, 2);
        assert_eq!(all[0].occurrence_count, 0);
        s.advance_triage_schedule(id, 3, 12345).unwrap();
        let all = s.list_triage_schedules(None).unwrap();
        assert_eq!(all[0].occurrence_count, 3);
        assert_eq!(all[0].last_checked_ms, Some(12345));
        assert!(s.remove_triage_schedule(id).unwrap());
        assert!(!s.remove_triage_schedule(id).unwrap());
    }

    #[test]
    fn schedule_occurrence_fires_respects_every_n() {
        assert!(schedule_occurrence_fires(1, 1));
        assert!(schedule_occurrence_fires(1, 0));
        assert!(!schedule_occurrence_fires(1, 2));
        assert!(schedule_occurrence_fires(2, 2));
        assert!(!schedule_occurrence_fires(3, 2));
        assert!(schedule_occurrence_fires(6, 3));
    }

    #[test]
    fn advance_schedule_steps_daily_cron_and_reports_firing() {
        // Daily at midnight UTC, anchored at day 0. `every_n=2` means
        // "every other day".
        let day_ms = 86_400_000_i64;
        let cron_expr = "0 0 0 * * *";
        // First occurrence after anchor is day 1 (day 0 itself is the
        // schedule's "at" time, but `after` is exclusive of the start).
        let now = day_ms * 3; // three days later
        let (count, last, fired) =
            advance_schedule(cron_expr, 0, None, 2, 0, now).expect("should advance");
        assert_eq!(count, 3, "days 1, 2, 3 all passed");
        assert_eq!(last, day_ms * 3);
        assert!(fired, "day 2 (occurrence #2) qualifies for every_n=2");
    }

    #[test]
    fn run_schedule_tick_fires_a_due_schedule_and_creates_a_review() {
        let store = std::sync::Arc::new(std::sync::Mutex::new(Store::open_in_memory().unwrap()));
        {
            let guard = store.lock().unwrap();
            guard
                .record_triage_pool_cell("proj", "security", "squad-1", 0, 0, "b1", "main")
                .unwrap();
            // Anchored a few seconds in the past so this "every second" cron
            // is already due without needing to catch up decades of history
            // from the epoch (`now_ms()` is real wall-clock time here, since
            // `run_schedule_tick` doesn't take an injectable clock).
            guard
                .add_triage_schedule("proj", "security", "* * * * * *", now_ms() - 5_000, 1)
                .unwrap();
        }
        run_schedule_tick(&store);
        let guard = store.lock().unwrap();
        assert_eq!(guard.triage_pool_count("proj", "security").unwrap(), 0);
        let guardians = guard.list_guardians().unwrap();
        assert_eq!(guardians.len(), 1);
        assert_eq!(
            guardians[0].origin,
            crate::guardian::GUARDIAN_ORIGIN_ARBITER
        );
        let schedules = guard.list_triage_schedules(None).unwrap();
        assert!(schedules[0].occurrence_count >= 1);
        assert!(schedules[0].last_checked_ms.is_some());
    }

    #[test]
    fn run_schedule_tick_does_not_fire_a_not_yet_due_schedule() {
        let store = std::sync::Arc::new(std::sync::Mutex::new(Store::open_in_memory().unwrap()));
        {
            let guard = store.lock().unwrap();
            guard
                .record_triage_pool_cell("proj", "security", "squad-1", 0, 0, "b1", "main")
                .unwrap();
            // Yearly on Jan 1st, anchored at "now" -- no occurrence has
            // passed yet.
            guard
                .add_triage_schedule("proj", "security", "0 0 0 1 1 *", now_ms(), 1)
                .unwrap();
        }
        run_schedule_tick(&store);
        let guard = store.lock().unwrap();
        assert_eq!(guard.triage_pool_count("proj", "security").unwrap(), 1);
        assert!(guard.list_guardians().unwrap().is_empty());
    }

    #[test]
    fn advance_schedule_returns_none_when_nothing_passed_yet() {
        let cron_expr = "0 0 0 1 1 * "; // yearly, Jan 1st
        let now = 1000; // barely past the anchor
        assert!(advance_schedule(cron_expr, 0, None, 1, 0, now).is_none());
    }

    fn task_file_with_triage_type(triage_type: &str) -> TaskFile {
        let src = format!(
            "[[task]]\nname=\"t\"\nproject=\"proj\"\n[[task.cell]]\ncwd=\"/repo\"\nprompt=\"p\"\ntriage=true\ntriage_type=\"{triage_type}\"\n"
        );
        toml::from_str(&src).expect("parse task file")
    }

    #[test]
    fn validate_task_file_triage_types_accepts_a_registered_type() {
        let s = store();
        s.register_triage_type("security", "Security", "").unwrap();
        let file = task_file_with_triage_type("security");
        let src = "[[task]]\nname=\"t\"\nproject=\"proj\"\n[[task.cell]]\ncwd=\"/repo\"\nprompt=\"p\"\ntriage=true\ntriage_type=\"security\"\n";
        assert!(validate_task_file_triage_types(&s, src, &file).is_empty());
    }

    #[test]
    fn validate_task_file_triage_types_accepts_the_builtin_unclassified_type() {
        let s = store();
        let file = task_file_with_triage_type(UNCLASSIFIED_TYPE);
        let src = format!(
            "[[task]]\nname=\"t\"\nproject=\"proj\"\n[[task.cell]]\ncwd=\"/repo\"\nprompt=\"p\"\ntriage=true\ntriage_type=\"{UNCLASSIFIED_TYPE}\"\n"
        );
        assert!(validate_task_file_triage_types(&s, &src, &file).is_empty());
    }

    #[test]
    fn validate_task_file_triage_types_rejects_unregistered_type_with_line_and_list() {
        let s = store();
        s.register_triage_type("security", "Security", "").unwrap();
        let src = "[[task]]\nname=\"t\"\nproject=\"proj\"\n[[task.cell]]\ncwd=\"/repo\"\nprompt=\"p\"\ntriage=true\ntriage_type=\"nope\"\n";
        let file: TaskFile = toml::from_str(src).unwrap();
        let errors = validate_task_file_triage_types(&s, src, &file);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].message.contains("nope"));
        assert!(errors[0].message.contains("security"));
        assert!(errors[0].message.contains(UNCLASSIFIED_TYPE));
        assert_eq!(errors[0].line, Some(8));
    }

    #[test]
    fn validate_task_file_triage_types_checks_every_element_of_a_multi_type_cell() {
        let s = store();
        s.register_triage_type("bug", "Bug", "").unwrap();
        let src = "[[task]]\nname=\"t\"\nproject=\"proj\"\n[[task.cell]]\ncwd=\"/repo\"\nprompt=\"p\"\ntriage=true\ntriage_type=[\"bug\",\"nope\"]\n";
        let file: TaskFile = toml::from_str(src).unwrap();
        let errors = validate_task_file_triage_types(&s, src, &file);
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].message.contains("nope"));
    }
}
