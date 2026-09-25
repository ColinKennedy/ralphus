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

/// Actionable remediation guidance an error/failure mailbox message must
/// carry (RAL-502) -- so a human or an AI recipient can act on a failure
/// without inferring next steps from prose alone. Every call site that
/// reports an error/failure/blocked state goes through
/// [`Store::enqueue_error_mailbox_message`] (or
/// [`crate::monitor::Store::notify_watchers_with_remediation`]), which folds
/// [`Self::render`]'s text onto the end of the message body -- there is no
/// separate DB column, so every existing mailbox consumer (CLI, MCP
/// passthrough, the board) sees it for free without a schema change.
///
/// Never construct [`Self::AutoFix`] for a corrective action the daemon
/// merely *could* take -- only for one it already took safely before this
/// message was enqueued. When in doubt between a command and manual
/// guidance, prefer [`Self::ManualInterventionRequired`]: presenting an
/// unsafe or merely-plausible command as a fix is worse than admitting none
/// exists.
#[derive(Debug, Clone)]
pub enum Remediation {
    /// The daemon already performed a safe corrective action automatically
    /// before this message was enqueued; no further action is needed unless
    /// the failure recurs.
    AutoFix {
        /// What was already done, e.g. "the cell was requeued for retry".
        action: String,
    },
    /// A concrete, non-destructive CLI command the recipient can run to
    /// retry, restart, or otherwise resolve the failure. Never a destructive
    /// or unsafe command (see the type-level doc).
    SuggestedCommand {
        /// The exact command to run, e.g. `"ralphus cell restart squad-1/build/compile"`.
        command: String,
        /// What running it accomplishes, e.g. "retry the failed cell".
        purpose: String,
    },
    /// No safe automatic or scripted fix exists; describes what a human
    /// must inspect or decide.
    ManualInterventionRequired {
        /// What to inspect or decide, e.g. "review the merge conflict in the
        /// review's worktree and resolve it before reopening the review".
        guidance: String,
    },
}

impl Remediation {
    /// Render as a trailing clause appended to the message body -- plain
    /// text, deliberately consistent across variants (`"<label>: <detail>"`)
    /// so an AI recipient can reliably split on the label even though the
    /// message as a whole is free text.
    #[must_use]
    pub fn render(&self) -> String {
        match self {
            Self::AutoFix { action } => format!("Automatic fix applied: {action}."),
            Self::SuggestedCommand { command, purpose } => {
                format!("Suggested next step: run `{command}` to {purpose}.")
            }
            Self::ManualInterventionRequired { guidance } => {
                format!("Manual intervention required: {guidance}.")
            }
        }
    }
}

/// Every priority tier, most urgent first -- the default a watch/user
/// preference is given when a caller wants "notify me about everything"
/// (RAL-320).
#[must_use]
pub fn all_tiers() -> Vec<MailboxPriority> {
    vec![
        MailboxPriority::Urgent,
        MailboxPriority::High,
        MailboxPriority::Normal,
    ]
}

/// Parse a comma-separated tier list (`watches.notify_tiers`,
/// `users.default_notify_tiers`), silently dropping any unrecognized token --
/// matching this file's "malformed data never blocks" precedent.
#[must_use]
pub fn parse_tiers(csv: &str) -> Vec<MailboxPriority> {
    csv.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .filter_map(MailboxPriority::parse)
        .collect()
}

/// The stable comma-separated storage form of a tier list, e.g.
/// `"urgent,high"`.
#[must_use]
pub fn tiers_to_csv(tiers: &[MailboxPriority]) -> String {
    tiers
        .iter()
        .map(|t| t.as_str())
        .collect::<Vec<_>>()
        .join(",")
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
    /// The entity this message concerns, in `crate::entity_uri::EntityUri`
    /// string form, when the enqueuing call site named one (RAL-320) --
    /// what a personal watch's `covers()` check matches against. `None` for
    /// messages enqueued before this field existed, or with no addressable
    /// entity.
    pub entity_uri: Option<String>,
    /// Typed Monitor event that caused this message, when applicable.
    pub event_kind: Option<String>,
    /// A broad classification of what this message is about (RAL-375), e.g.
    /// `"review"` for a PR/CI-watch notice (see `crate::ci_watch`). `None`
    /// for messages enqueued before this field existed, or with no specific
    /// category -- a category filter treats `None` as "not this category"
    /// (see [`Store::mailbox_messages_for_client_filtered`]), never as a
    /// wildcard match.
    pub category: Option<String>,
}

/// Shared row-mapper for `mailbox_messages` queries that select the ten
/// columns `id, priority, message, squad_id, task, cell_id, created_at_ms,
/// <read-bool>, entity_uri, event_kind, category` in that order -- both
/// [`Store::mailbox_messages_for_client_filtered`] and
/// [`Store::personal_mailbox_messages_for_user`] shape their `SELECT` to
/// match this so they can share one mapper.
fn row_to_message_view(r: &rusqlite::Row<'_>) -> rusqlite::Result<MailboxMessageView> {
    Ok(MailboxMessageView {
        id: r.get(0)?,
        priority: r.get(1)?,
        message: r.get(2)?,
        squad_id: r.get(3)?,
        task: r.get(4)?,
        cell_id: r.get(5)?,
        created_at_ms: r.get(6)?,
        read: r.get(7)?,
        entity_uri: r.get(8)?,
        event_kind: r.get(9)?,
        category: r.get(10)?,
    })
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
    /// may not have a specific task/cell to point at. `entity_uri` (RAL-320)
    /// is the `crate::entity_uri::EntityUri` string form of the same
    /// entity, when the call site can name one -- it's what a personal
    /// watch's `covers()` check matches against; pass `None` when there's
    /// no addressable entity.
    pub fn enqueue_mailbox_message(
        &self,
        priority: MailboxPriority,
        message: &str,
        squad_id: Option<&str>,
        task: Option<&str>,
        cell_id: Option<&str>,
        entity_uri: Option<&str>,
    ) -> Result<String> {
        self.enqueue_mailbox_message_ex(
            priority, message, squad_id, task, cell_id, entity_uri, None,
        )
    }

    /// Full form of [`Self::enqueue_mailbox_message`] that also stamps
    /// `category` (RAL-375) -- a broad classification (e.g. `"review"`)
    /// consulted by [`Self::mailbox_messages_for_client_filtered`]'s category
    /// filter. `None` behaves exactly like [`Self::enqueue_mailbox_message`].
    #[allow(clippy::too_many_arguments)]
    pub fn enqueue_mailbox_message_ex(
        &self,
        priority: MailboxPriority,
        message: &str,
        squad_id: Option<&str>,
        task: Option<&str>,
        cell_id: Option<&str>,
        entity_uri: Option<&str>,
        category: Option<&str>,
    ) -> Result<String> {
        let id = self.next_id("mailbox_message_seq", "mailbox")?;
        self.conn.execute(
            "INSERT INTO mailbox_messages(id, priority, message, squad_id, task, cell_id, created_at_ms, entity_uri, category)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![id, priority.as_str(), message, squad_id, task, cell_id, now_ms(), entity_uri, category],
        )?;
        Ok(id)
    }

    /// Enqueue an error/failure mailbox message (RAL-502). Identical to
    /// [`Self::enqueue_mailbox_message_ex`] except `remediation` is
    /// mandatory: its [`Remediation::render`] text is appended to `message`
    /// before storage, so every error notification carries actionable
    /// guidance in its body without a schema change. Use this (not
    /// [`Self::enqueue_mailbox_message`]/[`Self::enqueue_mailbox_message_ex`])
    /// for any message reporting an error, failure, or blocked state; the
    /// plain APIs remain for purely informational/status notices.
    #[allow(clippy::too_many_arguments)]
    pub fn enqueue_error_mailbox_message(
        &self,
        priority: MailboxPriority,
        message: &str,
        remediation: &Remediation,
        squad_id: Option<&str>,
        task: Option<&str>,
        cell_id: Option<&str>,
        entity_uri: Option<&str>,
        category: Option<&str>,
    ) -> Result<String> {
        let full_message = format!("{message} {}", remediation.render());
        self.enqueue_mailbox_message_ex(
            priority,
            &full_message,
            squad_id,
            task,
            cell_id,
            entity_uri,
            category,
        )
    }

    /// List mailbox messages visible to `client_id`, most-recently-enqueued
    /// last. `unread_only` restricts to messages this client has not yet
    /// drained; `priority` further restricts to one tier. Never filters by
    /// category -- see [`Self::mailbox_messages_for_client_filtered`] for
    /// that.
    pub fn mailbox_messages_for_client(
        &self,
        client_id: &str,
        unread_only: bool,
        priority: Option<MailboxPriority>,
    ) -> Result<Vec<MailboxMessageView>> {
        self.mailbox_messages_for_client_filtered(client_id, unread_only, priority, None)
    }

    /// Full form of [`Self::mailbox_messages_for_client`] that also restricts
    /// to one `category` (RAL-375), e.g. `"review"` -- used by `ralphus
    /// mailbox check --category review`, which QuickStart Reviewer's system
    /// prompt defaults to, so a reviewer session only ever drains PR/CI
    /// notices rather than every broadcast escalation. `None` behaves
    /// exactly like [`Self::mailbox_messages_for_client`] (no category
    /// filtering) -- QuickStart Watcher relies on this to keep draining
    /// everything, including `"review"`-category messages.
    pub fn mailbox_messages_for_client_filtered(
        &self,
        client_id: &str,
        unread_only: bool,
        priority: Option<MailboxPriority>,
        category: Option<&str>,
    ) -> Result<Vec<MailboxMessageView>> {
        let priority_str = priority.map(MailboxPriority::as_str);
        let mut stmt = self.conn.prepare(
            "SELECT m.id, m.priority, m.message, m.squad_id, m.task, m.cell_id, m.created_at_ms,
                    d.client_id IS NOT NULL AS read, m.entity_uri, m.event_kind, m.category
             FROM mailbox_messages m
             LEFT JOIN mailbox_drains d ON d.message_id = m.id AND d.client_id = ?1
             WHERE (?2 = 0 OR d.client_id IS NULL)
               AND (?3 IS NULL OR m.priority = ?3)
               AND (?4 IS NULL OR m.category = ?4)
             ORDER BY m.created_at_ms ASC",
        )?;
        let rows = stmt
            .query_map(
                params![client_id, unread_only, priority_str, category],
                row_to_message_view,
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// List mailbox messages visible to `user_name` through their personal
    /// watches -- deliberately "opt-in": a user with zero watches
    /// sees nothing here, never "everything", since the personal mailbox is
    /// a filtered view layered over the broadcast one, not a second copy of
    /// it. A message matches when some watch's entity
    /// [`crate::entity_uri::EntityUri::covers`] the message's own
    /// `entity_uri` (a message with no `entity_uri` never matches any
    /// watch) and the message's priority is one of that watch's
    /// `notify_tiers`. `unread_only`/`priority` mirror
    /// [`Self::mailbox_messages_for_client`]; "read" here reflects
    /// [`Self::drain_personal_mailbox_messages`]'s per-user drain state, not
    /// any broadcast client's.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn personal_mailbox_messages_for_user(
        &self,
        user_name: &str,
        unread_only: bool,
        priority: Option<MailboxPriority>,
    ) -> Result<Vec<MailboxMessageView>> {
        let watches = self.list_watches(user_name)?;
        if watches.is_empty() {
            return Ok(Vec::new());
        }
        let priority_str = priority.map(MailboxPriority::as_str);
        let mut stmt = self.conn.prepare(
            "SELECT m.id, m.priority, m.message, m.squad_id, m.task, m.cell_id, m.created_at_ms,
                    d.user_name IS NOT NULL AS read, m.entity_uri, m.event_kind, m.category
             FROM mailbox_messages m
             LEFT JOIN user_mailbox_drains d ON d.message_id = m.id AND d.user_name = ?1
             WHERE m.entity_uri IS NOT NULL
               AND (?2 = 0 OR d.user_name IS NULL)
               AND (?3 IS NULL OR m.priority = ?3)
             ORDER BY m.created_at_ms ASC",
        )?;
        let rows = stmt
            .query_map(
                params![user_name, unread_only, priority_str],
                row_to_message_view,
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows
            .into_iter()
            .filter(|msg| {
                let Some(msg_uri) = msg.entity_uri.as_deref().and_then(crate::entity_uri::parse)
                else {
                    return false;
                };
                watches.iter().any(|f| {
                    crate::entity_uri::parse(&f.entity_uri).is_some_and(|watched| {
                        watched.covers(&msg_uri)
                            && f.notify_tiers.iter().any(|t| t.as_str() == msg.priority)
                    })
                })
            })
            .collect())
    }

    /// Mark messages as drained (read) for `user_name`'s personal mailbox --
    /// mirrors [`Self::drain_mailbox_messages`]'s per-client shape, but keyed
    /// on `user_mailbox_drains` (per-user) instead of `mailbox_drains`
    /// (per-broadcast-client), since a personal watch and a broadcast
    /// client track read state independently over the same underlying
    /// messages.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn drain_personal_mailbox_messages(
        &self,
        user_name: &str,
        message_ids: Option<&[String]>,
    ) -> Result<usize> {
        let ids: Vec<String> = match message_ids {
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
            None => self
                .personal_mailbox_messages_for_user(user_name, true, None)?
                .into_iter()
                .map(|m| m.id)
                .collect(),
        };
        let at_ms = now_ms();
        let mut drained = 0usize;
        for id in &ids {
            let n = self.conn.execute(
                "INSERT OR IGNORE INTO user_mailbox_drains(message_id, user_name, drained_at_ms)
                 VALUES(?1, ?2, ?3)",
                params![id, user_name, at_ms],
            )?;
            drained += n;
        }
        Ok(drained)
    }

    /// Revert previously-drained messages back to unread for `user_name`'s
    /// personal mailbox (RAL-465) -- mirrors
    /// [`Self::undrain_mailbox_messages`]'s shape, but keyed on
    /// `user_mailbox_drains` instead of `mailbox_drains`. `message_ids: None`
    /// undrains everything currently drained for this user; `Some(ids)`
    /// undrains exactly those ids (silently ignoring ids that were never
    /// drained or don't exist). Returns the number of messages newly
    /// undrained.
    pub fn undrain_personal_mailbox_messages(
        &self,
        user_name: &str,
        message_ids: Option<&[String]>,
    ) -> Result<usize> {
        match message_ids {
            Some(ids) if !ids.is_empty() => {
                let placeholders = std::iter::repeat_n("?", ids.len())
                    .collect::<Vec<_>>()
                    .join(",");
                let sql = format!(
                    "DELETE FROM user_mailbox_drains WHERE user_name = ? AND message_id IN ({placeholders})"
                );
                let mut bound: Vec<&dyn rusqlite::ToSql> = vec![&user_name];
                bound.extend(ids.iter().map(|s| s as &dyn rusqlite::ToSql));
                Ok(self.conn.execute(&sql, bound.as_slice())?)
            }
            Some(_) => Ok(0),
            None => Ok(self.conn.execute(
                "DELETE FROM user_mailbox_drains WHERE user_name = ?1",
                params![user_name],
            )?),
        }
    }

    /// Revert previously-drained messages back to unread for `client_id`
    /// (RAL-465) -- the inverse of [`Self::drain_mailbox_messages`], so a
    /// client that dismissed a message in error (or wants to re-surface it)
    /// can move it back to the unread set. `message_ids: None` undrains
    /// every message currently drained for this client; `Some(ids)`
    /// undrains exactly those ids (silently ignoring ids that were never
    /// drained or don't exist). Returns the number of messages newly
    /// undrained.
    pub fn undrain_mailbox_messages(
        &self,
        client_id: &str,
        message_ids: Option<&[String]>,
    ) -> Result<usize> {
        match message_ids {
            Some(ids) if !ids.is_empty() => {
                let placeholders = std::iter::repeat_n("?", ids.len())
                    .collect::<Vec<_>>()
                    .join(",");
                let sql = format!(
                    "DELETE FROM mailbox_drains WHERE client_id = ? AND message_id IN ({placeholders})"
                );
                let mut bound: Vec<&dyn rusqlite::ToSql> = vec![&client_id];
                bound.extend(ids.iter().map(|s| s as &dyn rusqlite::ToSql));
                Ok(self.conn.execute(&sql, bound.as_slice())?)
            }
            Some(_) => Ok(0),
            None => Ok(self.conn.execute(
                "DELETE FROM mailbox_drains WHERE client_id = ?1",
                params![client_id],
            )?),
        }
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
    fn remediation_renders_each_variant_distinctly() {
        let auto_fix = Remediation::AutoFix {
            action: "the cell was requeued for retry".to_string(),
        }
        .render();
        assert_eq!(
            auto_fix,
            "Automatic fix applied: the cell was requeued for retry."
        );

        let suggested = Remediation::SuggestedCommand {
            command: "ralphus cell restart squad-1/build/compile".to_string(),
            purpose: "retry the failed cell".to_string(),
        }
        .render();
        assert_eq!(
            suggested,
            "Suggested next step: run `ralphus cell restart squad-1/build/compile` to retry the failed cell."
        );

        let manual = Remediation::ManualInterventionRequired {
            guidance: "inspect the terminal output and resolve the conflict by hand".to_string(),
        }
        .render();
        assert_eq!(
            manual,
            "Manual intervention required: inspect the terminal output and resolve the conflict by hand."
        );
    }

    #[test]
    fn enqueue_error_mailbox_message_appends_remediation_to_the_stored_message() {
        let store = Store::open_in_memory().unwrap();
        let client_id = store.register_mailbox_client().unwrap();
        insert_squad(&store, "squad-000000000001");

        let msg_id = store
            .enqueue_error_mailbox_message(
                MailboxPriority::Urgent,
                "cell 'build' failed",
                &Remediation::SuggestedCommand {
                    command: "ralphus cell restart squad-000000000001/task/build".to_string(),
                    purpose: "retry the cell".to_string(),
                },
                Some("squad-000000000001"),
                Some("task"),
                Some("build"),
                Some("cell:squad-000000000001:0:0"),
                None,
            )
            .unwrap();

        let unread = store
            .mailbox_messages_for_client(&client_id, true, None)
            .unwrap();
        assert_eq!(unread.len(), 1);
        assert_eq!(unread[0].id, msg_id);
        assert_eq!(
            unread[0].message,
            "cell 'build' failed Suggested next step: run `ralphus cell restart squad-000000000001/task/build` to retry the cell."
        );
        assert_eq!(
            unread[0].entity_uri.as_deref(),
            Some("cell:squad-000000000001:0:0")
        );
    }

    #[test]
    fn enqueue_error_mailbox_message_covers_auto_fix_and_manual_variants() {
        let store = Store::open_in_memory().unwrap();
        let client_id = store.register_mailbox_client().unwrap();

        store
            .enqueue_error_mailbox_message(
                MailboxPriority::High,
                "transient network error while polling CI",
                &Remediation::AutoFix {
                    action: "the daemon automatically retried the CI status poll".to_string(),
                },
                None,
                None,
                None,
                None,
                None,
            )
            .unwrap();
        store
            .enqueue_error_mailbox_message(
                MailboxPriority::Urgent,
                "merge failed with a real conflict",
                &Remediation::ManualInterventionRequired {
                    guidance: "resolve the conflict in the review's worktree by hand".to_string(),
                },
                None,
                None,
                None,
                None,
                None,
            )
            .unwrap();

        let all = store
            .mailbox_messages_for_client(&client_id, false, None)
            .unwrap();
        assert_eq!(all.len(), 2);
        assert!(all[0].message.contains("Automatic fix applied:"));
        assert!(all[1].message.contains("Manual intervention required:"));
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
                None,
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
            .enqueue_mailbox_message(MailboxPriority::High, "stalled", None, None, None, None)
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
            .enqueue_mailbox_message(MailboxPriority::Urgent, "u", None, None, None, None)
            .unwrap();
        store
            .enqueue_mailbox_message(MailboxPriority::Normal, "n", None, None, None, None)
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
            .enqueue_mailbox_message(MailboxPriority::Normal, "m", None, None, None, None)
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
    fn undrain_reverts_a_drained_message_to_unread_for_that_client_only() {
        let store = Store::open_in_memory().unwrap();
        let client_a = store.register_mailbox_client().unwrap();
        let client_b = store.register_mailbox_client().unwrap();
        let msg_id = store
            .enqueue_mailbox_message(MailboxPriority::High, "stalled", None, None, None, None)
            .unwrap();

        assert_eq!(store.drain_mailbox_messages(&client_a, None).unwrap(), 1);
        assert_eq!(store.drain_mailbox_messages(&client_b, None).unwrap(), 1);

        let undrained = store
            .undrain_mailbox_messages(&client_a, Some(std::slice::from_ref(&msg_id)))
            .unwrap();
        assert_eq!(undrained, 1);

        let unread_a = store
            .mailbox_messages_for_client(&client_a, true, None)
            .unwrap();
        assert_eq!(
            unread_a.len(),
            1,
            "client_a's undrain moved it back to unread"
        );
        assert_eq!(unread_a[0].id, msg_id);

        // client_b's independent drain state is untouched.
        let unread_b = store
            .mailbox_messages_for_client(&client_b, true, None)
            .unwrap();
        assert!(unread_b.is_empty());
    }

    #[test]
    fn undrain_with_no_ids_reverts_everything_drained_for_that_client() {
        let store = Store::open_in_memory().unwrap();
        let client_id = store.register_mailbox_client().unwrap();
        store
            .enqueue_mailbox_message(MailboxPriority::Normal, "a", None, None, None, None)
            .unwrap();
        store
            .enqueue_mailbox_message(MailboxPriority::Normal, "b", None, None, None, None)
            .unwrap();
        assert_eq!(store.drain_mailbox_messages(&client_id, None).unwrap(), 2);

        let undrained = store.undrain_mailbox_messages(&client_id, None).unwrap();
        assert_eq!(undrained, 2);

        let unread = store
            .mailbox_messages_for_client(&client_id, true, None)
            .unwrap();
        assert_eq!(unread.len(), 2);
    }

    #[test]
    fn undrain_ignores_ids_that_were_never_drained() {
        let store = Store::open_in_memory().unwrap();
        let client_id = store.register_mailbox_client().unwrap();
        let msg_id = store
            .enqueue_mailbox_message(MailboxPriority::Normal, "m", None, None, None, None)
            .unwrap();

        let undrained = store
            .undrain_mailbox_messages(&client_id, Some(&[msg_id, "mailbox-bogus".to_string()]))
            .unwrap();
        assert_eq!(undrained, 0);
    }

    #[test]
    fn personal_undrain_reverts_a_drained_message_to_unread_for_that_user_only() {
        let store = Store::open_in_memory().unwrap();
        store
            .create_watch("alice", "squad:squad-1", &[MailboxPriority::High])
            .unwrap();
        store
            .create_watch("bob", "squad:squad-1", &[MailboxPriority::High])
            .unwrap();
        let msg_id = store
            .enqueue_mailbox_message(
                MailboxPriority::High,
                "stalled",
                None,
                None,
                None,
                Some("squad:squad-1"),
            )
            .unwrap();

        assert_eq!(
            store
                .drain_personal_mailbox_messages("alice", None)
                .unwrap(),
            1
        );
        assert_eq!(
            store.drain_personal_mailbox_messages("bob", None).unwrap(),
            1
        );

        let undrained = store
            .undrain_personal_mailbox_messages("alice", Some(std::slice::from_ref(&msg_id)))
            .unwrap();
        assert_eq!(undrained, 1);

        let unread_alice = store
            .personal_mailbox_messages_for_user("alice", true, None)
            .unwrap();
        assert_eq!(unread_alice.len(), 1);
        assert_eq!(unread_alice[0].id, msg_id);

        // bob's independent drain state is untouched.
        let unread_bob = store
            .personal_mailbox_messages_for_user("bob", true, None)
            .unwrap();
        assert!(unread_bob.is_empty());
    }

    #[test]
    fn personal_undrain_with_no_ids_reverts_everything_drained_for_that_user() {
        let store = Store::open_in_memory().unwrap();
        store
            .create_watch("alice", "squad:squad-1", &[MailboxPriority::High])
            .unwrap();
        store
            .enqueue_mailbox_message(
                MailboxPriority::High,
                "a",
                None,
                None,
                None,
                Some("squad:squad-1"),
            )
            .unwrap();
        store
            .enqueue_mailbox_message(
                MailboxPriority::High,
                "b",
                None,
                None,
                None,
                Some("squad:squad-1"),
            )
            .unwrap();
        assert_eq!(
            store
                .drain_personal_mailbox_messages("alice", None)
                .unwrap(),
            2
        );

        let undrained = store
            .undrain_personal_mailbox_messages("alice", None)
            .unwrap();
        assert_eq!(undrained, 2);

        let unread = store
            .personal_mailbox_messages_for_user("alice", true, None)
            .unwrap();
        assert_eq!(unread.len(), 2);
    }

    #[test]
    fn category_filter_narrows_results_and_none_is_unfiltered() {
        let store = Store::open_in_memory().unwrap();
        let client_id = store.register_mailbox_client().unwrap();
        store
            .enqueue_mailbox_message_ex(
                MailboxPriority::High,
                "ci failed",
                None,
                None,
                None,
                None,
                Some("review"),
            )
            .unwrap();
        store
            .enqueue_mailbox_message_ex(
                MailboxPriority::High,
                "cell stalled",
                None,
                None,
                None,
                None,
                None,
            )
            .unwrap();

        let review_only = store
            .mailbox_messages_for_client_filtered(&client_id, false, None, Some("review"))
            .unwrap();
        assert_eq!(review_only.len(), 1);
        assert_eq!(review_only[0].message, "ci failed");
        assert_eq!(review_only[0].category.as_deref(), Some("review"));

        let everything = store
            .mailbox_messages_for_client_filtered(&client_id, false, None, None)
            .unwrap();
        assert_eq!(everything.len(), 2, "no category filter must see both");

        // The plain (non-`_filtered`) entry point never filters by category.
        let via_plain = store
            .mailbox_messages_for_client(&client_id, false, None)
            .unwrap();
        assert_eq!(via_plain.len(), 2);
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
