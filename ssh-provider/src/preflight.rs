//! The `preflight` verb on a POSIX machine: whether an agent backend can be
//! started there, answered by the machine's own `ralphus-runner preflight`
//! (the same check the daemon runs for a local cell), so the answer reflects
//! the machine's `PATH` and the agent path its provider configured.
//!
//! An unavailable agent is a normal answer (`available: false`, the runner's
//! reason in `detail`); only failing to ask at all (ssh, a missing runner) is
//! an error reply.

use serde_json::{Value, json};

use crate::exec::EffectiveConfig;
use crate::transport::shell_quote_single;
use crate::windows::{Output, RemoteShell, Ssh};

/// Run `preflight` against `uri` with the JSON `payload`
/// (`{"agent": ..., "executable": ...}`).
///
/// # Errors
/// The request is malformed, the runner could not be readied, or the machine
/// could not be reached.
pub fn run(uri: &str, payload: &str, config: &EffectiveConfig) -> Result<Value, String> {
    let target = crate::uri::parse(uri).map_err(|e| e.to_string())?;
    let runner =
        match crate::runner_install::policy_from_env(config.target_runner_config.as_deref())? {
            Some(policy) => crate::runner_install::ensure(&target, &policy, config)?,
            None => config.remote_runner_cmd.clone(),
        };
    run_with(&Ssh { uri, config }, &runner, payload)
}

/// [`run`] once the runner command is known, over any [`RemoteShell`].
///
/// # Errors
/// The request is malformed or the machine could not be reached.
pub fn run_with(shell: &dyn RemoteShell, runner: &str, payload: &str) -> Result<Value, String> {
    let request: Value = serde_json::from_str(payload)
        .map_err(|e| format!("could not parse the preflight request on stdin: {e}"))?;
    let agent = request
        .get("agent")
        .and_then(Value::as_str)
        .filter(|a| !a.trim().is_empty())
        .ok_or_else(|| "a preflight request is missing a non-empty \"agent\"".to_string())?;
    let executable = request
        .get("executable")
        .and_then(Value::as_str)
        .filter(|e| !e.trim().is_empty());
    let mut command = format!("{runner} preflight --agent {}", shell_quote_single(agent));
    if let Some(executable) = executable {
        command.push_str(" --executable ");
        command.push_str(&shell_quote_single(executable));
    }
    Ok(answer(&shell.run(&command, b"")?))
}

/// The reply fields for one `ralphus-runner preflight` run's `output`.
fn answer(output: &Output) -> Value {
    if output.code == Some(0) {
        return json!({"available": true});
    }
    let detail = output.stderr.trim();
    let detail = if detail.is_empty() {
        format!(
            "ralphus-runner preflight exited with {}",
            output
                .code
                .map_or_else(|| "a signal".to_string(), |c| c.to_string())
        )
    } else {
        detail.to_string()
    };
    json!({"available": false, "detail": detail})
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    struct Scripted {
        seen: RefCell<Vec<String>>,
        reply: Output,
    }

    impl RemoteShell for Scripted {
        fn run(&self, command_line: &str, _stdin: &[u8]) -> Result<Output, String> {
            self.seen.borrow_mut().push(command_line.to_string());
            Ok(Output {
                stdout: self.reply.stdout.clone(),
                stderr: self.reply.stderr.clone(),
                code: self.reply.code,
            })
        }

        fn copy_in(&self, _local: &std::path::Path, _remote_path: &str) -> Result<(), String> {
            Err("not used".to_string())
        }
    }

    fn shell(stderr: &str, code: i32) -> Scripted {
        Scripted {
            seen: RefCell::new(Vec::new()),
            reply: Output {
                stdout: Vec::new(),
                stderr: stderr.to_string(),
                code: Some(code),
            },
        }
    }

    #[test]
    fn runs_the_machine_runner_preflight_with_the_agent_quoted() {
        let shell = shell("", 0);
        let reply = run_with(
            &shell,
            "/opt/r/ralphus-runner",
            r#"{"agent":"codex","executable":"/home/me/bin/codex"}"#,
        )
        .unwrap();
        assert_eq!(reply, json!({"available": true}));
        assert_eq!(
            shell.seen.borrow().as_slice(),
            ["/opt/r/ralphus-runner preflight --agent 'codex' --executable '/home/me/bin/codex'"]
        );
    }

    #[test]
    fn a_failing_preflight_is_an_answer_with_the_runners_reason() {
        let shell = shell("program \"claude\" is not resolvable on PATH\n", 1);
        let reply = run_with(&shell, "ralphus-runner", r#"{"agent":"claude-code"}"#).unwrap();
        assert_eq!(reply["available"], false);
        assert_eq!(
            reply["detail"],
            "program \"claude\" is not resolvable on PATH"
        );
        assert_eq!(
            shell.seen.borrow().as_slice(),
            ["ralphus-runner preflight --agent 'claude-code'"]
        );
    }

    #[test]
    fn a_request_without_an_agent_is_refused() {
        assert!(run_with(&shell("", 0), "ralphus-runner", "{}").is_err());
    }
}
