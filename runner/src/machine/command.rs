//! The `run` verb: one git command, or one authored shell command line, in a
//! workspace -- with stdout and stderr merged into one text the way the SSH
//! provider's `2>&1` merges them, and the command's own exit code.

use std::io::Read as _;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

use serde_json::Value;

use super::paths::Root;

/// Validate a `run` request and run it.
///
/// # Errors
/// A malformed request, a `cwd` outside the root, a program other than git
/// or one authored command, or a command that could not be started. A
/// command that runs and exits non-zero is not an error: its exit code is in
/// the result.
pub fn run(request: &Value, root: &Root) -> Result<(String, i64), String> {
    let cwd = super::required_str(request, "cwd", "run request")?;
    let cwd = root.contain(cwd)?;
    let program = request
        .get("program")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let args: Vec<String> = match request.get("args") {
        Some(Value::Null) | None => Vec::new(),
        Some(value) => serde_json::from_value(value.clone())
            .map_err(|e| format!("run request \"args\" must be strings: {e}"))?,
    };
    let env = super::env_map(request.get("env"), "run request \"env\"")?;
    let mut command = if program == "git" {
        let mut command = Command::new("git");
        command.args(&args);
        command
    } else if program.is_empty() && args.len() == 1 {
        let shell = request
            .get("shell")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .unwrap_or(default_shell());
        ralphus_core::shellcmd::shell_command(shell, &args[0])
    } else {
        return Err(format!(
            "run supports git argv or one authored shell command; got program {program:?}"
        ));
    };
    command.current_dir(&cwd).envs(&env);
    let (output, code) = combined_output(&mut command).map_err(|e| {
        format!(
            "could not start {:?} in {cwd}: {e}",
            display_program(program)
        )
    })?;
    Ok((
        output.trim_end_matches(['\n', '\r']).to_string(),
        code.map_or(-1, i64::from),
    ))
}

fn display_program(program: &str) -> &str {
    if program.is_empty() {
        "the shell"
    } else {
        program
    }
}

/// The shell an authored command runs under when the request names none:
/// `cmd` on Windows, `sh` elsewhere -- the daemon's own default for a
/// command with no shell (`config::effective_command_shell`).
#[must_use]
pub fn default_shell() -> &'static str {
    if cfg!(windows) { "cmd" } else { "sh" }
}

/// Run `command` with stdin closed and return its stdout and stderr merged
/// in the order they arrived, and its exit code (`None` when killed by a
/// signal).
///
/// # Errors
/// The command could not be started or waited for.
pub fn combined_output(command: &mut Command) -> std::io::Result<(String, Option<i32>)> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let merged = Arc::new(Mutex::new(Vec::new()));
    let readers: Vec<_> = [
        child
            .stdout
            .take()
            .map(|s| Box::new(s) as Box<dyn std::io::Read + Send>),
        child
            .stderr
            .take()
            .map(|s| Box::new(s) as Box<dyn std::io::Read + Send>),
    ]
    .into_iter()
    .flatten()
    .map(|mut stream| {
        let merged = Arc::clone(&merged);
        std::thread::spawn(move || {
            let mut buffer = [0_u8; 8192];
            loop {
                match stream.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => merged
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .extend_from_slice(&buffer[..n]),
                }
            }
        })
    })
    .collect();
    let status = child.wait()?;
    for reader in readers {
        let _ = reader.join();
    }
    let bytes = std::mem::take(
        &mut *merged
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    );
    Ok((String::from_utf8_lossy(&bytes).into_owned(), status.code()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn root(dir: &std::path::Path) -> Root {
        Root::new(&dir.to_string_lossy()).unwrap()
    }

    #[test]
    fn an_authored_command_runs_in_the_workspace_with_its_environment() {
        let dir = ralphus_test_support::TestDir::new("machine-run");
        let line = if cfg!(windows) {
            "echo %RALPHUS_T% & echo to-stderr 1>&2 & exit 3"
        } else {
            "echo \"$RALPHUS_T\"; echo to-stderr >&2; exit 3"
        };
        let (stdout, code) = run(
            &json!({
                "cwd": dir.to_string_lossy(),
                "program": "",
                "args": [line],
                "env": {"RALPHUS_T": "from-env"},
            }),
            &root(&dir),
        )
        .unwrap();
        assert_eq!(code, 3);
        assert!(stdout.contains("from-env"), "{stdout}");
        assert!(stdout.contains("to-stderr"), "{stdout}");
    }

    #[test]
    fn git_runs_with_its_arguments_and_reports_its_exit_code() {
        let dir = ralphus_test_support::TestDir::new("machine-run-git");
        let (stdout, code) = run(
            &json!({"cwd": dir.to_string_lossy(), "program": "git", "args": ["rev-parse", "--git-dir"]}),
            &root(&dir),
        )
        .unwrap();
        assert_ne!(code, 0, "not a repository: {stdout}");
        assert!(
            stdout.to_lowercase().contains("not a git repository"),
            "{stdout}"
        );
    }

    #[test]
    fn requests_outside_the_root_or_for_other_programs_are_refused() {
        let dir = ralphus_test_support::TestDir::new("machine-run-refused");
        let outside = dir.parent().unwrap().to_string_lossy().into_owned();
        let err = run(
            &json!({"cwd": outside, "program": "git", "args": ["status"]}),
            &root(&dir),
        )
        .unwrap_err();
        assert!(err.contains("outside"), "{err}");
        let err = run(
            &json!({"cwd": dir.to_string_lossy(), "program": "rm", "args": ["-rf", "/"]}),
            &root(&dir),
        )
        .unwrap_err();
        assert!(err.contains("git argv"), "{err}");
        let err = run(
            &json!({"cwd": dir.to_string_lossy(), "program": "git", "args": [], "env": {"A B": "x"}}),
            &root(&dir),
        )
        .unwrap_err();
        assert!(err.contains("environment variable name"), "{err}");
    }
}
