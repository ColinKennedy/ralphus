//! `ralphus-core` — shared types and logic used by the daemon and librarian.
//!
//! This crate is deliberately dependency-light and side-effect-free so it can be
//! unit-tested quickly and reused across the Rust executables. Heavier concerns
//! (HTTP, SQLite, process spawning) live in the `daemon` and `librarian` crates.

pub mod agent_resume;
mod bench_demo;
pub mod cors;
pub mod license;
pub mod schema;
pub mod uri;
pub mod validate;

pub use bench_demo::ralphus_bench_tests;

use std::path::PathBuf;

/// The workspace version, surfaced so every executable reports the same string.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Returns the crate version. Kept as a function (not just the constant) so
/// callers have a stable API even if the source of the version changes later.
#[must_use]
pub fn version() -> &'static str {
    VERSION
}

/// The current user's home directory, per `%USERPROFILE%` (checked first,
/// since it's always set on Windows even under a shell -- Git Bash, WSL
/// interop -- that also sets `$HOME` to something else) falling back to
/// `$HOME`. `None` when neither is set.
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
}

/// Resolve ralphus's shared state directory (`~/.ralphus`), creating it if
/// needed. Falls back to `./.ralphus` when no home directory is set.
///
/// Lives here — rather than only in `ralphus-daemon`, which owns most of what
/// it stores (the SQLite DB, tmux pane snapshots, terminal logs) — because the
/// daemon's bearer-token file (RAL-219, [`daemon_token_path`] below) must also
/// be resolvable by the librarian, without pulling in the whole daemon crate
/// (SQLite, scheduler, ...) just for a path join. `ralphus_daemon::state_dir()`
/// delegates to this same function, so every Rust process agrees on one path.
///
/// RAL-230: on Unix the directory is created (or re-tightened, if it already
/// existed from an older, looser-permissioned run) to `0o700` on every call,
/// not just the first. `state_dir()` is called on every daemon startup, so
/// this is the point where a pre-existing directory's permissions get
/// corrected going forward -- a deliberate choice over leaving old,
/// looser-permissioned directories alone, since the fix is cheap (one
/// syscall) and "every future startup self-heals" is a stronger guarantee
/// than "only a fresh install is protected". Windows is left to inherited
/// ACL defaults -- no ACL crate is in this dependency tree, and CI is
/// Ubuntu-only, so no Windows ACL work is done here.
#[must_use]
pub fn state_dir() -> PathBuf {
    let dir = home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".ralphus");
    let _ = std::fs::create_dir_all(&dir);
    #[cfg(unix)]
    tighten_unix_dir_permissions(&dir);
    dir
}

/// RAL-230: restrict `dir` to owner-only `0o700`. Factored out of
/// [`state_dir`] so it can be unit-tested against a throwaway directory
/// instead of the real (and test-order-sensitive) `$HOME/.ralphus`.
#[cfg(unix)]
fn tighten_unix_dir_permissions(dir: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
}

/// Path to the daemon's bearer-token file (RAL-219) — see `docs/daemon-api.md`'s
/// "Authentication" section. The daemon generates/persists it at startup;
/// clients (the CLI, the librarian's proxy) read it to send
/// `Authorization: Bearer <token>` on every request.
#[must_use]
pub fn daemon_token_path() -> PathBuf {
    state_dir().join("daemon.token")
}

/// Expand a leading `~` in `path` to the resolved home directory ([`home_dir`]
/// -- the same `USERPROFILE`/`HOME` resolution `state_dir` uses), when `path`
/// is exactly `~` or starts with `~/` or `~\`. Deliberately narrow: `~other`
/// (another user's home directory, a shell convention this never had to
/// honor since it's not itself a shell) is left untouched, as is any path
/// with no leading `~` at all. A path is returned unexpanded, `~` and all, if
/// no home directory can be resolved.
///
/// Windows accepts `/` interchangeably with `\` in its own path APIs, so a
/// path like `~/repositories/ralphus` expands to `<home>/repositories/ralphus`
/// with no separator rewriting needed -- `canonicalize()` normalizes it from
/// there.
#[must_use]
pub fn expand_home(path: &str) -> PathBuf {
    let Some(home) = home_dir() else {
        return PathBuf::from(path);
    };
    if path == "~" {
        return home;
    }
    if let Some(rest) = path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\")) {
        return home.join(rest);
    }
    PathBuf::from(path)
}

/// Strip the `\\?\`/`\\?\UNC\` "verbatim" prefix that `Path::canonicalize`
/// adds on Windows (it calls `GetFinalPathNameByHandleW` under the hood,
/// which always returns the extended-length form). Programs built on
/// Cygwin/MSYS -- including the `git.exe` Git for Windows ships -- don't
/// reliably accept verbatim paths as arguments or as a `cwd`, so any path
/// canonicalized for that purpose (or for storing as a project root that
/// later feeds `git` invocations) must go through this first. A no-op for
/// any path that isn't already verbatim, which includes every path on a
/// non-Windows target.
#[must_use]
pub fn strip_verbatim_prefix(path: PathBuf) -> PathBuf {
    let s = path.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    if let Some(rest) = s.strip_prefix(r"\\?\") {
        return PathBuf::from(rest);
    }
    path
}

/// Path to a mailbox client's locally persisted `client_id` (RAL-241) --
/// shared by `ralphus mailbox check` and `ralphus quick-start watcher ...`,
/// which register once with the daemon (`POST /api/mailbox/register`) and
/// persist the resulting id here so it's stable across restarts of the same
/// client, mirroring [`daemon_token_path`]'s file-based persistence.
#[must_use]
pub fn mailbox_client_id_path() -> PathBuf {
    state_dir().join("mailbox-client-id")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_is_non_empty() {
        assert!(!version().is_empty());
    }

    #[test]
    fn version_matches_constant() {
        assert_eq!(version(), VERSION);
    }

    /// RAL-230: the state directory must be owner-only on Unix, not whatever
    /// the process umask happens to leave it at.
    #[cfg(unix)]
    #[test]
    fn tighten_unix_dir_permissions_sets_owner_only_mode() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("ral230-dir-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create dir");
        // Start from a deliberately looser mode, so the test actually
        // exercises tightening rather than trusting whatever mkdir left.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777))
            .expect("loosen for test");

        tighten_unix_dir_permissions(&dir);

        let mode = std::fs::metadata(&dir)
            .expect("stat dir")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o700, "expected 0o700, got {mode:o}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn strip_verbatim_prefix_strips_a_verbatim_disk_path() {
        assert_eq!(
            strip_verbatim_prefix(PathBuf::from(r"\\?\C:\Users\me\repo")),
            PathBuf::from(r"C:\Users\me\repo")
        );
    }

    #[test]
    fn strip_verbatim_prefix_strips_a_verbatim_unc_path() {
        assert_eq!(
            strip_verbatim_prefix(PathBuf::from(r"\\?\UNC\server\share\repo")),
            PathBuf::from(r"\\server\share\repo")
        );
    }

    #[test]
    fn strip_verbatim_prefix_leaves_a_plain_path_unchanged() {
        assert_eq!(
            strip_verbatim_prefix(PathBuf::from(r"C:\Users\me\repo")),
            PathBuf::from(r"C:\Users\me\repo")
        );
    }

    #[test]
    fn strip_verbatim_prefix_leaves_a_unix_style_path_unchanged() {
        assert_eq!(
            strip_verbatim_prefix(PathBuf::from("/home/me/repo")),
            PathBuf::from("/home/me/repo")
        );
    }

    #[test]
    fn expand_home_expands_bare_tilde() {
        let home = home_dir().expect("test environment must have a home dir");
        assert_eq!(expand_home("~"), home);
    }

    #[test]
    fn expand_home_expands_tilde_slash() {
        let home = home_dir().expect("test environment must have a home dir");
        assert_eq!(
            expand_home("~/repositories/ralphus"),
            home.join("repositories/ralphus")
        );
    }

    #[test]
    fn expand_home_expands_tilde_backslash() {
        let home = home_dir().expect("test environment must have a home dir");
        assert_eq!(
            expand_home(r"~\repositories\ralphus"),
            home.join(r"repositories\ralphus")
        );
    }

    #[test]
    fn expand_home_leaves_a_plain_path_unchanged() {
        assert_eq!(
            expand_home("C:\\Users\\me\\repo"),
            PathBuf::from("C:\\Users\\me\\repo")
        );
    }

    #[test]
    fn expand_home_does_not_touch_another_users_tilde() {
        assert_eq!(expand_home("~other/repo"), PathBuf::from("~other/repo"));
    }
}
