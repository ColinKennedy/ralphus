//! Building the source-transfer commands that copy a local worktree onto the
//! remote host before `exec` runs a session there (RAL-200).
//!
//! `rsync` is not on the PATH of the Windows host this daemon may run on, but
//! `ssh`/`scp`/`tar` are (Windows ships OpenSSH + bsdtar in `System32`). So
//! the mandatory fallback everywhere is a `tar | ssh` stream: `tar` writes an
//! archive of the local directory to its own stdout, which is piped directly
//! into an `ssh` child whose stdin feeds a remote `tar -x`. Nothing here
//! spawns a shell to build that pipe -- the caller wires the two child
//! processes' stdio handles together directly (`Stdio::from(child.stdout)`),
//! so no OS-specific shell-pipe syntax (`|` in `cmd.exe` vs POSIX sh) is
//! needed at all. `rsync` stays an opportunistic fast path, used only when
//! detected on PATH -- which in practice means a non-Windows daemon host.
//!
//! Every function here is a pure argv/string builder: nothing spawns a
//! process, so the per-OS branching and exact flags are fully unit-testable
//! without a live remote host.

/// The local daemon host's OS, as far as transport selection cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostOs {
    Windows,
    Unix,
}

/// The local daemon host's actual OS.
#[must_use]
pub fn local_os() -> HostOs {
    if cfg!(windows) {
        HostOs::Windows
    } else {
        HostOs::Unix
    }
}

/// Which transport to use for one source sync.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    /// `rsync` over the same non-interactive `ssh` invocation.
    Rsync,
    /// `tar | ssh tar -x` -- always available given stock OpenSSH + bsdtar/GNU
    /// tar, so this is the guaranteed-working fallback.
    TarSsh,
}

/// Pick a transport for `os`, given whether `rsync` was found on PATH.
///
/// On [`HostOs::Windows`] `rsync` is never even probed for -- it is not part
/// of the stock Windows tooling this provider targets, so a PATH lookup would
/// just be a wasted process spawn on the common case. Every other OS uses it
/// when available and falls back otherwise.
#[must_use]
pub fn choose_transport(os: HostOs, rsync_available: bool) -> Transport {
    match os {
        HostOs::Windows => Transport::TarSsh,
        HostOs::Unix => {
            if rsync_available {
                Transport::Rsync
            } else {
                Transport::TarSsh
            }
        }
    }
}

/// Build/vendor directories excluded from every source sync by default
/// (RAL-200). Cheap to get wrong and expensive over the wire -- these are the
/// directories that are both large and reproducible from source, so shipping
/// them is pure waste.
pub const DEFAULT_EXCLUDES: &[&str] = &[
    ".git",
    "target",
    "node_modules",
    ".venv",
    "venv",
    "__pycache__",
    ".mypy_cache",
    ".pytest_cache",
    ".ruff_cache",
    "dist",
    "build",
    ".next",
    ".turbo",
    "*.pyc",
    ".DS_Store",
];

/// Merge the default exclude list with operator-configured extras
/// (`RALPHUS_SSH_EXCLUDE`, comma-separated), deduplicated and order-preserving.
/// Additive rather than replacing: an operator adding one more pattern should
/// never have to remember to re-list every default just to avoid accidentally
/// shipping `.git` or `target` over the wire.
#[must_use]
pub fn merge_excludes(defaults: &[&str], extra: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(defaults.len() + extra.len());
    for pat in defaults
        .iter()
        .map(|s| (*s).to_string())
        .chain(extra.iter().cloned())
    {
        let pat = pat.trim().to_string();
        if !pat.is_empty() && !out.contains(&pat) {
            out.push(pat);
        }
    }
    out
}

/// POSIX single-quote a string for safe embedding in a remote shell command
/// string: wraps in `'...'`, escaping any embedded `'` as `'\''`.
#[must_use]
pub fn shell_quote_single(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Build the local `tar` argv (program args only, not the program name) that
/// archives `local_dir`'s contents to stdout, excluding `excludes`.
///
/// GNU tar and bsdtar (Windows' `tar.exe`) both accept this exact flag set,
/// which is why no further per-OS branching is needed here beyond which
/// transport got chosen in the first place.
#[must_use]
pub fn tar_create_args(local_dir: &str, excludes: &[String]) -> Vec<String> {
    let mut args: Vec<String> = excludes.iter().map(|p| format!("--exclude={p}")).collect();
    args.push("-cf".to_string());
    args.push("-".to_string());
    args.push("-C".to_string());
    args.push(local_dir.to_string());
    args.push(".".to_string());
    args
}

/// Build the remote shell command string that extracts a tar stream (read
/// from stdin) into `remote_dir`, creating it first. Assumes a POSIX-like
/// remote shell (the common case for an SSH-reachable dev/build box); see
/// `docs/machine-providers.md`'s SSH section for the scope note.
#[must_use]
pub fn remote_tar_extract_command(remote_dir: &str) -> String {
    let q = shell_quote_single(remote_dir);
    format!("mkdir -p {q} && tar -xf - -C {q}")
}

/// Build the `rsync` argv (program args only) syncing `local_dir` onto
/// `target:remote_dir` over the given non-interactive `ssh` invocation
/// (`ssh_program` + `ssh_args`, joined into the `-e` value rsync expects).
///
/// `--delete` keeps the remote workspace a mirror of the local one rather
/// than an ever-growing superset across repeated `exec` calls against the
/// same session.
#[must_use]
pub fn rsync_args(
    local_dir: &str,
    target: &str,
    remote_dir: &str,
    excludes: &[String],
    ssh_program: &str,
    ssh_args: &[String],
) -> Vec<String> {
    let mut e_value = ssh_program.to_string();
    for a in ssh_args {
        e_value.push(' ');
        e_value.push_str(a);
    }
    let mut args = vec!["-a".to_string(), "--delete".to_string()];
    args.extend(excludes.iter().map(|p| format!("--exclude={p}")));
    args.push("-e".to_string());
    args.push(e_value);
    // Trailing slash on the source: copy `local_dir`'s *contents*, not a new
    // directory named after it, into `remote_dir`.
    args.push(format!("{}/", local_dir.trim_end_matches(['/', '\\'])));
    args.push(format!("{target}:{remote_dir}/"));
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_always_uses_tar_ssh_even_if_rsync_would_be_available() {
        assert_eq!(choose_transport(HostOs::Windows, true), Transport::TarSsh);
        assert_eq!(choose_transport(HostOs::Windows, false), Transport::TarSsh);
    }

    #[test]
    fn unix_prefers_rsync_when_available_and_falls_back_otherwise() {
        assert_eq!(choose_transport(HostOs::Unix, true), Transport::Rsync);
        assert_eq!(choose_transport(HostOs::Unix, false), Transport::TarSsh);
    }

    #[test]
    fn merge_excludes_is_additive_and_deduplicates() {
        let merged = merge_excludes(
            &["a", "b"],
            &["b".to_string(), "c".to_string(), "  ".to_string()],
        );
        assert_eq!(merged, vec!["a", "b", "c"]);
    }

    #[test]
    fn default_excludes_cover_the_common_build_and_vendor_directories() {
        for must_have in [".git", "target", "node_modules", ".venv", "__pycache__"] {
            assert!(
                DEFAULT_EXCLUDES.contains(&must_have),
                "missing default exclude: {must_have}"
            );
        }
    }

    #[test]
    fn tar_create_args_excludes_before_the_archive_flags_and_ends_with_dot() {
        let args = tar_create_args("C:/work/repo", &["target".to_string(), ".git".to_string()]);
        assert_eq!(
            args,
            vec![
                "--exclude=target",
                "--exclude=.git",
                "-cf",
                "-",
                "-C",
                "C:/work/repo",
                ".",
            ]
        );
    }

    #[test]
    fn remote_tar_extract_command_quotes_the_path_and_creates_it_first() {
        let cmd = remote_tar_extract_command("/home/alice/work with space");
        assert_eq!(
            cmd,
            "mkdir -p '/home/alice/work with space' && tar -xf - -C '/home/alice/work with space'"
        );
    }

    #[test]
    fn remote_tar_extract_command_escapes_embedded_single_quotes() {
        let cmd = remote_tar_extract_command("/home/alice/o'brien");
        assert!(cmd.contains(r"o'\''brien"), "{cmd}");
    }

    #[test]
    fn rsync_args_carry_archive_delete_excludes_and_the_ssh_transport() {
        let ssh_args = vec!["-o".to_string(), "BatchMode=yes".to_string()];
        let args = rsync_args(
            "/local/repo",
            "alice@host",
            "/remote/repo",
            &["target".to_string()],
            "ssh",
            &ssh_args,
        );
        assert!(args.contains(&"-a".to_string()));
        assert!(args.contains(&"--delete".to_string()));
        assert!(args.contains(&"--exclude=target".to_string()));
        let e_idx = args.iter().position(|a| a == "-e").expect("has -e flag");
        assert_eq!(args[e_idx + 1], "ssh -o BatchMode=yes");
        assert_eq!(args.last().unwrap(), "alice@host:/remote/repo/");
        assert!(args[e_idx + 2].ends_with('/'), "source must end with '/'");
    }

    #[test]
    fn shell_quote_single_escapes_embedded_quotes() {
        assert_eq!(shell_quote_single("plain"), "'plain'");
        assert_eq!(shell_quote_single("o'brien"), r"'o'\''brien'");
    }
}
