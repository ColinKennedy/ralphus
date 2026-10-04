//! RAL-546: a generic `<program> <version-args>` probe shared by every
//! external-tool health check that only needs "run it, read a version out of
//! its output" (git, gh, glab, tmux, Claude Code, Codex) -- one spawn/drain/
//! timeout/classify implementation parameterized by program, version flag(s),
//! and an extraction closure, instead of a bespoke module per tool. Generalizes
//! the exact shape [`crate::ripgrep`]'s `probe_version`/`probe_version_at`
//! established for ripgrep (RAL-522), which keeps its own dedicated module
//! for its stricter `ripgrep <version>` output check.
//!
//! Like ripgrep's probe, this never reports a hard failure of its own --
//! [`VersionProbe::status`] is a clean pass or a `warn`. Each tool-specific
//! call site (`daemon/src/health_sweep.rs`) decides whether a `NotFound`
//! should sink its own check to `fail` (git, tmux -- load-bearing) or stay a
//! `pass` (gh, glab -- optional, agents fall back without them), the same
//! per-tool judgment call ripgrep's callers already make.

use std::io::Read as _;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use ralphus_core::process::which;

/// Normal upper bound for a version command. Callers may choose a tighter
/// bound when their health-check budget requires it; the probe kills a broken
/// or shimmed binary when that bound elapses.
pub const DEFAULT_VERSION_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Outcome of probing a program's version. Every non-[`VersionProbe::Ok`]
/// shape carries its own distinct detail so an operator can tell "not
/// installed" apart from "installed but broken".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VersionProbe {
    /// The version command ran cleanly and the extractor recognized a version.
    Ok { path: String, version: String },
    /// The program does not resolve on PATH, so there is nothing to invoke.
    NotFound,
    /// The program resolved but could not be spawned/executed.
    SpawnFailed { path: String, detail: String },
    /// The program ran but exited non-zero, timed out, or the extractor did
    /// not recognize its output.
    BadOutput { path: String, detail: String },
}

impl VersionProbe {
    /// The check status this outcome reports -- a clean pass or a `warn`,
    /// never a `fail`. Callers that want a given tool's absence to sink the
    /// whole check to `fail` (e.g. git, tmux) decide that themselves from
    /// `matches!(probe, VersionProbe::NotFound)`, rather than this type
    /// hard-coding that judgment for every tool.
    #[must_use]
    pub fn status(&self) -> &'static str {
        match self {
            Self::Ok { .. } => "pass",
            _ => "warn",
        }
    }

    /// The human-readable observation for this outcome.
    #[must_use]
    pub fn detail(&self, program: &str) -> String {
        match self {
            Self::Ok { path, version } => format!("{path} (version {version})"),
            Self::NotFound => format!("{program} not found on PATH"),
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

/// Resolves `program` on PATH and, when found, invokes it with
/// `version_args`, bounded by `timeout`, extracting a version from its output
/// via `extract`.
#[must_use]
pub fn probe_version(
    program: &str,
    version_args: &[&str],
    timeout: Duration,
    extract: impl Fn(&str) -> Option<String>,
) -> VersionProbe {
    match which(program) {
        Some(path) => probe_version_at(&path, version_args, timeout, extract),
        None => VersionProbe::NotFound,
    }
}

/// Runs `<resolved_path> <version_args>` with `timeout` and classifies the
/// result via `extract`. Split out from [`probe_version`] so tests and callers
/// that already have a resolved path (e.g. a backend's configured command, or
/// tmux's env-override/embedded-psmux path) can exercise every failure shape
/// without going through a second PATH resolution.
#[must_use]
pub fn probe_version_at(
    resolved_path: &str,
    version_args: &[&str],
    timeout: Duration,
    extract: impl Fn(&str) -> Option<String>,
) -> VersionProbe {
    let args_display = version_args.join(" ");
    let mut child = match Command::new(resolved_path)
        .args(version_args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            return VersionProbe::SpawnFailed {
                path: resolved_path.to_string(),
                detail: format!("could not run {resolved_path} {args_display}: {error}"),
            };
        }
    };

    // Drain stdout/stderr on background threads while polling for exit so
    // the read side can never deadlock behind a filled pipe buffer, killing
    // the child once the timeout elapses without a natural exit -- the same
    // shape as ripgrep's and pi_backend's version probes.
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
                return VersionProbe::SpawnFailed {
                    path: resolved_path.to_string(),
                    detail: format!("{resolved_path} {args_display}: {error}"),
                };
            }
        }
        if start.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            return VersionProbe::BadOutput {
                path: resolved_path.to_string(),
                detail: format!("{resolved_path} {args_display} did not exit within {timeout:?}"),
            };
        }
        std::thread::sleep(Duration::from_millis(50));
    };

    let stdout = stdout_thread.join().unwrap_or_default();
    let stderr = stderr_thread.join().unwrap_or_default();
    if !status.success() {
        return VersionProbe::BadOutput {
            path: resolved_path.to_string(),
            detail: format!(
                "{resolved_path} {args_display} exited with {status}: {}",
                first_line(stderr.trim())
            ),
        };
    }
    match extract(&stdout) {
        Some(version) => VersionProbe::Ok {
            path: resolved_path.to_string(),
            version,
        },
        None => VersionProbe::BadOutput {
            path: resolved_path.to_string(),
            detail: format!(
                "{resolved_path} {args_display} did not report a recognizable version: {}",
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

/// A ready-made `extract` closure body for the common "first line starts
/// with a literal prefix, the version is the whitespace token right after
/// it" shape -- e.g. `"git version 2.44.0.windows.1"` with prefix
/// `"git version "` -> `"2.44.0.windows.1"`. An empty prefix just takes the
/// first line's first token, which is what Claude Code's own
/// `"2.1.229 (Claude Code)"` output needs (no literal prefix to strip).
/// `None` when the first line doesn't start with `prefix` at all, which is
/// how a shadowing binary announces it isn't the real tool.
#[must_use]
pub fn first_token_after_prefix(output: &str, prefix: &str) -> Option<String> {
    let first = first_line(output).trim();
    let rest = first.strip_prefix(prefix)?.trim_start();
    let token = rest.split_whitespace().next()?;
    Some(token.to_string())
}

/// The `extract` closure body for `nvidia-smi --version`, whose output is a
/// `key : value` table rather than a single version line:
///
/// ```text
/// NVIDIA-SMI version  : 616.56
/// NVML version        : 616.56
/// DRIVER version      : Deprecated, see "KMD version" instead
/// CUDA version        : Deprecated, see "CUDA UMD version" instead
/// KMD version         : 616.56
/// CUDA UMD version    : 13.4
/// ```
///
/// Returns the `NVIDIA-SMI version` followed by the driver and CUDA versions,
/// e.g. `"616.56, KMD 616.56, CUDA UMD 13.4"`. Older drivers report the
/// driver/CUDA versions under `DRIVER version`/`CUDA version` instead, so
/// those are the fallback whenever their value isn't a `Deprecated` notice.
/// `None` when there is no `NVIDIA-SMI version` row at all.
#[must_use]
pub fn nvidia_smi_version(output: &str) -> Option<String> {
    let value_of = |key: &str| {
        output.lines().find_map(|line| {
            let (name, value) = line.split_once(':')?;
            let value = value.trim();
            (name.trim().eq_ignore_ascii_case(key)
                && !value.is_empty()
                && !value.starts_with("Deprecated"))
            .then(|| value.to_string())
        })
    };
    let mut parts = vec![value_of("NVIDIA-SMI version")?];
    if let Some(driver) = value_of("KMD version") {
        parts.push(format!("KMD {driver}"));
    } else if let Some(driver) = value_of("DRIVER version") {
        parts.push(format!("driver {driver}"));
    }
    if let Some(cuda) = value_of("CUDA UMD version") {
        parts.push(format!("CUDA UMD {cuda}"));
    } else if let Some(cuda) = value_of("CUDA version") {
        parts.push(format!("CUDA {cuda}"));
    }
    Some(parts.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nvidia_smi_version_reads_the_kmd_and_cuda_umd_rows() {
        let output = "NVIDIA-SMI version  : 616.56\n\
                      NVML version        : 616.56\n\
                      DRIVER version      : Deprecated, see \"KMD version\" instead\n\
                      CUDA version        : Deprecated, see \"CUDA UMD version\" instead\n\
                      KMD version         : 616.56\n\
                      CUDA UMD version    : 13.4\n";
        assert_eq!(
            nvidia_smi_version(output),
            Some("616.56, KMD 616.56, CUDA UMD 13.4".to_string())
        );
    }

    #[test]
    fn nvidia_smi_version_falls_back_to_the_older_driver_and_cuda_rows() {
        let output = "NVIDIA-SMI version  : 550.54.14\n\
                      NVML version        : 550.54\n\
                      DRIVER version      : 550.54.14\n\
                      CUDA Version        : 12.4\n";
        assert_eq!(
            nvidia_smi_version(output),
            Some("550.54.14, driver 550.54.14, CUDA 12.4".to_string())
        );
    }

    #[test]
    fn nvidia_smi_version_rejects_output_without_an_nvidia_smi_row() {
        assert_eq!(nvidia_smi_version(""), None);
        assert_eq!(
            nvidia_smi_version("Invalid combination of input arguments."),
            None
        );
    }

    #[test]
    fn first_token_after_prefix_strips_a_literal_prefix() {
        assert_eq!(
            first_token_after_prefix("ripgrep 14.1.1\n", "ripgrep "),
            Some("14.1.1".to_string())
        );
        assert_eq!(
            first_token_after_prefix("git version 2.44.0.windows.1\n", "git version "),
            Some("2.44.0.windows.1".to_string())
        );
        assert_eq!(
            first_token_after_prefix("gh version 2.93.0 (2026-05-27)\nhttps://...", "gh version "),
            Some("2.93.0".to_string())
        );
        assert_eq!(
            first_token_after_prefix("glab 1.114.0 (4d7c6cd)\n", "glab "),
            Some("1.114.0".to_string())
        );
        assert_eq!(
            first_token_after_prefix("tmux 3.3.8\npsmux 3.3.8 (66cf613 2026-08-18)\n", "tmux "),
            Some("3.3.8".to_string())
        );
    }

    #[test]
    fn first_token_after_prefix_with_empty_prefix_takes_the_first_token() {
        assert_eq!(
            first_token_after_prefix("2.1.229 (Claude Code)\n", ""),
            Some("2.1.229".to_string())
        );
        assert_eq!(
            first_token_after_prefix("codex-cli 0.157.0\n", "codex-cli "),
            Some("0.157.0".to_string())
        );
    }

    #[test]
    fn first_token_after_prefix_rejects_unrecognizable_output() {
        assert_eq!(
            first_token_after_prefix("not git at all", "git version "),
            None
        );
        assert_eq!(
            first_token_after_prefix("git version", "git version "),
            None
        );
        assert_eq!(
            first_token_after_prefix("git version \n", "git version "),
            None
        );
        assert_eq!(first_token_after_prefix("", "git version "), None);
    }

    #[test]
    fn probe_version_at_reports_spawn_failure_for_a_missing_path() {
        let probe = probe_version_at(
            "definitely-not-a-real-program-ral546",
            &["--version"],
            DEFAULT_VERSION_PROBE_TIMEOUT,
            |_| None,
        );
        match &probe {
            VersionProbe::SpawnFailed { path, detail } => {
                assert_eq!(path, "definitely-not-a-real-program-ral546");
                assert!(detail.contains("could not run"), "{detail}");
            }
            other => panic!("expected SpawnFailed, got {other:?}"),
        }
        assert_eq!(probe.status(), "warn");
        assert_eq!(
            probe.resolved_path(),
            Some("definitely-not-a-real-program-ral546")
        );
    }

    #[test]
    fn probe_version_at_reports_bad_output_when_extract_finds_nothing() {
        // `cargo` is on PATH for this test itself to have run, and its
        // `--version` exits cleanly, but the extractor is rigged to never match.
        let probe = probe_version_at(
            "cargo",
            &["--version"],
            DEFAULT_VERSION_PROBE_TIMEOUT,
            |_| None,
        );
        match &probe {
            VersionProbe::BadOutput { path, detail } => {
                assert_eq!(path, "cargo");
                assert!(
                    detail.contains("did not report a recognizable version"),
                    "{detail}"
                );
            }
            other => panic!("expected BadOutput, got {other:?}"),
        }
        assert_eq!(probe.status(), "warn");
    }

    #[test]
    fn probe_version_at_extracts_a_real_cargo_version() {
        let probe = probe_version_at(
            "cargo",
            &["--version"],
            DEFAULT_VERSION_PROBE_TIMEOUT,
            |out| first_token_after_prefix(out, "cargo "),
        );
        match &probe {
            VersionProbe::Ok { path, version } => {
                assert_eq!(path, "cargo");
                assert!(!version.is_empty(), "{version:?}");
            }
            other => panic!("expected Ok, got {other:?}"),
        }
        assert_eq!(probe.status(), "pass");
    }

    #[test]
    fn not_found_detail_names_the_program() {
        let probe = VersionProbe::NotFound;
        assert_eq!(probe.detail("glab"), "glab not found on PATH");
        assert_eq!(probe.resolved_path(), None);
    }

    #[test]
    fn probe_version_reports_not_found_for_an_unresolvable_program() {
        let probe = probe_version(
            "definitely-not-a-real-program-ral546",
            &["--version"],
            DEFAULT_VERSION_PROBE_TIMEOUT,
            |_| None,
        );
        assert_eq!(probe, VersionProbe::NotFound);
    }
}
