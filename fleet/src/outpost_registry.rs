//! The **outpost registry**: what the fleet remembers about its paired
//! outpost machines, in `<config>.outposts.json` (or `outpost_registry`).
//!
//! Per machine: the public key it paired with (never anything that lets the
//! fleet, or a thief of this file, act as the machine), a pending pairing
//! code's derived key and expiry, and the machine's latest readiness report.
//! `ralphus-fleet pair` and `revoke` change it from the command line while
//! `ralphus-fleet serve` runs; `serve` re-reads it when it changes, so a
//! revoke takes effect on the machine's next poll. Every change takes a lock
//! file and writes the whole file atomically (owner-only on Unix).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use serde::{Deserialize, Serialize};

use crate::allocations::now_ms;
use crate::outpost_keys::{self, PAIRING_TTL_MS};
use crate::readiness::Report;

/// Failed pairing attempts that void every pending code.
pub const MAX_PAIR_FAILURES: u32 = 20;

/// The whole registry file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegistryFile {
    #[serde(default)]
    pub machines: BTreeMap<String, Entry>,
}

/// One machine's entry.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    /// The Ed25519 public key (base64) the machine paired with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paired_at_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pairing: Option<PendingPairing>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub readiness: Option<Report>,
}

/// A pairing code waiting to be used.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PendingPairing {
    /// [`outpost_keys::pairing_key`] of the code; the code itself is never
    /// written down.
    pub key: String,
    pub expires_at_ms: i64,
    #[serde(default)]
    pub failures: u32,
}

/// The registry file at one path.
#[derive(Debug, Clone)]
pub struct Registry {
    path: PathBuf,
}

/// A completed pairing, as the fleet answers it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paired {
    pub name: String,
    /// The pairing key that verified, for authenticating the answer.
    pub key: String,
}

impl Registry {
    #[must_use]
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// When the file last changed, if it exists.
    #[must_use]
    pub fn modified(&self) -> Option<SystemTime> {
        std::fs::metadata(&self.path)
            .and_then(|m| m.modified())
            .ok()
    }

    /// Read the registry (empty when the file does not exist).
    ///
    /// # Errors
    /// An unreadable or corrupt file.
    pub fn load(&self) -> Result<RegistryFile, String> {
        match std::fs::read_to_string(&self.path) {
            Ok(text) => serde_json::from_str(&text)
                .map_err(|e| format!("{} is corrupt: {e}", self.path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(RegistryFile::default()),
            Err(e) => Err(format!("could not read {}: {e}", self.path.display())),
        }
    }

    /// Change the registry under its lock: read it fresh, apply `change`,
    /// and write it back when `change` succeeds.
    ///
    /// # Errors
    /// The lock, the read or the write failed, or `change` refused.
    pub fn update<T>(
        &self,
        change: impl FnOnce(&mut RegistryFile) -> Result<T, String>,
    ) -> Result<T, String> {
        let _lock = Lock::take(&self.path)?;
        let mut file = self.load()?;
        let result = change(&mut file)?;
        if let Some(parent) = self.path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("could not create {}: {e}", parent.display()))?;
        }
        let text = serde_json::to_string_pretty(&file).map_err(|e| e.to_string())?;
        crate::secret_store::write_private(&self.path, text.as_bytes())?;
        Ok(result)
    }

    /// Start pairing `name`: a fresh code (shown once, never stored), valid
    /// for [`PAIRING_TTL_MS`], replacing any earlier unused one.
    ///
    /// # Errors
    /// No randomness, or the registry could not be written.
    pub fn start_pairing(&self, name: &str) -> Result<String, String> {
        let code = outpost_keys::new_pairing_code()?;
        let key = outpost_keys::pairing_key(
            &outpost_keys::normalize_pairing_code(&code).ok_or("a malformed pairing code")?,
        );
        self.update(|file| {
            file.machines.entry(name.to_string()).or_default().pairing = Some(PendingPairing {
                key,
                expires_at_ms: now_ms() + PAIRING_TTL_MS,
                failures: 0,
            });
            Ok(())
        })?;
        Ok(code)
    }

    /// Forget `name`'s paired key and any pending code; whether it had one.
    ///
    /// # Errors
    /// The registry could not be written.
    pub fn revoke(&self, name: &str) -> Result<bool, String> {
        self.update(|file| {
            let Some(entry) = file.machines.get_mut(name) else {
                return Ok(false);
            };
            let had = entry.public_key.is_some() || entry.pairing.is_some();
            entry.public_key = None;
            entry.paired_at_ms = None;
            entry.pairing = None;
            Ok(had)
        })
    }

    /// Complete a pairing: find the pending code `verifies` accepts, record
    /// `public_key` for its machine and forget the code. A request no
    /// pending code accepts counts against every pending code, and voids
    /// them after [`MAX_PAIR_FAILURES`].
    ///
    /// # Errors
    /// No pending code accepted it, or the registry could not be written.
    pub fn complete_pairing(
        &self,
        public_key: &str,
        verifies: impl Fn(&str) -> bool,
    ) -> Result<Paired, String> {
        let outcome = self.update(|file| {
            let now = now_ms();
            for entry in file.machines.values_mut() {
                if entry
                    .pairing
                    .as_ref()
                    .is_some_and(|p| p.expires_at_ms <= now)
                {
                    entry.pairing = None;
                }
            }
            let found = file.machines.iter_mut().find_map(|(name, entry)| {
                let key = entry.pairing.as_ref()?.key.clone();
                verifies(&key).then(move || (name.clone(), key, entry))
            });
            if let Some((name, key, entry)) = found {
                entry.public_key = Some(public_key.to_string());
                entry.paired_at_ms = Some(now);
                entry.pairing = None;
                entry.readiness = None;
                return Ok(Some(Paired { name, key }));
            }
            for entry in file.machines.values_mut() {
                if let Some(pending) = entry.pairing.as_mut() {
                    pending.failures += 1;
                    if pending.failures >= MAX_PAIR_FAILURES {
                        entry.pairing = None;
                    }
                }
            }
            Ok(None)
        })?;
        outcome.ok_or_else(|| "no pending pairing code matches".to_string())
    }

    /// Record `name`'s latest readiness report.
    ///
    /// # Errors
    /// The registry could not be written.
    pub fn set_readiness(&self, name: &str, report: &Report) -> Result<(), String> {
        self.update(|file| {
            file.machines.entry(name.to_string()).or_default().readiness = Some(report.clone());
            Ok(())
        })
    }
}

/// An exclusive lock on a registry: `<path>.lock`, created exclusively and
/// removed on drop. A lock older than [`STALE_LOCK`] was left by a process
/// that died and is taken over.
struct Lock {
    path: PathBuf,
}

const STALE_LOCK: Duration = Duration::from_secs(30);

impl Lock {
    fn take(registry: &Path) -> Result<Self, String> {
        let mut name = registry.as_os_str().to_owned();
        name.push(".lock");
        let path = PathBuf::from(name);
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("could not create {}: {e}", parent.display()))?;
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(_) => return Ok(Self { path }),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    let stale = std::fs::metadata(&path)
                        .and_then(|m| m.modified())
                        .ok()
                        .and_then(|at| at.elapsed().ok())
                        .is_some_and(|age| age > STALE_LOCK);
                    if stale {
                        let _ = std::fs::remove_file(&path);
                        continue;
                    }
                    if Instant::now() >= deadline {
                        return Err(format!(
                            "{} is held by another ralphus-fleet; try again",
                            path.display()
                        ));
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
                Err(e) => return Err(format!("could not lock {}: {e}", path.display())),
            }
        }
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::outpost_keys::{Identity, normalize_pairing_code, pairing_key};

    fn registry(dir: &Path) -> Registry {
        Registry::new(dir.join("fleet.toml.outposts.json"))
    }

    #[test]
    fn a_pairing_code_pairs_once_and_is_never_written_down() {
        let dir = ralphus_test_support::TestDir::new("registry-pair");
        let registry = registry(&dir);
        let code = registry.start_pairing("winbox").unwrap();
        let text = std::fs::read_to_string(registry.path()).unwrap();
        assert!(!text.contains(&code), "the code itself is not stored");
        let key = pairing_key(&normalize_pairing_code(&code).unwrap());
        let machine = Identity::generate().unwrap();

        let paired = registry
            .complete_pairing(&machine.public_key(), |k| k == key)
            .unwrap();
        assert_eq!(paired.name, "winbox");
        let file = registry.load().unwrap();
        assert_eq!(
            file.machines["winbox"].public_key.as_deref(),
            Some(machine.public_key().as_str())
        );
        assert!(file.machines["winbox"].pairing.is_none());
        let text = std::fs::read_to_string(registry.path()).unwrap();
        assert!(
            !text.contains(&machine.private_b64()),
            "only the public key is kept"
        );
        assert!(
            registry
                .complete_pairing(&machine.public_key(), |k| k == key)
                .is_err(),
            "a code works once"
        );
    }

    #[test]
    fn wrong_guesses_void_pending_codes_and_revoke_forgets_the_key() {
        let dir = ralphus_test_support::TestDir::new("registry-guess");
        let registry = registry(&dir);
        registry.start_pairing("winbox").unwrap();
        for _ in 0..MAX_PAIR_FAILURES {
            assert!(registry.complete_pairing("pk", |_| false).is_err());
        }
        assert!(
            registry.load().unwrap().machines["winbox"]
                .pairing
                .is_none()
        );

        let code = registry.start_pairing("winbox").unwrap();
        let key = pairing_key(&normalize_pairing_code(&code).unwrap());
        registry.complete_pairing("pk", |k| k == key).unwrap();
        assert!(registry.revoke("winbox").unwrap());
        assert!(
            registry.load().unwrap().machines["winbox"]
                .public_key
                .is_none()
        );
        assert!(!registry.revoke("winbox").unwrap());
        assert!(!registry.revoke("never-paired").unwrap());
    }

    #[test]
    fn an_expired_code_does_not_pair() {
        let dir = ralphus_test_support::TestDir::new("registry-expired");
        let registry = registry(&dir);
        let code = registry.start_pairing("winbox").unwrap();
        registry
            .update(|file| {
                file.machines
                    .get_mut("winbox")
                    .and_then(|e| e.pairing.as_mut())
                    .ok_or("no pairing")?
                    .expires_at_ms = now_ms() - 1;
                Ok(())
            })
            .unwrap();
        let key = pairing_key(&normalize_pairing_code(&code).unwrap());
        assert!(registry.complete_pairing("pk", |k| k == key).is_err());
    }
}
