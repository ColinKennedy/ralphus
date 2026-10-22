//! Where long-lived secrets live: an outpost's machine credential and the
//! fleet's identity key.
//!
//! [`OsStore`] is the operating system's encrypted per-user store -- the
//! Windows Credential Manager (DPAPI-encrypted under the user's login), the
//! macOS Keychain, or the Linux Secret Service (GNOME Keyring, KWallet). Only
//! where no such store is reachable (a headless Linux host with no Secret
//! Service) does a secret fall back to a [`FileStore`]: one file readable
//! and writable by its owner only (`0600`), with a warning saying so. A
//! secret is never written to a configuration file, a command line or a log.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// A place secrets are kept, by key.
pub trait SecretStore: Send + Sync {
    /// The secret stored under `key`, if any.
    ///
    /// # Errors
    /// The store could not be read.
    fn get(&self, key: &str) -> Result<Option<String>, String>;
    /// Store `value` under `key`, replacing any previous value.
    ///
    /// # Errors
    /// The store could not be written.
    fn set(&self, key: &str, value: &str) -> Result<(), String>;
    /// Forget `key`; whether there was anything to forget.
    ///
    /// # Errors
    /// The store could not be written.
    fn delete(&self, key: &str) -> Result<bool, String>;
    /// What this store is, for messages (never a secret).
    fn describe(&self) -> String;
}

/// Which kind of store holds a secret. Recorded (not the secret) so the
/// reader looks in the same place the writer used.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StoreKind {
    /// The OS's encrypted per-user store.
    Os,
    /// An owner-only file.
    File,
}

/// The operating system's encrypted per-user secret store, entries named
/// `<service>` / `<key>`.
#[derive(Debug, Clone)]
pub struct OsStore {
    service: String,
}

impl OsStore {
    #[must_use]
    pub fn new(service: &str) -> Self {
        Self {
            service: service.to_string(),
        }
    }

    fn entry(&self, key: &str) -> Result<keyring::Entry, String> {
        keyring::Entry::new(&self.service, key).map_err(|e| describe_keyring_error(&e))
    }
}

fn describe_keyring_error(error: &keyring::Error) -> String {
    // keyring's errors never carry the secret itself.
    describe_store_failure(&error.to_string())
}

/// The message for an OS-store failure, with what to do about the one a
/// user is likely to hit: Windows has no Credential Manager in a session
/// signed in over SSH with a key (`ERROR_NO_SUCH_LOGON_SESSION`), so the
/// store refuses there even though it works at the desktop.
fn describe_store_failure(error: &str) -> String {
    let mut message = format!("the OS credential store refused: {error}");
    if error.contains("ERROR_NO_SUCH_LOGON_SESSION") {
        message.push_str(
            ". This session cannot use the Windows Credential Manager -- a session signed \
             in over SSH with a key has no access to it, even when the credential is there. \
             Run this at the machine's desktop (or over Remote Desktop), or from SSH through \
             a one-time Scheduled Task that runs in your logon session: `schtasks /create \
             /tn ralphus-outpost-once /sc once /st 00:00 /it /tr \"<this command>\"`, then \
             `schtasks /run /tn ralphus-outpost-once` and `schtasks /delete /tn \
             ralphus-outpost-once /f`",
        );
    }
    message
}

/// The name of this platform's OS store, for messages.
#[must_use]
pub fn os_store_name() -> &'static str {
    if cfg!(windows) {
        "the Windows Credential Manager"
    } else if cfg!(target_os = "macos") {
        "the macOS Keychain"
    } else {
        "the Secret Service (login keyring)"
    }
}

impl SecretStore for OsStore {
    fn get(&self, key: &str) -> Result<Option<String>, String> {
        match self.entry(key)?.get_password() {
            Ok(value) => Ok(Some(value)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(describe_keyring_error(&e)),
        }
    }

    fn set(&self, key: &str, value: &str) -> Result<(), String> {
        self.entry(key)?
            .set_password(value)
            .map_err(|e| describe_keyring_error(&e))
    }

    fn delete(&self, key: &str) -> Result<bool, String> {
        match self.entry(key)?.delete_credential() {
            Ok(()) => Ok(true),
            Err(keyring::Error::NoEntry) => Ok(false),
            Err(e) => Err(describe_keyring_error(&e)),
        }
    }

    fn describe(&self) -> String {
        format!("{} (entry {:?})", os_store_name(), self.service)
    }
}

/// Secrets as owner-only files in one directory: `<dir>/<key>.secret`.
#[derive(Debug, Clone)]
pub struct FileStore {
    dir: PathBuf,
}

impl FileStore {
    #[must_use]
    pub fn new(dir: &Path) -> Self {
        Self {
            dir: dir.to_path_buf(),
        }
    }

    fn path(&self, key: &str) -> PathBuf {
        let safe: String = key
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        self.dir.join(format!("{safe}.secret"))
    }
}

impl SecretStore for FileStore {
    fn get(&self, key: &str) -> Result<Option<String>, String> {
        match std::fs::read_to_string(self.path(key)) {
            Ok(value) => Ok(Some(value)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("could not read {}: {e}", self.path(key).display())),
        }
    }

    fn set(&self, key: &str, value: &str) -> Result<(), String> {
        create_private_dir(&self.dir)?;
        write_private(&self.path(key), value.as_bytes())
    }

    fn delete(&self, key: &str) -> Result<bool, String> {
        match std::fs::remove_file(self.path(key)) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(format!(
                "could not remove {}: {e}",
                self.path(key).display()
            )),
        }
    }

    fn describe(&self) -> String {
        format!("owner-only files in {}", self.dir.display())
    }
}

/// Create `dir` (and its parents), owner-only on Unix.
///
/// # Errors
/// The directory could not be created.
pub fn create_private_dir(dir: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("could not create {}: {e}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| format!("could not restrict {}: {e}", dir.display()))?;
    }
    Ok(())
}

/// Write `bytes` to `path` atomically, readable and writable by its owner
/// only on Unix (on Windows the file inherits its per-user directory's
/// access list).
///
/// # Errors
/// The file could not be written.
pub fn write_private(path: &Path, bytes: &[u8]) -> Result<(), String> {
    use std::io::Write as _;
    let mut temporary = path.as_os_str().to_owned();
    temporary.push(format!(".{}.tmp", std::process::id()));
    let temporary = PathBuf::from(temporary);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let result = options
        .open(&temporary)
        .and_then(|mut file| {
            file.write_all(bytes)?;
            file.sync_all()
        })
        .and_then(|()| rename_with_retry(&temporary, path));
    if let Err(e) = result {
        let _ = std::fs::remove_file(&temporary);
        return Err(format!("could not write {}: {e}", path.display()));
    }
    Ok(())
}

/// `rename`, retried briefly: on Windows a scanner or indexer holding the
/// freshly written file open makes it fail with a sharing violation.
fn rename_with_retry(from: &Path, to: &Path) -> std::io::Result<()> {
    let mut attempt = 0;
    loop {
        match std::fs::rename(from, to) {
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied && attempt < 20 => {
                attempt += 1;
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
            other => return other,
        }
    }
}

/// Store `value` under `key` in the OS store, or -- only where the OS store
/// is unreachable and this platform allows it (not Windows, whose
/// Credential Manager is always there) -- in an owner-only file under
/// `fallback_dir`. Returns where it went and, for the file fallback, the
/// warning to show.
///
/// # Errors
/// Neither store could keep it.
pub fn store_with_fallback(
    os: &dyn SecretStore,
    fallback_dir: &Path,
    key: &str,
    value: &str,
) -> Result<(StoreKind, Option<String>), String> {
    match os.set(key, value) {
        Ok(()) => Ok((StoreKind::Os, None)),
        Err(os_error) if cfg!(windows) => Err(os_error),
        Err(os_error) => {
            let file = FileStore::new(fallback_dir);
            file.set(key, value)?;
            Ok((
                StoreKind::File,
                Some(format!(
                    "{os_error}; the secret was written to {} instead (an owner-only 0600 file, \
                     NOT encrypted at rest). Anyone who can read your files as you, or a backup \
                     of them, can use it. Start a Secret Service (gnome-keyring, KWallet) and \
                     run install again to move it there",
                    file.path(key).display()
                )),
            ))
        }
    }
}

/// The store of `kind`.
#[must_use]
pub fn open(kind: StoreKind, service: &str, fallback_dir: &Path) -> Box<dyn SecretStore> {
    match kind {
        StoreKind::Os => Box::new(OsStore::new(service)),
        StoreKind::File => Box::new(FileStore::new(fallback_dir)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_session_without_the_credential_manager_says_where_to_run_instead() {
        let message = describe_store_failure(
            "Couldn't access platform secure storage: Windows ERROR_NO_SUCH_LOGON_SESSION",
        );
        assert!(
            message.contains("signed in over SSH with a key"),
            "{message}"
        );
        assert!(message.contains("schtasks /create"), "{message}");
        let other = describe_store_failure("no such entry");
        assert_eq!(other, "the OS credential store refused: no such entry");
    }

    struct Refusing;

    impl SecretStore for Refusing {
        fn get(&self, _: &str) -> Result<Option<String>, String> {
            Err("no store".to_string())
        }
        fn set(&self, _: &str, _: &str) -> Result<(), String> {
            Err("no Secret Service".to_string())
        }
        fn delete(&self, _: &str) -> Result<bool, String> {
            Err("no store".to_string())
        }
        fn describe(&self) -> String {
            "refusing".to_string()
        }
    }

    #[test]
    fn a_file_store_keeps_owner_only_secrets() {
        let dir = ralphus_test_support::TestDir::new("secret-store-file");
        let store = FileStore::new(&dir.join("secrets"));
        assert_eq!(store.get("a/b").unwrap(), None);
        store.set("a/b", "value-1").unwrap();
        store.set("a/b", "value-2").unwrap();
        assert_eq!(store.get("a/b").unwrap().as_deref(), Some("value-2"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(store.path("a/b"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        assert!(store.delete("a/b").unwrap());
        assert!(!store.delete("a/b").unwrap());
    }

    #[test]
    fn an_unreachable_os_store_falls_back_to_a_file_only_off_windows() {
        let dir = ralphus_test_support::TestDir::new("secret-store-fallback");
        let result = store_with_fallback(&Refusing, &dir, "credential", "s3cret-value");
        if cfg!(windows) {
            assert!(result.is_err(), "Windows never falls back to a file");
        } else {
            let (kind, warning) = result.unwrap();
            assert_eq!(kind, StoreKind::File);
            let warning = warning.unwrap();
            assert!(warning.contains("0600"), "{warning}");
            assert!(!warning.contains("s3cret-value"), "never show the secret");
            assert_eq!(
                FileStore::new(&dir).get("credential").unwrap().as_deref(),
                Some("s3cret-value")
            );
        }
    }
}
