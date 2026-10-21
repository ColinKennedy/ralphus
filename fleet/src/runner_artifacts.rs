//! The `ralphus-runner` binaries the fleet hands its outposts
//! (`outpost_runners` in `ralphus-fleet.toml`).
//!
//! Each poll answer tells an outpost the sha256 and size of the runner for
//! its os/arch; an outpost whose cached runner differs downloads it over the
//! sealed channel, checks the sha256, and runs new jobs with it. The hash is
//! recomputed only when the file's size or modification time changes, so
//! replacing the file on the fleet host rolls every outpost of that platform
//! forward at its next poll.

use std::collections::BTreeMap;
use std::io::Read as _;
use std::sync::Mutex;
use std::time::SystemTime;

use ring::digest;
use serde_json::{Value, json};

/// What one runner file looked like when it was last hashed.
#[derive(Debug, Clone)]
struct Hashed {
    len: u64,
    modified: Option<SystemTime>,
    sha256: String,
}

/// `<os>-<arch>` → runner file, with cached hashes.
#[derive(Debug, Default)]
pub struct RunnerArtifacts {
    paths: BTreeMap<String, String>,
    hashed: Mutex<BTreeMap<String, Hashed>>,
}

/// The sha256 of `bytes`, as lowercase hex.
#[must_use]
pub fn sha256_hex(bytes: &[u8]) -> String {
    digest::digest(&digest::SHA256, bytes)
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn hash_file(path: &str) -> Result<String, String> {
    let mut file = std::fs::File::open(path).map_err(|e| format!("could not open {path}: {e}"))?;
    let mut context = digest::Context::new(&digest::SHA256);
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        let n = file
            .read(&mut buffer)
            .map_err(|e| format!("could not read {path}: {e}"))?;
        if n == 0 {
            break;
        }
        context.update(&buffer[..n]);
    }
    Ok(context
        .finish()
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

impl RunnerArtifacts {
    #[must_use]
    pub fn new(paths: BTreeMap<String, String>) -> Self {
        Self {
            paths,
            hashed: Mutex::new(BTreeMap::new()),
        }
    }

    fn platform(os: &str, arch: &str) -> String {
        format!("{os}-{arch}")
    }

    /// `{"sha256", "size"}` of the runner for `os`/`arch`, or `None` when
    /// none is configured (or it cannot be read, which is logged).
    #[must_use]
    pub fn expected(&self, os: &str, arch: &str) -> Option<Value> {
        let platform = Self::platform(os, arch);
        let path = self.paths.get(&platform)?;
        let metadata = match std::fs::metadata(path) {
            Ok(m) => m,
            Err(e) => {
                eprintln!("ralphus-fleet: outpost runner for {platform} ({path}): {e}");
                return None;
            }
        };
        let modified = metadata.modified().ok();
        let mut hashed = self
            .hashed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let fresh = hashed
            .get(&platform)
            .is_some_and(|h| h.len == metadata.len() && h.modified == modified);
        if !fresh {
            match hash_file(path) {
                Ok(sha256) => {
                    hashed.insert(
                        platform.clone(),
                        Hashed {
                            len: metadata.len(),
                            modified,
                            sha256,
                        },
                    );
                }
                Err(e) => {
                    eprintln!("ralphus-fleet: outpost runner for {platform}: {e}");
                    return None;
                }
            }
        }
        hashed
            .get(&platform)
            .map(|h| json!({"sha256": h.sha256, "size": h.len}))
    }

    /// The runner for `os`/`arch`, which must still hash to `sha256`.
    ///
    /// # Errors
    /// None is configured, it cannot be read, or it changed since the
    /// outpost was told its hash (it asks again at its next poll).
    pub fn read(&self, os: &str, arch: &str, sha256: &str) -> Result<Vec<u8>, String> {
        let platform = Self::platform(os, arch);
        let path = self
            .paths
            .get(&platform)
            .ok_or_else(|| format!("this fleet has no outpost runner for {platform}"))?;
        let bytes = std::fs::read(path).map_err(|e| format!("could not read {path}: {e}"))?;
        if sha256_hex(&bytes) != sha256 {
            return Err(format!(
                "the outpost runner for {platform} changed; ask again"
            ));
        }
        Ok(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_replaced_runner_gets_a_new_hash_and_a_stale_hash_is_refused() {
        let dir = ralphus_test_support::TestDir::new("runner-artifacts");
        let path = dir.join("ralphus-runner");
        std::fs::write(&path, b"runner one").unwrap();
        let artifacts = RunnerArtifacts::new(BTreeMap::from([(
            "linux-x86_64".to_string(),
            path.to_string_lossy().into_owned(),
        )]));
        assert!(artifacts.expected("windows", "x86_64").is_none());
        let first = artifacts.expected("linux", "x86_64").unwrap();
        assert_eq!(first["sha256"], sha256_hex(b"runner one"));
        assert_eq!(first["size"], 10);
        assert_eq!(
            artifacts
                .read("linux", "x86_64", &sha256_hex(b"runner one"))
                .unwrap(),
            b"runner one"
        );

        std::fs::write(&path, b"runner two, longer").unwrap();
        let second = artifacts.expected("linux", "x86_64").unwrap();
        assert_eq!(second["sha256"], sha256_hex(b"runner two, longer"));
        assert!(
            artifacts
                .read("linux", "x86_64", &sha256_hex(b"runner one"))
                .is_err()
        );
    }
}
