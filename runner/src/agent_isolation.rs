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
    std::env::temp_dir()
        .join("ralphus")
        .join("agent_isolation")
        .join(backend)
        .join(basename)
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
    let Some(real_dir) = real_dir else {
        return;
    };
    let source = real_dir.join(filename);
    if !source.is_file() {
        return;
    }
    if let Err(e) = create_private_dir(isolated_dir) {
        emit_isolation_warning(
            "could not create isolated config directory",
            isolated_dir,
            filename,
            &e,
        );
        return;
    }
    let copy = isolated_dir.join(filename);
    match std::fs::copy(source, &copy).and_then(|_| make_owner_only(&copy)) {
        Ok(()) => crate::cartographer::emit(
            "runner",
            "preserved file into isolated config directory",
            "debug",
            crate::cartographer::EventContext::default(),
            serde_json::json!({
                "isolated_dir": isolated_dir.display().to_string(),
                "file": filename,
            }),
        ),
        Err(e) => emit_isolation_warning(
            "could not preserve file into isolated config directory",
            isolated_dir,
            filename,
            &e,
        ),
    }
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
/// Rule shared by release and the sweep: copies in a directory are removed
/// only while no *live* lease marker remains in it. A marker is live when the
/// process whose pid it names is still running, so a concurrently running
/// cell's directory (and a second run in the same workspace) is never
/// stripped. A dead runner's pid that has since been reused by an unrelated
/// process keeps its directory spared until that process exits -- the sweep
/// errs on the side of not deleting.
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
            Ok(()) => write_lease_marker(isolated_dir),
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
    pub fn preserve(&self, real_dir: Option<&Path>, filename: &str) {
        preserve_auth_file(real_dir, &self.dir, filename);
    }

    /// Drop this run's marker and, unless another live lease still holds the
    /// directory, remove the preserved files. `Drop` calls it; calling it
    /// first is harmless.
    pub fn release(&mut self) {
        if self.released {
            return;
        }
        self.released = true;
        if let Some(marker) = self.marker.take() {
            let _ = remove_file_retrying(&marker);
        }
        if has_live_lease(&self.dir) {
            return;
        }
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
        if dir == own_dir || !dir.is_dir() || has_live_lease(&dir) {
            continue;
        }
        remove_preserved_files(&dir, names);
    }
}

fn remove_preserved_files(dir: &Path, names: &[&str]) {
    for name in names {
        let path = dir.join(name);
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
        let real =
            std::env::temp_dir().join(format!("ralphus-auth-mode-real-{}", std::process::id()));
        let isolated =
            std::env::temp_dir().join(format!("ralphus-auth-mode-iso-{}", std::process::id()));
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
        let _ = std::fs::remove_dir_all(&real);
        let _ = std::fs::remove_dir_all(&isolated);
    }

    #[test]
    fn preserve_auth_file_copies_when_present() {
        let real = std::env::temp_dir().join(format!("ralphus-auth-real-{}", std::process::id()));
        let isolated =
            std::env::temp_dir().join(format!("ralphus-auth-isolated-{}", std::process::id()));
        std::fs::create_dir_all(&real).unwrap();
        std::fs::write(real.join("auth.json"), "{\"token\":\"secret\"}").unwrap();

        preserve_auth_file(Some(&real), &isolated, "auth.json");

        assert_eq!(
            std::fs::read_to_string(isolated.join("auth.json")).unwrap(),
            "{\"token\":\"secret\"}"
        );

        let _ = std::fs::remove_dir_all(&real);
        let _ = std::fs::remove_dir_all(&isolated);
    }

    #[test]
    fn preserve_auth_file_is_a_no_op_when_source_is_missing() {
        let real = std::env::temp_dir().join(format!("ralphus-auth-empty-{}", std::process::id()));
        let isolated = std::env::temp_dir().join(format!(
            "ralphus-auth-empty-isolated-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&real).unwrap();

        preserve_auth_file(Some(&real), &isolated, "auth.json");

        assert!(!isolated.exists());

        let _ = std::fs::remove_dir_all(&real);
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

    #[test]
    fn lease_marker_pid_parses_only_lease_markers() {
        assert_eq!(lease_marker_pid(".ralphus-lease-42-7"), Some(42));
        assert_eq!(lease_marker_pid(".credentials.json"), None);
        assert_eq!(lease_marker_pid(".ralphus-lease-x-1"), None);
    }

    #[test]
    fn preserve_auth_file_is_a_no_op_when_real_dir_is_none() {
        let isolated =
            std::env::temp_dir().join(format!("ralphus-auth-none-isolated-{}", std::process::id()));
        preserve_auth_file(None, &isolated, "auth.json");
        assert!(!isolated.exists());
    }
}
