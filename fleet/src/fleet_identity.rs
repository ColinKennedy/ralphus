//! The fleet's **identity key**: the Ed25519 key pair paired outposts know
//! this fleet by. Each paired outpost pinned its public half when it paired
//! and will take requests only from whoever can sign with the private half.
//!
//! It is kept in the OS's encrypted per-user store of the user running
//! `ralphus-fleet serve` (an owner-only file where none is reachable; see
//! [`crate::secret_store`]) under a key derived from the registry's path, so
//! two fleets on one host have two identities. Losing it means re-pairing
//! every outpost, so a new one is made only when none can have existed.

use std::path::Path;

use ring::digest;

use crate::outpost_keys::Identity;
use crate::secret_store::{FileStore, SecretStore, store_with_fallback};

/// The OS store service name the identity is kept under.
pub const SERVICE: &str = "ralphus-fleet";

/// The store key for the fleet whose registry is at `registry_path`.
#[must_use]
pub fn key_for(registry_path: &Path) -> String {
    let canonical = std::fs::canonicalize(registry_path.parent().unwrap_or(Path::new(".")))
        .unwrap_or_else(|_| registry_path.to_path_buf())
        .join(registry_path.file_name().unwrap_or_default());
    let hash: String = digest::digest(&digest::SHA256, canonical.to_string_lossy().as_bytes())
        .as_ref()
        .iter()
        .take(8)
        .map(|b| format!("{b:02x}"))
        .collect();
    format!("identity-{hash}")
}

/// Load the identity stored under `key`, or make and store one.
///
/// `has_pairings` says whether any machine is already paired: then an
/// identity must exist, and a store that cannot be read is an error rather
/// than a reason to make a new one (which would silently unpair them all).
///
/// # Errors
/// The stores could not be read or written, or hold something that is not a
/// key.
pub fn load_or_create(
    os: &dyn SecretStore,
    fallback_dir: &Path,
    key: &str,
    has_pairings: bool,
) -> Result<Identity, String> {
    let file = FileStore::new(fallback_dir);
    let from_os = os.get(key);
    if let Ok(Some(text)) = &from_os {
        return Identity::from_private_b64(text.trim());
    }
    if let Some(text) = file.get(key)? {
        return Identity::from_private_b64(text.trim());
    }
    if let Err(e) = &from_os {
        if has_pairings || cfg!(windows) {
            return Err(format!(
                "could not read this fleet's identity key from {}: {e}",
                os.describe()
            ));
        }
    } else if has_pairings {
        return Err(format!(
            "this fleet's identity key is missing from {} although machines are paired with it; \
             re-pair them (`ralphus-fleet pair <machine>`) after removing their registry entries \
             with `ralphus-fleet revoke <machine>`",
            os.describe()
        ));
    }
    let identity = Identity::generate()?;
    let (_, warning) = store_with_fallback(os, fallback_dir, key, &identity.private_b64())?;
    if let Some(warning) = warning {
        eprintln!("ralphus-fleet: warning: {warning}");
    }
    Ok(identity)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Locked;

    impl SecretStore for Locked {
        fn get(&self, _: &str) -> Result<Option<String>, String> {
            Err("the keyring is locked".to_string())
        }
        fn set(&self, _: &str, _: &str) -> Result<(), String> {
            Err("the keyring is locked".to_string())
        }
        fn delete(&self, _: &str) -> Result<bool, String> {
            Err("the keyring is locked".to_string())
        }
        fn describe(&self) -> String {
            "a locked keyring".to_string()
        }
    }

    #[test]
    fn the_identity_is_made_once_and_kept() {
        let dir = ralphus_test_support::TestDir::new("fleet-identity");
        let os = FileStore::new(&dir.join("os"));
        let first = load_or_create(&os, &dir.join("fallback"), "identity-x", false).unwrap();
        let again = load_or_create(&os, &dir.join("fallback"), "identity-x", true).unwrap();
        assert_eq!(first.public_key(), again.public_key());
    }

    #[test]
    fn an_unreadable_store_never_replaces_an_identity_machines_paired_with() {
        let dir = ralphus_test_support::TestDir::new("fleet-identity-locked");
        let err = load_or_create(&Locked, &dir, "identity-x", true).unwrap_err();
        assert!(err.contains("locked"), "{err}");
        let missing =
            load_or_create(&FileStore::new(&dir.join("os")), &dir, "identity-y", true).unwrap_err();
        assert!(missing.contains("missing"), "{missing}");
    }

    #[test]
    fn the_key_differs_per_registry() {
        let dir = ralphus_test_support::TestDir::new("fleet-identity-key");
        assert_ne!(
            key_for(&dir.join("a.outposts.json")),
            key_for(&dir.join("b.outposts.json"))
        );
    }
}
