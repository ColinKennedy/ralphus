//! An installed outpost's files: its **home** directory and the settings in
//! it (`outpost.toml` -- the fleet URL, machine name, `remote_root`, and
//! which store holds the machine credential; never the credential itself).
//!
//! | Platform | Home |
//! |---|---|
//! | Windows | `%LOCALAPPDATA%\ralphus\outpost` |
//! | macOS | `~/Library/Application Support/ralphus/outpost` |
//! | Linux | `$XDG_STATE_HOME/ralphus/outpost` (`~/.local/state/ralphus/outpost`) |
//!
//! `RALPHUS_OUTPOST_HOME` overrides it (one user running two outposts).
//! Inside: `outpost.toml`, `bin/` (the installed executable), `logs/`,
//! `status.json` (the connection state `status` shows) and, only where the
//! OS store is unreachable, `secret/` (the owner-only credential file).

use std::path::{Path, PathBuf};

use ralphus_fleet::secret_store::{self, SecretStore as _, StoreKind};
use serde::{Deserialize, Serialize};

use crate::pairing::Credential;

/// The OS store service name machine credentials are kept under.
pub const CREDENTIAL_SERVICE: &str = "ralphus-outpost";
/// The environment variable that overrides the home directory.
pub const HOME_ENV: &str = "RALPHUS_OUTPOST_HOME";

/// An installed outpost's settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    pub fleet_url: String,
    pub name: String,
    pub remote_root: String,
    /// Which store holds the machine credential.
    pub credential_store: StoreKind,
}

/// The default home directory, or `RALPHUS_OUTPOST_HOME`.
///
/// # Errors
/// The platform's per-user directory is unknown.
pub fn default_home() -> Result<PathBuf, String> {
    if let Some(home) = std::env::var_os(HOME_ENV).filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(home));
    }
    let var = |name: &str| {
        std::env::var_os(name)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    };
    let base = if cfg!(windows) {
        var("LOCALAPPDATA").ok_or("LOCALAPPDATA is not set")?
    } else if cfg!(target_os = "macos") {
        var("HOME")
            .ok_or("HOME is not set")?
            .join("Library/Application Support")
    } else {
        match var("XDG_STATE_HOME") {
            Some(state) => state,
            None => var("HOME").ok_or("HOME is not set")?.join(".local/state"),
        }
    };
    Ok(base.join("ralphus").join("outpost"))
}

/// Paths inside one home directory.
#[derive(Debug, Clone)]
pub struct Home {
    pub dir: PathBuf,
}

impl Home {
    #[must_use]
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    #[must_use]
    pub fn settings_file(&self) -> PathBuf {
        self.dir.join("outpost.toml")
    }

    #[must_use]
    pub fn log_file(&self) -> PathBuf {
        self.dir.join("logs").join("outpost.log")
    }

    #[must_use]
    pub fn status_file(&self) -> PathBuf {
        self.dir.join("status.json")
    }

    #[must_use]
    pub fn secret_dir(&self) -> PathBuf {
        self.dir.join("secret")
    }

    #[must_use]
    pub fn bin_dir(&self) -> PathBuf {
        self.dir.join("bin")
    }

    /// The store key of this home's credential: one per home directory, so
    /// two outposts of one user keep two credentials.
    #[must_use]
    pub fn credential_key(&self) -> String {
        let hash = ralphus_fleet::runner_artifacts::sha256_hex(
            self.dir.to_string_lossy().to_lowercase().as_bytes(),
        );
        format!("credential-{}", &hash[..16])
    }

    /// The settings, if installed.
    ///
    /// # Errors
    /// The settings file is unreadable or invalid.
    pub fn load(&self) -> Result<Option<Settings>, String> {
        let path = self.settings_file();
        match std::fs::read_to_string(&path) {
            Ok(text) => toml::from_str(&text)
                .map(Some)
                .map_err(|e| format!("{} is invalid: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("could not read {}: {e}", path.display())),
        }
    }

    /// Write the settings.
    ///
    /// # Errors
    /// The file could not be written.
    pub fn save(&self, settings: &Settings) -> Result<(), String> {
        secret_store::create_private_dir(&self.dir)?;
        let text = toml::to_string_pretty(settings).map_err(|e| e.to_string())?;
        secret_store::write_private(&self.settings_file(), text.as_bytes())
    }

    /// Keep `credential` in the OS store (or, where that is unreachable off
    /// Windows, an owner-only file); which store, and any warning to show.
    ///
    /// # Errors
    /// Neither store could keep it.
    pub fn store_credential(
        &self,
        os: &dyn secret_store::SecretStore,
        credential: &Credential,
    ) -> Result<(StoreKind, Option<String>), String> {
        secret_store::store_with_fallback(
            os,
            &self.secret_dir(),
            &self.credential_key(),
            &credential.to_secret(),
        )
    }

    /// The machine credential from the store `kind`.
    ///
    /// # Errors
    /// The store could not be read or holds no credential.
    pub fn load_credential(
        &self,
        os: &dyn secret_store::SecretStore,
        kind: StoreKind,
    ) -> Result<Credential, String> {
        let file = secret_store::FileStore::new(&self.secret_dir());
        let store: &dyn secret_store::SecretStore = match kind {
            StoreKind::Os => os,
            StoreKind::File => &file,
        };
        let text = store.get(&self.credential_key())?.ok_or_else(|| {
            format!(
                "no machine credential in {}; pair again with `ralphus-outpost install --fleet \
                 <url> --code <code>`",
                store.describe()
            )
        })?;
        Credential::from_secret(&text)
    }

    /// Forget the machine credential from both stores.
    pub fn delete_credential(&self, os: &dyn secret_store::SecretStore) {
        let _ = os.delete(&self.credential_key());
        let _ = secret_store::FileStore::new(&self.secret_dir()).delete(&self.credential_key());
    }
}

/// `path` with forward slashes, as `remote_root` is given.
#[must_use]
pub fn slashed(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use ralphus_fleet::outpost_keys::Identity;
    use ralphus_fleet::secret_store::FileStore;
    use std::sync::Arc;

    #[test]
    fn settings_hold_no_secret_and_the_credential_round_trips_through_a_store() {
        let dir = ralphus_test_support::TestDir::new("outpost-settings");
        let home = Home::new(dir.join("home"));
        assert_eq!(home.load().unwrap(), None);
        let os = FileStore::new(&dir.join("os-store"));
        let credential = Credential {
            machine: Arc::new(Identity::generate().unwrap()),
            fleet_public_key: Identity::generate().unwrap().public_key(),
        };
        let (kind, warning) = home.store_credential(&os, &credential).unwrap();
        assert_eq!(kind, StoreKind::Os);
        assert!(warning.is_none());
        let settings = Settings {
            fleet_url: "http://fleet:7900".to_string(),
            name: "winbox".to_string(),
            remote_root: "C:/r".to_string(),
            credential_store: kind,
        };
        home.save(&settings).unwrap();
        assert_eq!(home.load().unwrap(), Some(settings));
        let text = std::fs::read_to_string(home.settings_file()).unwrap();
        assert!(!text.contains(&credential.machine.private_b64()));
        let loaded = home.load_credential(&os, StoreKind::Os).unwrap();
        assert_eq!(loaded.machine.public_key(), credential.machine.public_key());
        home.delete_credential(&os);
        assert!(home.load_credential(&os, StoreKind::Os).is_err());
    }
}
