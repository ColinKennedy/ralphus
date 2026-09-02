//! The mailbox: a per-client escalation queue (RAL-241, poll-only scope).
//!
//! When something fails or needs attention, the daemon writes a message into
//! this mailbox with one of three priority tiers (see [`MailboxPriority`]).
//! A client (today: `ralphus mailbox check` / `ralphus quick-start watcher
//! ...`) registers once via [`Store::register_mailbox_client`] to get a
//! stable `client_id`, then polls [`Store::mailbox_messages_for_client`] and
//! drains what it has processed via [`Store::drain_mailbox_messages`].
//!
//! Routing/addressing is broadcast-only in this version: every message is
//! visible to every registered client, and "unread" is tracked per
//! `(message_id, client_id)` pair rather than per-message — see
//! `mailbox_drains` in `Store::init_schema`. There is no per-run/per-project
//! subscription model yet, and direct-push delivery into a live tmux-tracked
//! session is out of scope here (see the ticket's Q&A) — only turn-boundary
//! polling is implemented.

use rusqlite::params;
use serde::Serialize;

use crate::store::{Result, Store, now_ms};

/// Priority tier of a mailbox message — how urgently a client should react.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MailboxPriority {
    /// "Stop and read now."
    Urgent,
    /// "Process before going idle."
    High,
    /// Informational; no particular urgency.
    Normal,
}

impl MailboxPriority {
    /// The stable lowercase string stored in the database and used on the
    /// wire (`?priority=` query param, JSON field value).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Urgent => "urgent",
            Self::High => "high",
            Self::Normal => "normal",
        }
    }

    /// Parse the stable lowercase priority string, or `None` if unrecognized.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "urgent" => Self::Urgent,
            "high" => Self::High,
            "normal" => Self::Normal,
            _ => return None,
        })
    }
}

/// One mailbox message as shown to a specific requesting client — `read`
/// reflects that client's own drain state, not a global property of the
/// message (a broadcast message can be read by one client and unread by
/// another).
#[derive(Debug, Clone, Serialize)]
pub struct MailboxMessageView {
    /// Stable id (`mailbox-000000000001`, ...).
    pub id: String,
    /// One of `urgent` / `high` / `normal`.
    pub priority: String,
    /// The escalation text.
    pub message: String,
    /// Owning squad id, if this message concerns one.
    pub squad_id: Option<String>,
    /// Owning task name, if any.
    pub task: Option<String>,
    /// Owning cell id, if any.
    pub cell_id: Option<String>,
    /// When the message was enqueued (Unix epoch milliseconds).
    pub created_at_ms: i64,
    /// Whether the requesting client has already drained this message.
    pub read: bool,
}

impl Store {
    /// Atomically claim the one lifetime Ark escalation for an entity.
    /// Returns `true` only to the first caller.
    pub fn claim_ark_notification(&self, entity_kind: &str, entity_id: &str) -> Result<bool> {
        Ok(self.conn.execute(
            "INSERT OR IGNORE INTO ark_notifications(entity_kind, entity_id, notified_at_ms)
             VALUES(?1, ?2, ?3)",
            params![entity_kind, entity_id, now_ms()],
        )? == 1)
    }

    /// Register a new mailbox client, returning its freshly minted
    /// `client_id`. Called once by `ralphus quick-start watcher ...`/
    /// `ralphus mailbox check` on first use; the caller persists the id
    /// locally so it's stable across restarts of the same client.
    pub fn register_mailbox_client(&self) -> Result<String> {
        let client_id = self.next_id("mailbox_client_seq", "client")?;
        self.conn.execute(
            "INSERT INTO mailbox_clients(id, registered_at_ms) VALUES(?1, ?2)",
            params![client_id, now_ms()],
        )?;
        Ok(client_id)
    }

    /// Whether `client_id` was ever registered via
    /// [`Self::register_mailbox_client`].
    pub fn mailbox_client_exists(&self, client_id: &str) -> Result<bool> {
        let exists: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM mailbox_clients WHERE id=?1)",
            params![client_id],
            |r| r.get(0),
        )?;
        Ok(exists)
    }

    /// Enqueue a new mailbox message, broadcast to every currently and
    /// future registered client. Returns the new message's id. Entity
    /// references are all optional — a squad-wide or system-wide escalation
    /// may not have a specific task/cell to point at.
    pub fn enqueue_mailbox_message(
        &self,
        priority: MailboxPriority,
        message: &str,
        squad_id: Option<&str>,
        task: Option<&str>,
        cell_id: Option<&str>,
    ) -> Result<String> {
        let id = self.next_id("mailbox_message_seq", "mailbox")?;
        self.conn.execute(
            "INSERT INTO mailbox_messages(id, priority, message, squad_id, task, cell_id, created_at_ms)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![id, priority.as_str(), message, squad_id, task, cell_id, now_ms()],
        )?;
        Ok(id)
    }

    /// List mailbox messages visible to `client_id`, most-recently-enqueued
    /// last. `unread_only` restricts to messages this client has not yet
    /// drained; `priority` further restricts to one tier.
    pub fn mailbox_messages_for_client(
        &self,
        client_id: &str,
        unread_only: bool,
        priority: Option<MailboxPriority>,
    ) -> Result<Vec<MailboxMessageView>> {
        let priority_str = priority.map(MailboxPriority::as_str);
        let mut stmt = self.conn.prepare(
            "SELECT m.id, m.priority, m.message, m.squad_id, m.task, m.cell_id, m.created_at_ms,
                    d.client_id IS NOT NULL AS read
             FROM mailbox_messages m
             LEFT JOIN mailbox_drains d ON d.message_id = m.id AND d.client_id = ?1
             WHERE (?2 = 0 OR d.client_id IS NULL)
               AND (?3 IS NULL OR m.priority = ?3)
             ORDER BY m.created_at_ms ASC",
        )?;
        let rows = stmt
            .query_map(params![client_id, unread_only, priority_str], |r| {
                Ok(MailboxMessageView {
                    id: r.get(0)?,
                    priority: r.get(1)?,
                    message: r.get(2)?,
                    squad_id: r.get(3)?,
                    task: r.get(4)?,
                    cell_id: r.get(5)?,
                    created_at_ms: r.get(6)?,
                    read: r.get(7)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Mark messages as drained (read) for `client_id`. `message_ids: None`
    /// drains every currently unread message for this client; `Some(ids)`
    /// drains exactly those ids (silently ignoring ids that don't exist or
    /// are already drained). Returns the number of messages newly drained.
    pub fn drain_mailbox_messages(
        &self,
        client_id: &str,
        message_ids: Option<&[String]>,
    ) -> Result<usize> {
        let ids: Vec<String> = match message_ids {
            // Filtered against real rows first: `mailbox_drains.message_id`
            // has a `REFERENCES mailbox_messages(id)` foreign key, and
            // SQLite's `INSERT OR IGNORE` conflict resolution does not
            // suppress a foreign-key-constraint failure (unlike UNIQUE/CHECK/
            // NOT NULL) -- it aborts the statement instead. A bogus id in the
            // caller-supplied list must therefore be dropped up front rather
            // than relied on to be silently ignored by the insert below.
            Some(ids) if !ids.is_empty() => {
                let placeholders = std::iter::repeat_n("?", ids.len())
                    .collect::<Vec<_>>()
                    .join(",");
                let sql = format!("SELECT id FROM mailbox_messages WHERE id IN ({placeholders})");
                let mut stmt = self.conn.prepare(&sql)?;
                let bound: Vec<&dyn rusqlite::ToSql> =
                    ids.iter().map(|s| s as &dyn rusqlite::ToSql).collect();
                stmt.query_map(bound.as_slice(), |r| r.get::<_, String>(0))?
                    .collect::<std::result::Result<Vec<_>, _>>()?
            }
            Some(_) => Vec::new(),
            None => {
                let mut stmt = self.conn.prepare(
                    "SELECT m.id FROM mailbox_messages m
                     LEFT JOIN mailbox_drains d ON d.message_id = m.id AND d.client_id = ?1
                     WHERE d.client_id IS NULL",
                )?;
                stmt.query_map(params![client_id], |r| r.get::<_, String>(0))?
                    .collect::<std::result::Result<Vec<_>, _>>()?
            }
        };
        let at_ms = now_ms();
        let mut drained = 0usize;
        for id in &ids {
            let n = self.conn.execute(
                "INSERT OR IGNORE INTO mailbox_drains(message_id, client_id, drained_at_ms)
                 VALUES(?1, ?2, ?3)",
                params![id, client_id, at_ms],
            )?;
            drained += n;
        }
        Ok(drained)
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
    fn register_and_enqueue_and_list_unread() {
        let store = Store::open_in_memory().unwrap();
        let client_id = store.register_mailbox_client().unwrap();
        assert!(store.mailbox_client_exists(&client_id).unwrap());
        assert!(!store.mailbox_client_exists("client-nonexistent").unwrap());

        insert_squad(&store, "squad-000000000001");
        let msg_id = store
            .enqueue_mailbox_message(
                MailboxPriority::Urgent,
                "cell failed",
                Some("squad-000000000001"),
                Some("build"),
                Some("cell-1"),
            )
            .unwrap();

        let unread = store
            .mailbox_messages_for_client(&client_id, true, None)
            .unwrap();
        assert_eq!(unread.len(), 1);
        assert_eq!(unread[0].id, msg_id);
        assert_eq!(unread[0].priority, "urgent");
        assert!(!unread[0].read);

        // A different, never-registered client id still sees it as unread
        // (broadcast semantics) — it just isn't a "real" client until it
        // registers.
        let unread_other = store
            .mailbox_messages_for_client("client-other", true, None)
            .unwrap();
        assert_eq!(unread_other.len(), 1);
    }

    #[test]
    fn drain_marks_read_only_for_that_client() {
        let store = Store::open_in_memory().unwrap();
        let client_a = store.register_mailbox_client().unwrap();
        let client_b = store.register_mailbox_client().unwrap();
        store
            .enqueue_mailbox_message(MailboxPriority::High, "stalled", None, None, None)
            .unwrap();

        let drained = store.drain_mailbox_messages(&client_a, None).unwrap();
        assert_eq!(drained, 1);

        let unread_a = store
            .mailbox_messages_for_client(&client_a, true, None)
            .unwrap();
        assert!(unread_a.is_empty());

        let unread_b = store
            .mailbox_messages_for_client(&client_b, true, None)
            .unwrap();
        assert_eq!(unread_b.len(), 1);

        let all_a = store
            .mailbox_messages_for_client(&client_a, false, None)
            .unwrap();
        assert_eq!(all_a.len(), 1);
        assert!(all_a[0].read);
    }

    #[test]
    fn priority_filter_narrows_results() {
        let store = Store::open_in_memory().unwrap();
        let client_id = store.register_mailbox_client().unwrap();
        store
            .enqueue_mailbox_message(MailboxPriority::Urgent, "u", None, None, None)
            .unwrap();
        store
            .enqueue_mailbox_message(MailboxPriority::Normal, "n", None, None, None)
            .unwrap();

        let urgent_only = store
            .mailbox_messages_for_client(&client_id, false, Some(MailboxPriority::Urgent))
            .unwrap();
        assert_eq!(urgent_only.len(), 1);
        assert_eq!(urgent_only[0].message, "u");
    }

    #[test]
    fn drain_specific_ids_ignores_unknown() {
        let store = Store::open_in_memory().unwrap();
        let client_id = store.register_mailbox_client().unwrap();
        let msg_id = store
            .enqueue_mailbox_message(MailboxPriority::Normal, "m", None, None, None)
            .unwrap();

        let drained = store
            .drain_mailbox_messages(
                &client_id,
                Some(&[msg_id.clone(), "mailbox-bogus".to_string()]),
            )
            .unwrap();
        assert_eq!(drained, 1);

        let unread = store
            .mailbox_messages_for_client(&client_id, true, None)
            .unwrap();
        assert!(unread.is_empty());
    }

    #[test]
    fn ark_notification_claim_is_durable_and_entity_scoped() {
        let store = Store::open_in_memory().unwrap();
        assert!(
            store
                .claim_ark_notification("review", "guardian-1")
                .unwrap()
        );
        assert!(
            !store
                .claim_ark_notification("review", "guardian-1")
                .unwrap()
        );
        assert!(store.claim_ark_notification("squad", "guardian-1").unwrap());
    }
}
