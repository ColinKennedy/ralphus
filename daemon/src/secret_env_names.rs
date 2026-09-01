//! User-configurable list of env-var *names* treated as secret (RAL-281).
//!
//! Additive to `crate::redact`'s value-based registry (RAL-264): that
//! mechanism only ever registers the resolved value of an agent-profile
//! `from_env` indirection. This module lets a user register a variable by
//! *name* instead, so its resolved value gets scrubbed from durable pane
//! text even when it's a literal in a task/cell's TOML `environment=` table
//! or a hierarchical env-override set via the API, not a `from_env` profile
//! entry.
//!
//! Consulted at the single choke point where a cell's or proof step's final
//! env map is fully known -- `daemon/src/scheduler.rs`, right after the
//! agent-profile env and the resolved `env_overrides` layers are merged --
//! rather than in the hot pane-text redaction path itself:
//! `redact::redact_all` (called from `tmux.rs`/`terminal_log.rs`/
//! `runner.rs` on every pane snapshot) only ever replaces *already
//! registered* values, so it never needs to know about names or hit the DB.
//! [`Store::secret_env_names_cached`] is what the scheduler's choke point
//! calls, and it costs a DB read only right after this list has changed
//! (every mutating method below invalidates `Store::secret_env_names_cache`).

use std::collections::BTreeSet;

use rusqlite::OptionalExtension as _;
use serde::Serialize;

use crate::store::{Result as StoreResult, Store, StoreError, now_ms};

/// Small default set seeded once, the first time `secret_env_names` is
/// created (`Store::init_schema`) -- never re-seeded afterward, so deleting
/// one of these is honored across every later restart.
pub const DEFAULT_SECRET_ENV_NAMES: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_OAUTH_TOKEN",
    "ANT_LING_API_KEY",
    "OPENAI_API_KEY",
    "AZURE_OPENAI_API_KEY",
    "DEEPSEEK_API_KEY",
    "NVIDIA_API_KEY",
    "GEMINI_API_KEY",
    "GROQ_API_KEY",
    "CEREBRAS_API_KEY",
    "XAI_API_KEY",
    "FIREWORKS_API_KEY",
    "TOGETHER_API_KEY",
    "BASETEN_API_KEY",
    "OPENROUTER_API_KEY",
    "AI_GATEWAY_API_KEY",
    "ZAI_API_KEY",
    "ZAI_CODING_CN_API_KEY",
    "MISTRAL_API_KEY",
    "MINIMAX_API_KEY",
    "MOONSHOT_API_KEY",
    "OPENCODE_API_KEY",
    "KIMI_API_KEY",
    "CLOUDFLARE_API_KEY",
    "QWEN_TOKEN_PLAN_API_KEY",
    "QWEN_TOKEN_PLAN_CN_API_KEY",
    "XIAOMI_API_KEY",
    "XIAOMI_TOKEN_PLAN_CN_API_KEY",
    "XIAOMI_TOKEN_PLAN_AMS_API_KEY",
    "XIAOMI_TOKEN_PLAN_SGP_API_KEY",
    "AWS_ACCESS_KEY_ID",
    "AWS_SECRET_ACCESS_KEY",
    "AWS_BEARER_TOKEN_BEDROCK",
    "GITHUB_TOKEN",
    "NPM_TOKEN",
    "DATABASE_PASSWORD",
];

/// A registered secret env-var name.
#[derive(Debug, Clone, Serialize)]
pub struct SecretEnvNameView {
    pub name: String,
    pub created_at_ms: i64,
}

impl Store {
    fn invalidate_secret_env_names_cache(&self) {
        if let Ok(mut guard) = self.secret_env_names_cache.write() {
            *guard = None;
        }
    }

    /// Register a new secret env-var name. Unlike `create_user`/
    /// `register_project` this does **not** upsert -- RAL-281 requires an
    /// already-registered name to be rejected, not silently merged.
    ///
    /// # Errors
    /// `StoreError::InvalidTransition` if `name` is already registered;
    /// otherwise propagates any SQLite failure.
    pub fn add_secret_env_name(&self, name: &str) -> StoreResult<()> {
        let n = self.conn.execute(
            "INSERT INTO secret_env_names(name, created_at_ms) VALUES(?,?)
             ON CONFLICT(name) DO NOTHING",
            rusqlite::params![name, now_ms()],
        )?;
        if n == 0 {
            return Err(StoreError::InvalidTransition(format!(
                "secret env-var name {name:?} is already registered"
            )));
        }
        self.invalidate_secret_env_names_cache();
        crate::rlog!(INFO, "ralphus [store] secret env-var name {name:?} added");
        Ok(())
    }

    /// All registered secret env-var names, alphabetical.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn list_secret_env_names(&self) -> StoreResult<Vec<SecretEnvNameView>> {
        let mut stmt = self
            .conn
            .prepare("SELECT name, created_at_ms FROM secret_env_names ORDER BY name")?;
        let rows = stmt
            .query_map([], |r| {
                Ok(SecretEnvNameView {
                    name: r.get(0)?,
                    created_at_ms: r.get(1)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Rename a registered secret env-var name in place.
    ///
    /// # Errors
    /// `StoreError::NotFound` if `old` isn't registered,
    /// `StoreError::InvalidTransition` if `new` is already registered by a
    /// different entry; otherwise propagates any SQLite failure.
    pub fn rename_secret_env_name(&self, old: &str, new: &str) -> StoreResult<()> {
        let exists: bool = self
            .conn
            .query_row(
                "SELECT 1 FROM secret_env_names WHERE name = ?",
                rusqlite::params![old],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if !exists {
            return Err(StoreError::NotFound);
        }
        if old != new {
            let conflict: bool = self
                .conn
                .query_row(
                    "SELECT 1 FROM secret_env_names WHERE name = ?",
                    rusqlite::params![new],
                    |_| Ok(()),
                )
                .optional()?
                .is_some();
            if conflict {
                return Err(StoreError::InvalidTransition(format!(
                    "secret env-var name {new:?} is already registered"
                )));
            }
            self.conn.execute(
                "UPDATE secret_env_names SET name = ? WHERE name = ?",
                rusqlite::params![new, old],
            )?;
            self.invalidate_secret_env_names_cache();
            crate::rlog!(
                INFO,
                "ralphus [store] secret env-var name {old:?} renamed to {new:?}"
            );
        }
        Ok(())
    }

    /// Remove a registered secret env-var name. Returns `false` if no such
    /// name was registered.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn delete_secret_env_name(&self, name: &str) -> StoreResult<bool> {
        let n = self.conn.execute(
            "DELETE FROM secret_env_names WHERE name = ?",
            rusqlite::params![name],
        )?;
        if n > 0 {
            self.invalidate_secret_env_names_cache();
            crate::rlog!(INFO, "ralphus [store] secret env-var name {name:?} removed");
        }
        Ok(n > 0)
    }

    /// The current name list, served from this `Store`'s cache and only
    /// re-read from the DB after a mutation above has invalidated it. Called
    /// from the scheduler's per-cell/per-proof-step env-merge choke point
    /// (see this module's doc comment) rather than from the hot pane-text
    /// redaction path itself.
    ///
    /// # Errors
    /// Propagates any SQLite failure from the (cache-miss-only) reload.
    pub fn secret_env_names_cached(&self) -> StoreResult<BTreeSet<String>> {
        if let Some(set) = self
            .secret_env_names_cache
            .read()
            .ok()
            .and_then(|g| g.clone())
        {
            return Ok(set);
        }
        let set: BTreeSet<String> = self
            .list_secret_env_names()?
            .into_iter()
            .map(|v| v.name)
            .collect();
        if let Ok(mut guard) = self.secret_env_names_cache.write() {
            *guard = Some(set.clone());
        }
        Ok(set)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seeds_the_default_list_on_a_fresh_store() {
        let store = Store::open_in_memory().unwrap();
        let names: BTreeSet<String> = store
            .list_secret_env_names()
            .unwrap()
            .into_iter()
            .map(|v| v.name)
            .collect();
        for default in DEFAULT_SECRET_ENV_NAMES {
            assert!(names.contains(*default), "missing default {default:?}");
        }
    }

    #[test]
    fn add_rejects_a_duplicate_name() {
        let store = Store::open_in_memory().unwrap();
        store.add_secret_env_name("MY_SECRET").unwrap();
        let err = store.add_secret_env_name("MY_SECRET").unwrap_err();
        assert!(matches!(err, StoreError::InvalidTransition(_)));
    }

    #[test]
    fn rename_moves_the_name_and_rejects_an_existing_target() {
        let store = Store::open_in_memory().unwrap();
        store.add_secret_env_name("OLD_NAME").unwrap();
        store.add_secret_env_name("TAKEN").unwrap();
        store
            .rename_secret_env_name("OLD_NAME", "NEW_NAME")
            .unwrap();
        let names: BTreeSet<String> = store
            .list_secret_env_names()
            .unwrap()
            .into_iter()
            .map(|v| v.name)
            .collect();
        assert!(names.contains("NEW_NAME"));
        assert!(!names.contains("OLD_NAME"));
        let err = store
            .rename_secret_env_name("NEW_NAME", "TAKEN")
            .unwrap_err();
        assert!(matches!(err, StoreError::InvalidTransition(_)));
    }

    #[test]
    fn rename_of_an_unregistered_name_is_not_found() {
        let store = Store::open_in_memory().unwrap();
        let err = store
            .rename_secret_env_name("NOPE", "ALSO_NOPE")
            .unwrap_err();
        assert!(matches!(err, StoreError::NotFound));
    }

    #[test]
    fn delete_reports_whether_a_row_existed() {
        let store = Store::open_in_memory().unwrap();
        store.add_secret_env_name("TO_DELETE").unwrap();
        assert!(store.delete_secret_env_name("TO_DELETE").unwrap());
        assert!(!store.delete_secret_env_name("TO_DELETE").unwrap());
    }

    #[test]
    fn cache_reflects_mutations_without_a_stale_read() {
        let store = Store::open_in_memory().unwrap();
        let before = store.secret_env_names_cached().unwrap();
        assert!(!before.contains("FRESH_SECRET"));
        store.add_secret_env_name("FRESH_SECRET").unwrap();
        let after = store.secret_env_names_cached().unwrap();
        assert!(after.contains("FRESH_SECRET"));
        store.delete_secret_env_name("FRESH_SECRET").unwrap();
        let final_set = store.secret_env_names_cached().unwrap();
        assert!(!final_set.contains("FRESH_SECRET"));
    }
}
