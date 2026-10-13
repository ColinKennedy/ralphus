//! RAL-642: durable "this checkout is in use" records for review checkouts.
//!
//! A board-launched or auto-run review action runs in the review's prepared
//! checkout (`review-postmerge`, see `guardian_merge::claim_post_merge_checkout`),
//! and a live action's agent runs there too. A newer preparation generation
//! must never reset, clean or remove a checkout while one of those runs is
//! still using it -- it claims a sibling directory instead.
//!
//! A hold is a small JSON file under `<state dir>/checkout-holds/`, written
//! when a run starts and deleted when the [`CheckoutHold`] guard drops. It is
//! a file rather than in-memory state on purpose: a check run is a detached
//! terminal window and a live run is a tmux session, and both outlive a daemon
//! restart. A daemon that restarts mid-run must still see the checkout as busy.
//! Every hold carries an expiry, so a hold whose owner died without releasing
//! it (a crashed daemon) stops blocking once its run could no longer be alive.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// What one hold file records.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HoldRecord {
    /// The held checkout, normalized with
    /// [`crate::guardian_merge::normalized_worktree_path`].
    pub path: String,
    /// Who holds it, for diagnostics (e.g. `check-run:<guardian>:action:0`).
    pub owner: String,
    pub started_at_ms: i64,
    /// After this instant the hold is ignored and swept.
    pub expires_at_ms: i64,
}

/// A held checkout. Dropping it releases the hold.
#[derive(Debug)]
pub struct CheckoutHold {
    file: PathBuf,
}

impl Drop for CheckoutHold {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.file);
    }
}

/// The directory hold files live in.
#[must_use]
pub fn holds_dir() -> PathBuf {
    crate::state_dir().join("checkout-holds")
}

fn normalize(path: &str) -> String {
    crate::guardian_merge::normalized_worktree_path(Path::new(path))
}

/// Hold `path` for `owner` until the returned guard drops or `max_age` passes.
///
/// # Errors
///
/// When the hold file cannot be written. Callers treat that as "could not
/// protect this run" and log it; the run itself still goes ahead.
pub fn hold(
    path: &str,
    owner: &str,
    max_age: std::time::Duration,
) -> std::io::Result<CheckoutHold> {
    hold_in(&holds_dir(), path, owner, max_age)
}

/// [`hold`] against an explicit directory (tests use a temp dir).
///
/// # Errors
///
/// When the hold directory or file cannot be written.
pub fn hold_in(
    dir: &Path,
    path: &str,
    owner: &str,
    max_age: std::time::Duration,
) -> std::io::Result<CheckoutHold> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);

    std::fs::create_dir_all(dir)?;
    let now = crate::store::now_ms();
    let record = HoldRecord {
        path: normalize(path),
        owner: owner.to_string(),
        started_at_ms: now,
        expires_at_ms: now.saturating_add(i64::try_from(max_age.as_millis()).unwrap_or(i64::MAX)),
    };
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let file = dir.join(format!("{}-{}-{seq}.json", std::process::id(), now));
    let encoded = serde_json::to_string(&record).map_err(std::io::Error::other)?;
    std::fs::write(&file, encoded)?;
    Ok(CheckoutHold { file })
}

/// Every unexpired hold on `path`. Expired and unreadable hold files are
/// removed as a side effect.
#[must_use]
pub fn holders(path: &str) -> Vec<HoldRecord> {
    holders_in(&holds_dir(), path)
}

/// [`holders`] against an explicit directory.
#[must_use]
pub fn holders_in(dir: &Path, path: &str) -> Vec<HoldRecord> {
    let wanted = normalize(path);
    let now = crate::store::now_ms();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for entry in entries.flatten() {
        let file = entry.path();
        if file.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let record = std::fs::read_to_string(&file)
            .ok()
            .and_then(|raw| serde_json::from_str::<HoldRecord>(&raw).ok());
        match record {
            Some(record) if record.expires_at_ms > now => {
                if record.path == wanted {
                    found.push(record);
                }
            }
            // Expired, or a partial/corrupt file left by a crash mid-write.
            _ => {
                let _ = std::fs::remove_file(&file);
            }
        }
    }
    found
}

/// Whether anything currently holds `path`.
#[must_use]
pub fn is_held(path: &str) -> bool {
    !holders(path).is_empty()
}

/// [`is_held`] against an explicit directory.
#[must_use]
pub fn is_held_in(dir: &Path, path: &str) -> bool {
    !holders_in(dir, path).is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// A fresh, empty directory unique to one test.
    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ralphus-checkout-hold-{tag}-{}-{}",
            std::process::id(),
            crate::store::now_ms()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_hold_is_visible_until_its_guard_drops() {
        let dir = scratch("visible");
        let checkout = dir.join("review-postmerge");
        std::fs::create_dir_all(&checkout).unwrap();
        let checkout = checkout.to_string_lossy().to_string();
        let holds = dir.join("holds");

        assert!(!is_held_in(&holds, &checkout));
        let guard = hold_in(&holds, &checkout, "test", Duration::from_secs(60)).unwrap();
        assert!(is_held_in(&holds, &checkout));
        let other = dir.join("review-postmerge-1");
        assert!(!is_held_in(&holds, &other.to_string_lossy()));
        drop(guard);
        assert!(!is_held_in(&holds, &checkout));
    }

    #[test]
    fn two_holds_on_one_checkout_release_independently() {
        let dir = scratch("independent");
        let checkout = dir.to_string_lossy().to_string();
        let holds = dir.join("holds");
        let first = hold_in(&holds, &checkout, "a", Duration::from_secs(60)).unwrap();
        let second = hold_in(&holds, &checkout, "b", Duration::from_secs(60)).unwrap();
        assert_eq!(holders_in(&holds, &checkout).len(), 2);
        drop(first);
        assert!(is_held_in(&holds, &checkout));
        drop(second);
        assert!(!is_held_in(&holds, &checkout));
    }

    #[test]
    fn an_expired_hold_no_longer_blocks_and_is_swept() {
        let dir = scratch("expired");
        let checkout = dir.to_string_lossy().to_string();
        let holds = dir.join("holds");
        let guard = hold_in(&holds, &checkout, "old", Duration::from_secs(0)).unwrap();
        // The guard is still alive, but its run can no longer be.
        assert!(!is_held_in(&holds, &checkout));
        assert!(!guard.file.exists(), "an expired hold file is swept");
    }

    #[test]
    fn a_hold_survives_its_process_forgetting_it() {
        // A restarted daemon has no guard, only the file on disk.
        let dir = scratch("forgotten");
        let checkout = dir.to_string_lossy().to_string();
        let holds = dir.join("holds");
        let guard = hold_in(&holds, &checkout, "run", Duration::from_secs(60)).unwrap();
        std::mem::forget(guard);
        assert!(is_held_in(&holds, &checkout));
    }
}
