//! Cross-squad waypoints (RAL-400) -- schema/store layer (Phase 1).
//!
//! A waypoint is a named, open/closed join point that tracks a roster of
//! reviews/squads and accumulates append-only guidance ("bearings") for
//! them. This module owns the roster/bearing/injection CRUD and the
//! terminal-state auto-close computation; the survey pass that populates
//! `survey_verdict`/`survey_rationale` (Phase 2) and the actual injection
//! delivery mechanism (Phase 5) are not implemented here -- see
//! `.agent/waypoints-phase0-decisions.md` for the full design.

use std::collections::{BTreeMap, BTreeSet};

use rusqlite::{OptionalExtension, params};
use serde::Serialize;

use crate::guardian::GuardianStatus;
use crate::store::{Result as StoreResult, Store, StoreError, now_ms};
use crate::triage::SubprojectResolution;

/// A waypoint/review/squad's aggregate monorepo-subproject footprint (the
/// Phase 0 actionable-notification matching model, see
/// `.agent/waypoints-phase0-decisions.md`). Not yet consumed by anything --
/// the survey pass that reads this to decide which candidates to check
/// (RAL-400 Phase 2) hasn't been built.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Scope {
    /// At least one contributing cell couldn't be narrowed to a concrete
    /// subproject set (not a monorepo, or resolution just hasn't run yet).
    /// Contagious under union: a false negative (silently skipping a
    /// candidate that should have been surveyed) is unacceptable, while a
    /// false positive only costs one extra survey call.
    RepoWide,
    /// The union of every contributing cell's resolved subproject set.
    Areas(BTreeSet<String>),
}

/// Aggregate a set of per-cell subproject resolutions into one [`Scope`]:
/// `RepoWide` if any cell is `NotApplicable`/`Unresolved`, else the union of
/// every `Resolved` cell's subproject set.
#[must_use]
pub fn aggregate_scope(cells: &[SubprojectResolution]) -> Scope {
    let mut areas = BTreeSet::new();
    for cell in cells {
        match cell {
            SubprojectResolution::NotApplicable | SubprojectResolution::Unresolved => {
                return Scope::RepoWide;
            }
            SubprojectResolution::Resolved { subprojects, .. } => {
                areas.extend(subprojects.iter().cloned());
            }
        }
    }
    Scope::Areas(areas)
}

/// Merge two scopes for the same project. `RepoWide` wins (contagious),
/// matching [`aggregate_scope`]'s own false-negatives-unacceptable rule.
fn union_scope(a: &Scope, b: &Scope) -> Scope {
    match (a, b) {
        (Scope::RepoWide, _) | (_, Scope::RepoWide) => Scope::RepoWide,
        (Scope::Areas(x), Scope::Areas(y)) => Scope::Areas(x.union(y).cloned().collect()),
    }
}

/// Which kind of entity a roster entry or bearing producer refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RosterEntryKind {
    Review,
    Squad,
}

impl RosterEntryKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Review => "review",
            Self::Squad => "squad",
        }
    }

    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "review" => Self::Review,
            "squad" => Self::Squad,
            _ => return None,
        })
    }
}

/// Whether a roster entry's waypoint guidance is a hard gate (`block`,
/// delivery is required) or informational (`advisory`, delivery is
/// best-effort). Only meaningful on waypoints with `allow_advisory = true`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RosterMode {
    Block,
    Advisory,
}

impl RosterMode {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Block => "block",
            Self::Advisory => "advisory",
        }
    }

    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "block" => Self::Block,
            "advisory" => Self::Advisory,
            _ => return None,
        })
    }
}

/// Whether a roster entry's waypoint guidance has reached it yet.
/// `via_restack` distinguishes delivery folded into an unrelated rebase from
/// a dedicated injection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryStatus {
    Undelivered,
    Delivered,
    ViaRestack,
    Failed,
}

impl DeliveryStatus {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Undelivered => "undelivered",
            Self::Delivered => "delivered",
            Self::ViaRestack => "via-restack",
            Self::Failed => "failed",
        }
    }

    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "undelivered" => Self::Undelivered,
            "delivered" => Self::Delivered,
            "via-restack" => Self::ViaRestack,
            "failed" => Self::Failed,
            _ => return None,
        })
    }
}

/// One row of `waypoint_roster`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RosterEntryView {
    pub waypoint_id: String,
    pub kind: RosterEntryKind,
    pub entry_id: String,
    pub mode: RosterMode,
    pub survey_verdict: Option<String>,
    pub survey_rationale: Option<String>,
    pub delivery_status: DeliveryStatus,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

/// One row of `waypoint_bearings`. Append-only: rows are created via
/// [`Store::append_waypoint_bearing`] and never updated or deleted, so `id`
/// (an autoincrement rowid) doubles as the stable arrival order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BearingView {
    pub id: i64,
    pub waypoint_id: String,
    pub producer_kind: RosterEntryKind,
    pub producer_id: String,
    pub summary: String,
    pub entity_uri: Option<String>,
    pub commit_id: Option<String>,
    pub commit_summary: Option<String>,
    pub created_at_ms: i64,
}

/// One row of `pending_injections` (Phase 5/v2 mechanism; schema only in
/// Phase 1 -- nothing yet enqueues, drains, or delivers these).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PendingInjectionView {
    pub id: i64,
    pub target_squad: String,
    pub target_task: i64,
    pub target_idx: i64,
    pub payload: String,
    pub status: String,
    pub batch_id: Option<String>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

impl Store {
    /// Create a new open waypoint. Callers are responsible for generating
    /// `id` (mirrors every other entity id in this store -- see
    /// `crate::ids`).
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn create_waypoint(
        &self,
        id: &str,
        label: Option<&str>,
        prompt: &str,
        agent: Option<&str>,
        model: Option<&str>,
        allow_advisory: bool,
    ) -> StoreResult<()> {
        let now = now_ms();
        self.conn.execute(
            "INSERT INTO waypoints(id, label, prompt, agent, model, allow_advisory, state, created_at_ms, updated_at_ms)
             VALUES(?,?,?,?,?,?,'open',?,?)",
            params![id, label, prompt, agent, model, allow_advisory, now, now],
        )?;
        Ok(())
    }

    /// A squad's aggregate [`Scope`], per project. Groups every cell in the
    /// squad by its task's (already project/fallback-resolved) project
    /// name, then applies [`aggregate_scope`] within each project bucket --
    /// a cell with no recorded row in `cell_subprojects` is treated as
    /// `Unresolved` (RAL-346's binding decision: "nothing resolved yet" is
    /// never silently treated as an empty match).
    ///
    /// # Errors
    /// Propagates any SQLite failure, including the squad not existing.
    pub fn squad_scope_by_project(&self, squad_id: &str) -> StoreResult<BTreeMap<String, Scope>> {
        let squad = self.get_squad(squad_id)?;
        let resolved = Store::subprojects_by_cell(&self.conn, squad_id)?;
        let mut by_project: BTreeMap<String, Vec<SubprojectResolution>> = BTreeMap::new();
        for (task_idx, task) in squad.tasks.iter().enumerate() {
            let bucket = by_project.entry(task.project.clone()).or_default();
            for idx in 0..task.cells.len() {
                let key = (task_idx as i64, idx as i64);
                let resolution = match resolved.get(&key) {
                    Some((subprojects, inferred)) => SubprojectResolution::Resolved {
                        subprojects: subprojects.clone(),
                        inferred: *inferred,
                    },
                    None => SubprojectResolution::Unresolved,
                };
                bucket.push(resolution);
            }
        }
        Ok(by_project
            .into_iter()
            .map(|(project, cells)| (project, aggregate_scope(&cells)))
            .collect())
    }

    /// A review's aggregate [`Scope`], per project -- the union of every
    /// squad's cells feeding into one of this guardian's branches (a
    /// manually-assembled review can pull cells from more than one squad,
    /// so this doesn't assume a single originating squad). Mirrors
    /// `Store::collecting_guardians_for_cells`'s cell/guardian join in the
    /// opposite direction: prefers each cell's direct `review_guardian_id`,
    /// falling back to a `review_branch` string match against
    /// `guardian_branches` for a cell with no direct linkage.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn review_scope_by_project(
        &self,
        guardian_id: &str,
    ) -> StoreResult<BTreeMap<String, Scope>> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT s.squad_id, s.task_idx, s.idx FROM cells s
             WHERE s.review_guardian_id = ?
                OR (
                    s.review_guardian_id IS NULL
                    AND EXISTS (
                        SELECT 1 FROM guardian_branches gb
                        WHERE gb.guardian_id = ? AND gb.branch = s.review_branch
                    )
                )
             ORDER BY s.squad_id, s.task_idx, s.idx",
        )?;
        let rows = stmt
            .query_map(params![guardian_id, guardian_id], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        drop(stmt);

        let mut per_squad: BTreeMap<String, Vec<(i64, i64)>> = BTreeMap::new();
        for (squad_id, task_idx, idx) in rows {
            per_squad.entry(squad_id).or_default().push((task_idx, idx));
        }

        let mut by_project: BTreeMap<String, Vec<SubprojectResolution>> = BTreeMap::new();
        for (squad_id, cells) in per_squad {
            let squad = self.get_squad(&squad_id)?;
            let resolved = Store::subprojects_by_cell(&self.conn, &squad_id)?;
            for (task_idx, idx) in cells {
                let project = squad
                    .tasks
                    .get(usize::try_from(task_idx).unwrap_or(usize::MAX))
                    .map_or_else(|| "unassigned".to_string(), |t| t.project.clone());
                let resolution = match resolved.get(&(task_idx, idx)) {
                    Some((subprojects, inferred)) => SubprojectResolution::Resolved {
                        subprojects: subprojects.clone(),
                        inferred: *inferred,
                    },
                    None => SubprojectResolution::Unresolved,
                };
                by_project.entry(project).or_default().push(resolution);
            }
        }

        Ok(by_project
            .into_iter()
            .map(|(project, cells)| (project, aggregate_scope(&cells)))
            .collect())
    }

    /// A waypoint's aggregate [`Scope`], per project -- the union of every
    /// roster entry's own [`Store::squad_scope_by_project`] /
    /// [`Store::review_scope_by_project`], merged project-by-project via
    /// [`union_scope`]. Applies the same aggregation identically to review
    /// and squad roster entries, per Phase 0's design.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn waypoint_scope_by_project(
        &self,
        waypoint_id: &str,
    ) -> StoreResult<BTreeMap<String, Scope>> {
        let mut merged: BTreeMap<String, Scope> = BTreeMap::new();
        for entry in self.list_roster_entries(waypoint_id)? {
            let entry_scope = match entry.kind {
                RosterEntryKind::Squad => self.squad_scope_by_project(&entry.entry_id)?,
                RosterEntryKind::Review => self.review_scope_by_project(&entry.entry_id)?,
            };
            for (project, scope) in entry_scope {
                merged
                    .entry(project)
                    .and_modify(|existing| *existing = union_scope(existing, &scope))
                    .or_insert(scope);
            }
        }
        Ok(merged)
    }

    /// Whether a waypoint is `open` (`true`) or `closed`/nonexistent
    /// (`false`).
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn waypoint_is_open(&self, id: &str) -> StoreResult<bool> {
        let state: Option<String> = self
            .conn
            .query_row("SELECT state FROM waypoints WHERE id=?", params![id], |r| {
                r.get(0)
            })
            .optional()?;
        Ok(state.as_deref() == Some("open"))
    }

    /// Close a waypoint (idempotent -- closing an already-closed waypoint is
    /// a no-op). Returns `true` if this call actually transitioned it.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn close_waypoint(&self, id: &str) -> StoreResult<bool> {
        let now = now_ms();
        let n = self.conn.execute(
            "UPDATE waypoints SET state='closed', updated_at_ms=?, closed_at_ms=? WHERE id=? AND state != 'closed'",
            params![now, now, id],
        )?;
        Ok(n > 0)
    }

    /// Add (or update the mode of, if already present) one roster entry.
    /// Unique on `(waypoint_id, kind, entry_id)` -- re-adding the same entry
    /// updates its `mode` in place rather than duplicating the row.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn add_roster_entry(
        &self,
        waypoint_id: &str,
        kind: RosterEntryKind,
        entry_id: &str,
        mode: RosterMode,
    ) -> StoreResult<()> {
        let now = now_ms();
        self.conn.execute(
            "INSERT INTO waypoint_roster(waypoint_id, kind, entry_id, mode, delivery_status, created_at_ms, updated_at_ms)
             VALUES(?,?,?,?,'undelivered',?,?)
             ON CONFLICT(waypoint_id, kind, entry_id) DO UPDATE SET mode=excluded.mode, updated_at_ms=excluded.updated_at_ms",
            params![waypoint_id, kind.as_str(), entry_id, mode.as_str(), now, now],
        )?;
        Ok(())
    }

    /// Remove one roster entry. Returns `true` if a row was actually
    /// removed.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn remove_roster_entry(
        &self,
        waypoint_id: &str,
        kind: RosterEntryKind,
        entry_id: &str,
    ) -> StoreResult<bool> {
        let n = self.conn.execute(
            "DELETE FROM waypoint_roster WHERE waypoint_id=? AND kind=? AND entry_id=?",
            params![waypoint_id, kind.as_str(), entry_id],
        )?;
        Ok(n > 0)
    }

    /// Every roster entry for a waypoint, oldest first.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn list_roster_entries(&self, waypoint_id: &str) -> StoreResult<Vec<RosterEntryView>> {
        let mut stmt = self.conn.prepare(
            "SELECT waypoint_id, kind, entry_id, mode, survey_verdict, survey_rationale, delivery_status, created_at_ms, updated_at_ms
             FROM waypoint_roster WHERE waypoint_id=? ORDER BY created_at_ms, entry_id",
        )?;
        let rows = stmt
            .query_map(params![waypoint_id], Self::map_roster_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    fn map_roster_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<RosterEntryView> {
        let kind_s: String = r.get(1)?;
        let mode_s: String = r.get(3)?;
        let delivery_s: String = r.get(6)?;
        Ok(RosterEntryView {
            waypoint_id: r.get(0)?,
            kind: RosterEntryKind::parse(&kind_s).unwrap_or(RosterEntryKind::Squad),
            entry_id: r.get(2)?,
            mode: RosterMode::parse(&mode_s).unwrap_or(RosterMode::Block),
            survey_verdict: r.get(4)?,
            survey_rationale: r.get(5)?,
            delivery_status: DeliveryStatus::parse(&delivery_s)
                .unwrap_or(DeliveryStatus::Undelivered),
            created_at_ms: r.get(7)?,
            updated_at_ms: r.get(8)?,
        })
    }

    /// Record a roster entry's delivery status transition.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn set_roster_delivery_status(
        &self,
        waypoint_id: &str,
        kind: RosterEntryKind,
        entry_id: &str,
        status: DeliveryStatus,
    ) -> StoreResult<()> {
        self.conn.execute(
            "UPDATE waypoint_roster SET delivery_status=?, updated_at_ms=? WHERE waypoint_id=? AND kind=? AND entry_id=?",
            params![status.as_str(), now_ms(), waypoint_id, kind.as_str(), entry_id],
        )?;
        Ok(())
    }

    /// Whether every roster entry on a waypoint has reached a terminal state
    /// (squad terminal states, per [`SquadState::is_terminal`], count the
    /// same as review terminal states, per
    /// [`GuardianStatus::is_terminal_status`]). A waypoint with an empty
    /// roster is never considered terminal -- there is nothing to have
    /// finished yet.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn all_roster_entries_terminal(&self, waypoint_id: &str) -> StoreResult<bool> {
        let entries = self.list_roster_entries(waypoint_id)?;
        if entries.is_empty() {
            return Ok(false);
        }
        for entry in &entries {
            let terminal = match entry.kind {
                RosterEntryKind::Squad => match self.squad_state(&entry.entry_id) {
                    Ok(state) => state.is_terminal(),
                    Err(StoreError::NotFound) => false,
                    Err(e) => return Err(e),
                },
                RosterEntryKind::Review => {
                    let status: Option<String> = self
                        .conn
                        .query_row(
                            "SELECT status FROM guardians WHERE id=?",
                            params![entry.entry_id],
                            |r| r.get(0),
                        )
                        .optional()?;
                    match status {
                        Some(s) => GuardianStatus::is_terminal_status(&s),
                        None => false,
                    }
                }
            };
            if !terminal {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Close a waypoint if every roster entry has reached a terminal state.
    /// A no-op (returns `false`) if the waypoint is already closed, has no
    /// roster entries, or has at least one still-active entry.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn maybe_auto_close_waypoint(&self, waypoint_id: &str) -> StoreResult<bool> {
        if !self.waypoint_is_open(waypoint_id)? {
            return Ok(false);
        }
        if !self.all_roster_entries_terminal(waypoint_id)? {
            return Ok(false);
        }
        self.close_waypoint(waypoint_id)
    }

    /// Append one bearing to a waypoint's guidance history. Append-only --
    /// there is no corresponding update/delete method.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    #[allow(clippy::too_many_arguments)]
    pub fn append_waypoint_bearing(
        &self,
        waypoint_id: &str,
        producer_kind: RosterEntryKind,
        producer_id: &str,
        summary: &str,
        entity_uri: Option<&str>,
        commit_id: Option<&str>,
        commit_summary: Option<&str>,
    ) -> StoreResult<BearingView> {
        let now = now_ms();
        self.conn.execute(
            "INSERT INTO waypoint_bearings(waypoint_id, producer_kind, producer_id, summary, entity_uri, commit_id, commit_summary, created_at_ms)
             VALUES(?,?,?,?,?,?,?,?)",
            params![
                waypoint_id,
                producer_kind.as_str(),
                producer_id,
                summary,
                entity_uri,
                commit_id,
                commit_summary,
                now
            ],
        )?;
        let id = self.conn.last_insert_rowid();
        Ok(BearingView {
            id,
            waypoint_id: waypoint_id.to_string(),
            producer_kind,
            producer_id: producer_id.to_string(),
            summary: summary.to_string(),
            entity_uri: entity_uri.map(str::to_string),
            commit_id: commit_id.map(str::to_string),
            commit_summary: commit_summary.map(str::to_string),
            created_at_ms: now,
        })
    }

    /// Every bearing recorded for one waypoint, in arrival order. Scoped
    /// strictly to `waypoint_id` -- never returns another waypoint's rows.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn list_waypoint_bearings(&self, waypoint_id: &str) -> StoreResult<Vec<BearingView>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, waypoint_id, producer_kind, producer_id, summary, entity_uri, commit_id, commit_summary, created_at_ms
             FROM waypoint_bearings WHERE waypoint_id=? ORDER BY id ASC",
        )?;
        let rows = stmt
            .query_map(params![waypoint_id], |r| {
                let kind_s: String = r.get(2)?;
                Ok(BearingView {
                    id: r.get(0)?,
                    waypoint_id: r.get(1)?,
                    producer_kind: RosterEntryKind::parse(&kind_s)
                        .unwrap_or(RosterEntryKind::Squad),
                    producer_id: r.get(3)?,
                    summary: r.get(4)?,
                    entity_uri: r.get(5)?,
                    commit_id: r.get(6)?,
                    commit_summary: r.get(7)?,
                    created_at_ms: r.get(8)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Queue an injection payload for one cell, optionally as part of a
    /// batch. Phase 5/v2 mechanism -- nothing drives this yet in Phase 1-4.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn enqueue_injection(
        &self,
        target_squad: &str,
        target_task: i64,
        target_idx: i64,
        payload: &str,
        batch_id: Option<&str>,
    ) -> StoreResult<i64> {
        let now = now_ms();
        self.conn.execute(
            "INSERT INTO pending_injections(target_squad, target_task, target_idx, payload, status, batch_id, created_at_ms, updated_at_ms)
             VALUES(?,?,?,?,'queued',?,?,?)",
            params![target_squad, target_task, target_idx, payload, batch_id, now, now],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Cancel every still-queued injection in a batch. Cancel wins over a
    /// concurrent drain: both use the same `status='queued'` guard, so
    /// whichever transaction commits first determines each row's outcome,
    /// and a row can never be both delivered and cancelled. Returns the
    /// number of rows cancelled.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn cancel_injection_batch(&self, batch_id: &str) -> StoreResult<usize> {
        let n = self.conn.execute(
            "UPDATE pending_injections SET status='cancelled', updated_at_ms=? WHERE batch_id=? AND status='queued'",
            params![now_ms(), batch_id],
        )?;
        Ok(n)
    }

    /// Drain every still-queued injection targeting one cell: marks each
    /// `delivered` and returns them, oldest first. Exactly-once -- a row
    /// already `delivered` or `cancelled` is never returned again, so
    /// draining the same cell twice in a row returns an empty `Vec` the
    /// second time.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn drain_injections(
        &self,
        target_squad: &str,
        target_task: i64,
        target_idx: i64,
    ) -> StoreResult<Vec<PendingInjectionView>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, target_squad, target_task, target_idx, payload, status, batch_id, created_at_ms, updated_at_ms
             FROM pending_injections
             WHERE target_squad=? AND target_task=? AND target_idx=? AND status='queued'
             ORDER BY id ASC",
        )?;
        let rows = stmt
            .query_map(
                params![target_squad, target_task, target_idx],
                Self::map_injection_row,
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let now = now_ms();
        for row in &rows {
            self.conn.execute(
                "UPDATE pending_injections SET status='delivered', updated_at_ms=? WHERE id=? AND status='queued'",
                params![now, row.id],
            )?;
        }
        Ok(rows
            .into_iter()
            .map(|mut r| {
                r.status = "delivered".to_string();
                r
            })
            .collect())
    }

    fn map_injection_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<PendingInjectionView> {
        Ok(PendingInjectionView {
            id: r.get(0)?,
            target_squad: r.get(1)?,
            target_task: r.get(2)?,
            target_idx: r.get(3)?,
            payload: r.get(4)?,
            status: r.get(5)?,
            batch_id: r.get(6)?,
            created_at_ms: r.get(7)?,
            updated_at_ms: r.get(8)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::SquadState;

    fn open_waypoint(store: &Store, id: &str) {
        store
            .create_waypoint(id, Some("label"), "prompt text", None, None, false)
            .unwrap();
    }

    fn insert_bare_squad(store: &Store, id: &str, state: SquadState) {
        let now = now_ms();
        store
            .conn
            .execute(
                "INSERT INTO squads(id, state, created_at_ms, updated_at_ms) VALUES(?,?,?,?)",
                params![id, state.as_str(), now, now],
            )
            .unwrap();
    }

    fn insert_bare_task(store: &Store, squad_id: &str, idx: i64, project: &str) {
        store
            .conn
            .execute(
                "INSERT INTO tasks(squad_id, idx, name, project, state) VALUES(?,?,?,?,'pending')",
                params![squad_id, idx, format!("task-{idx}"), project],
            )
            .unwrap();
    }

    fn insert_bare_cell(
        store: &Store,
        squad_id: &str,
        task_idx: i64,
        idx: i64,
        review_guardian_id: Option<&str>,
        review_branch: Option<&str>,
    ) {
        store
            .conn
            .execute(
                "INSERT INTO cells(squad_id, task_idx, idx, sid, agent, state, review_guardian_id, review_branch)
                 VALUES(?,?,?,?,'claude-code','pending',?,?)",
                params![squad_id, task_idx, idx, format!("s{task_idx}-{idx}"), review_guardian_id, review_branch],
            )
            .unwrap();
    }

    fn insert_cell_subproject(
        store: &Store,
        squad_id: &str,
        task_idx: i64,
        idx: i64,
        subproject: &str,
        inferred: bool,
    ) {
        store
            .conn
            .execute(
                "INSERT INTO cell_subprojects(squad_id, task_idx, idx, subproject, inferred, created_at_ms) VALUES(?,?,?,?,?,?)",
                params![squad_id, task_idx, idx, subproject, inferred, now_ms()],
            )
            .unwrap();
    }

    fn insert_bare_guardian(store: &Store, id: &str) {
        let now = now_ms();
        store
            .conn
            .execute(
                "INSERT INTO guardians(id, name, base_branch, git_root, status, created_at_ms, updated_at_ms) VALUES(?,?,?,?,'collecting',?,?)",
                params![id, "review", "main", "/tmp/repo", now, now],
            )
            .unwrap();
    }

    #[test]
    fn roster_add_remove_review_and_squad_entries() {
        let store = Store::open_in_memory().unwrap();
        open_waypoint(&store, "waypoint-1");

        store
            .add_roster_entry(
                "waypoint-1",
                RosterEntryKind::Review,
                "guardian-1",
                RosterMode::Block,
            )
            .unwrap();
        store
            .add_roster_entry(
                "waypoint-1",
                RosterEntryKind::Squad,
                "squad-1",
                RosterMode::Advisory,
            )
            .unwrap();

        let entries = store.list_roster_entries("waypoint-1").unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].kind, RosterEntryKind::Review);
        assert_eq!(entries[0].mode, RosterMode::Block);
        assert_eq!(entries[1].kind, RosterEntryKind::Squad);
        assert_eq!(entries[1].mode, RosterMode::Advisory);

        assert!(
            store
                .remove_roster_entry("waypoint-1", RosterEntryKind::Review, "guardian-1")
                .unwrap()
        );
        assert!(
            !store
                .remove_roster_entry("waypoint-1", RosterEntryKind::Review, "guardian-1")
                .unwrap()
        );
        let entries = store.list_roster_entries("waypoint-1").unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].kind, RosterEntryKind::Squad);
    }

    #[test]
    fn re_adding_a_roster_entry_updates_mode_in_place() {
        let store = Store::open_in_memory().unwrap();
        open_waypoint(&store, "waypoint-1");
        store
            .add_roster_entry(
                "waypoint-1",
                RosterEntryKind::Squad,
                "squad-1",
                RosterMode::Block,
            )
            .unwrap();
        store
            .add_roster_entry(
                "waypoint-1",
                RosterEntryKind::Squad,
                "squad-1",
                RosterMode::Advisory,
            )
            .unwrap();
        let entries = store.list_roster_entries("waypoint-1").unwrap();
        assert_eq!(entries.len(), 1, "re-adding must not duplicate the row");
        assert_eq!(entries[0].mode, RosterMode::Advisory);
    }

    #[test]
    fn auto_close_requires_every_roster_entry_terminal_squad_and_review_alike() {
        let store = Store::open_in_memory().unwrap();
        open_waypoint(&store, "waypoint-1");

        let squad_id = "squad-1";
        insert_bare_squad(&store, squad_id, SquadState::Pending);
        store
            .add_roster_entry(
                "waypoint-1",
                RosterEntryKind::Squad,
                squad_id,
                RosterMode::Block,
            )
            .unwrap();
        assert!(!store.maybe_auto_close_waypoint("waypoint-1").unwrap());
        assert!(store.waypoint_is_open("waypoint-1").unwrap());

        store.set_squad_state(squad_id, SquadState::Done).unwrap();
        assert!(store.maybe_auto_close_waypoint("waypoint-1").unwrap());
        assert!(!store.waypoint_is_open("waypoint-1").unwrap());
    }

    #[test]
    fn auto_close_stays_open_while_any_entry_is_non_terminal() {
        let store = Store::open_in_memory().unwrap();
        open_waypoint(&store, "waypoint-1");

        let squad_a = "squad-a";
        let squad_b = "squad-b";
        insert_bare_squad(&store, squad_a, SquadState::Pending);
        insert_bare_squad(&store, squad_b, SquadState::Pending);
        store
            .add_roster_entry(
                "waypoint-1",
                RosterEntryKind::Squad,
                squad_a,
                RosterMode::Block,
            )
            .unwrap();
        store
            .add_roster_entry(
                "waypoint-1",
                RosterEntryKind::Squad,
                squad_b,
                RosterMode::Block,
            )
            .unwrap();
        store.set_squad_state(squad_a, SquadState::Done).unwrap();
        store.set_squad_state(squad_b, SquadState::Running).unwrap();

        assert!(!store.maybe_auto_close_waypoint("waypoint-1").unwrap());
        assert!(store.waypoint_is_open("waypoint-1").unwrap());
    }

    #[test]
    fn auto_close_never_fires_on_an_empty_roster() {
        let store = Store::open_in_memory().unwrap();
        open_waypoint(&store, "waypoint-1");
        assert!(!store.maybe_auto_close_waypoint("waypoint-1").unwrap());
        assert!(store.waypoint_is_open("waypoint-1").unwrap());
    }

    #[test]
    fn bearings_are_appended_in_order_and_never_mutated() {
        let store = Store::open_in_memory().unwrap();
        open_waypoint(&store, "waypoint-1");

        let b1 = store
            .append_waypoint_bearing(
                "waypoint-1",
                RosterEntryKind::Squad,
                "squad-1",
                "first summary",
                None,
                None,
                None,
            )
            .unwrap();
        let b2 = store
            .append_waypoint_bearing(
                "waypoint-1",
                RosterEntryKind::Review,
                "guardian-1",
                "second summary",
                Some("squad:squad-1"),
                Some("deadbeef"),
                Some("fix: thing"),
            )
            .unwrap();

        assert!(b2.id > b1.id);
        let bearings = store.list_waypoint_bearings("waypoint-1").unwrap();
        assert_eq!(bearings.len(), 2);
        assert_eq!(bearings[0].id, b1.id);
        assert_eq!(bearings[0].summary, "first summary");
        assert_eq!(bearings[0].entity_uri, None);
        assert_eq!(bearings[0].commit_id, None);
        assert_eq!(bearings[1].id, b2.id);
        assert_eq!(bearings[1].summary, "second summary");
        assert_eq!(bearings[1].entity_uri.as_deref(), Some("squad:squad-1"));
        assert_eq!(bearings[1].commit_id.as_deref(), Some("deadbeef"));
        assert_eq!(bearings[1].commit_summary.as_deref(), Some("fix: thing"));
    }

    #[test]
    fn bearings_are_scoped_per_waypoint_with_no_cross_leakage() {
        let store = Store::open_in_memory().unwrap();
        open_waypoint(&store, "waypoint-1");
        open_waypoint(&store, "waypoint-2");

        store
            .append_waypoint_bearing(
                "waypoint-1",
                RosterEntryKind::Squad,
                "squad-1",
                "for one",
                None,
                None,
                None,
            )
            .unwrap();
        store
            .append_waypoint_bearing(
                "waypoint-2",
                RosterEntryKind::Squad,
                "squad-2",
                "for two",
                None,
                None,
                None,
            )
            .unwrap();

        let one = store.list_waypoint_bearings("waypoint-1").unwrap();
        let two = store.list_waypoint_bearings("waypoint-2").unwrap();
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].summary, "for one");
        assert_eq!(two.len(), 1);
        assert_eq!(two[0].summary, "for two");
    }

    #[test]
    fn injection_drain_is_exactly_once() {
        let store = Store::open_in_memory().unwrap();
        store
            .enqueue_injection("squad-1", 0, 0, "payload a", None)
            .unwrap();
        store
            .enqueue_injection("squad-1", 0, 0, "payload b", None)
            .unwrap();

        let drained = store.drain_injections("squad-1", 0, 0).unwrap();
        assert_eq!(drained.len(), 2);
        assert!(drained.iter().all(|r| r.status == "delivered"));

        let drained_again = store.drain_injections("squad-1", 0, 0).unwrap();
        assert!(
            drained_again.is_empty(),
            "a second drain must return nothing"
        );
    }

    #[test]
    fn injection_cancel_wins_over_a_later_drain() {
        let store = Store::open_in_memory().unwrap();
        store
            .enqueue_injection("squad-1", 0, 0, "payload", Some("batch-1"))
            .unwrap();

        let cancelled = store.cancel_injection_batch("batch-1").unwrap();
        assert_eq!(cancelled, 1);

        let drained = store.drain_injections("squad-1", 0, 0).unwrap();
        assert!(
            drained.is_empty(),
            "a cancelled injection must never be drained"
        );

        let cancelled_again = store.cancel_injection_batch("batch-1").unwrap();
        assert_eq!(
            cancelled_again, 0,
            "cancelling an already-cancelled batch is a no-op"
        );
    }

    #[test]
    fn injection_batches_only_affect_their_own_batch() {
        let store = Store::open_in_memory().unwrap();
        store
            .enqueue_injection("squad-1", 0, 0, "payload a", Some("batch-1"))
            .unwrap();
        store
            .enqueue_injection("squad-1", 0, 0, "payload b", Some("batch-2"))
            .unwrap();

        store.cancel_injection_batch("batch-1").unwrap();
        let drained = store.drain_injections("squad-1", 0, 0).unwrap();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].payload, "payload b");
    }

    #[test]
    fn injection_drain_is_scoped_per_target_cell() {
        let store = Store::open_in_memory().unwrap();
        store
            .enqueue_injection("squad-1", 0, 0, "for cell a", None)
            .unwrap();
        store
            .enqueue_injection("squad-1", 0, 1, "for cell b", None)
            .unwrap();

        let drained_a = store.drain_injections("squad-1", 0, 0).unwrap();
        assert_eq!(drained_a.len(), 1);
        assert_eq!(drained_a[0].payload, "for cell a");

        let drained_b = store.drain_injections("squad-1", 0, 1).unwrap();
        assert_eq!(drained_b.len(), 1);
        assert_eq!(drained_b[0].payload, "for cell b");
    }

    #[test]
    fn aggregate_scope_unions_resolved_subprojects() {
        let cells = vec![
            SubprojectResolution::Resolved {
                subprojects: vec!["core".to_string()],
                inferred: false,
            },
            SubprojectResolution::Resolved {
                subprojects: vec!["daemon".to_string(), "core".to_string()],
                inferred: true,
            },
        ];
        assert_eq!(
            aggregate_scope(&cells),
            Scope::Areas(BTreeSet::from(["core".to_string(), "daemon".to_string()]))
        );
    }

    #[test]
    fn aggregate_scope_is_repo_wide_if_any_cell_is_unresolved() {
        let cells = vec![
            SubprojectResolution::Resolved {
                subprojects: vec!["core".to_string()],
                inferred: false,
            },
            SubprojectResolution::Unresolved,
        ];
        assert_eq!(aggregate_scope(&cells), Scope::RepoWide);
    }

    #[test]
    fn aggregate_scope_is_repo_wide_if_any_cell_is_not_applicable() {
        assert_eq!(
            aggregate_scope(&[SubprojectResolution::NotApplicable]),
            Scope::RepoWide
        );
    }

    #[test]
    fn aggregate_scope_of_empty_cells_is_empty_areas() {
        assert_eq!(aggregate_scope(&[]), Scope::Areas(BTreeSet::new()));
    }

    #[test]
    fn squad_scope_groups_by_project_and_unions_resolved_subprojects() {
        let store = Store::open_in_memory().unwrap();
        insert_bare_squad(&store, "squad-1", SquadState::Pending);
        insert_bare_task(&store, "squad-1", 0, "core");
        insert_bare_cell(&store, "squad-1", 0, 0, None, None);
        insert_cell_subproject(&store, "squad-1", 0, 0, "auth", false);
        insert_bare_task(&store, "squad-1", 1, "daemon");
        insert_bare_cell(&store, "squad-1", 1, 0, None, None);
        insert_cell_subproject(&store, "squad-1", 1, 0, "store", true);

        let scope = store.squad_scope_by_project("squad-1").unwrap();
        assert_eq!(scope.len(), 2);
        assert_eq!(
            scope["core"],
            Scope::Areas(BTreeSet::from(["auth".to_string()]))
        );
        assert_eq!(
            scope["daemon"],
            Scope::Areas(BTreeSet::from(["store".to_string()]))
        );
    }

    #[test]
    fn squad_scope_is_repo_wide_for_a_project_with_an_unresolved_cell() {
        let store = Store::open_in_memory().unwrap();
        insert_bare_squad(&store, "squad-1", SquadState::Pending);
        insert_bare_task(&store, "squad-1", 0, "core");
        insert_bare_cell(&store, "squad-1", 0, 0, None, None);

        let scope = store.squad_scope_by_project("squad-1").unwrap();
        assert_eq!(scope.get("core"), Some(&Scope::RepoWide));
    }

    #[test]
    fn review_scope_follows_direct_review_guardian_id_linkage() {
        let store = Store::open_in_memory().unwrap();
        insert_bare_guardian(&store, "guardian-1");
        insert_bare_squad(&store, "squad-1", SquadState::Pending);
        insert_bare_task(&store, "squad-1", 0, "core");
        insert_bare_cell(&store, "squad-1", 0, 0, Some("guardian-1"), None);
        insert_cell_subproject(&store, "squad-1", 0, 0, "auth", false);

        let scope = store.review_scope_by_project("guardian-1").unwrap();
        assert_eq!(
            scope.get("core"),
            Some(&Scope::Areas(BTreeSet::from(["auth".to_string()])))
        );
    }

    #[test]
    fn review_scope_falls_back_to_branch_match_when_no_direct_linkage() {
        let store = Store::open_in_memory().unwrap();
        insert_bare_guardian(&store, "guardian-1");
        store
            .conn
            .execute(
                "INSERT INTO guardian_branches(guardian_id, position, branch, merge_status) VALUES(?,?,?,?)",
                params!["guardian-1", 0, "feature-x", "pending"],
            )
            .unwrap();
        insert_bare_squad(&store, "squad-1", SquadState::Pending);
        insert_bare_task(&store, "squad-1", 0, "core");
        insert_bare_cell(&store, "squad-1", 0, 0, None, Some("feature-x"));
        insert_cell_subproject(&store, "squad-1", 0, 0, "auth", false);

        let scope = store.review_scope_by_project("guardian-1").unwrap();
        assert_eq!(
            scope.get("core"),
            Some(&Scope::Areas(BTreeSet::from(["auth".to_string()])))
        );
    }

    #[test]
    fn review_scope_pulls_cells_from_multiple_squads() {
        let store = Store::open_in_memory().unwrap();
        insert_bare_guardian(&store, "guardian-1");

        insert_bare_squad(&store, "squad-a", SquadState::Pending);
        insert_bare_task(&store, "squad-a", 0, "core");
        insert_bare_cell(&store, "squad-a", 0, 0, Some("guardian-1"), None);
        insert_cell_subproject(&store, "squad-a", 0, 0, "auth", false);

        insert_bare_squad(&store, "squad-b", SquadState::Pending);
        insert_bare_task(&store, "squad-b", 0, "core");
        insert_bare_cell(&store, "squad-b", 0, 0, Some("guardian-1"), None);
        insert_cell_subproject(&store, "squad-b", 0, 0, "billing", false);

        let scope = store.review_scope_by_project("guardian-1").unwrap();
        assert_eq!(
            scope.get("core"),
            Some(&Scope::Areas(BTreeSet::from([
                "auth".to_string(),
                "billing".to_string()
            ])))
        );
    }

    #[test]
    fn waypoint_scope_unions_squad_and_review_roster_entries_repo_wide_wins() {
        let store = Store::open_in_memory().unwrap();
        open_waypoint(&store, "waypoint-1");

        insert_bare_guardian(&store, "guardian-1");
        insert_bare_squad(&store, "squad-a", SquadState::Pending);
        insert_bare_task(&store, "squad-a", 0, "core");
        insert_bare_cell(&store, "squad-a", 0, 0, Some("guardian-1"), None);
        insert_cell_subproject(&store, "squad-a", 0, 0, "auth", false);

        insert_bare_squad(&store, "squad-b", SquadState::Pending);
        insert_bare_task(&store, "squad-b", 0, "core");
        insert_bare_cell(&store, "squad-b", 0, 0, None, None);

        store
            .add_roster_entry(
                "waypoint-1",
                RosterEntryKind::Review,
                "guardian-1",
                RosterMode::Block,
            )
            .unwrap();
        store
            .add_roster_entry(
                "waypoint-1",
                RosterEntryKind::Squad,
                "squad-b",
                RosterMode::Block,
            )
            .unwrap();

        let scope = store.waypoint_scope_by_project("waypoint-1").unwrap();
        assert_eq!(scope.get("core"), Some(&Scope::RepoWide));
    }

    #[test]
    fn waypoint_scope_unions_disjoint_areas_across_roster_entries() {
        let store = Store::open_in_memory().unwrap();
        open_waypoint(&store, "waypoint-1");

        insert_bare_squad(&store, "squad-a", SquadState::Pending);
        insert_bare_task(&store, "squad-a", 0, "core");
        insert_bare_cell(&store, "squad-a", 0, 0, None, None);
        insert_cell_subproject(&store, "squad-a", 0, 0, "auth", false);

        insert_bare_squad(&store, "squad-b", SquadState::Pending);
        insert_bare_task(&store, "squad-b", 0, "core");
        insert_bare_cell(&store, "squad-b", 0, 0, None, None);
        insert_cell_subproject(&store, "squad-b", 0, 0, "billing", false);

        store
            .add_roster_entry(
                "waypoint-1",
                RosterEntryKind::Squad,
                "squad-a",
                RosterMode::Block,
            )
            .unwrap();
        store
            .add_roster_entry(
                "waypoint-1",
                RosterEntryKind::Squad,
                "squad-b",
                RosterMode::Block,
            )
            .unwrap();

        let scope = store.waypoint_scope_by_project("waypoint-1").unwrap();
        assert_eq!(
            scope.get("core"),
            Some(&Scope::Areas(BTreeSet::from([
                "auth".to_string(),
                "billing".to_string()
            ])))
        );
    }
}
