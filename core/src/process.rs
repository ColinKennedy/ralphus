//! Small, dependency-free process/PATH helpers shared by more than one
//! crate's health checks (RAL-416) -- `cli/src/health.rs` has its own
//! long-established `which` used by every CLI-process-local check; this is
//! a separate copy for `daemon/src/health_sweep.rs`, which cannot depend on
//! `ralphus-cli` (the dependency runs the other way: `cli` depends on
//! `daemon`). Kept here, in the one crate both already depend on, rather
//! than duplicated a third time.

use std::path::{Path, PathBuf};

/// Whether `value` is a compound shell command rather than one executable
/// name or path. A fully quoted path may contain spaces without being a
/// compound command.
#[must_use]
pub fn is_compound_shell_command(value: &str) -> bool {
    let stripped = value.trim();
    if !stripped.contains(' ') {
        return false;
    }
    let bytes = stripped.as_bytes();
    if bytes.len() >= 2 {
        let first = bytes[0];
        let last = bytes[bytes.len() - 1];
        if first == last && (first == b'\'' || first == b'"') {
            let inner = &stripped[1..stripped.len() - 1];
            if !inner.contains(first as char) {
                return false;
            }
        }
    }
    true
}

/// Strips one layer of wrapping quotes from a bare executable path.
#[must_use]
pub fn unquote_path(value: &str) -> String {
    let stripped = value.trim();
    let bytes = stripped.as_bytes();
    if bytes.len() >= 2 {
        let first = bytes[0];
        let last = bytes[bytes.len() - 1];
        if first == last && (first == b'\'' || first == b'"') {
            return stripped[1..stripped.len() - 1].to_string();
        }
    }
    stripped.to_string()
}

/// `shutil.which` equivalent: PATH search only (no cwd-first search).
/// Windows additionally tries every `PATHEXT` suffix (falling back to the
/// usual `.COM;.EXE;.BAT;.CMD` when unset, matching `cmd.exe`'s own
/// default).
#[must_use]
pub fn which(program: &str) -> Option<String> {
    let path_var = std::env::var("PATH").ok()?;
    let mut suffixes = vec![String::new()];
    if cfg!(windows) {
        let pathext =
            std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string());
        suffixes.extend(
            pathext
                .split(';')
                .filter(|s| !s.is_empty())
                .map(str::to_string),
        );
    }
    for dir in std::env::split_paths(&path_var) {
        for suffix in &suffixes {
            let candidate: PathBuf = dir.join(format!("{program}{suffix}"));
            if is_executable(&candidate) {
                return Some(candidate.to_string_lossy().into_owned());
            }
        }
    }
    None
}

/// Whether `path` names a file the host can execute.
///
/// Unix requires at least one execute permission bit. Windows has no
/// equivalent permission bit, so executable filename extensions are the
/// meaningful check there; `is_file` also ensures the target is accessible
/// as a regular file.
#[must_use]
pub fn is_executable(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::metadata(path)
            .map(|metadata| metadata.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(windows)]
    {
        let extensions =
            std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string());
        let extension = path.extension().and_then(|value| value.to_str());
        extension.is_some_and(|extension| {
            extensions.split(';').any(|candidate| {
                candidate
                    .strip_prefix('.')
                    .is_some_and(|candidate| candidate.eq_ignore_ascii_case(extension))
            })
        })
    }
    #[cfg(not(any(unix, windows)))]
    {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn which_finds_a_binary_known_to_exist_in_this_test_environment() {
        // `cargo`/`rustc` must be on PATH for this test itself to have run.
        assert!(which("cargo").is_some() || which("rustc").is_some());
    }

    #[test]
    fn which_returns_none_for_a_program_that_cannot_exist() {
        assert!(which("definitely-not-a-real-program-ral416").is_none());
    }

    #[cfg(windows)]
    #[test]
    fn windows_executability_requires_a_pathext_extension() {
        use std::fs::File;

        let root = std::env::temp_dir().join(format!(
            "ralphus-health-executable-test-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).expect("create temporary test directory");
        let exe = root.join("agent.cmd");
        let text = root.join("agent.txt");
        File::create(&exe).expect("create executable-extension file");
        File::create(&text).expect("create non-executable-extension file");

        assert!(is_executable(&exe));
        assert!(!is_executable(&text));

        let _ = std::fs::remove_file(exe);
        let _ = std::fs::remove_file(text);
        let _ = std::fs::remove_dir(root);
    }
}
