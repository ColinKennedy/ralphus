//! The daemon-wide forge poll-token pool (RAL-635).
//!
//! Background pollers (CI watch, the PR cache refresh, base-state and reorder
//! polls) read the forge on a timer under whichever single token the review's
//! owner resolved, so one busy token runs a forge quota dry for everything.
//! An admin can register extra personal access tokens here, keyed by forge
//! kind + host (`github`/`gitlab`, `github.com`/`gitlab.example.com`, ...),
//! and `forge.rs`'s `poll_rotated` spreads polling reads across them:
//! the first token is used until the forge rejects it (403/429), then the
//! next, and the user's own token after the last one.
//!
//! Pool tokens are for polling reads only. Every write call (create/update/
//! close a PR, comments, labels, the stack API) keeps the acting user's token,
//! because a write must be attributable to a person and pool tokens belong to
//! nobody.
//!
//! The rows live in SQLite (`forge_poll_tokens`); the raw values are mirrored
//! into a process-global registry so a forge call deep inside a poller reaches
//! them without the store lock. The mirror is rebuilt after every add/remove
//! and once at startup ([`install`]). No API response ever carries a raw
//! token: [`PoolTokenSummary`] holds a label and a masked tail only.

use std::collections::{HashMap, HashSet};
use std::sync::{LazyLock, Mutex, OnceLock, RwLock};

use serde::Serialize;

use crate::mailbox::{MailboxPriority, Remediation};
use crate::store::{Result as StoreResult, Store, now_ms};
use crate::store_lock::StoreHandle;

/// One registered pool token as a listing shows it: no raw value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PoolTokenSummary {
    pub id: i64,
    /// `github` or `gitlab`.
    pub kind: String,
    pub host: String,
    pub label: String,
    /// The last few characters of the token behind a fixed mask, enough for
    /// an admin to tell two tokens apart.
    pub masked: String,
    pub created_at_ms: i64,
}

/// A pool token as the registry holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolToken {
    pub label: String,
    pub token: String,
}

type PoolKey = (String, String);

static POOL: LazyLock<RwLock<HashMap<PoolKey, Vec<PoolToken>>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

/// Index of the token currently in use, per `(kind, host)`. Polling stays on
/// it until the forge rejects it.
static STICKY: LazyLock<Mutex<HashMap<PoolKey, usize>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// `(kind, host)` pairs whose exhaustion has already been reported, so one
/// outage raises one notice rather than one per poll.
static EXHAUSTED: LazyLock<Mutex<HashSet<PoolKey>>> = LazyLock::new(|| Mutex::new(HashSet::new()));

/// Where exhaustion notices are recorded; unset in unit tests and any process
/// that never called [`install`], in which case only the stderr log is written.
static SINK: OnceLock<StoreHandle> = OnceLock::new();

fn key(kind: &str, host: &str) -> PoolKey {
    (kind.to_ascii_lowercase(), host.to_ascii_lowercase())
}

/// The mask shown for a token: a fixed run of dots plus its last four
/// characters (all dots when it is too short for the tail to be safe to show).
#[must_use]
pub fn mask_token(token: &str) -> String {
    let chars: Vec<char> = token.chars().collect();
    if chars.len() < 12 {
        return "••••".to_string();
    }
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("••••{tail}")
}

/// The tokens registered for `(kind, host)`, in rotation order.
#[must_use]
pub fn tokens(kind: &str, host: &str) -> Vec<PoolToken> {
    POOL.read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&key(kind, host))
        .cloned()
        .unwrap_or_default()
}

/// Index polling should start from for `(kind, host)` among `len` tokens.
#[must_use]
pub fn sticky_index(kind: &str, host: &str, len: usize) -> usize {
    let idx = STICKY
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&key(kind, host))
        .copied()
        .unwrap_or(0);
    if len == 0 { 0 } else { idx % len }
}

/// Remember `index` as the token to start from next time.
pub fn set_sticky_index(kind: &str, host: &str, index: usize) {
    STICKY
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(key(kind, host), index);
}

/// A poll through the pool succeeded: a later exhaustion is a new event.
pub fn note_recovered(kind: &str, host: &str) {
    EXHAUSTED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&key(kind, host));
}

/// Every token for `(kind, host)` was rejected. Returns `true` only on the
/// transition into exhausted, which is when the notice is raised.
pub fn note_exhausted(kind: &str, host: &str, pool_len: usize, detail: &str) -> bool {
    let newly = EXHAUSTED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(key(kind, host));
    if newly {
        report_exhausted(kind, host, pool_len, detail);
    }
    newly
}

fn report_exhausted(kind: &str, host: &str, pool_len: usize, detail: &str) {
    let message = format!(
        "all configured {kind} tokens for {host} were rejected \
         ({pool_len} pool token(s) and the user's own token): {detail}"
    );
    // ralphus[ignore-rlog-pair]: the structured row is written by the sink thread below, which owns the store handle
    crate::rlog!(ERROR, "ralphus [forge-pool] {message}");
    let Some(handle) = SINK.get() else {
        return;
    };
    let handle = handle.clone();
    let (kind, host) = (kind.to_string(), host.to_string());
    // The store lock is never taken on the polling thread: a poller can be
    // running with the lock-holding scheduler waiting on it.
    std::thread::spawn(move || {
        let store = handle.lock();
        crate::cartographer::Note::new("forge-pool")
            .level(crate::logging::LogLevel::ERROR)
            .scope("forge_pool")
            .emit(
                &store,
                &message,
                serde_json::json!({
                    "kind": kind,
                    "host": host,
                    "pool_tokens": pool_len,
                }),
            );
        let remediation = Remediation::ManualInterventionRequired {
            guidance: format!(
                "wait for the {kind} quota on {host} to reset, or add another token with \
                 `ralphus forge pool add {kind} <label> --host {host}` (see `ralphus forge pool list`)"
            ),
        };
        let _ = store.enqueue_error_mailbox_message(
            MailboxPriority::High,
            &message,
            &remediation,
            None,
            None,
            None,
            None,
            Some("forge-pool"),
        );
    });
}

/// Record the handle exhaustion notices are written through and load the
/// registry from the store. Called once by `serve()`.
pub fn install(handle: &StoreHandle) {
    let _ = SINK.set(handle.clone());
    let _ = handle.lock().sync_forge_pool();
}

/// Register `(label, token)` pairs for one `(kind, host)` without touching any
/// other key, so tests sharing a process don't clobber each other.
#[cfg(test)]
pub(crate) fn set_tokens_for_test(kind: &str, host: &str, tokens: &[(&str, &str)]) {
    POOL.write()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(
            key(kind, host),
            tokens
                .iter()
                .map(|(label, token)| PoolToken {
                    label: (*label).to_string(),
                    token: (*token).to_string(),
                })
                .collect(),
        );
}

/// Replace the registry contents (and drop rotation state for pairs that no
/// longer have tokens).
fn replace_registry(rows: Vec<(String, String, String, String)>) {
    let mut map: HashMap<PoolKey, Vec<PoolToken>> = HashMap::new();
    for (kind, host, label, token) in rows {
        map.entry(key(&kind, &host))
            .or_default()
            .push(PoolToken { label, token });
    }
    STICKY
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .retain(|k, _| map.contains_key(k));
    EXHAUSTED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .retain(|k| map.contains_key(k));
    *POOL
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = map;
}

impl Store {
    /// Register a pool token. `kind` is `github` or `gitlab`; `label` is unique
    /// per `(kind, host)`.
    ///
    /// # Errors
    /// Propagates any SQLite failure, including a duplicate label.
    pub fn add_forge_poll_token(
        &self,
        kind: &str,
        host: &str,
        label: &str,
        token: &str,
    ) -> StoreResult<PoolTokenSummary> {
        let (kind, host) = key(kind, host);
        let created_at_ms = now_ms();
        self.conn.execute(
            "INSERT INTO forge_poll_tokens(kind, host, label, token, created_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![kind, host, label, token, created_at_ms],
        )?;
        let id = self.conn.last_insert_rowid();
        self.sync_forge_pool()?;
        crate::cartographer::Note::new("store")
            .scope("forge_pool")
            // Deliberately no token value: Cartographer rows are readable by
            // anyone who can view a project's timeline.
            .emit(
                self,
                format!("forge poll token added kind={kind} host={host} label={label:?}"),
                serde_json::json!({ "kind": kind, "host": host, "label": label }),
            );
        Ok(PoolTokenSummary {
            id,
            kind,
            host,
            label: label.to_string(),
            masked: mask_token(token),
            created_at_ms,
        })
    }

    /// Every registered pool token, masked, in rotation order.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn list_forge_poll_tokens(&self) -> StoreResult<Vec<PoolTokenSummary>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, kind, host, label, token, created_at_ms FROM forge_poll_tokens \
             ORDER BY kind, host, id",
        )?;
        let rows = stmt
            .query_map([], |r| {
                let token: String = r.get(4)?;
                Ok(PoolTokenSummary {
                    id: r.get(0)?,
                    kind: r.get(1)?,
                    host: r.get(2)?,
                    label: r.get(3)?,
                    masked: mask_token(&token),
                    created_at_ms: r.get(5)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Remove the pool token labelled `label` for `(kind, host)`. Returns
    /// `false` when none existed.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn remove_forge_poll_token(
        &self,
        kind: &str,
        host: &str,
        label: &str,
    ) -> StoreResult<bool> {
        let (kind, host) = key(kind, host);
        let n = self.conn.execute(
            "DELETE FROM forge_poll_tokens WHERE kind=?1 AND host=?2 AND label=?3",
            rusqlite::params![kind, host, label],
        )?;
        if n > 0 {
            self.sync_forge_pool()?;
            crate::cartographer::Note::new("store")
                .scope("forge_pool")
                .emit(
                    self,
                    format!("forge poll token removed kind={kind} host={host} label={label:?}"),
                    serde_json::json!({ "kind": kind, "host": host, "label": label }),
                );
        }
        Ok(n > 0)
    }

    /// Rebuild the process-global registry from the table.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn sync_forge_pool(&self) -> StoreResult<()> {
        let mut stmt = self.conn.prepare(
            "SELECT kind, host, label, token FROM forge_poll_tokens ORDER BY kind, host, id",
        )?;
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        replace_registry(rows);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mask_shows_only_a_four_character_tail() {
        assert_eq!(mask_token("ghp_abcdefghijklmnop"), "••••mnop");
        assert_eq!(mask_token("short"), "••••");
    }

    #[test]
    fn add_list_remove_round_trip_never_returns_the_raw_token() {
        let store = Store::open_in_memory().unwrap();
        let added = store
            .add_forge_poll_token(
                "github",
                "pool-roundtrip.example",
                "ci-bot",
                "ghp_secretvalue1234",
            )
            .unwrap();
        assert_eq!(added.masked, "••••1234");
        let listed = store.list_forge_poll_tokens().unwrap();
        assert!(listed.iter().any(|t| t.label == "ci-bot"));
        let json = serde_json::to_string(&listed).unwrap();
        assert!(!json.contains("secretvalue"), "{json}");

        let in_registry = tokens("github", "pool-roundtrip.example");
        assert_eq!(in_registry.len(), 1);
        assert_eq!(in_registry[0].token, "ghp_secretvalue1234");

        assert!(
            store
                .remove_forge_poll_token("github", "pool-roundtrip.example", "ci-bot")
                .unwrap()
        );
        assert!(tokens("github", "pool-roundtrip.example").is_empty());
        assert!(
            !store
                .remove_forge_poll_token("github", "pool-roundtrip.example", "ci-bot")
                .unwrap()
        );
    }

    #[test]
    fn duplicate_label_per_host_is_rejected_and_order_is_insertion_order() {
        let store = Store::open_in_memory().unwrap();
        store
            .add_forge_poll_token(
                "gitlab",
                "pool-order.example",
                "a",
                "glpat-aaaaaaaaaaaaaaaa",
            )
            .unwrap();
        store
            .add_forge_poll_token(
                "gitlab",
                "pool-order.example",
                "b",
                "glpat-bbbbbbbbbbbbbbbb",
            )
            .unwrap();
        assert!(
            store
                .add_forge_poll_token(
                    "gitlab",
                    "pool-order.example",
                    "a",
                    "glpat-cccccccccccccccc"
                )
                .is_err()
        );
        let labels: Vec<_> = tokens("gitlab", "pool-order.example")
            .into_iter()
            .map(|t| t.label)
            .collect();
        assert_eq!(labels, ["a", "b"]);
    }

    #[test]
    fn exhaustion_is_reported_once_until_a_poll_recovers() {
        assert!(note_exhausted("github", "pool-notice.example", 2, "x"));
        assert!(!note_exhausted("github", "pool-notice.example", 2, "x"));
        note_recovered("github", "pool-notice.example");
        assert!(note_exhausted("github", "pool-notice.example", 2, "x"));
    }
}
