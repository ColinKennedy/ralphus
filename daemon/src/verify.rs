//! Verify-step execution.
//!
//! MVP scope: `command` verify steps (fmt/lint/test) run as shell commands in
//! the session's working directory — exit code is the verdict. `agent` and
//! `brain` verifiers need a model and are deferred (left pending) until the
//! native backend lands; `approval` verifiers need a human and are deferred too.

use std::process::{Command, Stdio};

use opentelemetry::Context;
use opentelemetry::trace::{SpanKind, Status};

/// Run a `command` verify step in `cwd`. Returns true when the command exits 0.
///
/// Used by callers (e.g. Guardian review checks) that have no trace to
/// continue; the span it creates starts a fresh trace of its own. Session/task
/// verify steps go through [`run_command_verify_capture`] directly so they can
/// pass the run's actual trace context (RAL-96).
#[must_use]
pub fn run_command_verify(cwd: &str, command: &str) -> bool {
    run_command_verify_capture(cwd, command, &Context::new()).0
}

/// Like [`run_command_verify`] but also captures the combined stdout+stderr
/// (truncated) so it can be shown in the log viewer (CCTL-99). Wraps the
/// subprocess in an OpenTelemetry span (RAL-96), as a child of `parent`.
#[must_use]
pub fn run_command_verify_capture(cwd: &str, command: &str, parent: &Context) -> (bool, String) {
    let span = crate::otel::start_span("verify.command", parent, SpanKind::Internal);
    span.set_attribute("verify.cwd", cwd.to_string());
    span.set_attribute("verify.command", command.to_string());
    crate::rlog!(
        DEBUG,
        "ralphus [verify] command starting cwd={cwd:?} command={command:?}"
    );
    let (shell, flag) = if cfg!(windows) {
        ("cmd", "/C")
    } else {
        ("sh", "-c")
    };
    let result = match Command::new(shell)
        .arg(flag)
        .arg(command)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .output()
    {
        Ok(out) => {
            let mut buf = String::new();
            buf.push_str(&String::from_utf8_lossy(&out.stdout));
            let err = String::from_utf8_lossy(&out.stderr);
            if !err.trim().is_empty() {
                buf.push_str(&err);
            }
            (out.status.success(), truncate_output(&buf))
        }
        Err(e) => (false, format!("could not run command: {e}")),
    };
    span.set_status(if result.0 {
        Status::Ok
    } else {
        Status::error("verify command failed")
    });
    crate::rlog!(
        DEBUG,
        "ralphus [verify] command completed passed={} cwd={cwd:?}",
        result.0
    );
    result
}

/// Cap captured output so a runaway verifier can't bloat the DB / UI.
fn truncate_output(s: &str) -> String {
    const MAX: usize = 16 * 1024;
    if s.len() <= MAX {
        return s.to_string();
    }
    let mut cut = MAX;
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}\n…(truncated)", &s[..cut])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passing_command_is_true() {
        assert!(run_command_verify(".", "exit 0"));
    }

    #[test]
    fn failing_command_is_false() {
        assert!(!run_command_verify(".", "exit 1"));
    }

    #[test]
    fn bad_cwd_is_false() {
        assert!(!run_command_verify("/no/such/dir/ralphus-xyz", "exit 0"));
    }

    #[test]
    fn capture_returns_output() {
        let (ok, out) = run_command_verify_capture(".", "echo hello-verify", &Context::new());
        assert!(ok);
        assert!(out.contains("hello-verify"), "captured: {out:?}");
    }

    #[test]
    fn truncate_caps_huge_output() {
        let big = "x".repeat(40_000);
        let out = truncate_output(&big);
        assert!(out.len() <= 16 * 1024 + 32, "not truncated: {}", out.len());
        assert!(out.ends_with("…(truncated)"));
        // Small output is returned verbatim.
        assert_eq!(truncate_output("short"), "short");
    }
}
