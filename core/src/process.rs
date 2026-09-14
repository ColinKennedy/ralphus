//! Small, dependency-free process/PATH helpers shared by more than one
//! crate's health checks (RAL-416) -- `cli/src/health.rs` has its own
//! long-established `which` used by every CLI-process-local check; this is
//! a separate copy for `daemon/src/health_sweep.rs`, which cannot depend on
//! `ralphus-cli` (the dependency runs the other way: `cli` depends on
//! `daemon`). Kept here, in the one crate both already depend on, rather
//! than duplicated a third time.

use std::path::PathBuf;

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
            if candidate.is_file() {
                return Some(candidate.to_string_lossy().into_owned());
            }
        }
    }
    None
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
}
