//! Building the `std::process::Command` for a `git` subprocess so that the
//! daemon's and the runner's high-frequency git calls cost as few OS process
//! creations as possible.
//!
//! On Windows every console program started by a process with no console of
//! its own gets a fresh `conhost.exe`, and Git for Windows' `cmd\git.exe` is a
//! launcher that starts the real `mingw64\bin\git.exe` as a second process.
//! A plain `Command::new("git")` from a console-less daemon therefore costs
//! three process creations per call (launcher, conhost, real git), and two
//! from a daemon that has a console. Endpoint security tooling counts each of
//! them.
//!
//! [`command`] cuts a *read-only* call to one process: it runs the real git
//! binary directly and starts it with `DETACHED_PROCESS`, so no console is
//! attached and no conhost is created. That flag is only safe for git
//! subcommands that never start console programs of their own -- a detached
//! git that runs `ssh`, a credential helper or a hook would give each of those
//! its own conhost instead -- so [`is_spawn_free`] is a conservative allowlist,
//! and every other subcommand is spawned exactly as `Command::new("git")`
//! always has been (PATH lookup, inherited console).
//!
//! Outside Windows both paths are plain `Command::new("git")`.

use std::process::Command;

/// Subcommands that never start another program, so detaching them from the
/// console cannot hand a console-less child to anything else.
const SPAWN_FREE_SUBCOMMANDS: &[&str] = &[
    "rev-parse",
    "for-each-ref",
    "show-ref",
    "symbolic-ref",
    "check-ref-format",
    "config",
    "ls-files",
    "ls-tree",
    "cat-file",
    "merge-base",
    "rev-list",
    "var",
];

/// The git subcommand in `args`, skipping global options (`-C <path>`,
/// `-c <k=v>`, `--git-dir=...`, `--no-pager`, ...).
fn subcommand<'a>(args: &[&'a str]) -> Option<&'a str> {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match *arg {
            "-C" | "-c" | "--git-dir" | "--work-tree" | "--namespace" => {
                iter.next();
            }
            a if a.starts_with('-') => {}
            a => return Some(a),
        }
    }
    None
}

/// Whether `git <args>` is known never to start another program.
///
/// `remote get-url` is included (it only reads config). `diff` is included
/// only with both `--no-ext-diff` and `--no-textconv`, since an external diff
/// driver or textconv filter is a program. `update-ref` is not: it runs the
/// `reference-transaction` hook.
#[must_use]
pub fn is_spawn_free(args: &[&str]) -> bool {
    match subcommand(args) {
        Some("diff") => args.contains(&"--no-ext-diff") && args.contains(&"--no-textconv"),
        Some("remote") => {
            let rest: Vec<&str> = args
                .iter()
                .skip_while(|a| **a != "remote")
                .skip(1)
                .copied()
                .collect();
            rest.first() == Some(&"get-url")
        }
        Some(sub) => SPAWN_FREE_SUBCOMMANDS.contains(&sub),
        None => false,
    }
}

/// A `git` command for `args`: one OS process for spawn-free subcommands on
/// Windows (see the module doc), otherwise exactly `Command::new("git")`.
///
/// The returned command has no arguments set yet; callers add `args` and the
/// rest of their configuration as before. A spawn-free command's stdin is set
/// to null, since a detached process has no console to inherit it from.
#[must_use]
pub fn command(args: &[&str]) -> Command {
    #[cfg(windows)]
    {
        if is_spawn_free(args) {
            if let Some(real) = windows::real_git() {
                return windows::detached(real);
            }
        }
    }
    let _ = args;
    Command::new("git")
}

#[cfg(windows)]
mod windows {
    use std::os::windows::process::CommandExt as _;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::sync::OnceLock;

    /// `DETACHED_PROCESS`: the child gets no console, so no conhost.
    const DETACHED_PROCESS: u32 = 0x0000_0008;

    /// The git binary that does the work, resolved once per process: when
    /// PATH's `git` is Git for Windows' `<root>\cmd\git.exe` launcher, its
    /// sibling `<root>\mingw64\bin\git.exe` (or `clangarm64`/`mingw32`);
    /// otherwise PATH's `git` itself. `None` when git is not on PATH.
    pub(super) fn real_git() -> Option<&'static Path> {
        static REAL: OnceLock<Option<PathBuf>> = OnceLock::new();
        REAL.get_or_init(|| {
            let found = PathBuf::from(crate::process::which("git")?);
            Some(behind_launcher(&found).unwrap_or(found))
        })
        .as_deref()
    }

    /// The real binary behind a `<root>\cmd\git.exe` launcher, if `path` is one.
    pub(super) fn behind_launcher(path: &Path) -> Option<PathBuf> {
        let dir = path.parent()?;
        if !dir
            .file_name()
            .is_some_and(|name| name.eq_ignore_ascii_case("cmd"))
        {
            return None;
        }
        let root = dir.parent()?;
        ["mingw64", "clangarm64", "mingw32"]
            .iter()
            .map(|arch| root.join(arch).join("bin").join("git.exe"))
            .find(|candidate| candidate.is_file())
    }

    pub(super) fn detached(program: &Path) -> Command {
        let mut command = Command::new(program);
        command
            .creation_flags(DETACHED_PROCESS)
            .stdin(Stdio::null());
        command
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_only_subcommands_are_spawn_free() {
        assert!(is_spawn_free(&["rev-parse", "HEAD"]));
        assert!(is_spawn_free(&["-C", "x", "for-each-ref", "refs/heads"]));
        assert!(is_spawn_free(&[
            "-c",
            "core.quotepath=off",
            "config",
            "--get",
            "a.b"
        ]));
        assert!(is_spawn_free(&["remote", "get-url", "origin"]));
        assert!(is_spawn_free(&[
            "diff",
            "--numstat",
            "--no-ext-diff",
            "--no-textconv",
            "HEAD"
        ]));
    }

    #[test]
    fn subcommands_that_can_start_programs_are_not_spawn_free() {
        for args in [
            &["fetch", "origin"][..],
            &["push", "origin", "x"],
            &["rebase", "main"],
            &["commit", "-m", "x"],
            &["worktree", "add", "p"],
            &["diff", "HEAD"],
            &["diff", "--no-ext-diff", "HEAD"],
            &["update-ref", "refs/x", "HEAD"],
            &["log", "-1"],
            &["status"],
            &["remote", "add", "o", "u"],
            &["ls-remote", "origin"],
            &[],
        ] {
            assert!(!is_spawn_free(args), "{args:?}");
        }
    }

    #[test]
    fn spawn_free_command_reports_the_same_result_as_plain_git() {
        let here = std::env::current_dir().expect("cwd");
        let ours = command(&["rev-parse", "--show-toplevel"])
            .args(["rev-parse", "--show-toplevel"])
            .current_dir(&here)
            .output()
            .expect("spawn git");
        let plain = Command::new("git")
            .args(["rev-parse", "--show-toplevel"])
            .current_dir(&here)
            .output()
            .expect("spawn git");
        assert_eq!(ours.status.success(), plain.status.success());
        assert_eq!(ours.stdout, plain.stdout);
    }

    #[cfg(windows)]
    #[test]
    fn launcher_resolves_to_the_real_binary_beside_it() {
        let root = std::env::temp_dir().join(format!("ralphus-git-spawn-{}", std::process::id()));
        let cmd_dir = root.join("cmd");
        let bin_dir = root.join("mingw64").join("bin");
        std::fs::create_dir_all(&cmd_dir).expect("cmd dir");
        std::fs::create_dir_all(&bin_dir).expect("bin dir");
        std::fs::write(cmd_dir.join("git.exe"), b"").expect("launcher");
        std::fs::write(bin_dir.join("git.exe"), b"").expect("real git");

        assert_eq!(
            windows::behind_launcher(&cmd_dir.join("git.exe")),
            Some(bin_dir.join("git.exe"))
        );
        assert_eq!(windows::behind_launcher(&bin_dir.join("git.exe")), None);

        let _ = std::fs::remove_dir_all(root);
    }
}
