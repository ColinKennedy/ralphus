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

use rusqlite::OptionalExtension as _;
use serde::Serialize;

use crate::store::{Result as StoreResult, Store, StoreError, now_ms};

/// A registered user placeholder.
#[derive(Debug, Clone, Serialize)]
pub struct UserView {
    /// Unique user name, e.g. `"colin"`. This is the identifier
    /// `UserContext.id` carries and `default_user` (`.ralphus.toml`) names.
    pub name: String,
    /// Registration time (Unix epoch milliseconds).
    pub created_at_ms: i64,
}

impl Store {
    /// Register a user by name. Upserts (re-registering just refreshes
    /// nothing, since there's nothing else to store yet) -- same idempotent
    /// shape as [`Store::register_project`].
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn create_user(&self, name: &str) -> StoreResult<()> {
        let name = name.trim();
        self.conn.execute(
            "INSERT INTO users(name, created_at_ms) VALUES(?,?)
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
                "SELECT name, created_at_ms FROM users WHERE name=?",
                rusqlite::params![name],
                |r| {
                    Ok(UserView {
                        name: r.get(0)?,
                        created_at_ms: r.get(1)?,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    /// All registered users, newest first.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn list_users(&self) -> StoreResult<Vec<UserView>> {
        let mut stmt = self
            .conn
            .prepare("SELECT name, created_at_ms FROM users ORDER BY created_at_ms DESC, name")?;
        let rows = stmt
            .query_map([], |r| {
                Ok(UserView {
                    name: r.get(0)?,
                    created_at_ms: r.get(1)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
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
            });
        }
        Ok(n > 0)
    }
}
