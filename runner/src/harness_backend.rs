//! Generic external-harness `ModelBackend` (e.g. `aider`), ported from
//! `cli/src/ralphus/runner/harness_backend.py`. No stream parsing -- the
//! whole prompt is a trailing argv token and the whole stdout/stderr is
//! captured at once. `append_system_prompt`/`resume_agent_session_id` are
//! accepted for trait-shape compatibility but unused: there is no portable
//! flag a generic harness is guaranteed to understand for either.

use std::process::Command;
use std::time::Duration;

use crate::backend::{BackendError, BackendOutcome, ModelBackend, RunOptions};
use crate::tools::Workspace;

/// Matches `cli/src/ralphus/config.py`'s `TaskConfig.maximum_timeout_seconds`
/// default -- used when a session sets no `timeout_sec` of its own.
const DEFAULT_TIMEOUT_SECS: u64 = 1800;

/// The tail of captured output kept when reporting a result -- matches
/// Python's `_tail(text, 2000)` truncation length (trailing, not leading, so
/// a marker on the final line survives).
const SUMMARY_TAIL_CHARS: usize = 2000;
const ERROR_TAIL_CHARS: usize = 500;

pub struct HarnessBackend {
    /// The external program name (the agent name itself, e.g. `"aider"`).
    pub program: String,
}

impl ModelBackend for HarnessBackend {
    fn run(
        &self,
        prompt: &str,
        workspace: &Workspace,
        options: &RunOptions<'_>,
    ) -> Result<BackendOutcome, BackendError> {
        let mut cmd = Command::new(&self.program);
        cmd.current_dir(workspace.root());
        if let Some(model) = options.model {
            cmd.arg("--model").arg(model);
        }
        cmd.arg(prompt);

        let child = cmd
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| BackendError(format!("could not spawn {}: {e}", self.program)))?;

        let timeout = Duration::from_secs(options.timeout_sec.unwrap_or(DEFAULT_TIMEOUT_SECS));
        let output = wait_with_timeout(child, timeout)
            .map_err(|e| BackendError(format!("{} failed: {e}", self.program)))?;

        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

        if !output.status.success() {
            let detail = tail(&stderr, ERROR_TAIL_CHARS);
            let detail = if detail.is_empty() {
                tail(&stdout, ERROR_TAIL_CHARS)
            } else {
                detail
            };
            return Err(BackendError(format!(
                "{} exited with {:?}: {detail}",
                self.program,
                output.status.code()
            )));
        }

        Ok(BackendOutcome {
            summary: tail(&stdout, SUMMARY_TAIL_CHARS),
            tokens_in: 0,
            tokens_out: 0,
            cost_usd: 0.0,
            agent_session_id: None,
        })
    }
}

fn tail(text: &str, limit: usize) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= limit {
        trimmed.to_string()
    } else {
        trimmed
            .chars()
            .rev()
            .take(limit)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect()
    }
}

fn wait_with_timeout(
    mut child: std::process::Child,
    timeout: Duration,
) -> std::io::Result<std::process::Output> {
    let start = std::time::Instant::now();
    loop {
        if let Some(_status) = child.try_wait()? {
            return child.wait_with_output();
        }
        if start.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Err(std::io::Error::other(format!(
                "timed out after {}s",
                timeout.as_secs()
            )));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_keeps_trailing_chars_when_over_limit() {
        let text = "a".repeat(10);
        assert_eq!(tail(&text, 4), "aaaa");
    }

    #[test]
    fn tail_keeps_whole_string_when_under_limit() {
        assert_eq!(tail("short", 2000), "short");
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn run_reports_nonzero_exit_as_backend_error() {
        let dir = std::env::temp_dir().join(format!("ralphus-harness-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let ws = Workspace::create(&dir).unwrap();
        let backend = HarnessBackend {
            program: "false".to_string(),
        };
        let err = backend.run("prompt", &ws, &RunOptions::default());
        assert!(err.is_err());
        std::fs::remove_dir_all(&dir).ok();
    }
}
