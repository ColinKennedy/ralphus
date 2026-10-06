//! Monitor watches: a per-user subscription layer over the
//! existing mailbox (`crate::mailbox`) event stream, not a parallel
//! notification system. Watching an entity (squad/task/cell/proof/review/
//! guardian) means messages whose `entity_uri` is covered by the watch
//! (see [`crate::entity_uri::EntityUri::covers`]) show up in that user's
//! personal mailbox, filtered to the notification tiers chosen for the
//! watch. See `crate::mailbox::personal_mailbox_messages_for_user`.
//!
//! Callers are expected to have already validated `entity_uri` (via
//! `crate::entity_uri::parse`) and `notify_tiers` (non-empty) at the HTTP
//! boundary -- this module trusts its inputs, matching
//! `Store::enqueue_mailbox_message`'s own precedent.

use rusqlite::params;
use serde::Serialize;

use crate::events::MailboxNotice;
use crate::mailbox::{self, MailboxPriority, Remediation};
use crate::store::{Result, Store};

pub use crate::watches::WatchView;

/// The deliberately bounded set of changes that Monitor can notify about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NotifiableEventKind {
    SquadContentChanged,
    SquadAttributesChanged,
    ReviewSettingsChanged,
    ReviewStatusChanged,
    ReviewManualChecksReady,
    ReviewManualChecksFailed,
    /// RAL-565: an auto-run manual check finished with a non-zero exit or
    /// timed out. The check's own result, not a system fault, so it is an
    /// informational notice without remediation and leaves the review's state
    /// alone.
    ReviewAutoRunCheckFailed,
    /// RAL-565: a rebase, feedback pass or auto-PR fix arrived while an
    /// auto-run manual check was running, so its environment may be out of
    /// date. Informational; the run is never killed.
    ReviewAutoRunSuperseded,
    SquadFailed,
    ReviewFailed,
    /// RAL-400 Phase 3: a squad's in-flight cell was halted because its
    /// squad-kind affected entry just became `mode=block` on an open waypoint.
    /// Not a failure in the ordinary sense (the cell will resume
    /// automatically once the waypoint closes or de-escalates), but RAL-502
    /// still requires remediation guidance since it is a blocked state --
    /// see [`Store::notify_watchers_with_remediation`]'s broadened assert.
    SquadWaypointHalted,
    /// RAL-400 Phase 8: a new waypoint just added a review or squad to its
    /// affected, notifying that affected entry's own watchers.
    WaypointCreated,
    /// RAL-400: this squad or review is now held by an open waypoint and
    /// cannot proceed until it closes or the entry de-escalates to advisory.
    ///
    /// Distinct from [`Self::SquadWaypointHalted`], which reports the narrower
    /// event of an *already-running cell* being stopped. This one covers work
    /// that is held before it ever starts (gated at submit, or by a `block`
    /// survey verdict) and a review whose approval is held -- states that
    /// previously produced no notification at all, so work could sit blocked
    /// indefinitely with nothing saying why. A blocked state, so it carries
    /// remediation per RAL-502.
    WaypointBlocked,
    /// RAL-400: a waypoint's guidance applies to this squad or review in
    /// `advisory` mode -- it is not held, but it is expected to take the
    /// guidance into account. Informational, so no remediation.
    WaypointAdvised,
    /// A merged review wrote `deferred` prophecies, and ralphus is offering to
    /// turn them into a follow-up squad (`crate::followup`). Informational:
    /// the user answers it with `review followup accept`/`decline`.
    ReviewFollowupOffered,
}

impl NotifiableEventKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SquadContentChanged => "squad_content_changed",
            Self::SquadAttributesChanged => "squad_attributes_changed",
            Self::ReviewSettingsChanged => "review_settings_changed",
            Self::ReviewStatusChanged => "review_status_changed",
            Self::ReviewManualChecksReady => "review_manual_checks_ready",
            Self::ReviewManualChecksFailed => "review_manual_checks_failed",
            Self::ReviewAutoRunCheckFailed => "review_auto_run_check_failed",
            Self::ReviewAutoRunSuperseded => "review_auto_run_superseded",
            Self::SquadFailed => "squad_failed",
            Self::ReviewFailed => "review_failed",
            Self::SquadWaypointHalted => "squad_waypoint_halted",
            Self::WaypointCreated => "waypoint_created",
            Self::WaypointBlocked => "waypoint_blocked",
            Self::WaypointAdvised => "waypoint_advised",
            Self::ReviewFollowupOffered => "review_followup_offered",
        }
    }
}

impl Store {
    /// Emit one shared, tagged mailbox row for a Monitor event. Ordinary and
    /// personal mailbox views consume the same row, preventing duplicate mail.
    pub fn notify_watchers(
        &self,
        event: NotifiableEventKind,
        entity_uri: &str,
        priority: MailboxPriority,
        message: &str,
        squad_id: Option<&str>,
    ) -> Result<String> {
        self.notify_watchers_with_context(
            event, entity_uri, priority, message, squad_id, None, None,
        )
    }

    /// Emit a tagged Monitor event while retaining its task/cell mailbox context.
    #[allow(clippy::too_many_arguments)]
    pub fn notify_watchers_with_context(
        &self,
        event: NotifiableEventKind,
        entity_uri: &str,
        priority: MailboxPriority,
        message: &str,
        squad_id: Option<&str>,
        task: Option<&str>,
        cell_id: Option<&str>,
    ) -> Result<String> {
        let id = self.enqueue_mailbox_message_unlogged(
            priority,
            message,
            squad_id,
            task,
            cell_id,
            Some(entity_uri),
            None,
        )?;
        self.conn.execute(
            "UPDATE mailbox_messages SET event_kind=?1 WHERE id=?2",
            params![event.as_str(), id],
        )?;
        self.event_bus().publish_mailbox(MailboxNotice {
            message_id: id.clone(),
            priority: priority.as_str().to_string(),
            message: message.to_string(),
            squad_id: squad_id.map(str::to_string),
            task: task.map(str::to_string),
            cell_id: cell_id.map(str::to_string),
            entity_uri: Some(entity_uri.to_string()),
            category: None,
        });
        self.log_mailbox_enqueued(&id, Some(event.as_str()));
        Ok(id)
    }

    /// Emit a tagged Monitor event for a genuine failure or blocked state
    /// (RAL-502) -- `event` must be [`NotifiableEventKind::SquadFailed`],
    /// [`NotifiableEventKind::ReviewFailed`], or
    /// [`NotifiableEventKind::SquadWaypointHalted`], the variants that
    /// represent an error/failure/block rather than an ordinary status
    /// change. `remediation` is mandatory: its rendered text is appended to
    /// `message` via `enqueue_error_mailbox_message_unlogged` (the same
    /// folding [`crate::mailbox::Store::enqueue_error_mailbox_message`]
    /// applies), so every such notification a watcher receives carries
    /// actionable guidance. The enqueue is logged after the bus push, so SSE
    /// subscribers see the message before its log row.
    #[allow(clippy::too_many_arguments)]
    pub fn notify_watchers_with_remediation(
        &self,
        event: NotifiableEventKind,
        entity_uri: &str,
        priority: MailboxPriority,
        message: &str,
        remediation: &Remediation,
        squad_id: Option<&str>,
        task: Option<&str>,
        cell_id: Option<&str>,
    ) -> Result<String> {
        debug_assert!(
            matches!(
                event,
                NotifiableEventKind::SquadFailed
                    | NotifiableEventKind::ReviewFailed
                    | NotifiableEventKind::SquadWaypointHalted
                    | NotifiableEventKind::WaypointBlocked
                    | NotifiableEventKind::ReviewManualChecksFailed
            ),
            "notify_watchers_with_remediation is for failure/blocked events only; use notify_watchers_with_context for ordinary status changes"
        );
        let id = self.enqueue_error_mailbox_message_unlogged(
            priority,
            message,
            remediation,
            squad_id,
            task,
            cell_id,
            Some(entity_uri),
            None,
        )?;
        self.conn.execute(
            "UPDATE mailbox_messages SET event_kind=?1 WHERE id=?2",
            params![event.as_str(), id],
        )?;
        self.event_bus().publish_mailbox(MailboxNotice {
            message_id: id.clone(),
            priority: priority.as_str().to_string(),
            message: format!("{message} {}", remediation.render()),
            squad_id: squad_id.map(str::to_string),
            task: task.map(str::to_string),
            cell_id: cell_id.map(str::to_string),
            entity_uri: Some(entity_uri.to_string()),
            category: None,
        });
        self.log_mailbox_enqueued(&id, Some(event.as_str()));
        Ok(id)
    }

    /// Every user watching one whole squad or review, oldest first.
    pub fn watchers_for_entity(&self, entity_uri: &str) -> Result<Vec<WatchView>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, user_name, entity_uri, notify_tiers, created_at_ms
             FROM watches WHERE entity_uri=?1 ORDER BY created_at_ms, user_name",
        )?;
        let rows = stmt
            .query_map(params![entity_uri], |r| {
                let tiers: String = r.get(3)?;
                Ok(WatchView {
                    id: r.get(0)?,
                    user_name: r.get(1)?,
                    entity_uri: r.get(2)?,
                    notify_tiers: mailbox::parse_tiers(&tiers),
                    created_at_ms: r.get(4)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Inserts a minimal `squads` row so `mailbox_messages.squad_id`'s
    /// foreign key is satisfiable.
    fn insert_squad(store: &Store, id: &str) {
        store
            .conn
            .execute(
                "INSERT INTO squads(id, state, created_at_ms, updated_at_ms) VALUES(?1, 'running', 0, 0)",
                params![id],
            )
            .unwrap();
    }

    #[test]
    fn create_list_and_delete_a_watch() {
        let store = Store::open_in_memory().unwrap();
        let watch = store
            .create_watch(
                "colin",
                "squad:squad-000000000001",
                &[MailboxPriority::Urgent, MailboxPriority::High],
            )
            .unwrap();
        assert_eq!(watch.entity_uri, "squad:squad-000000000001");
        assert_eq!(
            watch.notify_tiers,
            vec![MailboxPriority::Urgent, MailboxPriority::High]
        );

        let watches = store.list_watches("colin").unwrap();
        assert_eq!(watches.len(), 1);
        assert_eq!(watches[0].id, watch.id);

        assert!(
            store
                .delete_watch("colin", "squad:squad-000000000001")
                .unwrap()
        );
        assert!(store.list_watches("colin").unwrap().is_empty());
        assert!(
            !store
                .delete_watch("colin", "squad:squad-000000000001")
                .unwrap()
        );
    }

    #[test]
    fn rewatching_the_same_entity_updates_tiers_in_place() {
        let store = Store::open_in_memory().unwrap();
        store
            .create_watch("colin", "squad:squad-1", &[MailboxPriority::Urgent])
            .unwrap();
        let updated = store
            .create_watch(
                "colin",
                "squad:squad-1",
                &[MailboxPriority::Urgent, MailboxPriority::Normal],
            )
            .unwrap();
        let watches = store.list_watches("colin").unwrap();
        assert_eq!(watches.len(), 1, "re-watching must not duplicate the row");
        assert_eq!(watches[0].id, updated.id);
        assert_eq!(watches[0].notify_tiers.len(), 2);
    }

    #[test]
    fn watches_are_scoped_per_user() {
        let store = Store::open_in_memory().unwrap();
        store
            .create_watch("colin", "squad:squad-1", &[MailboxPriority::Urgent])
            .unwrap();
        store
            .create_watch("alex", "squad:squad-1", &[MailboxPriority::Normal])
            .unwrap();
        assert_eq!(store.list_watches("colin").unwrap().len(), 1);
        assert_eq!(store.list_watches("alex").unwrap().len(), 1);
        assert!(store.delete_watch("alex", "squad:squad-1").unwrap());
        assert_eq!(store.list_watches("colin").unwrap().len(), 1);
    }

    #[test]
    fn failure_notifications_require_and_render_remediation() {
        let store = Store::open_in_memory().unwrap();
        insert_squad(&store, "squad-1");
        store
            .create_watch("colin", "squad:squad-1", &[MailboxPriority::Urgent])
            .unwrap();
        let id = store
            .notify_watchers_with_remediation(
                NotifiableEventKind::SquadFailed,
                "squad:squad-1",
                MailboxPriority::Urgent,
                "squad squad-1 failed to materialize: disk full",
                &mailbox::Remediation::ManualInterventionRequired {
                    guidance: "free disk space, then run `ralphus squad retry squad-1`".to_string(),
                },
                Some("squad-1"),
                None,
                None,
            )
            .unwrap();

        let personal = store
            .personal_mailbox_messages_for_user("colin", false, None)
            .unwrap();
        assert_eq!(personal.len(), 1);
        assert_eq!(personal[0].id, id);
        assert_eq!(personal[0].event_kind.as_deref(), Some("squad_failed"));
        assert!(
            personal[0]
                .message
                .contains("Manual intervention required:")
        );
    }

    #[test]
    fn monitor_event_is_one_tagged_row_shared_by_broadcast_and_personal_views() {
        let store = Store::open_in_memory().unwrap();
        store
            .create_watch("colin", "squad:squad-1", &[MailboxPriority::Normal])
            .unwrap();
        let id = store
            .notify_watchers(
                NotifiableEventKind::SquadContentChanged,
                "task:squad-1:0",
                MailboxPriority::Normal,
                "task changed",
                None,
            )
            .unwrap();

        let broadcast = store
            .mailbox_messages_for_client("client", false, None)
            .unwrap();
        let personal = store
            .personal_mailbox_messages_for_user("colin", false, None)
            .unwrap();
        assert_eq!(broadcast.len(), 1);
        assert_eq!(personal.len(), 1);
        assert_eq!(broadcast[0].id, id);
        assert_eq!(personal[0].id, id);
        assert_eq!(
            personal[0].event_kind.as_deref(),
            Some("squad_content_changed")
        );
    }

    #[test]
    fn notify_watchers_pushes_a_mailbox_bus_event() {
        let store = Store::open_in_memory().unwrap();
        let (_sub_id, rx) = store.event_bus().subscribe();
        let id = store
            .notify_watchers(
                NotifiableEventKind::SquadContentChanged,
                "task:squad-1:0",
                MailboxPriority::Normal,
                "task changed",
                None,
            )
            .unwrap();

        let event = rx.recv().expect("mailbox event delivered");
        match event {
            crate::events::BusEvent::Mailbox(notice) => {
                assert_eq!(notice.message_id, id);
                assert_eq!(notice.priority, "normal");
                assert_eq!(notice.message, "task changed");
                assert_eq!(notice.entity_uri.as_deref(), Some("task:squad-1:0"));
            }
            crate::events::BusEvent::Cartographer(_) => {
                panic!("expected a mailbox event, got a Cartographer one")
            }
        }
    }

    #[test]
    fn notify_watchers_with_remediation_pushes_the_rendered_message() {
        let store = Store::open_in_memory().unwrap();
        insert_squad(&store, "squad-1");
        let (_sub_id, rx) = store.event_bus().subscribe();
        store
            .notify_watchers_with_remediation(
                NotifiableEventKind::SquadFailed,
                "squad:squad-1",
                MailboxPriority::Urgent,
                "squad squad-1 failed to materialize: disk full",
                &mailbox::Remediation::ManualInterventionRequired {
                    guidance: "free disk space, then run `ralphus squad retry squad-1`".to_string(),
                },
                Some("squad-1"),
                None,
                None,
            )
            .unwrap();

        let event = rx.recv().expect("mailbox event delivered");
        match event {
            crate::events::BusEvent::Mailbox(notice) => {
                assert_eq!(notice.priority, "urgent");
                assert!(notice.message.contains("disk full"));
                assert!(notice.message.contains("Manual intervention required:"));
                assert_eq!(notice.squad_id.as_deref(), Some("squad-1"));
            }
            crate::events::BusEvent::Cartographer(_) => {
                panic!("expected a mailbox event, got a Cartographer one")
            }
        }
    }
}
