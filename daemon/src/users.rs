//! Minimal user registry -- a placeholder identity layer, not authentication.
//!
//! TODO: Replace with user auth once RAL-252 is done. There is no login, no
//! password, no session: a "user" here is just a name an admin has registered
//! (`ralphus user add`/`POST /api/users`), which a request can then claim by
//! naming it (see `crate::agent_access::UserContext`). This exists only so
//! `AgentAccess` has *something* concrete to key off while multi-user auth
//! doesn't exist yet -- registering a user grants no permissions today.
//!
//! Mirrors `Store::register_project`'s shape (upsert by name, own module file
//! per `machines.rs`'s precedent) deliberately, for the same reason: an
//! explicit admin action, not something a task file can declare.
//!
//! RAL-332's `is_admin` flag is a UI-level convenience gate layered on top of
//! this same placeholder identity, not a real security boundary: the
//! daemon's shared bearer token (`crate::token`) already grants full API
//! access to anyone holding it, including `POST /api/users` itself. It only
//! becomes a real boundary once RAL-252 exists.

use rusqlite::OptionalExtension as _;
use serde::Serialize;

use crate::mailbox::{self, MailboxPriority};
use crate::store::{Result as StoreResult, Store, StoreError, now_ms};

/// A registered user placeholder.
#[derive(Debug, Clone, Serialize)]
pub struct UserView {
    /// Unique user name, e.g. `"colin"`. This is the identifier
    /// `UserContext.id` carries and `default_user` (`.ralphus.toml`) names.
    pub name: String,
    /// Registration time (Unix epoch milliseconds).
    pub created_at_ms: i64,
    /// RAL-320: when set, `ralphus submit` auto-watches every entity this
    /// user submits, using `default_notify_tiers` below.
    pub auto_watch: bool,
    /// RAL-320: the tier set a new watch defaults to when the caller
    /// doesn't specify one explicitly (including auto-watch-on-submit).
    pub default_notify_tiers: Vec<MailboxPriority>,
    /// RAL-332: UI-level convenience gate (admin-only tabs, Cartographer
    /// row visibility) -- see this module's doc comment for why it is not a
    /// real security boundary yet.
    pub is_admin: bool,
}

/// Shared row-mapper for `users` queries that select
/// `name, created_at_ms, auto_watch, default_notify_tiers, is_admin` in that order.
fn row_to_user_view(r: &rusqlite::Row<'_>) -> rusqlite::Result<UserView> {
    let tiers_csv: String = r.get(3)?;
    Ok(UserView {
        name: r.get(0)?,
        created_at_ms: r.get(1)?,
        auto_watch: r.get(2)?,
        default_notify_tiers: mailbox::parse_tiers(&tiers_csv),
        is_admin: r.get::<_, i64>(4)? != 0,
    })
}

impl Store {
    /// Register a user by name. Upserts (re-registering just refreshes
    /// nothing, since there's nothing else to store yet) -- same idempotent
    /// shape as [`Store::register_project`]. Never promotes an existing user
    /// to admin, and a fresh registration always starts non-admin.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn create_user(&self, name: &str) -> StoreResult<()> {
        let name = name.trim();
        self.conn.execute(
            "INSERT INTO users(name, created_at_ms, is_admin) VALUES(?,?,0)
             ON CONFLICT(name) DO NOTHING",
            rusqlite::params![name, now_ms()],
        )?;
        crate::rlog!(INFO, "ralphus [store] user {name:?} registered");
        let _ = self.cartographer_log(crate::cartographer::CartographerEntry {
            level: crate::logging::LogLevel::INFO,
            source: "store",
            message: "user registered",
            scope: Some("user"),
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

    /// A user by exact name, or `None` when not registered.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn get_user(&self, name: &str) -> StoreResult<Option<UserView>> {
        self.conn
            .query_row(
                "SELECT name, created_at_ms, auto_watch, default_notify_tiers, is_admin FROM users WHERE name=?",
                rusqlite::params![name],
                row_to_user_view,
            )
            .optional()
            .map_err(Into::into)
    }

    /// Whether `name` is a registered admin. A caller naming an unregistered
    /// user is treated as non-admin rather than an error, since every
    /// admin-gate call site just wants a yes/no.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn is_admin(&self, name: &str) -> StoreResult<bool> {
        Ok(self.get_user(name)?.is_some_and(|u| u.is_admin))
    }

    /// All registered users, newest first.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn list_users(&self) -> StoreResult<Vec<UserView>> {
        let mut stmt = self.conn.prepare(
            "SELECT name, created_at_ms, auto_watch, default_notify_tiers, is_admin FROM users
             ORDER BY created_at_ms DESC, name",
        )?;
        let rows = stmt
            .query_map([], row_to_user_view)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Register `name` if it isn't already, otherwise a no-op — used by
    /// call sites (e.g. [`Store::create_watch`]) that want "just make sure
    /// this user exists" without caring whether it's the first time.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn ensure_user_row(&self, name: &str) -> StoreResult<()> {
        self.create_user(name)
    }

    /// Set `name`'s notification preferences (RAL-320) -- registers the user
    /// first if needed, same as [`Store::create_watch`].
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn set_user_preferences(
        &self,
        name: &str,
        auto_watch: bool,
        default_notify_tiers: &[MailboxPriority],
    ) -> StoreResult<UserView> {
        self.ensure_user_row(name)?;
        let tiers_csv = mailbox::tiers_to_csv(default_notify_tiers);
        self.conn.execute(
            "UPDATE users SET auto_watch=?1, default_notify_tiers=?2 WHERE name=?3",
            rusqlite::params![auto_watch, tiers_csv, name],
        )?;
        crate::rlog!(
            INFO,
            "ralphus [store] {name:?} notification preferences updated (auto_watch={auto_watch})"
        );
        self.get_user(name)?.ok_or(StoreError::NotFound)
    }

    /// Sets or clears a registered user's admin flag. Idempotent -- setting
    /// it to its current value is a no-op beyond the `UPDATE`. Returns
    /// [`StoreError::NotFound`] if `name` isn't registered.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn set_user_admin(&self, name: &str, is_admin: bool) -> StoreResult<()> {
        let n = self.conn.execute(
            "UPDATE users SET is_admin=? WHERE name=?",
            rusqlite::params![i64::from(is_admin), name],
        )?;
        if n == 0 {
            return Err(StoreError::NotFound);
        }
        crate::rlog!(
            INFO,
            "ralphus [store] user {name:?} admin flag set to {is_admin}"
        );
        let _ = self.cartographer_log(crate::cartographer::CartographerEntry {
            level: crate::logging::LogLevel::INFO,
            source: "store",
            message: "user admin flag changed",
            scope: Some("user"),
            squad_id: None,
            guardian_id: None,
            cell_id: None,
            task: None,
            log_path: None,
            payload: serde_json::json!({ "name": name, "is_admin": is_admin }),
            admin_only: false,
        });
        Ok(())
    }

    /// Rename a registered user in place. Unlike e.g. a guardian rename, this
    /// table's primary key *is* the name, so a rename can collide with an
    /// existing row -- that case returns [`StoreError::InvalidTransition`]
    /// rather than a raw SQLite unique-constraint failure. Returns
    /// [`StoreError::NotFound`] if `old_name` isn't registered.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn rename_user(&self, old_name: &str, new_name: &str) -> StoreResult<()> {
        if old_name == new_name {
            return self
                .get_user(old_name)?
                .map(|_| ())
                .ok_or(StoreError::NotFound);
        }
        if self.get_user(new_name)?.is_some() {
            return Err(StoreError::InvalidTransition(format!(
                "user {new_name:?} is already registered"
            )));
        }
        let n = self.conn.execute(
            "UPDATE users SET name=? WHERE name=?",
            rusqlite::params![new_name, old_name],
        )?;
        if n == 0 {
            return Err(StoreError::NotFound);
        }
        crate::rlog!(
            INFO,
            "ralphus [store] user {old_name:?} renamed to {new_name:?}"
        );
        Ok(())
    }

    /// Remove a registered user. Returns `false` if no such user existed.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn delete_user(&self, name: &str) -> StoreResult<bool> {
        let n = self
            .conn
            .execute("DELETE FROM users WHERE name = ?", rusqlite::params![name])?;
        if n > 0 {
            crate::rlog!(INFO, "ralphus [store] user {name:?} removed");
            let _ = self.cartographer_log(crate::cartographer::CartographerEntry {
                level: crate::logging::LogLevel::INFO,
                source: "store",
                message: "user removed",
                scope: Some("user"),
                squad_id: None,
                guardian_id: None,
                cell_id: None,
                task: None,
                log_path: None,
                payload: serde_json::json!({ "name": name }),
                admin_only: false,
            });
        }
        Ok(n > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_newly_registered_user_is_not_admin() {
        let store = Store::open_in_memory().unwrap();
        store.create_user("alice").unwrap();
        assert!(!store.get_user("alice").unwrap().unwrap().is_admin);
        assert!(!store.is_admin("alice").unwrap());
    }

    #[test]
    fn set_user_admin_toggles_the_flag() {
        let store = Store::open_in_memory().unwrap();
        store.create_user("alice").unwrap();
        store.set_user_admin("alice", true).unwrap();
        assert!(store.is_admin("alice").unwrap());
        assert!(store.get_user("alice").unwrap().unwrap().is_admin);
        assert!(
            store
                .list_users()
                .unwrap()
                .iter()
                .find(|u| u.name == "alice")
                .unwrap()
                .is_admin
        );

        store.set_user_admin("alice", false).unwrap();
        assert!(!store.is_admin("alice").unwrap());
    }

    #[test]
    fn set_user_admin_on_an_unregistered_name_fails() {
        let store = Store::open_in_memory().unwrap();
        assert!(matches!(
            store.set_user_admin("nobody", true),
            Err(StoreError::NotFound)
        ));
    }

    #[test]
    fn is_admin_on_an_unregistered_name_is_false_not_an_error() {
        let store = Store::open_in_memory().unwrap();
        assert!(!store.is_admin("nobody").unwrap());
    }

    #[test]
    fn re_registering_an_existing_admin_does_not_demote_them() {
        let store = Store::open_in_memory().unwrap();
        store.create_user("alice").unwrap();
        store.set_user_admin("alice", true).unwrap();
        store.create_user("alice").unwrap();
        assert!(store.is_admin("alice").unwrap());
    }
}
