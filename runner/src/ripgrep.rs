//! RAL-522: ripgrep (`rg`) availability probe shared by `cli/src/health.rs`
//! (the `ralphus check health` Harness section) and
//! `daemon/src/health_sweep.rs` (the daemon's hourly Free-tier sweep, which
//! backs the board's Health tab). One implementation in the one crate both
//! call sites already depend on, so the CLI's report and the Health tab can
//! never disagree about whether `rg` is usable -- the same "one shared
//! probe" reasoning as [`crate::cli_agent_common::diagnose_command`].
//!
//! [`probe_version`] resolves `rg` on PATH and invokes the resolved
//! executable with `--version`, reported as a single `ripgrep` diagnostic
//! whose detail distinguishes "not installed" from "installed but broken".
//! It never reports a `fail`: agents are prompted to prefer `rg` and fall
//! back to `grep` when it's unavailable (`runner/src/runner.rs`'s tools
//! system prompt), so a missing or broken ripgrep degrades search speed,
//! not correctness.

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use ralphus_core::process::which;

/// The program name the probe resolves. No override env var exists for this
/// one: ripgrep is a convenience, not a load-bearing binary, so unlike
/// `tmux`/the runner binary there is no `RALPHUS_*_CMD` escape hatch to
/// honor.
pub const RG_PROGRAM: &str = "rg";

/// How long `rg --version` may run before the child is killed and the
/// version probe reported as a timeout failure. Real ripgrep prints its
/// version in milliseconds; this only bounds a broken or shimmed binary so
/// the hourly sweep can never hang on it.
const VERSION_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Outcome of the ripgrep health check. Every
/// non-[`RipgrepVersionProbe::Ok`] shape carries its own distinct detail so
/// an operator can tell "not installed" apart from "installed but broken".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RipgrepVersionProbe {
    /// `rg --version` ran cleanly and reported this version string.
    Ok { path: String, version: String },
    /// `rg` does not resolve on PATH, so there is nothing to invoke.
    NotFound,
    /// `rg` resolved but could not be spawned/executed.
    SpawnFailed { path: String, detail: String },
    /// `rg` ran but exited non-zero, timed out, or printed no recognizable
    /// `ripgrep <version>` line.
    BadOutput { path: String, detail: String },
}

impl RipgrepVersionProbe {
    /// The check status this outcome reports -- a clean pass or a `warn`,
    /// never a `fail`.
    #[must_use]
    pub fn status(&self) -> &'static str {
        match self {
            Self::Ok { .. } => "pass",
            _ => "warn",
        }
    }

    /// The human-readable observation for this outcome.
    #[must_use]
    pub fn detail(&self) -> String {
        match self {
            Self::Ok { path, version } => format!("{path} ({version})"),
            Self::NotFound => format!("{RG_PROGRAM} not found on PATH"),
            Self::SpawnFailed { detail, .. } | Self::BadOutput { detail, .. } => detail.clone(),
        }
    }

    /// The resolved executable path this outcome probed, when one was
    /// resolved at all -- lets callers attach it as provenance.
    #[must_use]
    pub fn resolved_path(&self) -> Option<&str> {
        match self {
            Self::Ok { path, .. }
            | Self::SpawnFailed { path, .. }
            | Self::BadOutput { path, .. } => Some(path),
            Self::NotFound => None,
        }
    }
}

/// Resolves `rg` on PATH (no cwd-first search -- the same PATH-only rule as
/// [`which`] itself) and, when found, invokes it with `--version`.
#[must_use]
pub fn probe_version() -> RipgrepVersionProbe {
    match which(RG_PROGRAM) {
        Some(path) => probe_version_at(&path),
        None => RipgrepVersionProbe::NotFound,
    }
}

/// Runs `<resolved_path> --version` and classifies the result. Split out
/// from [`probe_version`] so tests can exercise every failure shape against
/// a synthetic path without mutating the real PATH environment.
#[must_use]
pub fn probe_version_at(resolved_path: &str) -> RipgrepVersionProbe {
    let mut child = match Command::new(resolved_path)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            return RipgrepVersionProbe::SpawnFailed {
                path: resolved_path.to_string(),
                detail: format!("could not run {resolved_path} --version: {error}"),
            };
        }
    };

    // Drain stdout/stderr on background threads while polling for exit so
    // the read side can never deadlock behind a filled pipe buffer, killing
    // the child once the timeout elapses without a natural exit -- the same
    // shape as pi_backend's version probe.
    let mut stdout_pipe = child.stdout.take();
    let mut stderr_pipe = child.stderr.take();
    let stdout_thread = std::thread::spawn(move || {
        let mut buf = String::new();
        if let Some(pipe) = stdout_pipe.as_mut() {
            let _ = pipe.read_to_string(&mut buf);
        }
        buf
    });
    let stderr_thread = std::thread::spawn(move || {
        let mut buf = String::new();
        if let Some(pipe) = stderr_pipe.as_mut() {
            let _ = pipe.read_to_string(&mut buf);
        }
        buf
    });

    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(error) => {
                return RipgrepVersionProbe::SpawnFailed {
                    path: resolved_path.to_string(),
                    detail: format!("{resolved_path} --version: {error}"),
                };
            }
        }
        if start.elapsed() >= VERSION_PROBE_TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            return RipgrepVersionProbe::BadOutput {
                path: resolved_path.to_string(),
                detail: format!(
                    "{resolved_path} --version did not exit within {VERSION_PROBE_TIMEOUT:?}"
                ),
            };
        }
        std::thread::sleep(Duration::from_millis(50));
    };

    let stdout = stdout_thread.join().unwrap_or_default();
    let stderr = stderr_thread.join().unwrap_or_default();
    if !status.success() {
        return RipgrepVersionProbe::BadOutput {
            path: resolved_path.to_string(),
            detail: format!(
                "{resolved_path} --version exited with {status}: {}",
                first_line(stderr.trim())
            ),
        };
    }
    match parse_rg_version(&stdout) {
        Some(version) => RipgrepVersionProbe::Ok {
            path: resolved_path.to_string(),
            version: version.to_string(),
        },
        None => RipgrepVersionProbe::BadOutput {
            path: resolved_path.to_string(),
            detail: format!(
                "{resolved_path} --version did not report a ripgrep version: {}",
                first_line(stdout.trim())
            ),
        },
    }
}

/// The first line of a string, for quoting one line of a child's output in
/// a detail message.
fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or("")
}

/// Extracts the version from `rg --version` output -- its first line is
/// `ripgrep <version>` (e.g. `"ripgrep 14.1.1\n-PCRE2 ..."` -> `"14.1.1"`).
/// `None` when the first line isn't shaped that way, which is how a
/// shadowing `rg` that isn't actually ripgrep announces itself.
#[must_use]
fn parse_rg_version(output: &str) -> Option<&str> {
    let first = first_line(output).trim();
    let version = first.strip_prefix("ripgrep ")?.trim();
    if version.is_empty() {
        None
    } else {
        Some(version)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_rg_version_extracts_the_first_lines_version() {
        assert_eq!(parse_rg_version("ripgrep 14.1.1\n"), Some("14.1.1"));
        assert_eq!(
            parse_rg_version("ripgrep 13.0.0\n-PCRE2 10.39\n-JIT\n"),
            Some("13.0.0")
        );
        assert_eq!(parse_rg_version("ripgrep 14.1.1\n\nmore"), Some("14.1.1"));
    }

    #[test]
    fn parse_rg_version_rejects_unrecognizable_output() {
        assert_eq!(parse_rg_version("not ripgrep 1.2.3"), None);
        assert_eq!(parse_rg_version("ripgrep"), None);
        assert_eq!(parse_rg_version("ripgrep \n"), None);
        assert_eq!(parse_rg_version(""), None);
    }

    #[test]
    fn probe_version_at_reports_spawn_failure_for_a_missing_path() {
        let probe = probe_version_at("definitely-not-a-real-rg-ral522");
        match &probe {
            RipgrepVersionProbe::SpawnFailed { path, detail } => {
                assert_eq!(path, "definitely-not-a-real-rg-ral522");
                assert!(detail.contains("could not run"), "{detail}");
            }
            other => panic!("expected SpawnFailed, got {other:?}"),
        }
        assert_eq!(probe.status(), "warn");
    }

    #[test]
    fn probe_version_at_reports_spawn_failure_for_a_non_executable_file() {
        let path = std::env::temp_dir().join(format!(
            "ralphus-rg-probe-notexe-{}-{}",
            std::process::id(),
            line!()
        ));
        std::fs::write(&path, "this is not an executable").expect("write temp file");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))
                .expect("clear execute bits");
        }
        let probe = probe_version_at(&path.to_string_lossy());
        assert!(
            matches!(probe, RipgrepVersionProbe::SpawnFailed { .. }),
            "{probe:?}"
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn probe_version_at_reports_bad_output_for_a_non_ripgrep_program() {
        // `cargo` is on PATH for this test to have run, and its
        // `--version` exits cleanly -- but doesn't say "ripgrep".
        let probe = probe_version_at("cargo");
        match &probe {
            RipgrepVersionProbe::BadOutput { path, detail } => {
                assert_eq!(path, "cargo");
                assert!(
                    detail.contains("did not report a ripgrep version"),
                    "{detail}"
                );
            }
            other => panic!("expected BadOutput, got {other:?}"),
        }
    }

    #[test]
    fn probe_version_invokes_the_rg_resolved_on_path() {
        if let Some(path) = which(RG_PROGRAM) {
            let version_probe = probe_version();
            assert_eq!(version_probe.resolved_path(), Some(path.as_str()));
            if let RipgrepVersionProbe::Ok { version, .. } = &version_probe {
                assert!(!version.is_empty(), "{version:?}");
                assert_eq!(version_probe.status(), "pass");
            } else {
                // An `rg` on PATH that can't report a version is a legal
                // machine state (a broken install); the probe must still
                // never call it a failure.
                assert_eq!(version_probe.status(), "warn", "{version_probe:?}");
            }
        } else {
            assert_eq!(probe_version(), RipgrepVersionProbe::NotFound);
        }
    }
}
