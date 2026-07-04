//! Invoking the Python session runner.
//!
//! The daemon builds a [`RunnerSpec`] for each session, hands it to a [`Runner`],
//! and gets back a [`RunnerResult`]. The real implementation
//! ([`SubprocessRunner`]) spawns the `ralphus-runner` process and speaks the JSON
//! contract in `cli/src/ralphus/runner/spec.py`. The trait keeps the scheduler
//! testable with an in-process fake.

use std::io::Write;
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};

use crate::store::{NodeState, SessionRow};

/// The JSON spec sent to the runner on stdin (mirrors Python `SessionSpec`).
#[derive(Debug, Clone, Serialize)]
pub struct RunnerSpec {
    /// Owning run id.
    pub run_id: String,
    /// Owning task name.
    pub task: String,
    /// Session id.
    pub session_id: String,
    /// Working directory.
    pub cwd: String,
    /// AI prompt, if this is a prompt session.
    pub prompt: Option<String>,
    /// Shell command, if this is a command session.
    pub command: Option<String>,
    /// Agent program.
    pub agent: String,
    /// Model, if any.
    pub model: Option<String>,
}

impl RunnerSpec {
    /// Build a spec from a stored session row.
    #[must_use]
    pub fn from_row(run_id: &str, row: &SessionRow) -> Self {
        Self {
            run_id: run_id.to_string(),
            task: row.task_name.clone(),
            session_id: row.session_id.clone(),
            cwd: row.cwd.clone().unwrap_or_default(),
            prompt: row.prompt.clone(),
            command: row.command.clone(),
            agent: row.agent.clone(),
            model: row.model.clone(),
        }
    }
}

/// The JSON result read from the runner's stdout (mirrors Python `SessionResult`).
#[derive(Debug, Clone, Deserialize)]
pub struct RunnerResult {
    /// `"done"` or `"failed"`.
    pub status: String,
    /// Input tokens used.
    #[serde(default)]
    pub tokens_in: i64,
    /// Output tokens used.
    #[serde(default)]
    pub tokens_out: i64,
    /// Cost in USD.
    #[serde(default)]
    pub cost_usd: f64,
    /// Short summary of what happened.
    #[serde(default)]
    pub summary: String,
    /// Error detail when failed.
    #[serde(default)]
    pub error: Option<String>,
}

impl RunnerResult {
    /// A synthetic failure (e.g. the runner process could not be spawned).
    #[must_use]
    pub fn failure(error: impl Into<String>) -> Self {
        Self {
            status: "failed".to_string(),
            tokens_in: 0,
            tokens_out: 0,
            cost_usd: 0.0,
            summary: String::new(),
            error: Some(error.into()),
        }
    }

    /// Whether the session succeeded.
    #[must_use]
    pub fn is_done(&self) -> bool {
        self.status == "done"
    }

    /// Map to a node state.
    #[must_use]
    pub fn node_state(&self) -> NodeState {
        if self.is_done() {
            NodeState::Done
        } else {
            NodeState::Failed
        }
    }
}

/// Something that can execute a session. Send + Sync so worker threads can share it.
pub trait Runner: Send + Sync {
    /// Execute a session and report its result.
    fn run(&self, spec: &RunnerSpec) -> RunnerResult;
}

/// Runs a session by spawning the configured runner program and speaking JSON
/// over stdin/stdout.
pub struct SubprocessRunner {
    program: String,
    args: Vec<String>,
}

impl SubprocessRunner {
    /// Build from a command line like `"ralphus-runner"` or `"python -m ralphus.runner"`.
    #[must_use]
    pub fn new(command_line: &str) -> Self {
        let mut parts = command_line.split_whitespace().map(str::to_string);
        let program = parts.next().unwrap_or_else(|| "ralphus-runner".to_string());
        Self {
            program,
            args: parts.collect(),
        }
    }

    /// Resolve the runner command from `RALPHUS_RUNNER_CMD`, defaulting to
    /// `ralphus-runner`.
    #[must_use]
    pub fn from_env() -> Self {
        let cmd =
            std::env::var("RALPHUS_RUNNER_CMD").unwrap_or_else(|_| "ralphus-runner".to_string());
        Self::new(&cmd)
    }
}

impl Runner for SubprocessRunner {
    fn run(&self, spec: &RunnerSpec) -> RunnerResult {
        let payload = match serde_json::to_string(spec) {
            Ok(p) => p,
            Err(e) => return RunnerResult::failure(format!("could not serialize spec: {e}")),
        };

        let mut child = match Command::new(&self.program)
            .args(&self.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                return RunnerResult::failure(format!(
                    "could not spawn runner '{}': {e}",
                    self.program
                ));
            }
        };

        if let Some(mut stdin) = child.stdin.take() {
            if let Err(e) = stdin.write_all(payload.as_bytes()) {
                return RunnerResult::failure(format!("could not write spec to runner: {e}"));
            }
        }

        let output = match child.wait_with_output() {
            Ok(o) => o,
            Err(e) => return RunnerResult::failure(format!("runner wait failed: {e}")),
        };

        let stdout = String::from_utf8_lossy(&output.stdout);
        parse_result(&stdout).unwrap_or_else(|| {
            let stderr = String::from_utf8_lossy(&output.stderr);
            RunnerResult::failure(format!(
                "runner produced no valid result (stderr: {})",
                stderr.trim()
            ))
        })
    }
}

/// Parse the last JSON object line the runner printed.
fn parse_result(stdout: &str) -> Option<RunnerResult> {
    stdout
        .lines()
        .rev()
        .map(str::trim)
        .filter(|l| l.starts_with('{'))
        .find_map(|l| serde_json::from_str::<RunnerResult>(l).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_serializes_expected_fields() {
        let row = SessionRow {
            task_idx: 0,
            idx: 0,
            task_name: "build".to_string(),
            session_id: "s0".to_string(),
            cwd: Some("/repo".to_string()),
            prompt: None,
            command: Some("cargo build".to_string()),
            agent: "claude".to_string(),
            model: None,
            depends_on: vec![],
        };
        let spec = RunnerSpec::from_row("run-1", &row);
        let json = serde_json::to_string(&spec).unwrap();
        assert!(json.contains("\"run_id\":\"run-1\""));
        assert!(json.contains("\"command\":\"cargo build\""));
        assert!(json.contains("\"cwd\":\"/repo\""));
    }

    #[test]
    fn parses_result_json() {
        let r = parse_result("noise\n{\"status\":\"done\",\"tokens_in\":5,\"cost_usd\":0.1}\n")
            .unwrap();
        assert!(r.is_done());
        assert_eq!(r.tokens_in, 5);
        assert_eq!(r.node_state(), NodeState::Done);
    }

    #[test]
    fn parses_failed_result() {
        let r = parse_result("{\"status\":\"failed\",\"error\":\"boom\"}").unwrap();
        assert!(!r.is_done());
        assert_eq!(r.node_state(), NodeState::Failed);
        assert_eq!(r.error.as_deref(), Some("boom"));
    }

    #[test]
    fn no_json_yields_none() {
        assert!(parse_result("just some logs\nno json here").is_none());
    }

    #[test]
    fn command_line_splits_program_and_args() {
        let r = SubprocessRunner::new("python -m ralphus.runner");
        assert_eq!(r.program, "python");
        assert_eq!(r.args, vec!["-m", "ralphus.runner"]);
    }

    #[test]
    fn missing_program_fails_gracefully() {
        let runner = SubprocessRunner::new("definitely-not-a-real-program-xyz");
        let row = SessionRow {
            task_idx: 0,
            idx: 0,
            task_name: "t".to_string(),
            session_id: "s".to_string(),
            cwd: Some(".".to_string()),
            prompt: None,
            command: Some("echo hi".to_string()),
            agent: "claude".to_string(),
            model: None,
            depends_on: vec![],
        };
        let result = runner.run(&RunnerSpec::from_row("run-1", &row));
        assert!(!result.is_done());
        assert!(result.error.unwrap().contains("could not spawn"));
    }
}
