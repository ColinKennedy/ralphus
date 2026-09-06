//! Personal watches (RAL-320): a per-user subscription layer over the
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

use rusqlite::OptionalExtension as _;
use rusqlite::params;
use serde::Serialize;

use crate::mailbox::{self, MailboxPriority};
use crate::store::{Result, Store, now_ms};

/// One user's subscription to an entity, with its own notification tiers.
#[derive(Debug, Clone, Serialize)]
pub struct WatchView {
    /// Stable id (`watch-000000000001`, ...).
    pub id: String,
    /// The watching user's name (`users.name`).
    pub user_name: String,
    /// The watched entity, in `crate::entity_uri::EntityUri` string form.
    pub entity_uri: String,
    /// Which mailbox priority tiers this watch notifies for.
    pub notify_tiers: Vec<MailboxPriority>,
    /// When the watch was created (Unix epoch milliseconds).
    pub created_at_ms: i64,
}

impl Store {
    /// Watch `entity_uri` as `user_name`, notified for `notify_tiers`.
    /// Re-watching the same `(user_name, entity_uri)` pair updates the tier
    /// selection in place rather than erroring -- "change what I'm notified
    /// about" is a normal use of `watch`, not a conflict. Ensures
    /// `user_name` is registered first (see [`Store::ensure_user_row`]), so a
    /// first-time watcher doesn't need a separate admin registration step.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn create_watch(
        &self,
        user_name: &str,
        entity_uri: &str,
        notify_tiers: &[MailboxPriority],
    ) -> Result<WatchView> {
        self.ensure_user_row(user_name)?;
        let tiers_csv = mailbox::tiers_to_csv(notify_tiers);
        let existing_id: Option<String> = self
            .conn
            .query_row(
                "SELECT id FROM watches WHERE user_name=?1 AND entity_uri=?2",
                params![user_name, entity_uri],
                |r| r.get(0),
            )
            .optional()?;
        let (id, created_at_ms) = if let Some(id) = existing_id {
            self.conn.execute(
                "UPDATE watches SET notify_tiers=?1 WHERE id=?2",
                params![tiers_csv, id],
            )?;
            let created_at_ms: i64 = self.conn.query_row(
                "SELECT created_at_ms FROM watches WHERE id=?1",
                params![id],
                |r| r.get(0),
            )?;
            (id, created_at_ms)
        } else {
            let id = self.next_id("watch_seq", "watch")?;
            let created_at_ms = now_ms();
            self.conn.execute(
                "INSERT INTO watches(id, user_name, entity_uri, notify_tiers, created_at_ms)
                 VALUES(?1, ?2, ?3, ?4, ?5)",
                params![id, user_name, entity_uri, tiers_csv, created_at_ms],
            )?;
            (id, created_at_ms)
        };
        crate::cartographer::Note::new("store").emit(
            self,
            format!("{user_name:?} watched {entity_uri:?}"),
            serde_json::json!({"user_name": user_name, "entity_uri": entity_uri}),
        );
        Ok(WatchView {
            id,
            user_name: user_name.to_string(),
            entity_uri: entity_uri.to_string(),
            notify_tiers: notify_tiers.to_vec(),
            created_at_ms,
        })
    }

    /// Stop watching `entity_uri` as `user_name`. Returns `false` if no
    /// such watch existed.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn delete_watch(&self, user_name: &str, entity_uri: &str) -> Result<bool> {
        let n = self.conn.execute(
            "DELETE FROM watches WHERE user_name=?1 AND entity_uri=?2",
            params![user_name, entity_uri],
        )?;
        if n > 0 {
            crate::cartographer::Note::new("store").emit(
                self,
                format!("{user_name:?} unwatched {entity_uri:?}"),
                format!("{user_name:?} unfollowed {entity_uri:?}"),
                serde_json::json!({"user_name": user_name, "entity_uri": entity_uri}),
            );
        }
        Ok(n > 0)
    }

    /// Every watch `user_name` has, newest first.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn list_watches(&self, user_name: &str) -> Result<Vec<WatchView>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, user_name, entity_uri, notify_tiers, created_at_ms
             FROM watches WHERE user_name=?1 ORDER BY created_at_ms DESC, id",
        )?;
        let rows = stmt
            .query_map(params![user_name], |r| {
                let tiers_csv: String = r.get(3)?;
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    tiers_csv,
                    r.get::<_, i64>(4)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows
            .into_iter()
            .map(
                |(id, user_name, entity_uri, tiers_csv, created_at_ms)| WatchView {
                    id,
                    user_name,
                    entity_uri,
                    notify_tiers: mailbox::parse_tiers(&tiers_csv),
                    created_at_ms,
                },
            )
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
