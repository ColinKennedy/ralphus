//! RAL-400: waypoints -- named join points that let a human-authored note
//! retroactively bind already-submitted or running work (reviews and
//! squads, in v1's roster scope) without that join being foreseeable at
//! submit time. See `core::schema::WaypointDef` for the TOML shape this
//! store layer backs, and `MILESTONE_PLAN.local.md` for the full design.
//!
//! This module is Phase 1 scope only: the durable record (a waypoint, its
//! roster, and its append-only bearings) and basic CRUD. The survey pass
//! that assigns each roster entry's initial `mode`/verdict (Phase 2), the
//! scheduler gating and in-flight delivery/parking that act on a roster
//! entry's `delivery_status` (Phases 3-5), and lifecycle auto-close (Phase
//! 6) are not implemented here -- every roster entry created by this
//! module starts `block`/`undelivered` with no survey verdict, matching
//! this pass's fail-closed default until a survey exists to say otherwise.

use rusqlite::params;
use serde::Serialize;

use crate::store::{Result, Store, StoreError, now_ms};

/// Which kind of entity a roster entry names. v1's roster scope is reviews
/// and squads only -- see `MILESTONE_PLAN.local.md`'s Phase 0 decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RosterEntityKind {
    Review,
    Squad,
}

impl RosterEntityKind {
    /// The stable lowercase string stored in the database and used on the
    /// wire.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Review => "review",
            Self::Squad => "squad",
        }
    }

    /// Parse the stable lowercase kind string, or `None` if unrecognized.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "review" => Self::Review,
            "squad" => Self::Squad,
            _ => return None,
        })
    }
}

/// Whether a roster entry's delivery blocks its entity or merely informs
/// it. Starts at the survey's verdict, human-overridable afterward -- see
/// [`Store::set_roster_entry_mode`].
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

/// Whether a waypoint's prompt has reached a roster entry yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
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

/// One roster entry to seed a new waypoint with: which entity, and its
/// kind.
#[derive(Debug, Clone, Copy)]
pub struct RosterSeed<'a> {
    pub entity_kind: RosterEntityKind,
    pub entity_id: &'a str,
}

/// A waypoint as read back from the store.
#[derive(Debug, Clone, Serialize)]
pub struct WaypointView {
    /// Stable id (`waypoint-000000000001`, ...).
    pub id: String,
    pub label: String,
    pub prompt: String,
    pub agent: Option<String>,
    pub model: Option<String>,
    pub allow_advisory: bool,
    /// `None` while the waypoint is still open.
    pub closed_at_ms: Option<i64>,
    pub created_at_ms: i64,
}

/// One roster entry as read back from the store.
#[derive(Debug, Clone, Serialize)]
pub struct RosterEntryView {
    pub waypoint_id: String,
    pub entity_kind: String,
    pub entity_id: String,
    pub mode: String,
    pub survey_verdict: Option<String>,
    pub survey_rationale: Option<String>,
    pub delivery_status: String,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

/// One durable bearing (completed-work account) as read back from the
/// store.
#[derive(Debug, Clone, Serialize)]
pub struct BearingView {
    pub id: String,
    pub waypoint_id: String,
    pub commit_id: Option<String>,
    pub commit_summary: Option<String>,
    pub summary: String,
    pub entity_uri: Option<String>,
    pub created_at_ms: i64,
}

impl Store {
    /// Create a waypoint with its initial roster. Every entry starts
    /// `block`/`undelivered` with no survey verdict -- Phase 2's survey
    /// pass (not implemented here) is what would later narrow entries to
    /// `advisory`.
    ///
    /// # Errors
    /// Returns [`StoreError::InvalidTransition`] if `roster` is empty --
    /// a waypoint with a vacuous roster can never be delivered to anything,
    /// so creating one is rejected here as well as at the TOML validator
    /// (`core::validate::validate_waypoint_blocks`). Otherwise propagates
    /// any SQLite failure.
    pub fn create_waypoint(
        &self,
        label: &str,
        prompt: &str,
        agent: Option<&str>,
        model: Option<&str>,
        allow_advisory: bool,
        roster: &[RosterSeed<'_>],
    ) -> Result<WaypointView> {
        if roster.is_empty() {
            return Err(StoreError::InvalidTransition(
                "a waypoint requires at least one roster entry".to_string(),
            ));
        }
        let id = self.next_id("waypoint_seq", "waypoint")?;
        let created_at_ms = now_ms();
        self.conn.execute(
            "INSERT INTO waypoints(id, label, prompt, agent, model, allow_advisory, closed_at_ms, created_at_ms)
             VALUES(?1,?2,?3,?4,?5,?6,NULL,?7)",
            params![
                id,
                label,
                prompt,
                agent,
                model,
                i64::from(allow_advisory),
                created_at_ms
            ],
        )?;
        for entry in roster {
            self.conn.execute(
                "INSERT INTO waypoint_roster(
                     waypoint_id, entity_kind, entity_id, mode,
                     survey_verdict, survey_rationale, delivery_status,
                     created_at_ms, updated_at_ms
                 ) VALUES(?1,?2,?3,'block',NULL,NULL,'undelivered',?4,?4)",
                params![
                    id,
                    entry.entity_kind.as_str(),
                    entry.entity_id,
                    created_at_ms
                ],
            )?;
        }
        crate::cartographer::Note::new("store").emit(
            self,
            format!(
                "waypoint {id:?} {label:?} created with {} roster entr{}",
                roster.len(),
                if roster.len() == 1 { "y" } else { "ies" }
            ),
            serde_json::json!({"waypoint_id": id, "label": label, "roster_len": roster.len()}),
        );
        Ok(WaypointView {
            id,
            label: label.to_string(),
            prompt: prompt.to_string(),
            agent: agent.map(str::to_string),
            model: model.map(str::to_string),
            allow_advisory,
            closed_at_ms: None,
            created_at_ms,
        })
    }

    /// Look up one waypoint by id.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn waypoint(&self, id: &str) -> Result<Option<WaypointView>> {
        use rusqlite::OptionalExtension as _;
        self.conn
            .query_row(
                "SELECT id, label, prompt, agent, model, allow_advisory, closed_at_ms, created_at_ms
                 FROM waypoints WHERE id=?1",
                params![id],
                Self::row_to_waypoint,
            )
            .optional()
            .map_err(StoreError::from)
    }

    /// Every waypoint, newest first.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn list_waypoints(&self) -> Result<Vec<WaypointView>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, label, prompt, agent, model, allow_advisory, closed_at_ms, created_at_ms
             FROM waypoints ORDER BY created_at_ms DESC, id DESC",
        )?;
        let rows = stmt
            .query_map([], Self::row_to_waypoint)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    fn row_to_waypoint(r: &rusqlite::Row<'_>) -> rusqlite::Result<WaypointView> {
        Ok(WaypointView {
            id: r.get(0)?,
            label: r.get(1)?,
            prompt: r.get(2)?,
            agent: r.get(3)?,
            model: r.get(4)?,
            allow_advisory: r.get::<_, i64>(5)? != 0,
            closed_at_ms: r.get(6)?,
            created_at_ms: r.get(7)?,
        })
    }

    /// Every roster entry for `waypoint_id`, in the order they were added.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn waypoint_roster(&self, waypoint_id: &str) -> Result<Vec<RosterEntryView>> {
        let mut stmt = self.conn.prepare(
            "SELECT waypoint_id, entity_kind, entity_id, mode, survey_verdict,
                    survey_rationale, delivery_status, created_at_ms, updated_at_ms
             FROM waypoint_roster WHERE waypoint_id=?1
             ORDER BY created_at_ms, entity_kind, entity_id",
        )?;
        let rows = stmt
            .query_map(params![waypoint_id], |r| {
                Ok(RosterEntryView {
                    waypoint_id: r.get(0)?,
                    entity_kind: r.get(1)?,
                    entity_id: r.get(2)?,
                    mode: r.get(3)?,
                    survey_verdict: r.get(4)?,
                    survey_rationale: r.get(5)?,
                    delivery_status: r.get(6)?,
                    created_at_ms: r.get(7)?,
                    updated_at_ms: r.get(8)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Human-override a roster entry's `mode`. Leaves the original survey
    /// verdict/rationale in place so "why did this entry end up advisory"
    /// stays answerable after the override. Returns `false` if no such
    /// roster entry exists.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn set_roster_entry_mode(
        &self,
        waypoint_id: &str,
        entity_kind: RosterEntityKind,
        entity_id: &str,
        mode: RosterMode,
    ) -> Result<bool> {
        let n = self.conn.execute(
            "UPDATE waypoint_roster SET mode=?1, updated_at_ms=?2
             WHERE waypoint_id=?3 AND entity_kind=?4 AND entity_id=?5",
            params![
                mode.as_str(),
                now_ms(),
                waypoint_id,
                entity_kind.as_str(),
                entity_id
            ],
        )?;
        Ok(n > 0)
    }

    /// Record a roster entry's `delivery_status` as delivery is attempted
    /// or completed. Returns `false` if no such roster entry exists.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn set_roster_entry_delivery_status(
        &self,
        waypoint_id: &str,
        entity_kind: RosterEntityKind,
        entity_id: &str,
        status: DeliveryStatus,
    ) -> Result<bool> {
        let n = self.conn.execute(
            "UPDATE waypoint_roster SET delivery_status=?1, updated_at_ms=?2
             WHERE waypoint_id=?3 AND entity_kind=?4 AND entity_id=?5",
            params![
                status.as_str(),
                now_ms(),
                waypoint_id,
                entity_kind.as_str(),
                entity_id
            ],
        )?;
        Ok(n > 0)
    }

    /// Append a bearing (a durable, append-only account of actual
    /// completed work) to `waypoint_id`. There is no corresponding update
    /// or delete -- a waypoint's history is never rewritten, only added to.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn add_bearing(
        &self,
        waypoint_id: &str,
        commit_id: Option<&str>,
        commit_summary: Option<&str>,
        summary: &str,
        entity_uri: Option<&str>,
    ) -> Result<BearingView> {
        let id = self.next_id("waypoint_bearing_seq", "bearing")?;
        let created_at_ms = now_ms();
        self.conn.execute(
            "INSERT INTO waypoint_bearings(id, waypoint_id, commit_id, commit_summary, summary, entity_uri, created_at_ms)
             VALUES(?1,?2,?3,?4,?5,?6,?7)",
            params![id, waypoint_id, commit_id, commit_summary, summary, entity_uri, created_at_ms],
        )?;
        crate::cartographer::Note::new("store").emit(
            self,
            format!("waypoint {waypoint_id:?} gained a bearing: {summary:?}"),
            serde_json::json!({"waypoint_id": waypoint_id, "bearing_id": id}),
        );
        Ok(BearingView {
            id,
            waypoint_id: waypoint_id.to_string(),
            commit_id: commit_id.map(str::to_string),
            commit_summary: commit_summary.map(str::to_string),
            summary: summary.to_string(),
            entity_uri: entity_uri.map(str::to_string),
            created_at_ms,
        })
    }

    /// Every bearing recorded against `waypoint_id`, oldest first.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn list_bearings(&self, waypoint_id: &str) -> Result<Vec<BearingView>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, waypoint_id, commit_id, commit_summary, summary, entity_uri, created_at_ms
             FROM waypoint_bearings WHERE waypoint_id=?1 ORDER BY created_at_ms, id",
        )?;
        let rows = stmt
            .query_map(params![waypoint_id], |r| {
                Ok(BearingView {
                    id: r.get(0)?,
                    waypoint_id: r.get(1)?,
                    commit_id: r.get(2)?,
                    commit_summary: r.get(3)?,
                    summary: r.get(4)?,
                    entity_uri: r.get(5)?,
                    created_at_ms: r.get(6)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Close a waypoint, if it isn't already. Returns `false` if the
    /// waypoint was already closed.
    ///
    /// # Errors
    /// Returns [`StoreError::NotFound`] if no such waypoint exists.
    /// Otherwise propagates any SQLite failure.
    pub fn close_waypoint(&self, id: &str) -> Result<bool> {
        let Some(existing) = self.waypoint(id)? else {
            return Err(StoreError::NotFound);
        };
        if existing.closed_at_ms.is_some() {
            return Ok(false);
        }
        self.conn.execute(
            "UPDATE waypoints SET closed_at_ms=?1 WHERE id=?2",
            params![now_ms(), id],
        )?;
        crate::cartographer::Note::new("store").emit(
            self,
            format!("waypoint {id:?} closed"),
            serde_json::json!({"waypoint_id": id}),
        );
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> Store {
        Store::open_in_memory().expect("open in-memory store")
    }

    fn seed(entity_kind: RosterEntityKind, entity_id: &str) -> RosterSeed<'_> {
        RosterSeed {
            entity_kind,
            entity_id,
        }
    }

    #[test]
    fn creating_a_waypoint_with_no_roster_is_rejected() {
        let s = store();
        let err = s
            .create_waypoint("label", "do the thing", None, None, false, &[])
            .unwrap_err();
        assert!(matches!(err, StoreError::InvalidTransition(_)));
    }

    #[test]
    fn a_created_waypoint_reads_back_with_its_roster() {
        let s = store();
        let wp = s
            .create_waypoint(
                "cross-squad thing",
                "please coordinate",
                Some("claude-code"),
                Some("sonnet"),
                true,
                &[
                    seed(RosterEntityKind::Squad, "squad-1"),
                    seed(RosterEntityKind::Review, "review-1"),
                ],
            )
            .unwrap();
        assert!(wp.id.starts_with("waypoint-"));
        assert_eq!(wp.label, "cross-squad thing");
        assert!(wp.allow_advisory);
        assert_eq!(wp.closed_at_ms, None);

        let fetched = s.waypoint(&wp.id).unwrap().expect("waypoint exists");
        assert_eq!(fetched.prompt, "please coordinate");
        assert_eq!(fetched.model.as_deref(), Some("sonnet"));

        let roster = s.waypoint_roster(&wp.id).unwrap();
        assert_eq!(roster.len(), 2);
        assert!(roster.iter().all(|r| r.mode == "block"));
        assert!(roster.iter().all(|r| r.delivery_status == "undelivered"));
        assert!(roster.iter().all(|r| r.survey_verdict.is_none()));
        assert!(
            roster
                .iter()
                .any(|r| r.entity_kind == "squad" && r.entity_id == "squad-1")
        );
        assert!(
            roster
                .iter()
                .any(|r| r.entity_kind == "review" && r.entity_id == "review-1")
        );
    }

    #[test]
    fn missing_waypoint_reads_back_none() {
        let s = store();
        assert!(s.waypoint("waypoint-nope").unwrap().is_none());
    }

    #[test]
    fn list_waypoints_orders_newest_first() {
        let s = store();
        let a = s
            .create_waypoint(
                "a",
                "p",
                None,
                None,
                false,
                &[seed(RosterEntityKind::Squad, "s1")],
            )
            .unwrap();
        let b = s
            .create_waypoint(
                "b",
                "p",
                None,
                None,
                false,
                &[seed(RosterEntityKind::Squad, "s2")],
            )
            .unwrap();
        let all = s.list_waypoints().unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].id, b.id);
        assert_eq!(all[1].id, a.id);
    }

    #[test]
    fn overriding_roster_mode_keeps_the_original_survey_verdict() {
        let s = store();
        let wp = s
            .create_waypoint(
                "a",
                "p",
                None,
                None,
                true,
                &[seed(RosterEntityKind::Squad, "s1")],
            )
            .unwrap();
        assert!(
            s.set_roster_entry_mode(&wp.id, RosterEntityKind::Squad, "s1", RosterMode::Advisory)
                .unwrap()
        );
        let roster = s.waypoint_roster(&wp.id).unwrap();
        assert_eq!(roster[0].mode, "advisory");
        assert_eq!(roster[0].survey_verdict, None);

        assert!(
            !s.set_roster_entry_mode(
                &wp.id,
                RosterEntityKind::Squad,
                "no-such",
                RosterMode::Block
            )
            .unwrap()
        );
    }

    #[test]
    fn delivery_status_updates_the_named_roster_entry_only() {
        let s = store();
        let wp = s
            .create_waypoint(
                "a",
                "p",
                None,
                None,
                false,
                &[
                    seed(RosterEntityKind::Squad, "s1"),
                    seed(RosterEntityKind::Squad, "s2"),
                ],
            )
            .unwrap();
        assert!(
            s.set_roster_entry_delivery_status(
                &wp.id,
                RosterEntityKind::Squad,
                "s1",
                DeliveryStatus::Delivered
            )
            .unwrap()
        );
        let roster = s.waypoint_roster(&wp.id).unwrap();
        let s1 = roster.iter().find(|r| r.entity_id == "s1").unwrap();
        let s2 = roster.iter().find(|r| r.entity_id == "s2").unwrap();
        assert_eq!(s1.delivery_status, "delivered");
        assert_eq!(s2.delivery_status, "undelivered");
    }

    #[test]
    fn bearings_are_appended_in_order_and_scoped_per_waypoint() {
        let s = store();
        let wp1 = s
            .create_waypoint(
                "a",
                "p",
                None,
                None,
                false,
                &[seed(RosterEntityKind::Squad, "s1")],
            )
            .unwrap();
        let wp2 = s
            .create_waypoint(
                "b",
                "p",
                None,
                None,
                false,
                &[seed(RosterEntityKind::Squad, "s2")],
            )
            .unwrap();

        s.add_bearing(
            &wp1.id,
            Some("abc123"),
            Some("fix the thing"),
            "did work",
            None,
        )
        .unwrap();
        s.add_bearing(&wp1.id, None, None, "did more work", Some("squad:squad-1"))
            .unwrap();
        s.add_bearing(&wp2.id, None, None, "unrelated work", None)
            .unwrap();

        let bearings1 = s.list_bearings(&wp1.id).unwrap();
        assert_eq!(bearings1.len(), 2);
        assert_eq!(bearings1[0].summary, "did work");
        assert_eq!(bearings1[0].commit_id.as_deref(), Some("abc123"));
        assert_eq!(bearings1[1].summary, "did more work");
        assert_eq!(bearings1[1].entity_uri.as_deref(), Some("squad:squad-1"));

        let bearings2 = s.list_bearings(&wp2.id).unwrap();
        assert_eq!(bearings2.len(), 1);
        assert_eq!(bearings2[0].summary, "unrelated work");
    }

    #[test]
    fn closing_a_waypoint_is_idempotent_and_reflected_on_read() {
        let s = store();
        let wp = s
            .create_waypoint(
                "a",
                "p",
                None,
                None,
                false,
                &[seed(RosterEntityKind::Squad, "s1")],
            )
            .unwrap();
        assert!(s.waypoint(&wp.id).unwrap().unwrap().closed_at_ms.is_none());
        assert!(s.close_waypoint(&wp.id).unwrap());
        assert!(s.waypoint(&wp.id).unwrap().unwrap().closed_at_ms.is_some());
        assert!(!s.close_waypoint(&wp.id).unwrap(), "already closed");
    }

    #[test]
    fn closing_a_missing_waypoint_is_not_found() {
        let s = store();
        assert!(matches!(
            s.close_waypoint("waypoint-nope"),
            Err(StoreError::NotFound)
        ));
    }

    #[test]
    fn roster_entity_kind_and_mode_and_delivery_status_round_trip() {
        assert_eq!(
            RosterEntityKind::parse("review"),
            Some(RosterEntityKind::Review)
        );
        assert_eq!(
            RosterEntityKind::parse("squad"),
            Some(RosterEntityKind::Squad)
        );
        assert_eq!(RosterEntityKind::parse("bogus"), None);

        assert_eq!(RosterMode::parse("block"), Some(RosterMode::Block));
        assert_eq!(RosterMode::parse("advisory"), Some(RosterMode::Advisory));
        assert_eq!(RosterMode::parse("bogus"), None);

        assert_eq!(
            DeliveryStatus::parse("undelivered"),
            Some(DeliveryStatus::Undelivered)
        );
        assert_eq!(
            DeliveryStatus::parse("delivered"),
            Some(DeliveryStatus::Delivered)
        );
        assert_eq!(
            DeliveryStatus::parse("via-restack"),
            Some(DeliveryStatus::ViaRestack)
        );
        assert_eq!(
            DeliveryStatus::parse("failed"),
            Some(DeliveryStatus::Failed)
        );
        assert_eq!(DeliveryStatus::parse("bogus"), None);
    }
}
