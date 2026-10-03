//! Durable V1 manual-action delivery state.
//!
//! A client is a user's named review destination. V1 clients are local and
//! execute from a shared-store mount; later providers may add transfer or
//! remote execution without changing recipient or generation identity.

use rusqlite::OptionalExtension as _;
use serde::{Deserialize, Serialize};

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
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReviewDeliveryPreferences {
    pub on_rebase: String,
    pub on_feedback: String,
    pub on_auto_pr_fix: String,
    pub offline_delivery: String,
    pub on_reconnect: String,
    pub auto_register_submitter: bool,
}

/// Recipient-scoped readiness for one prepared action generation.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ActionGenerationView {
    pub guardian_id: String,
    pub action_key: String,
    pub client_id: String,
    pub generation: i64,
    pub state: String,
    pub definition_digest: String,
    pub manifest_path: Option<String>,
    pub manifest_sha256: Option<String>,
    pub published_root: Option<String>,
    pub lease_expires_at_ms: Option<i64>,
    pub detail: Option<String>,
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
    /// Enabled recipients for a review, including their current local client
    /// identity. A disabled/offline client deliberately receives no new V1
    /// preparation claim; its prior generation remains durable for retirement.
    pub fn guardian_recipients(&self, guardian_id: &str) -> StoreResult<Vec<ReviewClientView>> {
        let mut statement = self.conn.prepare(
            "SELECT c.id, c.user_name, c.label, c.enabled, c.local, c.created_at_ms, c.updated_at_ms
             FROM guardian_recipients r JOIN review_clients c ON c.id=r.client_id
             WHERE r.guardian_id=? ORDER BY r.subscribed_at_ms, c.id",
        )?;
        statement
            .query_map(rusqlite::params![guardian_id], row_to_client)?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// Atomically acquire the one V1 build slot for a recipient/action/generation.
    /// A duplicate click or a concurrent post-merge worker gets `false` and
    /// must attach to the existing progress rather than starting a second build.
    pub fn claim_action_generation(
        &self,
        guardian_id: &str,
        action_key: &str,
        client_id: &str,
        generation: i64,
        definition_digest: &str,
    ) -> StoreResult<bool> {
        let now = now_ms();
        let changed = self.conn.execute(
            "INSERT INTO guardian_action_generations(guardian_id, action_key, client_id, generation, state, definition_digest, created_at_ms, updated_at_ms)
             VALUES(?1, ?2, ?3, ?4, 'preparing', ?5, ?6, ?6)
             ON CONFLICT(guardian_id, action_key, client_id, generation) DO UPDATE SET
                state='preparing', definition_digest=excluded.definition_digest, detail=NULL, updated_at_ms=excluded.updated_at_ms
             WHERE guardian_action_generations.state NOT IN ('preparing', 'ready')",
            rusqlite::params![guardian_id, action_key, client_id, generation, definition_digest, now],
        )?;
        Ok(changed > 0)
    }

    /// Publish terminal recipient readiness and the immutable manifest data.
    #[allow(clippy::too_many_arguments)]
    pub fn finish_action_generation(
        &self,
        guardian_id: &str,
        action_key: &str,
        client_id: &str,
        generation: i64,
        state: &str,
        manifest_path: Option<&str>,
        manifest_sha256: Option<&str>,
        published_root: Option<&str>,
        lease_expires_at_ms: Option<i64>,
        detail: Option<&str>,
    ) -> StoreResult<()> {
        self.conn.execute(
            "UPDATE guardian_action_generations SET state=?1, manifest_path=?2, manifest_sha256=?3, published_root=?4, lease_expires_at_ms=?5, detail=?6, updated_at_ms=?7
             WHERE guardian_id=?8 AND action_key=?9 AND client_id=?10 AND generation=?11",
            rusqlite::params![state, manifest_path, manifest_sha256, published_root, lease_expires_at_ms, detail, now_ms(), guardian_id, action_key, client_id, generation],
        )?;
        Ok(())
    }

    /// Recipient-facing state, newest generation first.
    pub fn action_generations(&self, guardian_id: &str) -> StoreResult<Vec<ActionGenerationView>> {
        let mut statement = self.conn.prepare(
            "SELECT guardian_id, action_key, client_id, generation, state, definition_digest, manifest_path, manifest_sha256, published_root, lease_expires_at_ms, detail
             FROM guardian_action_generations WHERE guardian_id=? ORDER BY generation DESC, action_key, client_id",
        )?;
        statement
            .query_map(rusqlite::params![guardian_id], |row| {
                Ok(ActionGenerationView {
                    guardian_id: row.get(0)?,
                    action_key: row.get(1)?,
                    client_id: row.get(2)?,
                    generation: row.get(3)?,
                    state: row.get(4)?,
                    definition_digest: row.get(5)?,
                    manifest_path: row.get(6)?,
                    manifest_sha256: row.get(7)?,
                    published_root: row.get(8)?,
                    lease_expires_at_ms: row.get(9)?,
                    detail: row.get(10)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// Claim publications whose review is terminal and whose execution lease
    /// has elapsed. Marking them first makes cleanup idempotent across the
    /// daily sweep and a concurrent daemon restart.
    pub fn claim_expired_terminal_action_generations(
        &self,
    ) -> StoreResult<Vec<ActionGenerationView>> {
        let now = now_ms();
        let mut statement = self.conn.prepare(
            "SELECT g.guardian_id, g.action_key, g.client_id, g.generation, g.state, g.definition_digest, g.manifest_path, g.manifest_sha256, g.published_root, g.lease_expires_at_ms, g.detail
             FROM guardian_action_generations g JOIN guardians r ON r.id=g.guardian_id
             WHERE r.status IN ('merged', 'approved', 'cancelled', 'deployed')
               AND g.state='ready' AND (g.lease_expires_at_ms IS NULL OR g.lease_expires_at_ms<=?1)",
        )?;
        let rows = statement
            .query_map(rusqlite::params![now], |row| {
                Ok(ActionGenerationView {
                    guardian_id: row.get(0)?,
                    action_key: row.get(1)?,
                    client_id: row.get(2)?,
                    generation: row.get(3)?,
                    state: row.get(4)?,
                    definition_digest: row.get(5)?,
                    manifest_path: row.get(6)?,
                    manifest_sha256: row.get(7)?,
                    published_root: row.get(8)?,
                    lease_expires_at_ms: row.get(9)?,
                    detail: row.get(10)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        for row in &rows {
            self.conn.execute(
                "UPDATE guardian_action_generations SET state='retiring', updated_at_ms=?1
                 WHERE guardian_id=?2 AND action_key=?3 AND client_id=?4 AND generation=?5 AND state='ready'",
                rusqlite::params![now, row.guardian_id, row.action_key, row.client_id, row.generation],
            )?;
        }
        Ok(rows)
    }

    /// Complete the filesystem-retirement state after a sweep attempt.
    pub fn finish_action_generation_retirement(
        &self,
        row: &ActionGenerationView,
        detail: Option<&str>,
    ) -> StoreResult<()> {
        self.conn.execute(
            "UPDATE guardian_action_generations SET state='retired', detail=?1, updated_at_ms=?2
             WHERE guardian_id=?3 AND action_key=?4 AND client_id=?5 AND generation=?6 AND state='retiring'",
            rusqlite::params![detail, now_ms(), row.guardian_id, row.action_key, row.client_id, row.generation],
        )?;
        Ok(())
    }
    /// Subscribe a registered client to a review. Repeating the same request is
    /// intentionally idempotent, which is important when a submit/retry races
    /// the default submitter enrollment.
    pub fn subscribe_review_client(&self, guardian_id: &str, client_id: &str) -> StoreResult<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO guardian_recipients(guardian_id, client_id, subscribed_at_ms) VALUES(?1, ?2, ?3)",
            rusqlite::params![guardian_id, client_id, now_ms()],
        )?;
        Ok(())
    }

    /// Enrol the review owner onto their current local client when their
    /// delivery preference allows it. The default client is created lazily so
    /// submitters need no preceding setup for V1.
    pub fn auto_subscribe_review_submitter(
        &self,
        guardian_id: &str,
        owner: Option<&str>,
    ) -> StoreResult<()> {
        let Some(owner) = owner else {
            return Ok(());
        };
        if !self
            .review_delivery_preferences(owner)?
            .auto_register_submitter
        {
            return Ok(());
        }
        let clients = self.review_clients_for_user(owner)?;
        let client = clients
            .into_iter()
            .find(|client| client.enabled)
            .unwrap_or(self.upsert_review_client(owner, "Current machine", true)?);
        self.subscribe_review_client(guardian_id, &client.id)
    }

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

    #[test]
    fn identical_generation_claims_are_single_flight_and_terminal_rows_retire() {
        let store = Store::open_in_memory().unwrap();
        let guardian = store.create_guardian("delivery", "main", "/repo").unwrap();
        let client = store
            .upsert_review_client("korin", "Desktop", true)
            .unwrap();
        store
            .subscribe_review_client(&guardian, &client.id)
            .unwrap();
        assert!(
            store
                .claim_action_generation(&guardian, "action-0", &client.id, 1, "abc")
                .unwrap()
        );
        assert!(
            !store
                .claim_action_generation(&guardian, "action-0", &client.id, 1, "abc")
                .unwrap()
        );
        store
            .finish_action_generation(
                &guardian,
                "action-0",
                &client.id,
                1,
                "ready",
                Some("C:/shared/.ralphus-manifest.json"),
                Some("digest"),
                Some("C:/shared"),
                Some(0),
                None,
            )
            .unwrap();
        store
            .set_guardian_status(&guardian, crate::guardian::GuardianStatus::Cancelled, None)
            .unwrap();
        let rows = store.claim_expired_terminal_action_generations().unwrap();
        assert_eq!(rows.len(), 1);
        store
            .finish_action_generation_retirement(&rows[0], None)
            .unwrap();
        assert_eq!(
            store.action_generations(&guardian).unwrap()[0].state,
            "retired"
        );
    }
}
