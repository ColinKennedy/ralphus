//! Shared helpers for isolating a spawned agent session (Claude Code, Codex,
//! Pi) from the operator's personal CLI configuration and memory on the host
//! machine (RAL-336). By default a ralphus-spawned session should not read or
//! write the operator's real `~/.claude`, `~/.codex`, or Pi config directory
//! -- each backend redirects its own config-dir env var to a per-worktree
//! directory under here instead, only when its resolved
//! `allow_personal_settings`/`allow_personal_memory` options call for it.

use std::path::{Path, PathBuf};

/// The operator's home directory (`USERPROFILE` on Windows, `HOME`
/// elsewhere) -- used as the fallback root for a backend's real, ambient
/// config directory when its own dedicated env var
/// (`CLAUDE_CONFIG_DIR`/`CODEX_HOME`/`PI_CODING_AGENT_DIR`) isn't set, so
/// isolation still has somewhere to look for an existing login/settings file
/// worth preserving.
#[must_use]
pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
}

/// A per-worktree, per-backend directory to redirect an agent CLI's config-dir
/// env var to, so isolated runs don't read or write the operator's real
/// config/memory. Distinct workspaces never share a directory, so concurrent
/// cells never collide -- same "keyed off the workspace root's basename"
/// convention as [`crate::cli_agent_common::live_session_path`].
#[must_use]
pub fn isolated_config_dir(workspace_root: &Path, backend: &str) -> PathBuf {
    let basename = workspace_root
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "workspace".to_string());
    isolation_root().join(backend).join(basename)
}

/// The directory every backend's isolated config directories live under.
fn isolation_root() -> PathBuf {
    #[cfg(test)]
    if let Some(root) = test_hooks::ISOLATION_ROOT.with(|r| r.borrow().clone()) {
        return root;
    }
    std::env::temp_dir().join("ralphus").join("agent_isolation")
}

/// Best-effort: copy `filename` from `real_dir` into `isolated_dir` (creating
/// `isolated_dir` first if needed) before a backend's config dir is
/// redirected, so an OAuth/subscription login already stored on disk in the
/// operator's real config dir keeps working under isolation instead of
/// silently forcing a re-login. Every failure (missing source file, missing
/// real dir, a create/copy error) is non-fatal -- isolation must never fail a
/// cell just because there was nothing to preserve. A missing source is the
/// normal "nothing to preserve" case and stays silent; a create/copy error
/// on a file that does exist is recorded as a warning, since it is the likely
/// cause of an unexpected re-login prompt under isolation.
pub fn preserve_auth_file(real_dir: Option<&Path>, isolated_dir: &Path, filename: &str) {
    let _ = copy_into_isolation(real_dir, isolated_dir, filename);
}

/// [`preserve_auth_file`], returning the original's path and the exact bytes
/// written into the copy when the copy was made.
fn copy_into_isolation(
    real_dir: Option<&Path>,
    isolated_dir: &Path,
    filename: &str,
) -> Option<(PathBuf, Vec<u8>)> {
    let real_dir = real_dir?;
    let source = real_dir.join(filename);
    if !source.is_file() {
        return None;
    }
    if let Err(e) = create_private_dir(isolated_dir) {
        emit_isolation_warning(
            "could not create isolated config directory",
            isolated_dir,
            filename,
            &e,
        );
        return None;
    }
    let copy = isolated_dir.join(filename);
    let copied = std::fs::read(&source).and_then(|bytes| {
        write_owner_only(&copy, &bytes)?;
        Ok(bytes)
    });
    match copied {
        Ok(bytes) => {
            crate::cartographer::emit(
                "runner",
                "preserved file into isolated config directory",
                "debug",
                crate::cartographer::EventContext::default(),
                serde_json::json!({
                    "isolated_dir": isolated_dir.display().to_string(),
                    "file": filename,
                }),
            );
            Some((source, bytes))
        }
        Err(e) => {
            emit_isolation_warning(
                "could not preserve file into isolated config directory",
                isolated_dir,
                filename,
                &e,
            );
            None
        }
    }
}

/// Write `bytes` to `path`, leaving it readable only by its owner.
fn write_owner_only(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    drop(file);
    make_owner_only(path)
}

/// The filenames a Claude Code isolated config directory receives from the
/// operator's real one; every one is removed when the run ends.
pub const CLAUDE_CODE_PRESERVED_FILES: &[&str] =
    &[".credentials.json", "settings.json", "CLAUDE.md"];

/// The login a Codex or Pi isolated config directory receives from the
/// operator's real one, removed when the run ends. Their other preserved
/// files hold no secret (Codex's `config.toml` is also rewritten by
/// capability provisioning), so they stay.
pub const AUTH_JSON_PRESERVED_FILES: &[&str] = &["auth.json"];

/// Prefix of the marker file a live [`IsolationLease`] keeps in its isolated
/// directory: `<prefix><pid>-<seq>`, where `pid` is the runner process.
const LEASE_MARKER_PREFIX: &str = ".ralphus-lease-";

/// One agent run's hold on an isolated config directory, and the files it
/// copied into it from the operator's real config directory (a login among
/// them).
///
/// The copies are removed when the lease is released or dropped -- success,
/// error, timeout and cancel paths all leave a backend's `run()` through the
/// drop. A run whose runner process dies before the drop leaves its copies
/// behind; the next lease taken for the same backend on the same machine
/// sweeps them ([`IsolationLease::acquire`]).
///
/// Every check-then-act step on a directory runs under its [`DirLock`]:
/// acquire's marker write, each preserve copy, release's "remove own marker,
/// look for a live one, delete the copies", and the sweep's per-directory
/// "look for a live marker, delete the copies". A newcomer's marker is
/// therefore either visible to a concurrent release/sweep before it decides
/// to delete, or written only after that deletion finished -- never in
/// between, where the deletion would take the newcomer's fresh login.
///
/// Rule shared by release and the sweep: copies in a directory are removed
/// only while no *live* lease marker remains in it. A marker is live when the
/// process whose pid it names is still running, so a concurrently running
/// cell's directory (and a second run in the same workspace) is never
/// stripped. A dead runner's pid that has since been reused by an unrelated
/// process keeps its directory spared until that process exits -- the sweep
/// errs on the side of not deleting.
///
/// Before a login copy is removed -- by release or by the sweep -- a refresh
/// the agent made inside it is written back to the original
/// ([`sync_back`]), and a copy that cannot be written back is kept rather
/// than deleted: OAuth refresh tokens rotate, so the copy may hold the only
/// valid one.
///
/// The directory itself stays: it also holds the agent's own session
/// transcripts, which a later resume of the same cell reads.
#[must_use = "the preserved files are removed as soon as the lease is dropped"]
pub struct IsolationLease {
    dir: PathBuf,
    marker: Option<PathBuf>,
    preserved_names: &'static [&'static str],
    released: bool,
}

impl IsolationLease {
    /// Take a lease on `isolated_dir` (creating it owner-only), then sweep the
    /// preserved files out of every sibling directory -- the other workspaces
    /// of the same backend under `isolated_dir`'s parent -- that no live
    /// lease holds. `preserved_names` is every filename this backend ever
    /// copies in; those are what release and the sweep remove.
    pub fn acquire(isolated_dir: &Path, preserved_names: &'static [&'static str]) -> Self {
        let marker = match create_private_dir(isolated_dir) {
            Ok(()) => {
                // Without the lock the marker is still written: a login
                // left unprotected beats a run with no login at all.
                let _lock = DirLock::acquire(isolated_dir);
                write_lease_marker(isolated_dir)
            }
            Err(e) => {
                emit_isolation_warning(
                    "could not create isolated config directory",
                    isolated_dir,
                    "",
                    &e,
                );
                None
            }
        };
        if let Some(backend_root) = isolated_dir.parent() {
            sweep_stale_isolation_dirs(backend_root, isolated_dir, preserved_names);
        }
        Self {
            dir: isolated_dir.to_path_buf(),
            marker,
            preserved_names,
            released: false,
        }
    }

    /// The isolated directory this lease holds.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Copy `filename` from `real_dir` into the leased directory -- see
    /// [`preserve_auth_file`]. Only a `filename` among the lease's preserved
    /// names is removed when the run ends; any other copy stays.
    ///
    /// A login ([`SYNC_BACK_NAMES`]) is recorded with its original's path and
    /// the hash of what was copied, so a refresh the agent makes inside the
    /// copy is written back to the original before the copy is removed. A
    /// copy already in the directory (a still-running or crashed run's) is
    /// synced back first rather than overwritten; if that fails the existing
    /// copy is kept as is, since it may hold the only valid token.
    pub fn preserve(&self, real_dir: Option<&Path>, filename: &str) {
        let lock = DirLock::acquire(&self.dir);
        let is_login = SYNC_BACK_NAMES.contains(&filename);
        if is_login
            && self.dir.join(filename).is_file()
            && (lock.is_none() || sync_back(&self.dir, filename).is_err())
        {
            return;
        }
        let copied = copy_into_isolation(real_dir, &self.dir, filename);
        if !is_login {
            return;
        }
        match copied {
            Some((source, bytes)) => write_origin(&self.dir, filename, &source, &bytes),
            None => {
                let _ = remove_file_retrying(&origin_path(&self.dir, filename));
            }
        }
    }

    /// Drop this run's marker and, unless another live lease still holds the
    /// directory, remove the preserved files. `Drop` calls it; calling it
    /// first is harmless.
    pub fn release(&mut self) {
        if self.released {
            return;
        }
        self.released = true;
        // Without the lock the copies stay: a later sweep removes them once
        // no live lease holds the directory.
        let lock = DirLock::acquire(&self.dir);
        if let Some(marker) = self.marker.take() {
            let _ = remove_file_retrying(&marker);
        }
        if lock.is_none() || has_live_lease(&self.dir) {
            return;
        }
        #[cfg(test)]
        test_hooks::before_delete(&self.dir);
        remove_preserved_files(&self.dir, self.preserved_names);
    }
}

impl Drop for IsolationLease {
    fn drop(&mut self) {
        self.release();
    }
}

impl std::fmt::Debug for IsolationLease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IsolationLease")
            .field("dir", &self.dir)
            .finish_non_exhaustive()
    }
}

fn write_lease_marker(dir: &Path) -> Option<PathBuf> {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let marker = dir.join(format!("{LEASE_MARKER_PREFIX}{}-{seq}", std::process::id()));
    match std::fs::write(&marker, b"") {
        Ok(()) => Some(marker),
        Err(e) => {
            emit_isolation_warning("could not write isolation lease marker", dir, "", &e);
            None
        }
    }
}

/// The runner pid a lease marker's file name carries.
fn lease_marker_pid(name: &str) -> Option<u32> {
    name.strip_prefix(LEASE_MARKER_PREFIX)?
        .split('-')
        .next()?
        .parse()
        .ok()
}

/// Whether any lease marker in `dir` names a process that is still running.
/// Markers of processes that are gone are removed along the way.
fn has_live_lease(dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    let mut live = false;
    for entry in entries.flatten() {
        let Some(pid) = lease_marker_pid(&entry.file_name().to_string_lossy()) else {
            continue;
        };
        if process_is_running(pid) {
            live = true;
        } else {
            let _ = remove_file_retrying(&entry.path());
        }
    }
    live
}

fn process_is_running(pid: u32) -> bool {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
    if pid == std::process::id() {
        return true;
    }
    let pid = Pid::from_u32(pid);
    let mut sys = System::new();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[pid]),
        true,
        ProcessRefreshKind::nothing(),
    );
    sys.process(pid).is_some()
}

/// Remove `names` from every directory under `backend_root` other than
/// `own_dir` that no live lease holds.
fn sweep_stale_isolation_dirs(backend_root: &Path, own_dir: &Path, names: &[&str]) {
    let Ok(entries) = std::fs::read_dir(backend_root) else {
        return;
    };
    for entry in entries.flatten() {
        let dir = entry.path();
        if dir == own_dir || !dir.is_dir() {
            continue;
        }
        let Some(_lock) = DirLock::acquire(&dir) else {
            continue;
        };
        if has_live_lease(&dir) {
            continue;
        }
        #[cfg(test)]
        test_hooks::before_delete(&dir);
        remove_preserved_files(&dir, names);
    }
}

/// Name of the lock file inside each isolated directory. It is never
/// deleted: on Unix a deleted-and-recreated lock file would let two holders
/// lock different inodes.
const DIR_LOCK_NAME: &str = ".ralphus-isolation.lock";

/// How long [`DirLock::acquire`] waits for another holder. Holders keep the
/// lock only for a few file operations, never while an agent runs.
const DIR_LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

/// An exclusive, cross-process lock on one isolated directory, released on
/// drop.
///
/// The OS owns the lock and drops it when the holding process exits, so a
/// runner that dies while holding it never leaves a stale lock behind:
/// - Unix: `flock(LOCK_EX)` on [`DIR_LOCK_NAME`].
/// - Windows: [`DIR_LOCK_NAME`] opened with a share mode of 0, so every other
///   open of it fails with a sharing violation until the handle closes.
struct DirLock {
    #[cfg(unix)]
    _file: nix::fcntl::Flock<std::fs::File>,
    #[cfg(not(unix))]
    _file: std::fs::File,
}

impl DirLock {
    /// Lock `dir`, waiting up to [`DIR_LOCK_WAIT`]. `None` (with a warning
    /// recorded) when the lock could not be taken.
    fn acquire(dir: &Path) -> Option<Self> {
        let path = dir.join(DIR_LOCK_NAME);
        let deadline = std::time::Instant::now() + DIR_LOCK_WAIT;
        let mut contended = false;
        loop {
            let error = match Self::try_lock(&path) {
                Ok(Some(lock)) => return Some(lock),
                Ok(None) => std::io::Error::new(
                    std::io::ErrorKind::WouldBlock,
                    "isolation directory lock is held by another run",
                ),
                Err(e) => e,
            };
            if std::time::Instant::now() >= deadline {
                emit_isolation_warning("could not lock isolated config directory", dir, "", &error);
                return None;
            }
            if !contended {
                contended = true;
                #[cfg(test)]
                test_hooks::on_contended();
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    /// One attempt: `Ok(None)` when another holder has the lock.
    #[cfg(unix)]
    fn try_lock(path: &Path) -> std::io::Result<Option<Self>> {
        use nix::fcntl::{Flock, FlockArg};
        use std::os::unix::fs::OpenOptionsExt as _;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(path)?;
        match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
            Ok(file) => Ok(Some(Self { _file: file })),
            Err((_, nix::errno::Errno::EWOULDBLOCK | nix::errno::Errno::EINTR)) => Ok(None),
            Err((_, errno)) => Err(std::io::Error::from(errno)),
        }
    }

    /// One attempt: `Ok(None)` when another holder has the lock.
    #[cfg(not(unix))]
    fn try_lock(path: &Path) -> std::io::Result<Option<Self>> {
        use std::os::windows::fs::OpenOptionsExt as _;
        const ERROR_ACCESS_DENIED: i32 = 5;
        const ERROR_SHARING_VIOLATION: i32 = 32;
        match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .share_mode(0)
            .open(path)
        {
            Ok(file) => Ok(Some(Self { _file: file })),
            // Access denied also shows up transiently while another opener
            // is mid-create on the same file.
            Err(e)
                if matches!(
                    e.raw_os_error(),
                    Some(ERROR_SHARING_VIOLATION | ERROR_ACCESS_DENIED)
                ) =>
            {
                Ok(None)
            }
            Err(e) => Err(e),
        }
    }
}

/// Remove `names` from `dir`; the caller holds `dir`'s [`DirLock`]. A login
/// ([`SYNC_BACK_NAMES`]) is first synced back to its original
/// ([`sync_back`]); when that fails the copy is kept, because it may hold the
/// only valid token -- a later release or sweep retries.
fn remove_preserved_files(dir: &Path, names: &[&str]) {
    for name in names {
        let path = dir.join(name);
        if SYNC_BACK_NAMES.contains(name) {
            if path.is_file() && sync_back(dir, name).is_err() {
                continue;
            }
            let _ = remove_file_retrying(&origin_path(dir, name));
        }
        if !path.exists() {
            continue;
        }
        match remove_file_retrying(&path) {
            Ok(()) => crate::cartographer::emit(
                "runner",
                "removed preserved file from isolated config directory",
                "debug",
                crate::cartographer::EventContext::default(),
                serde_json::json!({
                    "isolated_dir": dir.display().to_string(),
                    "file": name,
                }),
            ),
            Err(e) => emit_isolation_warning(
                "could not remove preserved file from isolated config directory",
                dir,
                name,
                &e,
            ),
        }
    }
}

/// Remove `path`, retrying briefly while Windows reports it locked (an agent
/// process that is still exiting can hold it open). Already gone is success.
fn remove_file_retrying(path: &Path) -> std::io::Result<()> {
    const ATTEMPTS: u32 = 10;
    let mut attempt = 0;
    loop {
        match std::fs::remove_file(path) {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => {
                attempt += 1;
                if !cfg!(windows) || attempt >= ATTEMPTS {
                    return Err(e);
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }
    }
}

/// The preserved logins an agent may refresh in place: OAuth refresh tokens
/// rotate, so after a refresh inside the copy the original's token is
/// retired and the copy holds the only valid one. These are written back to
/// the original ([`sync_back`]) before the copy is removed.
const SYNC_BACK_NAMES: &[&str] = &[".credentials.json", "auth.json"];

/// Prefix of the provenance file recorded next to a preserved login:
/// `<prefix><login file name>`, holding the original's path and the SHA-256
/// of the bytes copied in (never the contents).
const ORIGIN_PREFIX: &str = ".ralphus-origin-";

fn origin_path(dir: &Path, name: &str) -> PathBuf {
    dir.join(format!("{ORIGIN_PREFIX}{name}"))
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest as _, Sha256};
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn write_origin(dir: &Path, name: &str, source: &Path, copied: &[u8]) {
    let record = serde_json::json!({
        "source": source.display().to_string(),
        "sha256": sha256_hex(copied),
    });
    if let Err(e) = write_owner_only(&origin_path(dir, name), record.to_string().as_bytes()) {
        emit_isolation_warning(
            "could not record where a preserved login came from",
            dir,
            name,
            &e,
        );
    }
}

/// The original's path and the hash of what was copied from it.
fn read_origin(dir: &Path, name: &str) -> Option<(PathBuf, String)> {
    let text = std::fs::read_to_string(origin_path(dir, name)).ok()?;
    let record: serde_json::Value = serde_json::from_str(&text).ok()?;
    Some((
        PathBuf::from(record.get("source")?.as_str()?),
        record.get("sha256")?.as_str()?.to_string(),
    ))
}

/// What [`sync_back`] did with a preserved login.
#[derive(Debug, PartialEq, Eq)]
enum SyncBack {
    /// The copy is what was copied in, or the original already holds it.
    Unchanged,
    /// No provenance record: a copy from before provenance was recorded, so
    /// there is no known original to write to.
    NoOrigin,
    /// The refreshed copy replaced the original.
    WroteBack,
    /// The copy changed, but the original changed too and is kept -- see
    /// [`sync_back`].
    KeptOriginal,
}

/// Write a preserved login that the agent refreshed back to the original it
/// was copied from, so removing the copy never loses the only valid token.
/// The caller holds `dir`'s [`DirLock`]. `Err` means the copy must be kept.
///
/// The rule, judged by content hash rather than mtime:
/// - copy unchanged since it was copied in: nothing to do;
/// - original still holds exactly what was copied in: the copy replaces it
///   (compare-and-swap on the original's hash);
/// - original changed meanwhile (another run synced back first, or the
///   operator's own CLI refreshed it): the copy replaces it only when the
///   freshness timestamp inside the JSON ([`login_freshness`]) is strictly
///   newer in the copy; with no comparable timestamp the original is kept;
/// - original deleted: the operator logged out, and the copy is not
///   resurrected.
///
/// Concurrent sync-backs of the same backend serialize on the backend root's
/// lock (every isolated dir of a backend shares that parent), so the
/// compare-and-swap holds between ralphus runs. A refresh the operator's own
/// CLI makes in the instant between reading the original and replacing it
/// cannot be excluded: that CLI takes no lock ralphus could share.
fn sync_back(dir: &Path, name: &str) -> Result<SyncBack, ()> {
    let copy_path = dir.join(name);
    let Some((source, copied_hash)) = read_origin(dir, name) else {
        return Ok(SyncBack::NoOrigin);
    };
    let copy = match std::fs::read(&copy_path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(SyncBack::Unchanged),
        Err(e) => {
            emit_isolation_warning("could not read preserved login", dir, name, &e);
            return Err(());
        }
    };
    let copy_hash = sha256_hex(&copy);
    if copy_hash == copied_hash {
        return Ok(SyncBack::Unchanged);
    }
    let backend_root = dir.parent().unwrap_or(dir);
    let Some(_sync_lock) = DirLock::acquire(backend_root) else {
        return Err(());
    };
    let original = match std::fs::read(&source) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            emit_sync_back(
                dir,
                name,
                &source,
                "original removed; refreshed copy not restored",
            );
            return Ok(SyncBack::KeptOriginal);
        }
        Err(e) => {
            emit_isolation_warning("could not read the original login", dir, name, &e);
            return Err(());
        }
    };
    let original_hash = sha256_hex(&original);
    if original_hash == copy_hash {
        return Ok(SyncBack::Unchanged);
    }
    if original_hash != copied_hash {
        let newer = match (login_freshness(&copy), login_freshness(&original)) {
            (Some(copy), Some(original)) => copy > original,
            _ => false,
        };
        if !newer {
            emit_sync_back(dir, name, &source, "original changed meanwhile and is kept");
            return Ok(SyncBack::KeptOriginal);
        }
    }
    #[cfg(test)]
    test_hooks::before_write_back(&source);
    match replace_file_atomically(&source, &copy) {
        Ok(()) => {
            emit_sync_back(dir, name, &source, "refreshed login written back");
            Ok(SyncBack::WroteBack)
        }
        Err(e) => {
            emit_isolation_warning(
                "could not write a refreshed login back; keeping the copy",
                dir,
                name,
                &e,
            );
            Err(())
        }
    }
}

fn emit_sync_back(dir: &Path, name: &str, source: &Path, outcome: &str) {
    crate::cartographer::emit(
        "runner",
        "synced preserved login back to its original",
        "info",
        crate::cartographer::EventContext::default(),
        serde_json::json!({
            "isolated_dir": dir.display().to_string(),
            "file": name,
            "original": source.display().to_string(),
            "outcome": outcome,
        }),
    );
}

/// The newest token timestamp in a login file, in milliseconds since the
/// epoch: the largest of every `expiresAt`/`expires_at`/`expires`/`expiry`
/// (Claude Code's `.credentials.json`, Pi's per-provider `auth.json`) and
/// `last_refresh`/`lastRefresh` (Codex's `auth.json`) value anywhere in the
/// document. Numbers below 10^11 are read as seconds; strings as RFC 3339.
/// `None` when the file is not JSON or carries none of them.
fn login_freshness(bytes: &[u8]) -> Option<i64> {
    const KEYS: &[&str] = &[
        "expiresAt",
        "expires_at",
        "expires",
        "expiry",
        "last_refresh",
        "lastRefresh",
    ];
    fn as_millis(value: &serde_json::Value) -> Option<i64> {
        match value {
            serde_json::Value::Number(n) => {
                let n = n.as_i64().or_else(|| n.as_f64().map(|f| f as i64))?;
                Some(if n < 100_000_000_000 { n * 1000 } else { n })
            }
            serde_json::Value::String(s) => chrono::DateTime::parse_from_rfc3339(s)
                .ok()
                .map(|t| t.timestamp_millis()),
            _ => None,
        }
    }
    fn walk(value: &serde_json::Value, best: &mut Option<i64>) {
        match value {
            serde_json::Value::Object(map) => {
                for (key, child) in map {
                    if KEYS.contains(&key.as_str())
                        && let Some(ms) = as_millis(child)
                    {
                        *best = Some(best.map_or(ms, |b| b.max(ms)));
                    }
                    walk(child, best);
                }
            }
            serde_json::Value::Array(items) => items.iter().for_each(|i| walk(i, best)),
            _ => {}
        }
    }
    let value: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    let mut best = None;
    walk(&value, &mut best);
    best
}

/// Replace `target` with `bytes` atomically: write a temp file in the same
/// directory, then rename it over `target` (on Windows `std::fs::rename`
/// replaces an existing file, retried briefly while the file is held open).
/// The temp file takes `target`'s permissions on Unix (owner-only for every
/// login the agent CLIs write); on Windows it inherits the directory's ACL
/// like any new file there.
fn replace_file_atomically(target: &Path, bytes: &[u8]) -> std::io::Result<()> {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let dir = target
        .parent()
        .ok_or_else(|| std::io::Error::other("login file has no parent directory"))?;
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let temp = dir.join(format!(".{name}.ralphus-{}-{seq}.tmp", std::process::id()));
    let result = (|| {
        use std::io::Write as _;
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = options.open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        #[cfg(unix)]
        if let Ok(meta) = std::fs::metadata(target) {
            std::fs::set_permissions(&temp, meta.permissions())?;
        }
        rename_retrying(&temp, target)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

fn rename_retrying(from: &Path, to: &Path) -> std::io::Result<()> {
    const ATTEMPTS: u32 = 10;
    let mut attempt = 0;
    loop {
        match std::fs::rename(from, to) {
            Ok(()) => return Ok(()),
            Err(e) => {
                attempt += 1;
                if !cfg!(windows) || attempt >= ATTEMPTS {
                    return Err(e);
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }
    }
}

/// Create `dir` (and missing parents) readable only by its owner: it holds a
/// copy of a login, and the temp directory it lives under is shared by every
/// account on the machine.
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(dir)
    }
}

/// Make a copied login file readable only by its owner, whatever the
/// original's mode.
fn make_owner_only(file: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o600))
    }
    #[cfg(not(unix))]
    {
        let _ = file;
        Ok(())
    }
}

fn emit_isolation_warning(message: &str, isolated_dir: &Path, filename: &str, e: &std::io::Error) {
    crate::cartographer::emit(
        "runner",
        message,
        "warning",
        crate::cartographer::EventContext::default(),
        serde_json::json!({
            "isolated_dir": isolated_dir.display().to_string(),
            "file": filename,
            "error": e.to_string(),
        }),
    );
}

/// Test-only, per-thread hooks: they fire only on the thread that set them,
/// so parallel tests never see each other's.
#[cfg(test)]
mod test_hooks {
    use std::cell::RefCell;
    use std::path::{Path, PathBuf};

    type DirHook = Box<dyn FnMut(&Path)>;
    type Hook = Box<dyn FnMut()>;

    thread_local! {
        /// Overrides [`super::isolation_root`].
        pub(super) static ISOLATION_ROOT: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
        /// Runs under a directory's lock, after the live-lease check and just
        /// before its preserved files are deleted.
        pub(super) static BEFORE_DELETE: RefCell<Option<DirHook>> = const { RefCell::new(None) };
        /// Runs the first time a lock attempt finds the lock held.
        pub(super) static ON_CONTENDED: RefCell<Option<Hook>> = const { RefCell::new(None) };
        /// Runs under the backend root's lock, just before a refreshed login
        /// replaces its original.
        pub(super) static BEFORE_WRITE_BACK: RefCell<Option<DirHook>> = const { RefCell::new(None) };
    }

    pub(super) fn before_write_back(original: &Path) {
        BEFORE_WRITE_BACK.with(|h| {
            if let Some(hook) = h.borrow_mut().as_mut() {
                hook(original);
            }
        });
    }

    pub(super) fn before_delete(dir: &Path) {
        BEFORE_DELETE.with(|h| {
            if let Some(hook) = h.borrow_mut().as_mut() {
                hook(dir);
            }
        });
    }

    pub(super) fn on_contended() {
        ON_CONTENDED.with(|h| {
            if let Some(hook) = h.borrow_mut().as_mut() {
                hook();
            }
        });
    }
}

/// Point [`isolated_config_dir`] at `root` for the rest of the calling
/// thread, so a test never touches the real temp-dir isolation tree.
#[cfg(test)]
pub(crate) fn set_test_isolation_root(root: &Path) {
    test_hooks::ISOLATION_ROOT.with(|r| *r.borrow_mut() = Some(root.to_path_buf()));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn isolated_config_dir_is_keyed_by_backend_and_workspace_basename() {
        let ws = Path::new("/tmp/some/workspace-abc");
        let claude = isolated_config_dir(ws, "claude-code");
        let codex = isolated_config_dir(ws, "codex");
        assert_ne!(claude, codex);
        assert!(claude.ends_with("claude-code/workspace-abc"));
        assert!(codex.ends_with("codex/workspace-abc"));
    }

    #[test]
    fn isolated_config_dir_differs_across_workspaces() {
        let a = isolated_config_dir(Path::new("/tmp/a"), "codex");
        let b = isolated_config_dir(Path::new("/tmp/b"), "codex");
        assert_ne!(a, b);
    }

    #[cfg(unix)]
    #[test]
    fn a_preserved_login_is_readable_only_by_its_owner() {
        use std::os::unix::fs::PermissionsExt as _;
        let root = ralphus_test_support::TestDir::new("auth-mode");
        let real = root.path().join("real");
        let isolated = root.path().join("isolated");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::write(real.join("auth.json"), "{}").unwrap();
        std::fs::set_permissions(
            real.join("auth.json"),
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();

        preserve_auth_file(Some(&real), &isolated, "auth.json");

        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&isolated), 0o700);
        assert_eq!(mode(&isolated.join("auth.json")), 0o600);
    }

    #[test]
    fn preserve_auth_file_copies_when_present() {
        let root = ralphus_test_support::TestDir::new("auth-copy");
        let real = root.path().join("real");
        let isolated = root.path().join("isolated");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::write(real.join("auth.json"), "{\"token\":\"secret\"}").unwrap();

        preserve_auth_file(Some(&real), &isolated, "auth.json");

        assert_eq!(
            std::fs::read_to_string(isolated.join("auth.json")).unwrap(),
            "{\"token\":\"secret\"}"
        );
    }

    #[test]
    fn preserve_auth_file_is_a_no_op_when_source_is_missing() {
        let root = ralphus_test_support::TestDir::new("auth-empty");
        let real = root.path().join("real");
        let isolated = root.path().join("isolated");
        std::fs::create_dir_all(&real).unwrap();

        preserve_auth_file(Some(&real), &isolated, "auth.json");

        assert!(!isolated.exists());
    }

    const NAMES: &[&str] = &[".credentials.json", "settings.json"];

    /// A real config dir holding a login, and a backend root to isolate under.
    fn lease_fixture(tag: &str) -> (ralphus_test_support::TestDir, PathBuf, PathBuf) {
        let root = ralphus_test_support::TestDir::new(tag);
        let real = root.path().join("real");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::write(real.join(".credentials.json"), "{\"token\":\"secret\"}").unwrap();
        let backend_root = root.path().join("agent_isolation").join("claude-code");
        (root, real, backend_root)
    }

    fn lease_markers(dir: &Path) -> usize {
        std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .filter(|e| lease_marker_pid(&e.file_name().to_string_lossy()).is_some())
            .count()
    }

    fn run_with_lease(dir: &Path, real: &Path, fail: bool) -> Result<(), String> {
        let lease = IsolationLease::acquire(dir, NAMES);
        lease.preserve(Some(real), ".credentials.json");
        assert!(dir.join(".credentials.json").is_file());
        if fail {
            return Err("agent failed".to_string());
        }
        drop(lease);
        Ok(())
    }

    #[test]
    fn lease_removes_the_copied_login_when_the_run_succeeds() {
        let (_root, real, backend_root) = lease_fixture("lease-ok");
        let dir = backend_root.join("ws-ok");

        run_with_lease(&dir, &real, false).unwrap();

        assert!(!dir.join(".credentials.json").exists());
        assert_eq!(lease_markers(&dir), 0);
        assert!(dir.is_dir(), "the directory itself stays for resume");
        assert!(real.join(".credentials.json").is_file());
    }

    #[test]
    fn lease_removes_the_copied_login_when_the_run_fails() {
        let (_root, real, backend_root) = lease_fixture("lease-err");
        let dir = backend_root.join("ws-err");

        assert!(run_with_lease(&dir, &real, true).is_err());

        assert!(!dir.join(".credentials.json").exists());
        assert_eq!(lease_markers(&dir), 0);
    }

    #[test]
    fn lease_keeps_the_login_while_another_live_lease_holds_the_dir() {
        let (_root, real, backend_root) = lease_fixture("lease-shared");
        let dir = backend_root.join("ws-shared");

        let outer = IsolationLease::acquire(&dir, NAMES);
        outer.preserve(Some(&real), ".credentials.json");
        run_with_lease(&dir, &real, false).unwrap();
        assert!(dir.join(".credentials.json").is_file());

        drop(outer);
        assert!(!dir.join(".credentials.json").exists());
    }

    #[test]
    fn acquiring_a_lease_sweeps_stale_dirs_but_spares_one_in_use() {
        let (_root, real, backend_root) = lease_fixture("lease-sweep");

        // A crashed run: login and settings left behind, a marker naming a
        // process that no longer exists.
        let stale = backend_root.join("ws-stale");
        std::fs::create_dir_all(&stale).unwrap();
        std::fs::write(stale.join(".credentials.json"), "old").unwrap();
        std::fs::write(stale.join("settings.json"), "old").unwrap();
        std::fs::write(stale.join("session.jsonl"), "transcript").unwrap();
        std::fs::write(
            stale.join(format!("{LEASE_MARKER_PREFIX}{}-0", u32::MAX)),
            "",
        )
        .unwrap();

        // A concurrently running cell, held by a live lease.
        let busy = backend_root.join("ws-busy");
        let busy_lease = IsolationLease::acquire(&busy, NAMES);
        busy_lease.preserve(Some(&real), ".credentials.json");

        let own = backend_root.join("ws-own");
        let own_lease = IsolationLease::acquire(&own, NAMES);

        assert!(!stale.join(".credentials.json").exists());
        assert!(!stale.join("settings.json").exists());
        assert_eq!(lease_markers(&stale), 0);
        assert!(
            stale.join("session.jsonl").is_file(),
            "only preserved files are swept"
        );
        assert!(busy.join(".credentials.json").is_file());

        drop(own_lease);
        drop(busy_lease);
        assert!(!busy.join(".credentials.json").exists());
    }

    /// Run `newcomer` on its own thread once `before_delete` fires on this
    /// thread for `target`, and hold that deletion until the newcomer is
    /// waiting on `target`'s lock -- the ordering in which an unlocked
    /// check-then-delete took the newcomer's fresh login.
    fn interleave_newcomer(
        target: &Path,
        newcomer: impl FnOnce() -> IsolationLease + Send + 'static,
    ) -> (
        std::thread::JoinHandle<IsolationLease>,
        std::rc::Rc<std::cell::Cell<bool>>,
    ) {
        use std::sync::mpsc;
        let (start_tx, start_rx) = mpsc::channel::<()>();
        let (waiting_tx, waiting_rx) = mpsc::channel::<()>();
        let handle = std::thread::spawn(move || {
            start_rx.recv().unwrap();
            test_hooks::ON_CONTENDED.with(|h| {
                *h.borrow_mut() = Some(Box::new(move || {
                    let _ = waiting_tx.send(());
                }));
            });
            newcomer()
        });
        let interleaved = std::rc::Rc::new(std::cell::Cell::new(false));
        let flag = std::rc::Rc::clone(&interleaved);
        let target = target.to_path_buf();
        let mut start_tx = Some(start_tx);
        test_hooks::BEFORE_DELETE.with(|h| {
            *h.borrow_mut() = Some(Box::new(move |dir: &Path| {
                if dir != target {
                    return;
                }
                let Some(start) = start_tx.take() else {
                    return;
                };
                start.send(()).unwrap();
                waiting_rx
                    .recv_timeout(std::time::Duration::from_secs(10))
                    .expect("the newcomer must wait for the directory lock");
                flag.set(true);
            }));
        });
        (handle, interleaved)
    }

    #[test]
    fn a_release_never_deletes_the_login_of_a_lease_taken_during_it() {
        let (root, real, backend_root) = lease_fixture("lease-race-release");
        let fresh = root.path().join("fresh");
        std::fs::create_dir_all(&fresh).unwrap();
        std::fs::write(fresh.join(".credentials.json"), "fresh").unwrap();
        let dir = backend_root.join("ws-race");

        let mut first = IsolationLease::acquire(&dir, NAMES);
        first.preserve(Some(&real), ".credentials.json");
        let newcomer_dir = dir.clone();
        let (newcomer, interleaved) = interleave_newcomer(&dir, move || {
            let lease = IsolationLease::acquire(&newcomer_dir, NAMES);
            lease.preserve(Some(&fresh), ".credentials.json");
            lease
        });
        first.release();
        let newcomer = newcomer.join().unwrap();

        assert!(interleaved.get(), "the release reached its deletion");
        assert_eq!(
            std::fs::read_to_string(dir.join(".credentials.json")).unwrap(),
            "fresh"
        );
        drop(newcomer);
        assert!(!dir.join(".credentials.json").exists());
    }

    #[test]
    fn a_sweep_never_deletes_the_login_of_a_lease_taken_during_it() {
        let (root, _real, backend_root) = lease_fixture("lease-race-sweep");
        let fresh = root.path().join("fresh");
        std::fs::create_dir_all(&fresh).unwrap();
        std::fs::write(fresh.join(".credentials.json"), "fresh").unwrap();

        // A crashed run's directory, which a new run is about to reuse.
        let stale = backend_root.join("ws-stale");
        std::fs::create_dir_all(&stale).unwrap();
        std::fs::write(stale.join(".credentials.json"), "old").unwrap();
        std::fs::write(
            stale.join(format!("{LEASE_MARKER_PREFIX}{}-0", u32::MAX)),
            "",
        )
        .unwrap();

        let newcomer_dir = stale.clone();
        let (newcomer, interleaved) = interleave_newcomer(&stale, move || {
            let lease = IsolationLease::acquire(&newcomer_dir, NAMES);
            lease.preserve(Some(&fresh), ".credentials.json");
            lease
        });
        let sweeper = IsolationLease::acquire(&backend_root.join("ws-sweeper"), NAMES);
        let newcomer = newcomer.join().unwrap();

        assert!(interleaved.get(), "the sweep reached its deletion");
        assert_eq!(
            std::fs::read_to_string(stale.join(".credentials.json")).unwrap(),
            "fresh"
        );
        drop(sweeper);
        drop(newcomer);
    }

    #[test]
    fn isolated_config_dir_honours_the_test_isolation_root() {
        let root = ralphus_test_support::TestDir::new("isolation-root");
        set_test_isolation_root(root.path());
        let dir = isolated_config_dir(Path::new("/tmp/ws"), "codex");
        assert_eq!(dir, root.path().join("codex").join("ws"));
    }

    #[test]
    fn lease_marker_pid_parses_only_lease_markers() {
        assert_eq!(lease_marker_pid(".ralphus-lease-42-7"), Some(42));
        assert_eq!(lease_marker_pid(".credentials.json"), None);
        assert_eq!(lease_marker_pid(".ralphus-lease-x-1"), None);
        assert_eq!(lease_marker_pid(DIR_LOCK_NAME), None);
    }

    fn creds(expires_at: i64, token: &str) -> String {
        format!("{{\"claudeAiOauth\":{{\"refreshToken\":\"{token}\",\"expiresAt\":{expires_at}}}}}")
    }

    /// A real config dir holding a Claude login that expires at `expires_at`.
    fn sync_fixture(
        tag: &str,
        expires_at: i64,
    ) -> (ralphus_test_support::TestDir, PathBuf, PathBuf) {
        let (root, real, backend_root) = lease_fixture(tag);
        std::fs::write(real.join(".credentials.json"), creds(expires_at, "r0")).unwrap();
        (root, real, backend_root)
    }

    /// The runner holding `lease` dies: no release, and its marker names a
    /// process that no longer exists.
    fn crash(mut lease: IsolationLease) {
        let marker = lease.marker.take().unwrap();
        std::fs::rename(
            &marker,
            lease
                .dir
                .join(format!("{LEASE_MARKER_PREFIX}{}-0", u32::MAX)),
        )
        .unwrap();
        std::mem::forget(lease);
    }

    fn read(path: &Path) -> String {
        std::fs::read_to_string(path).unwrap()
    }

    #[test]
    fn a_login_refreshed_during_the_run_is_written_back_then_removed() {
        let (_root, real, backend_root) = sync_fixture("sync-refreshed", 1_000);
        let dir = backend_root.join("ws");
        let lease = IsolationLease::acquire(&dir, NAMES);
        lease.preserve(Some(&real), ".credentials.json");
        // The agent refreshes inside the copy; the original's r0 is retired.
        std::fs::write(dir.join(".credentials.json"), creds(2_000, "r1")).unwrap();

        drop(lease);

        assert_eq!(read(&real.join(".credentials.json")), creds(2_000, "r1"));
        assert!(!dir.join(".credentials.json").exists());
        assert!(!origin_path(&dir, ".credentials.json").exists());
        let leftovers: Vec<_> = std::fs::read_dir(&real)
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        assert_eq!(
            leftovers.len(),
            1,
            "no temp file left beside the original: {leftovers:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_written_back_login_keeps_the_original_permissions() {
        use std::os::unix::fs::PermissionsExt as _;
        let (_root, real, backend_root) = sync_fixture("sync-mode", 1_000);
        let original = real.join(".credentials.json");
        std::fs::set_permissions(&original, std::fs::Permissions::from_mode(0o600)).unwrap();
        let dir = backend_root.join("ws");
        let lease = IsolationLease::acquire(&dir, NAMES);
        lease.preserve(Some(&real), ".credentials.json");
        std::fs::write(dir.join(".credentials.json"), creds(2_000, "r1")).unwrap();
        drop(lease);
        assert_eq!(read(&original), creds(2_000, "r1"));
        let mode = std::fs::metadata(&original).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn an_unchanged_login_is_only_removed() {
        let (_root, real, backend_root) = sync_fixture("sync-unchanged", 1_000);
        let original = real.join(".credentials.json");
        let dir = backend_root.join("ws");
        let lease = IsolationLease::acquire(&dir, NAMES);
        lease.preserve(Some(&real), ".credentials.json");
        let before = std::fs::metadata(&original).unwrap().modified().unwrap();
        // Same bytes, newer mtime: mtime alone never triggers a write-back.
        std::fs::write(dir.join(".credentials.json"), creds(1_000, "r0")).unwrap();
        test_hooks::BEFORE_WRITE_BACK.with(|h| {
            *h.borrow_mut() = Some(Box::new(|_: &Path| panic!("nothing to write back")));
        });

        drop(lease);

        test_hooks::BEFORE_WRITE_BACK.with(|h| *h.borrow_mut() = None);
        assert_eq!(read(&original), creds(1_000, "r0"));
        assert_eq!(
            std::fs::metadata(&original).unwrap().modified().unwrap(),
            before
        );
        assert!(!dir.join(".credentials.json").exists());
    }

    /// The original changed after it was copied in (the operator's own CLI or
    /// another run refreshed it): the copy wins only when its token
    /// timestamp is strictly newer; otherwise, or without timestamps, the
    /// original is kept. Either way the copy is removed.
    #[test]
    fn an_original_changed_meanwhile_is_kept_unless_the_copy_is_newer() {
        for (tag, original_now, copy_now, expected) in [
            (
                "sync-orig-newer",
                creds(3_000, "user"),
                creds(2_000, "run"),
                creds(3_000, "user"),
            ),
            (
                "sync-copy-newer",
                creds(2_000, "user"),
                creds(3_000, "run"),
                creds(3_000, "run"),
            ),
            (
                "sync-no-stamp",
                "{\"a\":1}".to_string(),
                "{\"a\":2}".to_string(),
                "{\"a\":1}".to_string(),
            ),
        ] {
            let (_root, real, backend_root) = sync_fixture(tag, 1_000);
            let dir = backend_root.join("ws");
            let lease = IsolationLease::acquire(&dir, NAMES);
            lease.preserve(Some(&real), ".credentials.json");
            std::fs::write(dir.join(".credentials.json"), &copy_now).unwrap();
            std::fs::write(real.join(".credentials.json"), &original_now).unwrap();

            drop(lease);

            assert_eq!(read(&real.join(".credentials.json")), expected, "{tag}");
            assert!(!dir.join(".credentials.json").exists(), "{tag}");
        }
    }

    #[test]
    fn a_deleted_original_is_not_resurrected() {
        let (_root, real, backend_root) = sync_fixture("sync-logged-out", 1_000);
        let dir = backend_root.join("ws");
        let lease = IsolationLease::acquire(&dir, NAMES);
        lease.preserve(Some(&real), ".credentials.json");
        std::fs::write(dir.join(".credentials.json"), creds(2_000, "r1")).unwrap();
        std::fs::remove_file(real.join(".credentials.json")).unwrap();
        drop(lease);
        assert!(!real.join(".credentials.json").exists());
        assert!(!dir.join(".credentials.json").exists());
    }

    #[test]
    fn a_copy_that_cannot_be_written_back_is_kept() {
        let (_root, real, backend_root) = sync_fixture("sync-fail", 1_000);
        let dir = backend_root.join("ws");
        let lease = IsolationLease::acquire(&dir, NAMES);
        lease.preserve(Some(&real), ".credentials.json");
        std::fs::write(dir.join(".credentials.json"), creds(2_000, "r1")).unwrap();
        // The original can neither be read nor replaced: the write-back
        // cannot land, so the only valid token must not be deleted.
        std::fs::remove_file(real.join(".credentials.json")).unwrap();
        std::fs::create_dir(real.join(".credentials.json")).unwrap();
        drop(lease);
        assert_eq!(read(&dir.join(".credentials.json")), creds(2_000, "r1"));
        assert!(origin_path(&dir, ".credentials.json").is_file());
    }

    #[test]
    fn the_sweep_writes_back_a_crashed_runs_refreshed_login() {
        let (_root, real, backend_root) = sync_fixture("sync-sweep", 1_000);
        let crashed = backend_root.join("ws-crashed");
        let lease = IsolationLease::acquire(&crashed, NAMES);
        lease.preserve(Some(&real), ".credentials.json");
        std::fs::write(crashed.join(".credentials.json"), creds(2_000, "r1")).unwrap();
        crash(lease);

        let next = IsolationLease::acquire(&backend_root.join("ws-next"), NAMES);

        assert_eq!(read(&real.join(".credentials.json")), creds(2_000, "r1"));
        assert!(!crashed.join(".credentials.json").exists());
        drop(next);
    }

    #[test]
    fn preserving_over_a_refreshed_leftover_writes_it_back_first() {
        let (_root, real, backend_root) = sync_fixture("sync-reuse", 1_000);
        let dir = backend_root.join("ws");
        let crashed = IsolationLease::acquire(&dir, NAMES);
        crashed.preserve(Some(&real), ".credentials.json");
        std::fs::write(dir.join(".credentials.json"), creds(2_000, "r1")).unwrap();
        crash(crashed);

        let next = IsolationLease::acquire(&dir, NAMES);
        next.preserve(Some(&real), ".credentials.json");

        assert_eq!(read(&real.join(".credentials.json")), creds(2_000, "r1"));
        assert_eq!(read(&dir.join(".credentials.json")), creds(2_000, "r1"));
        drop(next);
        assert!(!dir.join(".credentials.json").exists());
    }

    /// Two runs copied the same login and both refreshed it; whichever
    /// releases first, the newest refresh ends up in the original.
    #[test]
    fn concurrent_sync_backs_keep_the_newest_refresh() {
        for (tag, a_first) in [("sync-race-a", true), ("sync-race-b", false)] {
            let (_root, real, backend_root) = sync_fixture(tag, 1_000);
            let a_dir = backend_root.join("ws-a");
            let b_dir = backend_root.join("ws-b");
            let a = IsolationLease::acquire(&a_dir, NAMES);
            a.preserve(Some(&real), ".credentials.json");
            let b = IsolationLease::acquire(&b_dir, NAMES);
            b.preserve(Some(&real), ".credentials.json");
            std::fs::write(a_dir.join(".credentials.json"), creds(3_000, "a")).unwrap();
            std::fs::write(b_dir.join(".credentials.json"), creds(2_000, "b")).unwrap();

            let (first, second) = if a_first { (a, b) } else { (b, a) };
            let other = std::thread::spawn(move || drop(second));
            drop(first);
            other.join().unwrap();

            assert_eq!(
                read(&real.join(".credentials.json")),
                creds(3_000, "a"),
                "{tag}"
            );
            assert!(!a_dir.join(".credentials.json").exists());
            assert!(!b_dir.join(".credentials.json").exists());
        }
    }

    /// A second sync-back that starts while the first is mid-write waits for
    /// the backend lock and then sees the first's result, not the stale
    /// original it would otherwise compare against.
    #[test]
    fn a_sync_back_waits_for_one_in_progress() {
        use std::sync::mpsc;
        let (_root, real, backend_root) = sync_fixture("sync-serial", 1_000);
        let a_dir = backend_root.join("ws-a");
        let b_dir = backend_root.join("ws-b");
        let a = IsolationLease::acquire(&a_dir, NAMES);
        a.preserve(Some(&real), ".credentials.json");
        let b = IsolationLease::acquire(&b_dir, NAMES);
        b.preserve(Some(&real), ".credentials.json");
        std::fs::write(a_dir.join(".credentials.json"), creds(2_000, "a")).unwrap();
        std::fs::write(b_dir.join(".credentials.json"), creds(3_000, "b")).unwrap();

        let (start_tx, start_rx) = mpsc::channel::<()>();
        let (waiting_tx, waiting_rx) = mpsc::channel::<()>();
        let other = std::thread::spawn(move || {
            start_rx.recv().unwrap();
            test_hooks::ON_CONTENDED.with(|h| {
                *h.borrow_mut() = Some(Box::new(move || {
                    let _ = waiting_tx.send(());
                }));
            });
            drop(b);
        });
        let mut start_tx = Some(start_tx);
        test_hooks::BEFORE_WRITE_BACK.with(|h| {
            *h.borrow_mut() = Some(Box::new(move |_: &Path| {
                if let Some(start) = start_tx.take() {
                    start.send(()).unwrap();
                    waiting_rx
                        .recv_timeout(std::time::Duration::from_secs(10))
                        .expect("the second sync-back must wait for the backend lock");
                }
            }));
        });
        drop(a);
        test_hooks::BEFORE_WRITE_BACK.with(|h| *h.borrow_mut() = None);
        other.join().unwrap();

        assert_eq!(read(&real.join(".credentials.json")), creds(3_000, "b"));
    }

    #[test]
    fn login_freshness_reads_each_backends_timestamp() {
        assert_eq!(
            login_freshness(creds(1_700_000_000_000, "t").as_bytes()),
            Some(1_700_000_000_000)
        );
        let codex =
            br#"{"tokens":{"refresh_token":"x"},"last_refresh":"2026-01-02T03:04:05.123456Z"}"#;
        assert_eq!(login_freshness(codex), Some(1_767_323_045_123));
        let pi = br#"{"anthropic":{"type":"oauth","expires":1700000000000},"openai":{"expires":1800000000}}"#;
        assert_eq!(login_freshness(pi), Some(1_800_000_000_000));
        assert_eq!(login_freshness(b"not json"), None);
        assert_eq!(login_freshness(b"{\"a\":1}"), None);
    }

    #[test]
    fn preserve_auth_file_is_a_no_op_when_real_dir_is_none() {
        let root = ralphus_test_support::TestDir::new("auth-none");
        let isolated = root.path().join("isolated");
        preserve_auth_file(None, &isolated, "auth.json");
        assert!(!isolated.exists());
    }
}
