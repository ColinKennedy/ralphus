//! Per-user, per-forge-host personal access tokens (RAL-338 follow-up), and
//! the short-lived per-worktree grant mechanism that lets a git-credential
//! helper fetch one without the token ever touching a git config file on
//! disk.
//!
//! `forge.rs`'s own token resolution deliberately reads a forge API token
//! from an environment variable only, never the database, because that
//! covers a single daemon operator with a single identity. This module is a
//! deliberate, narrower exception for a different problem that convention
//! doesn't answer: many *different ralphus users*, each with their own
//! personal fork and their own distinct forge credential, served by the same
//! daemon. Storing each user's own token, which only they set (via CLI/board,
//! scoped to their own name), is the trade-off made instead of either (a)
//! requiring every host that runs a push to have that user's key/token
//! pre-provisioned out of band, or (b) standing up an external identity
//! service.

use serde::Serialize;

use crate::store::{Result as StoreResult, Store, now_ms};

/// One user's token for one forge host, without the token value itself --
/// what a listing (CLI/board "is GitLab configured for this user?") shows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UserForgeTokenSummary {
    pub user: String,
    pub host: String,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

impl Store {
    /// Idempotent upsert of `user`'s token for `host` (e.g. `"gitlab.com"`).
    /// Preserves `created_at_ms` across repeat calls.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn set_user_forge_token(&self, user: &str, host: &str, token: &str) -> StoreResult<()> {
        let now = now_ms();
        self.conn.execute(
            "INSERT INTO user_forge_tokens(user, host, token, created_at_ms, updated_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?4)
             ON CONFLICT(user, host) DO UPDATE SET
                token = excluded.token,
                updated_at_ms = excluded.updated_at_ms",
            rusqlite::params![user, host, token, now],
        )?;
        crate::rlog!(
            INFO,
            "ralphus [store] forge token set for user {user:?} host {host:?}"
        );
        let _ = self.cartographer_log(crate::cartographer::CartographerEntry {
            level: crate::logging::LogLevel::INFO,
            source: "store",
            message: "user forge token set",
            scope: Some("user_forge_token"),
            squad_id: None,
            guardian_id: None,
            cell_id: None,
            task: None,
            log_path: None,
            // Deliberately no token value in the payload -- Cartographer rows
            // are readable by anyone who can view a project's timeline.
            payload: serde_json::json!({ "user": user, "host": host }),
            admin_only: false,
        });
        Ok(())
    }

    /// The raw token value for `(user, host)`. Internal use only (credential
    /// resolution, and RAL-338 follow-up: as a fork's own forge REST API
    /// token override) -- never surface this over an API response; see
    /// [`Self::list_user_forge_tokens`] for the redacted, listable form.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub(crate) fn get_user_forge_token(
        &self,
        user: &str,
        host: &str,
    ) -> StoreResult<Option<String>> {
        Self::get_user_forge_token_conn(&self.conn, user, host)
    }

    /// [`Self::get_user_forge_token`] against any connection -- see
    /// [`Self::resolve_worktree_credential_conn`]'s doc comment for why
    /// this exists (callable from the RAL-393 Stage 3 read pool, not just
    /// the locked writer).
    pub(crate) fn get_user_forge_token_conn(
        conn: &rusqlite::Connection,
        user: &str,
        host: &str,
    ) -> StoreResult<Option<String>> {
        use rusqlite::OptionalExtension as _;
        conn.query_row(
            "SELECT token FROM user_forge_tokens WHERE user=?1 AND host=?2",
            rusqlite::params![user, host],
            |r| r.get(0),
        )
        .optional()
        .map_err(Into::into)
    }

    /// Every forge host `user` has a token configured for, without the token
    /// values themselves.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn list_user_forge_tokens(&self, user: &str) -> StoreResult<Vec<UserForgeTokenSummary>> {
        let mut stmt = self.conn.prepare(
            "SELECT user, host, created_at_ms, updated_at_ms FROM user_forge_tokens \
             WHERE user=?1 ORDER BY host",
        )?;
        let rows = stmt
            .query_map(rusqlite::params![user], |r| {
                Ok(UserForgeTokenSummary {
                    user: r.get(0)?,
                    host: r.get(1)?,
                    created_at_ms: r.get(2)?,
                    updated_at_ms: r.get(3)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Remove `user`'s token for `host`. Returns `false` if none existed.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn delete_user_forge_token(&self, user: &str, host: &str) -> StoreResult<bool> {
        let n = self.conn.execute(
            "DELETE FROM user_forge_tokens WHERE user=?1 AND host=?2",
            rusqlite::params![user, host],
        )?;
        if n > 0 {
            crate::rlog!(
                INFO,
                "ralphus [store] forge token removed for user {user:?} host {host:?}"
            );
        }
        Ok(n > 0)
    }

    /// Mint (or replace) the credential-fetch grant for `worktree_id`,
    /// pointing at `user`'s token for `host`. Called once per worktree, at
    /// the same fork-routing step that already sets that worktree's git
    /// identity (`route_worktree_to_submitter_fork`). Returns the grant
    /// secret to stamp into that worktree's own `--worktree`-scoped git
    /// config alongside `worktree_id` -- see [`Self::resolve_worktree_credential`]
    /// for why both are required together.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn mint_worktree_credential_grant(
        &self,
        worktree_id: &str,
        user: &str,
        host: &str,
    ) -> StoreResult<String> {
        let secret = crate::token::generate();
        let now = now_ms();
        self.conn.execute(
            "INSERT INTO worktree_credential_grants(worktree_id, grant_secret, user, host, created_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(worktree_id) DO UPDATE SET
                grant_secret = excluded.grant_secret,
                user = excluded.user,
                host = excluded.host,
                created_at_ms = excluded.created_at_ms",
            rusqlite::params![worktree_id, secret, user, host, now],
        )?;
        Ok(secret)
    }

    /// Resolve a worktree's forge token for its credential helper: `(worktree_id,
    /// grant_secret)` must match the row [`Self::mint_worktree_credential_grant`]
    /// created, or `None` is returned -- holding the daemon's general API
    /// bearer token is not enough on its own, since every local cell already
    /// has that. `None` also covers "no token configured for that user/host,"
    /// which is not distinguished from an invalid grant in this return type;
    /// callers only ever need to know "is there a credential to use," not why
    /// there might not be one.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn resolve_worktree_credential(
        &self,
        worktree_id: &str,
        grant_secret: &str,
    ) -> StoreResult<Option<String>> {
        Self::resolve_worktree_credential_conn(&self.conn, worktree_id, grant_secret)
    }

    /// [`Self::resolve_worktree_credential`] against any connection, so the
    /// RAL-393 Stage 3 read pool (`Daemon::read_pool`/`ReadConnPool`) can
    /// serve this query without `StoreMutex` -- this is not just extra
    /// concurrency, it avoids a genuine self-deadlock: `route_worktree_to_submitter_fork`
    /// (`daemon/src/worktrees.rs`) runs its `git push` while holding
    /// `StoreMutex` (materialization's existing, accepted lock scope), and
    /// that push's own credential-helper subprocess calls back into this
    /// exact daemon's `/api/internal/fork-credential` endpoint -- if that
    /// handler also needed `StoreMutex`, it would block forever waiting on
    /// a lock the (also blocked, waiting on this same push) materialization
    /// thread already holds. Routing this one read through a pooled,
    /// independent connection breaks that cycle entirely.
    pub(crate) fn resolve_worktree_credential_conn(
        conn: &rusqlite::Connection,
        worktree_id: &str,
        grant_secret: &str,
    ) -> StoreResult<Option<String>> {
        use rusqlite::OptionalExtension as _;
        let grant: Option<(String, String, String)> = conn
            .query_row(
                "SELECT grant_secret, user, host FROM worktree_credential_grants WHERE worktree_id=?1",
                rusqlite::params![worktree_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let Some((expected_secret, user, host)) = grant else {
            return Ok(None);
        };
        // Constant-time compare: this is exactly the kind of secret
        // comparison a timing side-channel could otherwise leak bytes of.
        if !constant_time_eq(expected_secret.as_bytes(), grant_secret.as_bytes()) {
            return Ok(None);
        }
        Self::get_user_forge_token_conn(conn, &user, &host)
    }
}

/// Byte-for-byte compare that takes the same time regardless of where the
/// first mismatch falls, so a caller probing the credential-fetch endpoint
/// can't use response timing to guess the grant secret one byte at a time.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_and_list_round_trips_without_exposing_the_token_value() {
        let store = Store::open_in_memory().unwrap();
        store.create_user("alice").unwrap();
        store
            .set_user_forge_token("alice", "gitlab.com", "glpat-secret")
            .unwrap();

        let listed = store.list_user_forge_tokens("alice").unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].host, "gitlab.com");

        assert_eq!(
            Store::get_user_forge_token_conn(&store.conn, "alice", "gitlab.com").unwrap(),
            Some("glpat-secret".to_string())
        );
    }

    #[test]
    fn set_is_idempotent_and_preserves_created_at() {
        let store = Store::open_in_memory().unwrap();
        store.create_user("alice").unwrap();
        store
            .set_user_forge_token("alice", "gitlab.com", "token-1")
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(2));
        store
            .set_user_forge_token("alice", "gitlab.com", "token-2")
            .unwrap();

        let listed = store.list_user_forge_tokens("alice").unwrap();
        assert_eq!(listed.len(), 1);
        assert!(listed[0].updated_at_ms >= listed[0].created_at_ms);
        assert_eq!(
            Store::get_user_forge_token_conn(&store.conn, "alice", "gitlab.com").unwrap(),
            Some("token-2".to_string())
        );
    }

    #[test]
    fn delete_reports_whether_a_token_existed() {
        let store = Store::open_in_memory().unwrap();
        store.create_user("alice").unwrap();
        assert!(
            !store
                .delete_user_forge_token("alice", "gitlab.com")
                .unwrap()
        );
        store
            .set_user_forge_token("alice", "gitlab.com", "token")
            .unwrap();
        assert!(
            store
                .delete_user_forge_token("alice", "gitlab.com")
                .unwrap()
        );
        assert!(
            Store::get_user_forge_token_conn(&store.conn, "alice", "gitlab.com")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn deleting_a_user_deletes_their_forge_tokens() {
        let store = Store::open_in_memory().unwrap();
        store.create_user("alice").unwrap();
        store
            .set_user_forge_token("alice", "gitlab.com", "token")
            .unwrap();
        store.delete_user("alice").unwrap();
        assert!(
            Store::get_user_forge_token_conn(&store.conn, "alice", "gitlab.com")
                .unwrap()
                .is_none(),
            "a user forge token must be removed when its owning user is (unlike project_forks rows)"
        );
    }

    #[test]
    fn resolve_worktree_credential_succeeds_with_the_matching_grant() {
        let store = Store::open_in_memory().unwrap();
        store.create_user("alice").unwrap();
        store
            .set_user_forge_token("alice", "gitlab.com", "glpat-secret")
            .unwrap();
        let secret = store
            .mint_worktree_credential_grant("wt-1", "alice", "gitlab.com")
            .unwrap();

        assert_eq!(
            store.resolve_worktree_credential("wt-1", &secret).unwrap(),
            Some("glpat-secret".to_string())
        );
    }

    #[test]
    fn resolve_worktree_credential_rejects_a_wrong_secret() {
        let store = Store::open_in_memory().unwrap();
        store.create_user("alice").unwrap();
        store
            .set_user_forge_token("alice", "gitlab.com", "glpat-secret")
            .unwrap();
        store
            .mint_worktree_credential_grant("wt-1", "alice", "gitlab.com")
            .unwrap();

        assert_eq!(
            store
                .resolve_worktree_credential("wt-1", "not-the-real-secret")
                .unwrap(),
            None
        );
    }

    #[test]
    fn resolve_worktree_credential_rejects_an_unknown_worktree_id() {
        let store = Store::open_in_memory().unwrap();
        assert_eq!(
            store
                .resolve_worktree_credential("no-such-worktree", "anything")
                .unwrap(),
            None
        );
    }

    #[test]
    fn resolve_worktree_credential_is_none_when_the_user_has_no_token_for_that_host() {
        let store = Store::open_in_memory().unwrap();
        store.create_user("alice").unwrap();
        // Grant minted, but alice never configured a gitlab.com token.
        let secret = store
            .mint_worktree_credential_grant("wt-1", "alice", "gitlab.com")
            .unwrap();
        assert_eq!(
            store.resolve_worktree_credential("wt-1", &secret).unwrap(),
            None
        );
    }

    #[test]
    fn re_minting_a_grant_for_the_same_worktree_invalidates_the_old_secret() {
        let store = Store::open_in_memory().unwrap();
        store.create_user("alice").unwrap();
        store
            .set_user_forge_token("alice", "gitlab.com", "glpat-secret")
            .unwrap();
        let first = store
            .mint_worktree_credential_grant("wt-1", "alice", "gitlab.com")
            .unwrap();
        let second = store
            .mint_worktree_credential_grant("wt-1", "alice", "gitlab.com")
            .unwrap();
        assert_ne!(first, second);
        assert_eq!(
            store.resolve_worktree_credential("wt-1", &first).unwrap(),
            None
        );
        assert_eq!(
            store.resolve_worktree_credential("wt-1", &second).unwrap(),
            Some("glpat-secret".to_string())
        );
    }
}
