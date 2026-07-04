//! Verify-step execution.
//!
//! MVP scope: `command` verify steps (fmt/lint/test) run as shell commands in
//! the session's working directory — exit code is the verdict. `agent` and
//! `brain` verifiers need a model and are deferred (left pending) until the
//! native backend lands; `approval` verifiers need a human and are deferred too.

use std::process::{Command, Stdio};

/// Run a `command` verify step in `cwd`. Returns true when the command exits 0.
#[must_use]
pub fn run_command_verify(cwd: &str, command: &str) -> bool {
    let (shell, flag) = if cfg!(windows) {
        ("cmd", "/C")
    } else {
        ("sh", "-c")
    };
    Command::new(shell)
        .arg(flag)
        .arg(command)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
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
}
