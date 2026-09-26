//! Cross-squad waypoints (RAL-400) -- schema/store layer (Phase 1) plus the
//! survey pass (Phase 2), squad gating (Phase 3), and review-feedback
//! delivery (Phase 4).
//!
//! A waypoint is a named, open/closed join point that tracks a roster of
//! reviews/squads and accumulates append-only guidance ("bearings") for
//! them. This module owns the roster/bearing/injection CRUD, the
//! terminal-state auto-close computation, the survey (the LLM pass that
//! decides, for every open review/non-terminal squad whose [`Scope`]
//! overlaps a waypoint's, whether it is impacted and at what [`RosterMode`]),
//! and delivery: [`run_pending_deliveries`] pushes an impacted review-kind
//! roster entry's guidance into its review worktree via the existing
//! `guardian_merge::start_feedback` path, and marks a squad-kind entry
//! whose squad finished before any review ever formed for it `via-restack`.
//! A separate, not-yet-implemented `pending_injections` mechanism
//! ([`PendingInjectionView`]) is schema-only -- see
//! `.agent/waypoints-phase0-decisions.md` for the full design.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use rusqlite::{OptionalExtension, params};
use serde::Serialize;

use crate::chat_client::{self, ChatMessage};
use crate::guardian::GuardianStatus;
use crate::runner::Runner;
use crate::store::{Result as StoreResult, Store, StoreError, now_ms};
use crate::triage::SubprojectResolution;

/// A waypoint/review/squad's aggregate monorepo-subproject footprint (the
/// Phase 0 actionable-notification matching model, see
/// `.agent/waypoints-phase0-decisions.md`). Consumed by
/// [`Store::waypoint_survey_candidates`] to pick the narrowest candidate set
/// before any survey call is made.
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

/// Whether two scopes for the *same project* overlap: `RepoWide` on either
/// side always overlaps (it's the conservative "could be anything" case,
/// same rationale as [`aggregate_scope`]'s contagion rule); two `Areas` sets
/// overlap iff they share at least one subproject name.
#[must_use]
fn scopes_overlap(a: &Scope, b: &Scope) -> bool {
    match (a, b) {
        (Scope::RepoWide, _) | (_, Scope::RepoWide) => true,
        (Scope::Areas(x), Scope::Areas(y)) => !x.is_disjoint(y),
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

/// One row of `waypoints`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WaypointView {
    pub id: String,
    pub label: Option<String>,
    pub prompt: String,
    pub agent: Option<String>,
    pub model: Option<String>,
    pub allow_advisory: bool,
    pub state: String,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub closed_at_ms: Option<i64>,
}

/// A candidate identified by [`Store::waypoint_survey_candidates`]: an open
/// review or non-terminal squad not already an explicit roster entry, whose
/// own [`Scope`] overlaps the waypoint's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SurveyCandidate {
    pub kind: RosterEntryKind,
    pub entry_id: String,
}

/// The result of surveying one candidate: whether it was judged impacted,
/// under what mode, and why. Always populated -- including on a call
/// failure/timeout/unparseable reply, which fail closed to `impacted = true`,
/// `mode = Block` (see [`Store::survey_candidate`]'s doc comment).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SurveyVerdict {
    pub impacted: bool,
    pub mode: RosterMode,
    pub rationale: String,
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

    /// Fetch one waypoint by id.
    ///
    /// # Errors
    /// Returns [`StoreError::NotFound`] if no such waypoint exists, or
    /// propagates any other SQLite failure.
    pub fn get_waypoint(&self, id: &str) -> StoreResult<WaypointView> {
        self.conn
            .query_row(
                "SELECT id, label, prompt, agent, model, allow_advisory, state, created_at_ms, updated_at_ms, closed_at_ms
                 FROM waypoints WHERE id=?",
                params![id],
                |r| {
                    Ok(WaypointView {
                        id: r.get(0)?,
                        label: r.get(1)?,
                        prompt: r.get(2)?,
                        agent: r.get(3)?,
                        model: r.get(4)?,
                        allow_advisory: r.get(5)?,
                        state: r.get(6)?,
                        created_at_ms: r.get(7)?,
                        updated_at_ms: r.get(8)?,
                        closed_at_ms: r.get(9)?,
                    })
                },
            )
            .optional()?
            .ok_or(StoreError::NotFound)
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

    /// The exact candidate set a waypoint's survey should invoke the LLM on
    /// (RAL-400 Phase 2): every open review / non-terminal squad that is not
    /// already an explicit roster entry, whose own [`Scope`] overlaps the
    /// waypoint's aggregate scope for a shared project (see
    /// [`scopes_overlap`]). Computed entirely from already-stored scope data
    /// -- no model call happens here -- so this is the "narrowest set"
    /// selection the survey itself then classifies.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn waypoint_survey_candidates(
        &self,
        waypoint_id: &str,
    ) -> StoreResult<Vec<SurveyCandidate>> {
        let waypoint_scopes = self.waypoint_scope_by_project(waypoint_id)?;
        if waypoint_scopes.is_empty() {
            return Ok(Vec::new());
        }
        let already_on_roster: Vec<(RosterEntryKind, String)> = self
            .list_roster_entries(waypoint_id)?
            .into_iter()
            .map(|e| (e.kind, e.entry_id))
            .collect();
        let is_rostered = |kind: RosterEntryKind, id: &str| {
            already_on_roster.iter().any(|(k, e)| *k == kind && e == id)
        };
        let overlaps = |cand_scopes: &BTreeMap<String, Scope>| {
            cand_scopes.iter().any(|(project, scope)| {
                waypoint_scopes
                    .get(project)
                    .is_some_and(|ws| scopes_overlap(ws, scope))
            })
        };

        let mut candidates = Vec::new();

        let mut stmt = self.conn.prepare("SELECT id, state FROM squads")?;
        let squad_rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        drop(stmt);
        for (squad_id, state) in squad_rows {
            let is_terminal = crate::store::SquadState::parse(&state)
                .is_some_and(crate::store::SquadState::is_terminal);
            if is_terminal || is_rostered(RosterEntryKind::Squad, &squad_id) {
                continue;
            }
            if overlaps(&self.squad_scope_by_project(&squad_id)?) {
                candidates.push(SurveyCandidate {
                    kind: RosterEntryKind::Squad,
                    entry_id: squad_id,
                });
            }
        }

        for (guardian_id, status) in self.list_guardian_status_pairs()? {
            if GuardianStatus::is_terminal_status(&status)
                || is_rostered(RosterEntryKind::Review, &guardian_id)
            {
                continue;
            }
            if overlaps(&self.review_scope_by_project(&guardian_id)?) {
                candidates.push(SurveyCandidate {
                    kind: RosterEntryKind::Review,
                    entry_id: guardian_id,
                });
            }
        }

        Ok(candidates)
    }

    /// Every currently-`open` waypoint's id.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn list_open_waypoint_ids(&self) -> StoreResult<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id FROM waypoints WHERE state='open'")?;
        let rows = stmt
            .query_map([], |r| r.get(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
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

    /// Record one candidate's survey outcome (Phase 2): sets `mode` and the
    /// `survey_verdict`/`survey_rationale` columns. Does not itself add the
    /// roster entry -- callers add it (or update its `mode` in place, since
    /// [`Store::add_roster_entry`] is an upsert) before calling this.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn set_roster_survey_result(
        &self,
        waypoint_id: &str,
        kind: RosterEntryKind,
        entry_id: &str,
        verdict: &SurveyVerdict,
    ) -> StoreResult<()> {
        self.conn.execute(
            "UPDATE waypoint_roster SET mode=?, survey_verdict=?, survey_rationale=?, updated_at_ms=?
             WHERE waypoint_id=? AND kind=? AND entry_id=?",
            params![
                verdict.mode.as_str(),
                if verdict.impacted { "impacted" } else { "not_impacted" },
                verdict.rationale,
                now_ms(),
                waypoint_id,
                kind.as_str(),
                entry_id
            ],
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

    /// The open waypoint (if any) that block-gates a squad (RAL-400 Phase 3,
    /// scenario 1): a `kind='squad'` roster entry for `squad_id` whose `mode`
    /// is `block` and whose owning waypoint is still `open`. A roster entry
    /// with `survey_verdict='not_impacted'` never gates regardless of `mode`
    /// (the survey found this squad isn't actually affected, so the `mode`
    /// column's leftover default value is moot -- see
    /// [`resolve_survey_verdict`]/[`parse_survey_reply`]); a `NULL`
    /// `survey_verdict` (not yet surveyed, or a manually-added entry) gates,
    /// matching Phase 0's fail-closed rule and giving Phase 3's "gate the
    /// squad until classification completes" its effect for free, since
    /// [`survey_candidate`] already writes the `block`-mode roster row before
    /// its LLM call resolves. Advisory-mode entries never gate (Phase 0: for
    /// a squad, advisory means "keep running, just inform").
    ///
    /// Consulted by [`Store::list_ready`] and the queue view's `blocked_by`
    /// construction -- both existing call sites, not a new scheduling path.
    /// Returns the earliest-created blocking waypoint's id when more than one
    /// applies.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn squad_block_gating_waypoint(&self, squad_id: &str) -> StoreResult<Option<String>> {
        self.conn
            .query_row(
                "SELECT wr.waypoint_id FROM waypoint_roster wr
                 JOIN waypoints w ON w.id = wr.waypoint_id
                 WHERE wr.kind = 'squad' AND wr.entry_id = ? AND wr.mode = 'block'
                   AND (wr.survey_verdict IS NULL OR wr.survey_verdict = 'impacted')
                   AND w.state = 'open'
                 ORDER BY wr.created_at_ms ASC
                 LIMIT 1",
                params![squad_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(StoreError::from)
    }

    /// Every cell currently halted because its squad-kind roster entry
    /// became `mode=block` on an open waypoint (RAL-400 Phase 3, see
    /// [`Store::mark_cell_waypoint_halted`]) -- `(squad_id, task_idx, idx)`
    /// triples, oldest halt first. Consumed by [`run_pending_waypoint_resumes`]
    /// to find cells worth re-checking against
    /// [`Store::squad_block_gating_waypoint`] once a waypoint closes or
    /// de-escalates.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn waypoint_halted_cells(&self) -> StoreResult<Vec<(String, i64, i64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT squad_id, task_idx, idx FROM cells
             WHERE waypoint_halted_at_ms IS NOT NULL
             ORDER BY waypoint_halted_at_ms ASC",
        )?;
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// RAL-400 Phase 3: once a squad that carries a `kind='squad'` roster
    /// entry completes and its review forms (`guardian_id` becomes known),
    /// retire that entry and add an equivalent `kind='review'` entry in its
    /// place -- same waypoint, mode, and survey verdict/rationale carried
    /// over -- so gating/delivery (Phase 4) continues through the review
    /// instead of the now-stale squad entry. A no-op if `squad_id` has no
    /// squad-kind roster entry on any waypoint, and idempotent if called more
    /// than once for the same `(squad_id, guardian_id)` pair (the review-kind
    /// insert is the same `ON CONFLICT` upsert [`Store::add_roster_entry`]
    /// uses elsewhere, keyed on `(waypoint_id, kind, entry_id)`).
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn transition_squad_roster_entries_to_review(
        &self,
        squad_id: &str,
        guardian_id: &str,
    ) -> StoreResult<()> {
        let mut stmt = self.conn.prepare(
            "SELECT waypoint_id, mode, survey_verdict, survey_rationale
             FROM waypoint_roster WHERE kind='squad' AND entry_id=?",
        )?;
        let rows = stmt
            .query_map(params![squad_id], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, Option<String>>(3)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        drop(stmt);
        let now = now_ms();
        for (waypoint_id, mode, survey_verdict, survey_rationale) in rows {
            self.conn.execute(
                "INSERT INTO waypoint_roster(waypoint_id, kind, entry_id, mode, survey_verdict, survey_rationale, delivery_status, created_at_ms, updated_at_ms)
                 VALUES(?,'review',?,?,?,?,'undelivered',?,?)
                 ON CONFLICT(waypoint_id, kind, entry_id) DO UPDATE SET
                     mode=excluded.mode,
                     survey_verdict=excluded.survey_verdict,
                     survey_rationale=excluded.survey_rationale,
                     updated_at_ms=excluded.updated_at_ms",
                params![waypoint_id, guardian_id, mode, survey_verdict, survey_rationale, now, now],
            )?;
            self.conn.execute(
                "DELETE FROM waypoint_roster WHERE waypoint_id=? AND kind='squad' AND entry_id=?",
                params![waypoint_id, squad_id],
            )?;
        }
        Ok(())
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

/// Build the survey's system prompt from the waypoint's own guidance prompt.
/// Mirrors `arbiter::subproject_inference_system_prompt`'s phrasing style:
/// a short role framing, the guidance verbatim, then a strict reply-format
/// spec so [`parse_survey_reply`] has a stable shape to key off. The
/// `allow_advisory` line is only present when the waypoint allows
/// de-escalation, per RAL-400 Phase 2 ("if yes and `allow_advisory` is set,
/// a second question deciding advisory de-escalation vs. keep blocking").
fn survey_system_prompt(waypoint_prompt: &str, allow_advisory: bool) -> String {
    let mode_line = if allow_advisory {
        "MODE: block or advisory -- advisory means this work only needs to be \
         informed of the guidance, not gated on it; block means it should wait \
         for/act on the guidance before proceeding. Only meaningful when \
         IMPACTED is yes; reply NONE when IMPACTED is no.\n"
    } else {
        ""
    };
    format!(
        "You are surveying one unit of work (a code review or an agent squad) \
         against the following cross-squad coordination guidance, to decide \
         whether that unit of work is actually impacted by it:\n\n\
         {waypoint_prompt}\n\n\
         Reply with exactly these lines, in this order, and nothing else -- no \
         extra commentary, no surrounding quotes:\n\
         IMPACTED: yes or no\n\
         {mode_line}\
         RATIONALE: one short sentence explaining the decision"
    )
}

/// Parse one survey reply into a [`SurveyVerdict`], at the same robustness
/// bar as `arbiter::parse_classification_reply`: tolerant of extra
/// whitespace/blank lines, case-insensitive keywords and values, and
/// surrounding quote/period punctuation on values; a line that isn't a
/// recognized `KEY: value` pair is ignored rather than rejecting the whole
/// reply. Returns `None` when no recognizable `IMPACTED:` line is present at
/// all -- callers treat that identically to a call failure (fail closed).
fn parse_survey_reply(reply: &str, allow_advisory: bool) -> Option<SurveyVerdict> {
    let mut impacted: Option<bool> = None;
    let mut advisory = false;
    let mut rationale = String::new();
    for line in reply.lines() {
        let Some((key, val)) = line.split_once(':') else {
            continue;
        };
        let val = val
            .trim()
            .trim_matches(|c: char| c == '"' || c == '\'' || c == '.');
        match key.trim().to_ascii_uppercase().as_str() {
            "IMPACTED" => {
                if val.eq_ignore_ascii_case("yes") {
                    impacted = Some(true);
                } else if val.eq_ignore_ascii_case("no") {
                    impacted = Some(false);
                }
            }
            "MODE" if allow_advisory => advisory = val.eq_ignore_ascii_case("advisory"),
            "RATIONALE" => rationale = val.to_string(),
            _ => {}
        }
    }
    let impacted = impacted?;
    let mode = if impacted && allow_advisory && advisory {
        RosterMode::Advisory
    } else {
        RosterMode::Block
    };
    if rationale.is_empty() {
        rationale = "model gave no rationale".to_string();
    }
    Some(SurveyVerdict {
        impacted,
        mode,
        rationale,
    })
}

/// Turn a completed survey call's outcome into a [`SurveyVerdict`], applying
/// the fail-closed rule uniformly whether the call itself failed or it
/// succeeded but returned an unparseable reply. Split out from
/// [`survey_candidate`] so the fail-closed and advisory-de-escalation
/// decision logic is unit-testable without an actual `chat_client` call.
fn resolve_survey_verdict(
    call_result: Result<String, String>,
    allow_advisory: bool,
) -> SurveyVerdict {
    match call_result {
        Ok(reply) => parse_survey_reply(&reply, allow_advisory).unwrap_or_else(|| SurveyVerdict {
            impacted: true,
            mode: RosterMode::Block,
            rationale: format!("unparseable survey reply, failing closed: {reply:?}"),
        }),
        Err(e) => SurveyVerdict {
            impacted: true,
            mode: RosterMode::Block,
            rationale: format!("survey call failed, failing closed: {e}"),
        },
    }
}

/// Survey one candidate against a waypoint (RAL-400 Phase 2): resolve the
/// survey agent/model, invoke the LLM once, and durably record the outcome
/// -- via [`Store::add_roster_entry`] + [`Store::set_roster_survey_result`]
/// and a Cartographer row -- on every path, including failure. The roster
/// entry is written regardless of the `impacted` verdict (not only when
/// `true`): the row is the single place both the positive and negative
/// outcome are recorded, it stops a later scheduler tick from re-surveying
/// the same still-non-terminal candidate every interval, and it is what a
/// later phase's delivery/gating logic must consult (`survey_verdict`) to
/// know whether this roster entry actually blocks/advises.
///
/// One LLM call per candidate, not batched across a waypoint's whole
/// candidate set: a batched reply covering N candidates at once would be
/// cheaper, but it couples every candidate's outcome to one reply -- a
/// single malformed/truncated batch reply (more likely as candidate count
/// grows) would force *all* of them to fail closed together, and a
/// slow/erroring call would stall every candidate in the batch rather than
/// just the one it concerns. Per-candidate calls trade some cost for that
/// failure isolation and for a Cartographer row that is attributable to
/// exactly the call that produced it; RAL-400 Phase 2 leaves this tradeoff
/// to this module and defaults to per-candidate absent a concrete cost
/// problem.
///
/// Fail-closed: any agent-resolution/call/parse failure resolves to
/// `impacted = true`, `mode = Block`, with the failure reason as the
/// rationale -- never silently dropped.
///
/// # Errors
/// Propagates a SQLite failure from reading the waypoint or persisting the
/// outcome. A survey-call failure itself is not a [`StoreError`] -- it
/// resolves to the fail-closed verdict described above instead.
pub fn survey_candidate(
    store: &crate::store_lock::StoreHandle,
    waypoint_id: &str,
    candidate: &SurveyCandidate,
    waypoint_halts: &crate::cancel::WaypointHalts,
) -> StoreResult<SurveyVerdict> {
    let guard = store.lock();
    let waypoint = guard.get_waypoint(waypoint_id)?;
    guard.add_roster_entry(
        waypoint_id,
        candidate.kind,
        &candidate.entry_id,
        RosterMode::Block,
    )?;
    drop(guard);
    // RAL-400 Phase 3: the roster entry above just went from "not rostered"
    // (unblocked) to `mode=block`. A squad-kind candidate may have a cell
    // actively running right now -- `cancel` is a no-op when nothing is
    // registered under this squad id, so this is safe to call unconditionally
    // rather than first checking squad/cell state. Review-kind candidates
    // never have a runner-registered token (a review has no cell of its own
    // to halt), so this is scoped to `Squad` only.
    if candidate.kind == RosterEntryKind::Squad {
        waypoint_halts.cancel(&candidate.entry_id);
    }

    let fallback = crate::config::global_review_config();
    let agent = waypoint
        .agent
        .clone()
        .unwrap_or_else(|| fallback.default_resolver_agent().to_string());
    let model = waypoint
        .model
        .clone()
        .or_else(|| fallback.default_resolver_model().map(str::to_string));

    let system = survey_system_prompt(&waypoint.prompt, waypoint.allow_advisory);
    let messages = [ChatMessage {
        role: "user",
        content: format!(
            "Unit of work under survey: {} {}",
            candidate.kind.as_str(),
            candidate.entry_id
        ),
        image: None,
    }];

    let call_result = chat_client::call_direct(&agent, model.as_deref(), &system, &messages);
    let verdict = resolve_survey_verdict(call_result, waypoint.allow_advisory);

    let guard = store.lock();
    guard.set_roster_survey_result(waypoint_id, candidate.kind, &candidate.entry_id, &verdict)?;
    let note = crate::cartographer::Note::new("waypoints").scope("waypoint");
    let note = match candidate.kind {
        RosterEntryKind::Squad => note.squad(&candidate.entry_id),
        RosterEntryKind::Review => note.guardian(&candidate.entry_id),
    };
    note.emit(
        &guard,
        format!(
            "waypoint {waypoint_id} survey of {} {}: impacted={} mode={}",
            candidate.kind.as_str(),
            candidate.entry_id,
            verdict.impacted,
            verdict.mode.as_str()
        ),
        serde_json::json!({
            "waypoint_id": waypoint_id,
            "candidate_kind": candidate.kind.as_str(),
            "candidate_id": candidate.entry_id,
            "impacted": verdict.impacted,
            "mode": verdict.mode.as_str(),
            "rationale": verdict.rationale,
        }),
    );
    Ok(verdict)
}

/// Scheduler-owned periodic sweep (RAL-400 Phase 2): survey every waiting
/// candidate on every open waypoint. Must only ever be invoked from
/// `scheduler::run_loop`'s periodic tick -- never synchronously inside the
/// waypoint-submit HTTP handler, since a single survey call can take seconds
/// and a waypoint may have many candidates. The candidate lookups are cheap
/// synchronous store reads done on the calling (scheduler) thread, but each
/// actual survey call is dispatched onto its own spawned thread -- mirroring
/// `crate::pr::poll_forge_reorders` -- so the scheduler loop is never
/// blocked for the cumulative duration of every open waypoint's every
/// candidate's LLM call.
pub fn run_pending_surveys(
    store: &crate::store_lock::StoreHandle,
    waypoint_halts: &crate::cancel::WaypointHalts,
) {
    let waypoint_ids = {
        let guard = store.lock();
        guard.list_open_waypoint_ids().unwrap_or_default()
    };
    for waypoint_id in waypoint_ids {
        let candidates = {
            let guard = store.lock();
            guard
                .waypoint_survey_candidates(&waypoint_id)
                .unwrap_or_default()
        };
        for candidate in candidates {
            let store = std::sync::Arc::clone(store);
            let waypoint_id = waypoint_id.clone();
            let waypoint_halts = waypoint_halts.clone();
            std::thread::spawn(move || {
                let _ = survey_candidate(&store, &waypoint_id, &candidate, &waypoint_halts);
            });
        }
    }
}

/// Scheduler-owned periodic sweep (RAL-400 Phase 3): the other half of a
/// waypoint halt. `run_cell_worker`'s `is_waypoint_halted()` branch stops a
/// cell the moment its squad-kind roster entry becomes `mode=block`, but
/// nothing else in that codepath ever hands the cell back -- a waypoint can
/// close, de-escalate to advisory, or lose its last blocking roster entry at
/// any later time, with no single call site to hook a "resume now" trigger
/// onto (unlike the halt itself, which is driven directly by
/// [`survey_candidate`] flipping a roster entry to `block`). So this sweep
/// re-checks every currently-halted cell on the same cadence as
/// [`run_pending_surveys`], purely synchronous store reads/writes (no LLM
/// call, no thread-spawn needed).
///
/// A cell only resumes once [`Store::squad_block_gating_waypoint`] no longer
/// names a blocking waypoint for its squad, and only if the DB still shows it
/// `Running` -- guarding against a cell that moved on through some other path
/// (a manual restart, say) while still carrying a stale
/// `waypoint_halted_at_ms`. Resuming sets the cell back to `pending`; a live
/// squad worker notices via the same Detached-revival reconciliation
/// `resume_detached_cell` relies on (`scheduler::execute_squad_inner`'s
/// `reclaimed_detached` check), so only a squad whose worker has already
/// exited (`!cancellations.is_active`) needs its own row nudged back to
/// `Pending` to be picked up by a fresh `scheduler::tick`.
pub fn run_pending_waypoint_resumes(
    store: &crate::store_lock::StoreHandle,
    cancellations: &crate::cancel::Cancellations,
) {
    let halted = {
        let guard = store.lock();
        guard.waypoint_halted_cells().unwrap_or_default()
    };
    let mut resumed_squads: BTreeSet<String> = BTreeSet::new();
    for (squad_id, task_idx, idx) in halted {
        let guard = store.lock();
        if guard
            .squad_block_gating_waypoint(&squad_id)
            .unwrap_or(None)
            .is_some()
        {
            continue;
        }
        if !matches!(
            guard.cell_state(&squad_id, task_idx, idx),
            Ok(Some(crate::store::NodeState::Running))
        ) {
            continue;
        }
        // Mirror `server::resume_automation`'s pairing: a session id is only
        // worth forcing a resume onto if one was actually captured live
        // before the halt (a cell halted before the agent ever reported a
        // session id has nothing of its own to continue) -- `run_cell_worker`
        // falls back to ordinary fresh-dispatch/cross-cell-sharing logic
        // otherwise, same as any other cell with no session to resume.
        if matches!(
            guard.get_cell_agent_resume(&squad_id, task_idx, idx),
            Ok((_, _, Some(_)))
        ) {
            let _ = guard.set_force_resume_own_session(&squad_id, task_idx, idx);
        }
        if guard
            .resume_waypoint_halted_cell(&squad_id, task_idx, idx)
            .is_ok()
        {
            resumed_squads.insert(squad_id);
        }
    }
    for squad_id in resumed_squads {
        if !cancellations.is_active(&squad_id) {
            let guard = store.lock();
            let _ = guard.set_squad_state(&squad_id, crate::store::SquadState::Pending);
        }
    }
}

/// Author attributed to a waypoint's delivered feedback messages, mirroring
/// `ci_watch.rs`'s `AUTO_FIX_AUTHOR`/`MANUAL_PR_FIX_AUTHOR` naming precedent
/// for automation-originated guardian messages.
pub const WAYPOINT_FEEDBACK_AUTHOR: &str = "Waypoint";

/// The topmost (highest-position) enabled branch with a worktree already
/// built, if any. This mirrors the readiness bar `guardian_merge::start_feedback`
/// itself enforces (its synchronous 409 "run the review merge before giving
/// feedback" path) -- checked here first so a not-yet-built review is simply
/// skipped for this sweep (left `Undelivered` for a later one to retry)
/// rather than surfaced as a `Failed` roster entry.
fn topmost_ready_branch(
    guardian: &crate::guardian::GuardianView,
) -> Option<&crate::guardian::BranchView> {
    guardian
        .branches
        .iter()
        .filter(|b| b.enabled && b.worktree.is_some())
        .max_by_key(|b| b.position)
}

/// Render a waypoint's guidance as one feedback message body.
fn waypoint_feedback_text(waypoint: &WaypointView) -> String {
    match &waypoint.label {
        Some(label) => format!("Waypoint \"{label}\": {}", waypoint.prompt),
        None => waypoint.prompt.clone(),
    }
}

/// Scheduler-owned periodic sweep (RAL-400 Phase 4): deliver every open
/// waypoint's guidance to every roster entry judged impacted. A `NULL` or
/// `"impacted"` `survey_verdict` both count as impacted here (mirroring
/// [`Store::squad_block_gating_waypoint`]'s own fail-closed reading of that
/// column); only an explicit `"not_impacted"` skips delivery.
///
/// - `Review`-kind entries are delivered through the *existing* review
///   feedback path -- `guardian_merge::start_feedback`, the same function
///   `POST /api/guardians/{id}/branches/{branch_id}/feedback` calls -- rather
///   than a new delivery mechanism. That function already does its own
///   `feedback:<branch_id>` worktree-lease queueing behind an in-flight
///   rebase; this sweep only decides *when* to call it, never reimplements
///   that serialization.
/// - `Squad`-kind entries never get a direct delivery call: a squad has no
///   review worktree to write feedback into. If such a squad has already
///   gone terminal (done/failed/cancelled) -- the "done-but-unreviewed" case,
///   where the squad finished before a review ever formed for it and before
///   this ticket's gating could apply -- its roster entry is marked
///   `via-restack` so the UI can say honestly that its guidance will only
///   reach it later, folded into the restack/rebase that runs once its
///   eventual review is built (at which point
///   `Store::transition_squad_roster_entries_to_review` converts the entry
///   to `Review`-kind and this sweep starts delivering to it directly). A
///   still-running squad's entry is left untouched -- there is nothing to do
///   for it yet.
///
/// Mirrors [`run_pending_surveys`]'s shape: cheap synchronous store reads on
/// the calling (scheduler) thread, gating what work happens; the potentially
/// slow part (`start_feedback`'s spawned background thread) is not owned by
/// this function's call stack at all, so no thread-spawn is needed here.
pub fn run_pending_deliveries(store: &crate::store_lock::StoreHandle, runner: &Arc<dyn Runner>) {
    let waypoint_ids = {
        let guard = store.lock();
        guard.list_open_waypoint_ids().unwrap_or_default()
    };
    for waypoint_id in waypoint_ids {
        let (waypoint, entries) = {
            let guard = store.lock();
            let Ok(waypoint) = guard.get_waypoint(&waypoint_id) else {
                continue;
            };
            let entries = guard.list_roster_entries(&waypoint_id).unwrap_or_default();
            (waypoint, entries)
        };
        for entry in entries {
            if entry.delivery_status != DeliveryStatus::Undelivered {
                continue;
            }
            if entry.survey_verdict.as_deref() == Some("not_impacted") {
                continue;
            }
            match entry.kind {
                RosterEntryKind::Review => {
                    deliver_to_review(store, runner, &waypoint_id, &waypoint, &entry.entry_id);
                }
                RosterEntryKind::Squad => {
                    mark_done_but_unreviewed_squad(store, &waypoint_id, &entry.entry_id);
                }
            }
        }
    }
}

/// Deliver one waypoint's guidance to one review's topmost ready branch via
/// the existing feedback path, recording the outcome on the roster entry.
/// Leaves the entry `Undelivered` (for a later sweep to retry) if the
/// guardian has no ready branch yet, or if `start_feedback` itself reports
/// `404` (stale roster entry, guardian/branch since gone) or `409` (branch
/// has no worktree yet -- an ordinary not-built-yet race, not a failure); any
/// other reply status is recorded as `Failed`.
fn deliver_to_review(
    store: &crate::store_lock::StoreHandle,
    runner: &Arc<dyn Runner>,
    waypoint_id: &str,
    waypoint: &WaypointView,
    guardian_id: &str,
) {
    let branch_id = {
        let guard = store.lock();
        let Ok(guardian) = guard.get_guardian(guardian_id) else {
            return;
        };
        match topmost_ready_branch(&guardian) {
            Some(branch) => branch.id.clone(),
            None => return,
        }
    };
    let reply = crate::guardian_merge::start_feedback(
        Arc::clone(store),
        Arc::clone(runner),
        guardian_id,
        &branch_id,
        waypoint_feedback_text(waypoint),
        Some(WAYPOINT_FEEDBACK_AUTHOR.to_string()),
        None,
    );
    let status = match reply.status {
        202 => DeliveryStatus::Delivered,
        404 | 409 => return,
        _ => DeliveryStatus::Failed,
    };
    let guard = store.lock();
    let _ =
        guard.set_roster_delivery_status(waypoint_id, RosterEntryKind::Review, guardian_id, status);
    let note = crate::cartographer::Note::new("waypoints")
        .scope("waypoint")
        .guardian(guardian_id);
    note.emit(
        &guard,
        format!(
            "waypoint {waypoint_id} delivered feedback to review {guardian_id}: status={}",
            status.as_str()
        ),
        serde_json::json!({
            "waypoint_id": waypoint_id,
            "guardian_id": guardian_id,
            "branch_id": branch_id,
            "delivery_status": status.as_str(),
        }),
    );
}

/// Mark a squad-kind roster entry `via-restack` once its squad has gone
/// terminal without a review ever having formed for it (the "done-but-
/// unreviewed" case). A no-op if the squad is not yet terminal, or is gone
/// entirely (treated the same as "not yet terminal" -- nothing to mark).
fn mark_done_but_unreviewed_squad(
    store: &crate::store_lock::StoreHandle,
    waypoint_id: &str,
    squad_id: &str,
) {
    let guard = store.lock();
    let terminal = matches!(guard.squad_state(squad_id), Ok(state) if state.is_terminal());
    if !terminal {
        return;
    }
    let _ = guard.set_roster_delivery_status(
        waypoint_id,
        RosterEntryKind::Squad,
        squad_id,
        DeliveryStatus::ViaRestack,
    );
    let note = crate::cartographer::Note::new("waypoints")
        .scope("waypoint")
        .squad(squad_id);
    note.emit(
        &guard,
        format!(
            "waypoint {waypoint_id} squad {squad_id} finished before its review formed -- via-restack"
        ),
        serde_json::json!({
            "waypoint_id": waypoint_id,
            "squad_id": squad_id,
            "delivery_status": DeliveryStatus::ViaRestack.as_str(),
        }),
    );
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::cancel::Cancellations;
    use crate::store::{NodeState, SquadState};
    use crate::store_lock::StoreMutex;

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
                Some("1234567890123456"),
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
        assert_eq!(bearings[1].commit_id.as_deref(), Some("1234567890123456"));
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

    fn insert_guardian_with_status(store: &Store, id: &str, status: &str) {
        let now = now_ms();
        store
            .conn
            .execute(
                "INSERT INTO guardians(id, name, base_branch, git_root, status, created_at_ms, updated_at_ms) VALUES(?,?,?,?,?,?,?)",
                params![id, "review", "main", "/tmp/repo", status, now, now],
            )
            .unwrap();
    }

    // ── parse_survey_reply ────────────────────────────────────────────────

    #[test]
    fn parse_survey_reply_matches_case_insensitively_and_trims_punctuation() {
        let verdict = parse_survey_reply(
            "  impacted:  YES.  \nrationale: \"touches the auth flow.\"  ",
            false,
        )
        .unwrap();
        assert!(verdict.impacted);
        assert_eq!(verdict.mode, RosterMode::Block);
        assert_eq!(verdict.rationale, "touches the auth flow");
    }

    #[test]
    fn parse_survey_reply_ignores_unrecognized_and_blank_lines() {
        let verdict = parse_survey_reply(
            "some preamble the model added\n\nIMPACTED: no\n\nRATIONALE: unrelated area\ntrailing junk",
            false,
        )
        .unwrap();
        assert!(!verdict.impacted);
        assert_eq!(verdict.rationale, "unrelated area");
    }

    #[test]
    fn parse_survey_reply_returns_none_when_impacted_line_missing() {
        assert!(parse_survey_reply("RATIONALE: no clear verdict given", false).is_none());
        assert!(parse_survey_reply("", false).is_none());
        assert!(parse_survey_reply("IMPACTED: maybe", false).is_none());
    }

    #[test]
    fn parse_survey_reply_defaults_rationale_when_model_omits_it() {
        let verdict = parse_survey_reply("IMPACTED: yes", false).unwrap();
        assert_eq!(verdict.rationale, "model gave no rationale");
    }

    #[test]
    fn parse_survey_reply_advisory_mode_only_applies_when_allowed_and_impacted() {
        // allow_advisory=true, impacted=yes, MODE: advisory -> Advisory.
        let verdict =
            parse_survey_reply("IMPACTED: yes\nMODE: advisory\nRATIONALE: fyi only", true).unwrap();
        assert_eq!(verdict.mode, RosterMode::Advisory);

        // allow_advisory=true, impacted=yes, MODE: block -> Block.
        let verdict = parse_survey_reply("IMPACTED: yes\nMODE: block\nRATIONALE: r", true).unwrap();
        assert_eq!(verdict.mode, RosterMode::Block);

        // allow_advisory=true but impacted=no -> always Block regardless of MODE.
        let verdict =
            parse_survey_reply("IMPACTED: no\nMODE: advisory\nRATIONALE: r", true).unwrap();
        assert_eq!(verdict.mode, RosterMode::Block);

        // allow_advisory=false -> a MODE line is ignored entirely, always Block.
        let verdict =
            parse_survey_reply("IMPACTED: yes\nMODE: advisory\nRATIONALE: r", false).unwrap();
        assert_eq!(verdict.mode, RosterMode::Block);
    }

    // ── resolve_survey_verdict (fail-closed + advisory de-escalation) ──────

    #[test]
    fn resolve_survey_verdict_fails_closed_on_call_error() {
        let verdict = resolve_survey_verdict(Err("connection refused".to_string()), false);
        assert!(verdict.impacted);
        assert_eq!(verdict.mode, RosterMode::Block);
        assert!(verdict.rationale.contains("connection refused"));
    }

    #[test]
    fn resolve_survey_verdict_fails_closed_on_unparseable_reply() {
        let verdict =
            resolve_survey_verdict(Ok("the model rambled without a verdict".to_string()), false);
        assert!(verdict.impacted);
        assert_eq!(verdict.mode, RosterMode::Block);
        assert!(verdict.rationale.contains("unparseable"));
    }

    #[test]
    fn resolve_survey_verdict_applies_advisory_deescalation_when_allowed() {
        let verdict = resolve_survey_verdict(
            Ok(
                "IMPACTED: yes\nMODE: advisory\nRATIONALE: just keep this squad informed"
                    .to_string(),
            ),
            true,
        );
        assert!(verdict.impacted);
        assert_eq!(verdict.mode, RosterMode::Advisory);
    }

    #[test]
    fn resolve_survey_verdict_keeps_block_when_advisory_not_allowed() {
        let verdict = resolve_survey_verdict(
            Ok("IMPACTED: yes\nMODE: advisory\nRATIONALE: r".to_string()),
            false,
        );
        assert!(verdict.impacted);
        assert_eq!(verdict.mode, RosterMode::Block);
    }

    #[test]
    fn resolve_survey_verdict_passes_through_a_clean_not_impacted_reply() {
        let verdict = resolve_survey_verdict(
            Ok("IMPACTED: no\nRATIONALE: different area entirely".to_string()),
            true,
        );
        assert!(!verdict.impacted);
        assert_eq!(verdict.mode, RosterMode::Block);
    }

    // ── waypoint_survey_candidates (matching-model population) ─────────────

    #[test]
    fn survey_candidates_include_overlapping_area_and_exclude_sibling_area() {
        let store = Store::open_in_memory().unwrap();
        open_waypoint(&store, "waypoint-1");

        // Seed the waypoint's scope with one roster entry in project "core",
        // area "auth" -- standing in for whatever adds the initial roster
        // entry a waypoint is declared against.
        insert_bare_squad(&store, "squad-seed", SquadState::Pending);
        insert_bare_task(&store, "squad-seed", 0, "core");
        insert_bare_cell(&store, "squad-seed", 0, 0, None, None);
        insert_cell_subproject(&store, "squad-seed", 0, 0, "auth", false);
        store
            .add_roster_entry(
                "waypoint-1",
                RosterEntryKind::Squad,
                "squad-seed",
                RosterMode::Block,
            )
            .unwrap();

        // Same project, same area -> must be surveyed.
        insert_bare_squad(&store, "squad-match", SquadState::Pending);
        insert_bare_task(&store, "squad-match", 0, "core");
        insert_bare_cell(&store, "squad-match", 0, 0, None, None);
        insert_cell_subproject(&store, "squad-match", 0, 0, "auth", false);

        // Same project, unrelated sibling area -> must never be surveyed.
        insert_bare_squad(&store, "squad-sibling", SquadState::Pending);
        insert_bare_task(&store, "squad-sibling", 0, "core");
        insert_bare_cell(&store, "squad-sibling", 0, 0, None, None);
        insert_cell_subproject(&store, "squad-sibling", 0, 0, "billing", false);

        // Different project entirely, same area name -> must never be
        // surveyed (a project match is required before area overlap even
        // applies).
        insert_bare_squad(&store, "squad-other-project", SquadState::Pending);
        insert_bare_task(&store, "squad-other-project", 0, "other");
        insert_bare_cell(&store, "squad-other-project", 0, 0, None, None);
        insert_cell_subproject(&store, "squad-other-project", 0, 0, "auth", false);

        let candidates = store.waypoint_survey_candidates("waypoint-1").unwrap();
        assert_eq!(
            candidates.len(),
            1,
            "unrelated-area/project candidates must gain zero roster/survey state: {candidates:?}"
        );
        assert_eq!(candidates[0].kind, RosterEntryKind::Squad);
        assert_eq!(candidates[0].entry_id, "squad-match");
    }

    #[test]
    fn survey_candidates_cover_two_explicit_named_areas() {
        let store = Store::open_in_memory().unwrap();
        open_waypoint(&store, "waypoint-1");

        // Seed the waypoint against both "auth" and "billing" via two
        // separately-scoped roster entries.
        insert_bare_squad(&store, "squad-seed-auth", SquadState::Pending);
        insert_bare_task(&store, "squad-seed-auth", 0, "core");
        insert_bare_cell(&store, "squad-seed-auth", 0, 0, None, None);
        insert_cell_subproject(&store, "squad-seed-auth", 0, 0, "auth", false);
        store
            .add_roster_entry(
                "waypoint-1",
                RosterEntryKind::Squad,
                "squad-seed-auth",
                RosterMode::Block,
            )
            .unwrap();

        insert_bare_squad(&store, "squad-seed-billing", SquadState::Pending);
        insert_bare_task(&store, "squad-seed-billing", 0, "core");
        insert_bare_cell(&store, "squad-seed-billing", 0, 0, None, None);
        insert_cell_subproject(&store, "squad-seed-billing", 0, 0, "billing", false);
        store
            .add_roster_entry(
                "waypoint-1",
                RosterEntryKind::Squad,
                "squad-seed-billing",
                RosterMode::Block,
            )
            .unwrap();

        for (id, area) in [("squad-auth", "auth"), ("squad-billing", "billing")] {
            insert_bare_squad(&store, id, SquadState::Pending);
            insert_bare_task(&store, id, 0, "core");
            insert_bare_cell(&store, id, 0, 0, None, None);
            insert_cell_subproject(&store, id, 0, 0, area, false);
        }
        // A third, unrelated area -- must stay excluded even though the
        // waypoint already spans two areas.
        insert_bare_squad(&store, "squad-shipping", SquadState::Pending);
        insert_bare_task(&store, "squad-shipping", 0, "core");
        insert_bare_cell(&store, "squad-shipping", 0, 0, None, None);
        insert_cell_subproject(&store, "squad-shipping", 0, 0, "shipping", false);

        let candidates = store.waypoint_survey_candidates("waypoint-1").unwrap();
        let mut ids: Vec<&str> = candidates.iter().map(|c| c.entry_id.as_str()).collect();
        ids.sort_unstable();
        assert_eq!(ids, vec!["squad-auth", "squad-billing"]);
    }

    #[test]
    fn survey_candidates_fall_back_to_repo_wide_within_scoped_project_only() {
        let store = Store::open_in_memory().unwrap();
        open_waypoint(&store, "waypoint-1");

        // Seed roster entry whose cell never got a subproject resolution --
        // this project's scope can't be safely narrowed, so it goes
        // repo-wide (conservative fallback), per Phase 0.
        insert_bare_squad(&store, "squad-seed", SquadState::Pending);
        insert_bare_task(&store, "squad-seed", 0, "core");
        insert_bare_cell(&store, "squad-seed", 0, 0, None, None);
        store
            .add_roster_entry(
                "waypoint-1",
                RosterEntryKind::Squad,
                "squad-seed",
                RosterMode::Block,
            )
            .unwrap();
        assert_eq!(
            store
                .waypoint_scope_by_project("waypoint-1")
                .unwrap()
                .get("core"),
            Some(&Scope::RepoWide)
        );

        // Same project, a totally unrelated area -- included anyway because
        // the project's scope is repo-wide.
        insert_bare_squad(&store, "squad-same-project", SquadState::Pending);
        insert_bare_task(&store, "squad-same-project", 0, "core");
        insert_bare_cell(&store, "squad-same-project", 0, 0, None, None);
        insert_cell_subproject(
            &store,
            "squad-same-project",
            0,
            0,
            "wholly-unrelated",
            false,
        );

        // A different project (a different repo/root) is not swept in by
        // another project's repo-wide fallback.
        insert_bare_squad(&store, "squad-cross-project", SquadState::Pending);
        insert_bare_task(&store, "squad-cross-project", 0, "other");
        insert_bare_cell(&store, "squad-cross-project", 0, 0, None, None);
        insert_cell_subproject(&store, "squad-cross-project", 0, 0, "auth", false);

        let candidates = store.waypoint_survey_candidates("waypoint-1").unwrap();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].entry_id, "squad-same-project");
    }

    #[test]
    fn survey_candidates_exclude_terminal_and_already_rostered_entries() {
        let store = Store::open_in_memory().unwrap();
        open_waypoint(&store, "waypoint-1");

        insert_bare_squad(&store, "squad-seed", SquadState::Pending);
        insert_bare_task(&store, "squad-seed", 0, "core");
        insert_bare_cell(&store, "squad-seed", 0, 0, None, None);
        insert_cell_subproject(&store, "squad-seed", 0, 0, "auth", false);
        store
            .add_roster_entry(
                "waypoint-1",
                RosterEntryKind::Squad,
                "squad-seed",
                RosterMode::Block,
            )
            .unwrap();

        // Overlapping scope but already terminal -> excluded.
        insert_bare_squad(&store, "squad-done", SquadState::Done);
        insert_bare_task(&store, "squad-done", 0, "core");
        insert_bare_cell(&store, "squad-done", 0, 0, None, None);
        insert_cell_subproject(&store, "squad-done", 0, 0, "auth", false);

        // Overlapping scope but already an explicit roster entry -> excluded
        // (it's already been surveyed/tracked, not a fresh candidate).
        insert_bare_squad(&store, "squad-already-rostered", SquadState::Pending);
        insert_bare_task(&store, "squad-already-rostered", 0, "core");
        insert_bare_cell(&store, "squad-already-rostered", 0, 0, None, None);
        insert_cell_subproject(&store, "squad-already-rostered", 0, 0, "auth", false);
        store
            .add_roster_entry(
                "waypoint-1",
                RosterEntryKind::Squad,
                "squad-already-rostered",
                RosterMode::Block,
            )
            .unwrap();

        // A terminal review, and a fresh matching review -- reviews follow
        // the identical exclusion rules as squads.
        insert_guardian_with_status(&store, "guardian-merged", "merged");
        insert_bare_squad(&store, "squad-for-merged-review", SquadState::Pending);
        insert_bare_task(&store, "squad-for-merged-review", 0, "core");
        insert_bare_cell(
            &store,
            "squad-for-merged-review",
            0,
            0,
            Some("guardian-merged"),
            None,
        );
        insert_cell_subproject(&store, "squad-for-merged-review", 0, 0, "auth", false);

        insert_guardian_with_status(&store, "guardian-open", "collecting");
        insert_bare_squad(&store, "squad-for-open-review", SquadState::Pending);
        insert_bare_task(&store, "squad-for-open-review", 0, "core");
        insert_bare_cell(
            &store,
            "squad-for-open-review",
            0,
            0,
            Some("guardian-open"),
            None,
        );
        insert_cell_subproject(&store, "squad-for-open-review", 0, 0, "auth", false);

        let candidates = store.waypoint_survey_candidates("waypoint-1").unwrap();
        let squad_ids: Vec<&str> = candidates
            .iter()
            .filter(|c| c.kind == RosterEntryKind::Squad)
            .map(|c| c.entry_id.as_str())
            .collect();
        let review_ids: Vec<&str> = candidates
            .iter()
            .filter(|c| c.kind == RosterEntryKind::Review)
            .map(|c| c.entry_id.as_str())
            .collect();
        assert_eq!(
            squad_ids,
            vec!["squad-for-merged-review", "squad-for-open-review"],
            "the squads backing both reviews are themselves fresh, non-terminal, un-rostered candidates too"
        );
        assert_eq!(review_ids, vec!["guardian-open"]);
    }

    #[test]
    fn survey_candidates_empty_until_the_waypoint_has_a_seeded_roster_scope() {
        let store = Store::open_in_memory().unwrap();
        open_waypoint(&store, "waypoint-1");
        insert_bare_squad(&store, "squad-1", SquadState::Pending);
        insert_bare_task(&store, "squad-1", 0, "core");
        insert_bare_cell(&store, "squad-1", 0, 0, None, None);
        insert_cell_subproject(&store, "squad-1", 0, 0, "auth", false);

        // A waypoint with an empty roster has no aggregate scope to overlap
        // against, so it must select nothing rather than guess.
        assert!(
            store
                .waypoint_survey_candidates("waypoint-1")
                .unwrap()
                .is_empty()
        );
    }

    /// Marks a cell as if `run_cell_worker`'s `is_waypoint_halted()` branch
    /// had just run: DB `state='running'` (what `record_cell_result`
    /// persists for a waypoint-halted `RunnerResult`) plus
    /// `waypoint_halted_at_ms` set via [`Store::mark_cell_waypoint_halted`].
    fn halt_cell(store: &Store, squad_id: &str, task_idx: i64, idx: i64) {
        store
            .conn
            .execute(
                "UPDATE cells SET state='running' WHERE squad_id=? AND task_idx=? AND idx=?",
                params![squad_id, task_idx, idx],
            )
            .unwrap();
        store
            .mark_cell_waypoint_halted(squad_id, task_idx, idx)
            .unwrap();
    }

    fn handle(store: Store) -> Arc<StoreMutex> {
        Arc::new(StoreMutex::new(store))
    }

    #[test]
    fn resume_sweep_leaves_a_halted_cell_alone_while_the_waypoint_still_blocks() {
        let store = Store::open_in_memory().unwrap();
        open_waypoint(&store, "waypoint-1");
        insert_bare_squad(&store, "squad-1", SquadState::Running);
        insert_bare_task(&store, "squad-1", 0, "core");
        insert_bare_cell(&store, "squad-1", 0, 0, None, None);
        store
            .add_roster_entry(
                "waypoint-1",
                RosterEntryKind::Squad,
                "squad-1",
                RosterMode::Block,
            )
            .unwrap();
        halt_cell(&store, "squad-1", 0, 0);

        let handle = handle(store);
        let cancellations = Cancellations::new();
        run_pending_waypoint_resumes(&handle, &cancellations);

        let store = handle.lock();
        assert_eq!(
            store.cell_state("squad-1", 0, 0).unwrap(),
            Some(NodeState::Running),
            "still blocked -- the halted cell must not be resumed"
        );
        assert_eq!(store.squad_state("squad-1").unwrap(), SquadState::Running);
    }

    #[test]
    fn resume_sweep_frees_a_halted_cell_and_squad_once_the_waypoint_closes() {
        let store = Store::open_in_memory().unwrap();
        open_waypoint(&store, "waypoint-1");
        insert_bare_squad(&store, "squad-1", SquadState::Running);
        insert_bare_task(&store, "squad-1", 0, "core");
        insert_bare_cell(&store, "squad-1", 0, 0, None, None);
        store
            .add_roster_entry(
                "waypoint-1",
                RosterEntryKind::Squad,
                "squad-1",
                RosterMode::Block,
            )
            .unwrap();
        halt_cell(&store, "squad-1", 0, 0);
        assert!(store.close_waypoint("waypoint-1").unwrap());

        let handle = handle(store);
        // No worker registered for "squad-1" -- simulates the worker thread
        // having already exited (`run_cell_worker` returned once its cell
        // halted), so the sweep must also nudge the squad's own row back to
        // `Pending` for a fresh `scheduler::tick` to pick it up.
        let cancellations = Cancellations::new();
        run_pending_waypoint_resumes(&handle, &cancellations);

        let store = handle.lock();
        assert_eq!(
            store.cell_state("squad-1", 0, 0).unwrap(),
            Some(NodeState::Pending),
            "waypoint closed -- the halted cell must resume"
        );
        assert_eq!(store.squad_state("squad-1").unwrap(), SquadState::Pending);
    }

    #[test]
    fn resume_sweep_leaves_the_squads_own_row_alone_while_its_worker_is_still_alive() {
        let store = Store::open_in_memory().unwrap();
        open_waypoint(&store, "waypoint-1");
        insert_bare_squad(&store, "squad-1", SquadState::Running);
        insert_bare_task(&store, "squad-1", 0, "core");
        insert_bare_cell(&store, "squad-1", 0, 0, None, None);
        store
            .add_roster_entry(
                "waypoint-1",
                RosterEntryKind::Squad,
                "squad-1",
                RosterMode::Block,
            )
            .unwrap();
        halt_cell(&store, "squad-1", 0, 0);
        assert!(store.close_waypoint("waypoint-1").unwrap());

        let handle = handle(store);
        let cancellations = Cancellations::new();
        // Still-registered token: the squad's worker thread hasn't exited,
        // so it will notice the cell's DB state flip via the same
        // reclaimed-detached reconciliation `resume_detached_cell` relies
        // on -- the sweep must not also stomp the squad's own row.
        let _token = cancellations.register("squad-1");
        run_pending_waypoint_resumes(&handle, &cancellations);

        let store = handle.lock();
        assert_eq!(
            store.cell_state("squad-1", 0, 0).unwrap(),
            Some(NodeState::Pending),
            "waypoint closed -- the halted cell must still resume"
        );
        assert_eq!(
            store.squad_state("squad-1").unwrap(),
            SquadState::Running,
            "worker still alive -- its own row must not be touched"
        );
    }

    // ── run_pending_deliveries (Phase 4) ─────────────────────────────────

    /// Always returns a `"done"` result -- delivery only cares about
    /// `start_feedback`'s own synchronous `Reply`, not what its spawned
    /// background thread (which calls `run_feedback` with this runner) goes
    /// on to do with a fake, non-existent worktree.
    struct DeliveryTestRunner;

    impl crate::runner::Runner for DeliveryTestRunner {
        fn run(&self, _spec: &crate::runner::RunnerSpec) -> crate::runner::RunnerResult {
            crate::runner::RunnerResult {
                status: "done".to_string(),
                ..crate::runner::RunnerResult::failure("unused")
            }
        }
    }

    /// Adds an enabled branch with a review branch/worktree already recorded
    /// -- the readiness bar both [`topmost_ready_branch`] and
    /// `guardian_merge::start_feedback` itself enforce -- and returns its id.
    fn open_review_branch(store: &Store, guardian_id: &str, branch: &str) -> String {
        let position = store.add_guardian_branch(guardian_id, branch).unwrap();
        let branch_id = store.get_guardian(guardian_id).unwrap().branches
            [usize::try_from(position).unwrap()]
        .id
        .clone();
        store
            .set_branch_review(
                guardian_id,
                &branch_id,
                "review-branch",
                "/tmp/fake-worktree",
            )
            .unwrap();
        branch_id
    }

    #[test]
    fn run_pending_deliveries_marks_an_impacted_review_entry_delivered() {
        let store = Store::open_in_memory().unwrap();
        open_waypoint(&store, "waypoint-1");
        insert_bare_guardian(&store, "guardian-1");
        open_review_branch(&store, "guardian-1", "feature-x");
        store
            .add_roster_entry(
                "waypoint-1",
                RosterEntryKind::Review,
                "guardian-1",
                RosterMode::Block,
            )
            .unwrap();

        let handle = handle(store);
        let runner: Arc<dyn crate::runner::Runner> = Arc::new(DeliveryTestRunner);
        run_pending_deliveries(&handle, &runner);

        let store = handle.lock();
        let entries = store.list_roster_entries("waypoint-1").unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].delivery_status, DeliveryStatus::Delivered);
    }

    #[test]
    fn run_pending_deliveries_skips_a_not_impacted_review_entry() {
        let store = Store::open_in_memory().unwrap();
        open_waypoint(&store, "waypoint-1");
        insert_bare_guardian(&store, "guardian-1");
        open_review_branch(&store, "guardian-1", "feature-x");
        store
            .add_roster_entry(
                "waypoint-1",
                RosterEntryKind::Review,
                "guardian-1",
                RosterMode::Block,
            )
            .unwrap();
        store
            .set_roster_survey_result(
                "waypoint-1",
                RosterEntryKind::Review,
                "guardian-1",
                &SurveyVerdict {
                    impacted: false,
                    mode: RosterMode::Block,
                    rationale: "no overlap".to_string(),
                },
            )
            .unwrap();

        let handle = handle(store);
        let runner: Arc<dyn crate::runner::Runner> = Arc::new(DeliveryTestRunner);
        run_pending_deliveries(&handle, &runner);

        let store = handle.lock();
        let entries = store.list_roster_entries("waypoint-1").unwrap();
        assert_eq!(
            entries[0].delivery_status,
            DeliveryStatus::Undelivered,
            "not_impacted must never be delivered to"
        );
    }

    #[test]
    fn run_pending_deliveries_marks_a_done_but_unreviewed_squad_via_restack() {
        let store = Store::open_in_memory().unwrap();
        open_waypoint(&store, "waypoint-1");
        insert_bare_squad(&store, "squad-1", SquadState::Done);
        store
            .add_roster_entry(
                "waypoint-1",
                RosterEntryKind::Squad,
                "squad-1",
                RosterMode::Block,
            )
            .unwrap();

        let handle = handle(store);
        let runner: Arc<dyn crate::runner::Runner> = Arc::new(DeliveryTestRunner);
        run_pending_deliveries(&handle, &runner);

        let store = handle.lock();
        let entries = store.list_roster_entries("waypoint-1").unwrap();
        assert_eq!(entries[0].delivery_status, DeliveryStatus::ViaRestack);
    }

    #[test]
    fn run_pending_deliveries_leaves_a_still_running_squad_entry_untouched() {
        let store = Store::open_in_memory().unwrap();
        open_waypoint(&store, "waypoint-1");
        insert_bare_squad(&store, "squad-1", SquadState::Running);
        store
            .add_roster_entry(
                "waypoint-1",
                RosterEntryKind::Squad,
                "squad-1",
                RosterMode::Block,
            )
            .unwrap();

        let handle = handle(store);
        let runner: Arc<dyn crate::runner::Runner> = Arc::new(DeliveryTestRunner);
        run_pending_deliveries(&handle, &runner);

        let store = handle.lock();
        let entries = store.list_roster_entries("waypoint-1").unwrap();
        assert_eq!(
            entries[0].delivery_status,
            DeliveryStatus::Undelivered,
            "still-running squad has nothing to do yet -- it is reached later, either \
             directly (once non-terminal) or via-restack (once terminal)"
        );
    }

    // ── waypoint_survey_candidates population filter (Phase 4 addition) ──

    #[test]
    fn survey_candidates_exclude_cancelled_squads_by_construction() {
        // RAL-400 Phase 4 AC: fully-done/cancelled squads are excluded from
        // the classification population *by construction* -- this covers the
        // `Cancelled` terminal variant specifically, alongside the existing
        // `survey_candidates_exclude_terminal_and_already_rostered_entries`
        // test's coverage of `Done`.
        let store = Store::open_in_memory().unwrap();
        open_waypoint(&store, "waypoint-1");

        insert_bare_squad(&store, "squad-seed", SquadState::Pending);
        insert_bare_task(&store, "squad-seed", 0, "core");
        insert_bare_cell(&store, "squad-seed", 0, 0, None, None);
        insert_cell_subproject(&store, "squad-seed", 0, 0, "auth", false);
        store
            .add_roster_entry(
                "waypoint-1",
                RosterEntryKind::Squad,
                "squad-seed",
                RosterMode::Block,
            )
            .unwrap();

        // Overlapping scope but cancelled -> excluded.
        insert_bare_squad(&store, "squad-cancelled", SquadState::Cancelled);
        insert_bare_task(&store, "squad-cancelled", 0, "core");
        insert_bare_cell(&store, "squad-cancelled", 0, 0, None, None);
        insert_cell_subproject(&store, "squad-cancelled", 0, 0, "auth", false);

        let candidates = store.waypoint_survey_candidates("waypoint-1").unwrap();
        assert!(
            candidates.is_empty(),
            "a cancelled squad must never surface as a survey candidate: {candidates:?}"
        );
    }
}
