//! Durable V1 manual-action delivery state.
//!
//! A client is a user's named review destination. V1 clients are local and
//! execute from a shared-store mount; later providers may add transfer or
//! remote execution without changing recipient or generation identity.

use rusqlite::OptionalExtension as _;
use serde::Serialize;

use crate::store::{Result as StoreResult, Store, StoreError, now_ms};

const PREPARE: &str = "prepare";
const IGNORE: &str = "ignore";
const RETAIN_LATEST: &str = "retain_latest";
const DEFER_PREPARATION: &str = "defer_preparation";
const DELIVER_LATEST: &str = "deliver_latest";
const MANUAL: &str = "manual";

/// A durable manual-review destination owned by one user.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ReviewClientView {
    pub id: String,
    pub user_name: String,
    pub label: String,
    pub enabled: bool,
    pub local: bool,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

/// User-controlled automatic delivery policy.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ReviewDeliveryPreferences {
    pub on_rebase: String,
    pub on_feedback: String,
    pub on_auto_pr_fix: String,
    pub offline_delivery: String,
    pub on_reconnect: String,
    pub auto_register_submitter: bool,
}

impl Default for ReviewDeliveryPreferences {
    fn default() -> Self {
        Self {
            on_rebase: IGNORE.to_string(),
            on_feedback: PREPARE.to_string(),
            on_auto_pr_fix: PREPARE.to_string(),
            offline_delivery: RETAIN_LATEST.to_string(),
            on_reconnect: DELIVER_LATEST.to_string(),
            auto_register_submitter: true,
        }
    }
}

fn validate_preferences(value: &ReviewDeliveryPreferences) -> StoreResult<()> {
    if ![PREPARE, IGNORE].contains(&value.on_rebase.as_str())
        || ![PREPARE, IGNORE].contains(&value.on_feedback.as_str())
        || ![PREPARE, IGNORE].contains(&value.on_auto_pr_fix.as_str())
        || ![RETAIN_LATEST, DEFER_PREPARATION].contains(&value.offline_delivery.as_str())
        || ![DELIVER_LATEST, MANUAL].contains(&value.on_reconnect.as_str())
    {
        return Err(StoreError::InvalidTransition(
            "invalid review delivery preference value".to_string(),
        ));
    }
    Ok(())
}

fn row_to_client(row: &rusqlite::Row<'_>) -> rusqlite::Result<ReviewClientView> {
    Ok(ReviewClientView {
        id: row.get(0)?,
        user_name: row.get(1)?,
        label: row.get(2)?,
        enabled: row.get::<_, i64>(3)? != 0,
        local: row.get::<_, i64>(4)? != 0,
        created_at_ms: row.get(5)?,
        updated_at_ms: row.get(6)?,
    })
}

impl Store {
    /// Register or update a V1 local review client for `user_name`.
    pub fn upsert_review_client(
        &self,
        user_name: &str,
        label: &str,
        enabled: bool,
    ) -> StoreResult<ReviewClientView> {
        let label = label.trim();
        if label.is_empty() {
            return Err(StoreError::InvalidTransition(
                "review client label must not be empty".to_string(),
            ));
        }
        self.ensure_user_row(user_name)?;
        let existing: Option<String> = self
            .conn
            .query_row(
                "SELECT id FROM review_clients WHERE user_name=? AND label=?",
                rusqlite::params![user_name, label],
                |row| row.get(0),
            )
            .optional()?;
        let id = existing.unwrap_or_else(|| {
            self.next_id("review_client_seq", "client")
                .unwrap_or_else(|_| format!("client-{user_name}"))
        });
        let now = now_ms();
        self.conn.execute(
            "INSERT INTO review_clients(id, user_name, label, enabled, local, created_at_ms, updated_at_ms)
             VALUES(?1, ?2, ?3, ?4, 1, ?5, ?5)
             ON CONFLICT(user_name, label) DO UPDATE SET enabled=excluded.enabled, updated_at_ms=excluded.updated_at_ms",
            rusqlite::params![id, user_name, label, i64::from(enabled), now],
        )?;
        self.conn.query_row(
            "SELECT id, user_name, label, enabled, local, created_at_ms, updated_at_ms FROM review_clients WHERE user_name=? AND label=?",
            rusqlite::params![user_name, label], row_to_client,
        ).map_err(Into::into)
    }

    /// All local review clients a user has registered.
    pub fn review_clients_for_user(&self, user_name: &str) -> StoreResult<Vec<ReviewClientView>> {
        let mut statement = self.conn.prepare(
            "SELECT id, user_name, label, enabled, local, created_at_ms, updated_at_ms
             FROM review_clients WHERE user_name=? ORDER BY created_at_ms, id",
        )?;
        statement
            .query_map(rusqlite::params![user_name], row_to_client)?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// Read a user's delivery policy, returning the locked V1 defaults until
    /// the user first saves an override.
    pub fn review_delivery_preferences(
        &self,
        user_name: &str,
    ) -> StoreResult<ReviewDeliveryPreferences> {
        self.conn.query_row(
            "SELECT on_rebase, on_feedback, on_auto_pr_fix, offline_delivery, on_reconnect, auto_register_submitter
             FROM user_review_delivery_preferences WHERE user_name=?",
            rusqlite::params![user_name],
            |row| Ok(ReviewDeliveryPreferences {
                on_rebase: row.get(0)?, on_feedback: row.get(1)?, on_auto_pr_fix: row.get(2)?,
                offline_delivery: row.get(3)?, on_reconnect: row.get(4)?, auto_register_submitter: row.get::<_, i64>(5)? != 0,
            }),
        ).optional().map(|value| value.unwrap_or_default()).map_err(Into::into)
    }

    /// Store one user's explicit delivery policy.
    pub fn set_review_delivery_preferences(
        &self,
        user_name: &str,
        value: &ReviewDeliveryPreferences,
    ) -> StoreResult<()> {
        validate_preferences(value)?;
        self.ensure_user_row(user_name)?;
        self.conn.execute(
            "INSERT INTO user_review_delivery_preferences(user_name, on_rebase, on_feedback, on_auto_pr_fix, offline_delivery, on_reconnect, auto_register_submitter, updated_at_ms)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(user_name) DO UPDATE SET on_rebase=excluded.on_rebase, on_feedback=excluded.on_feedback, on_auto_pr_fix=excluded.on_auto_pr_fix, offline_delivery=excluded.offline_delivery, on_reconnect=excluded.on_reconnect, auto_register_submitter=excluded.auto_register_submitter, updated_at_ms=excluded.updated_at_ms",
            rusqlite::params![user_name, value.on_rebase, value.on_feedback, value.on_auto_pr_fix, value.offline_delivery, value.on_reconnect, i64::from(value.auto_register_submitter), now_ms()],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_can_own_multiple_review_clients_and_delivery_defaults_are_locked() {
        let store = Store::open_in_memory().unwrap();
        let desktop = store
            .upsert_review_client("korin", "Desktop", true)
            .unwrap();
        let phone = store.upsert_review_client("korin", "Phone", false).unwrap();
        assert_ne!(desktop.id, phone.id);
        assert_eq!(store.review_clients_for_user("korin").unwrap().len(), 2);
        assert_eq!(
            store.review_delivery_preferences("korin").unwrap(),
            ReviewDeliveryPreferences::default()
        );
    }
}
