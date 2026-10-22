//! The `read-file`, `write-file`, `remove-path`, and `run` verbs (RAL-355
//! Phase 2/4 remainder): structured file access and one VCS command on the
//! remote host, without ever constructing an interpolated shell string out
//! of caller-supplied text.
//!
//! Scoped under the configured target's `remote_root` when one is
//! configured (`[machine.targets.*]`, RAL-355 Phase 2); unscoped in legacy
//! mode, matching [`crate::exec::run`]'s equally unscoped ephemeral
//! workspace. A merge does not only run git -- it hand-writes `.git`
//! worktree link files, reads conflict markers back out of files, and tears
//! directories down, which is why these are generic file ops rather than
//! git-specific ones (`docs/machine-providers.md`).

use serde::Deserialize;

use crate::exec::EffectiveConfig;
use crate::job::{optional_remote_root, require_under_root, ssh_command};
use crate::transport::shell_quote_single;
use crate::uri;

/// The `--uri`-addressed workspace op payload shared by `read-file`,
/// `write-file`, and `remove-path` (`FileRequest` in
/// `daemon/src/remote_runner.rs`).
#[derive(Debug, Deserialize)]
struct FileRequest {
    path: String,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    recursive: bool,
    #[serde(default)]
    executable: bool,
}

/// The `run` verb's payload (`RunRequest` in `daemon/src/remote_runner.rs`).
#[derive(Debug, Deserialize)]
struct RunRequest {
    cwd: String,
    program: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: std::collections::BTreeMap<String, String>,
    /// Shell executable for an authored command line; `sh` when absent.
    #[serde(default)]
    shell: Option<String>,
}

/// Marker this module's `run` script always appends to its combined
/// stdout/stderr, so the git command's real exit code can be read back out
/// of ordinary text rather than trusted to ssh's own exit status -- which
/// here reports whether the *wrapping script* succeeded (it always does; the
/// last thing it runs is `printf`), not whether the git command inside it
/// did. This mirrors why `job.rs`'s async-job scripts manage their own state
/// files instead of relying on raw ssh exit codes for anything but a real
/// transport failure.
const RUN_EXIT_MARKER: &str = "__RALPHUS_RUN_EXIT__:";

fn require_absolute_posix_path(path: &str) -> Result<(), String> {
    if path.starts_with('/') && !path.trim().is_empty() {
        Ok(())
    } else {
        Err(format!(
            "remote path {path:?} must be an absolute POSIX path"
        ))
    }
}

fn parent_dir(path: &str) -> Option<&str> {
    let trimmed = path.trim_end_matches('/');
    trimmed.rsplit_once('/').map(|(parent, _)| parent)
}

fn parse_file_request(payload_json: &str, verb: &str) -> Result<FileRequest, String> {
    let req: FileRequest = serde_json::from_str(payload_json)
        .map_err(|e| format!("could not parse the {verb} request on stdin: {e}"))?;
    require_absolute_posix_path(&req.path)?;
    Ok(req)
}

fn require_under_configured_root(path: &str, config: &EffectiveConfig) -> Result<(), String> {
    match optional_remote_root(config)? {
        Some(root) => require_under_root(path, &root),
        None => Ok(()),
    }
}

/// Return a file's contents. A missing/unreadable file surfaces as an `Err`
/// -- the daemon layer (`ProviderRunner::read_file`) treats any failure as
/// "absent" rather than distinguishing why, so no special-casing is needed
/// here (`docs/machine-providers.md`).
///
/// # Errors
/// An actionable message when the request is malformed, the path is outside
/// the configured remote root, or the remote `cat` fails for any reason
/// (missing file, no permission, unreachable host).
pub fn read_file(
    uri: &str,
    payload_json: &str,
    config: &EffectiveConfig,
) -> Result<String, String> {
    let target = uri::parse(uri).map_err(|e| e.to_string())?;
    let req = parse_file_request(payload_json, "read-file")?;
    require_under_configured_root(&req.path, config)?;
    let script = format!("cat {}", shell_quote_single(&req.path));
    ssh_command(&target, &script, config, None)
}

/// Write `content` to `path`, creating parent directories as needed, via an
/// atomic write-then-rename so a reader can never observe a partial file.
///
/// # Errors
/// An actionable message on a malformed/out-of-root request, or any
/// transport/filesystem failure.
pub fn write_file(uri: &str, payload_json: &str, config: &EffectiveConfig) -> Result<(), String> {
    let target = uri::parse(uri).map_err(|e| e.to_string())?;
    let req = parse_file_request(payload_json, "write-file")?;
    require_under_configured_root(&req.path, config)?;
    let content = req
        .content
        .ok_or_else(|| "write-file request is missing \"content\"".to_string())?;
    let quoted_path = shell_quote_single(&req.path);
    let script = if let Some(parent) = parent_dir(&req.path).filter(|p| !p.is_empty()) {
        format!(
            "set -eu; umask 077; mkdir -p {parent}; tmp={quoted_path}.tmp; cat > \"$tmp\"; mv \"$tmp\" {quoted_path}",
            parent = shell_quote_single(parent),
        )
    } else {
        format!(
            "set -eu; umask 077; tmp={quoted_path}.tmp; cat > \"$tmp\"; mv \"$tmp\" {quoted_path}"
        )
    };
    ssh_command(&target, &script, config, Some(content.as_bytes()))?;
    if req.executable {
        ssh_command(&target, &format!("chmod 700 {quoted_path}"), config, None)?;
    }
    Ok(())
}

/// Delete a file, or a directory tree when `recursive`. A path that does not
/// exist is success, not an error -- `rm -f`/`rm -rf` already behave that
/// way, so no special-casing is needed here either.
///
/// # Errors
/// An actionable message on a malformed/out-of-root request, or any
/// transport/permission failure.
pub fn remove_path(uri: &str, payload_json: &str, config: &EffectiveConfig) -> Result<(), String> {
    let target = uri::parse(uri).map_err(|e| e.to_string())?;
    let req = parse_file_request(payload_json, "remove-path")?;
    require_under_configured_root(&req.path, config)?;
    let flag = if req.recursive { "-rf" } else { "-f" };
    let script = format!("rm {flag} {}", shell_quote_single(&req.path));
    ssh_command(&target, &script, config, None)?;
    crate::emit_event(
        "removed remote path",
        "info",
        serde_json::json!({
            "target": target.to_string(),
            "path": req.path,
            "recursive": req.recursive,
        }),
    );
    Ok(())
}

/// Run one VCS command (`program` is always `"git"` today) in a workspace and
/// return its combined stdout/stderr plus exit code.
///
/// # Errors
/// An actionable message when the request is malformed, `program` is not
/// `"git"`, `cwd` is outside the configured remote root, or ssh itself fails
/// to reach the host. A git command that runs and exits non-zero is *not* an
/// `Err` here -- that is a normal outcome the caller reads off the returned
/// exit code, exactly like a local command.
pub fn run(
    uri: &str,
    payload_json: &str,
    config: &EffectiveConfig,
) -> Result<(String, i64), String> {
    let target = uri::parse(uri).map_err(|e| e.to_string())?;
    // The script (with any environment values in it) goes on ssh's stdin,
    // not its command line, so it never appears in a process listing on
    // either end.
    let script = run_script(payload_json, config, RUN_EXIT_MARKER, true)?;
    let raw = ssh_command(&target, "sh -s", config, Some(script.as_bytes()))?;
    split_run_output(&raw)
}

/// Validate a `run` request and build the shell script that runs it: `cd` to
/// the workspace, run the command with stderr merged into stdout, and print
/// its exit code after `marker` on a line of its own.
///
/// `detach_stdin` redirects the command's stdin from `/dev/null`. A one-shot
/// `ssh` has no stdin script to protect, but a [`crate::channel`] feeds many
/// scripts to one long-lived remote shell through its stdin, so a command
/// that read stdin would swallow the next request.
///
/// # Errors
/// A malformed request, a `cwd` outside the configured remote root, a
/// `program` other than git or one authored shell command, or an invalid
/// environment key.
pub(crate) fn run_script(
    payload_json: &str,
    config: &EffectiveConfig,
    marker: &str,
    detach_stdin: bool,
) -> Result<String, String> {
    let req: RunRequest = serde_json::from_str(payload_json)
        .map_err(|e| format!("could not parse the run request on stdin: {e}"))?;
    require_absolute_posix_path(&req.cwd)?;
    require_under_configured_root(&req.cwd, config)?;
    if req.program != "git" && !(req.program.is_empty() && req.args.len() == 1) {
        return Err(format!(
            "ralphus-ssh-provider's run supports git argv or one authored shell command; got {:?}",
            req.program
        ));
    }
    for key in req.env.keys() {
        if key.is_empty()
            || !key
                .chars()
                .all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
        {
            return Err(format!("invalid environment key in run request: {key:?}"));
        }
    }
    let invocation = if req.program == "git" {
        let quoted_args = req
            .args
            .iter()
            .map(|a| shell_quote_single(a))
            .collect::<Vec<_>>()
            .join(" ");
        format!("{} {quoted_args}", shell_quote_single(&req.program))
    } else {
        let shell = req.shell.as_deref().unwrap_or("sh");
        let flags = if req.shell.is_some() {
            ralphus_core::shellcmd::shell_command_flags(shell).join(" ")
        } else {
            "-c".to_string()
        };
        format!(
            "{} {flags} {}",
            shell_quote_single(shell),
            shell_quote_single(&req.args[0])
        )
    };
    // Exported by the shell itself, inside a subshell: values never become a
    // process's arguments (visible to anyone listing processes) and never
    // outlive this one command in a reused channel shell.
    let exports: String = req
        .env
        .iter()
        .map(|(key, value)| format!("export {key}={}; ", shell_quote_single(value)))
        .collect();
    let stdin = if detach_stdin { " </dev/null" } else { "" };
    let body = format!(
        "{exports}cd {cwd} && {{ {invocation}; }}{stdin} 2>&1",
        cwd = shell_quote_single(&req.cwd),
    );
    let body = if exports.is_empty() {
        body
    } else {
        format!("( {body} )")
    };
    Ok(format!("{body}; printf '\\n{marker}%s\\n' \"$?\""))
}

fn split_run_output(raw: &str) -> Result<(String, i64), String> {
    let idx = raw.rfind(RUN_EXIT_MARKER).ok_or_else(|| {
        format!(
            "remote run produced no exit-code marker: {}",
            truncate(raw, 300)
        )
    })?;
    let stdout = raw[..idx].trim_end_matches('\n').to_string();
    let code_str = raw[idx + RUN_EXIT_MARKER.len()..].trim();
    let code: i64 = code_str
        .parse()
        .map_err(|_| format!("remote run produced a non-numeric exit code {code_str:?}"))?;
    Ok((stdout, code))
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    format!("{}…", &s[..max])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> EffectiveConfig {
        EffectiveConfig {
            remote_base: crate::config::DEFAULT_REMOTE_BASE.to_string(),
            extra_excludes: vec![],
            connect_timeout_secs: crate::config::DEFAULT_CONNECT_TIMEOUT_SECS,
            remote_runner_cmd: crate::config::DEFAULT_REMOTE_RUNNER_CMD.to_string(),
            ssh_config_file: None,
            target_runner_config: None,
        }
    }

    #[test]
    fn relative_paths_are_rejected_before_touching_the_network() {
        let err = read_file("alice@host", r#"{"path":"relative/path"}"#, &config()).unwrap_err();
        assert!(err.contains("absolute"), "{err}");
    }

    #[test]
    fn write_file_without_content_is_rejected() {
        let err = write_file("alice@host", r#"{"path":"/a/b"}"#, &config()).unwrap_err();
        assert!(err.contains("content"), "{err}");
    }

    #[test]
    fn run_rejects_a_non_git_program() {
        let err = run(
            "alice@host",
            r#"{"cwd":"/a","program":"rm","args":["-rf","/"]}"#,
            &config(),
        )
        .unwrap_err();
        assert!(err.contains("git"), "{err}");
    }

    #[test]
    fn an_authored_command_runs_through_the_selected_shell() {
        let cfg = EffectiveConfig {
            remote_base: "/srv/ralphus".to_string(),
            ..config()
        };
        let cwd = "/srv/ralphus/w";
        let payload = format!(r#"{{"cwd":"{cwd}","program":"","args":["a ; b"],"shell":"pwsh"}}"#);
        let script = run_script(&payload, &cfg, "M", false).unwrap();
        assert!(
            script.contains("'pwsh' -NoLogo -Command 'a ; b'"),
            "{script}"
        );
        let payload = format!(r#"{{"cwd":"{cwd}","program":"","args":["a ; b"]}}"#);
        let script = run_script(&payload, &cfg, "M", false).unwrap();
        assert!(script.contains("'sh' -c 'a ; b'"), "{script}");
    }

    #[test]
    fn a_one_shot_script_run_by_sh_from_stdin_sees_its_environment() {
        // `run` sends the script to the machine as `sh -s` on stdin; do the
        // same with a local sh.
        let Ok(probe) = std::process::Command::new("sh")
            .args(["-c", "true"])
            .status()
        else {
            return;
        };
        if !probe.success() {
            return;
        }
        let cfg = EffectiveConfig {
            remote_base: "/".to_string(),
            ..config()
        };
        let payload = r#"{"cwd":"/","program":"","args":["printf '%s' \"$TOKEN\"; cat"],"env":{"TOKEN":"a b'c"}}"#;
        let script = run_script(payload, &cfg, RUN_EXIT_MARKER, true).unwrap();
        let mut child = std::process::Command::new("sh")
            .arg("-s")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        std::io::Write::write_all(&mut child.stdin.take().unwrap(), script.as_bytes()).unwrap();
        let out = child.wait_with_output().unwrap();
        let (stdout, code) = split_run_output(&String::from_utf8_lossy(&out.stdout)).unwrap();
        assert_eq!(code, 0);
        // `cat` read /dev/null, not the rest of the script.
        assert_eq!(stdout, "a b'c");
    }

    #[test]
    fn environment_values_are_exported_in_a_subshell_not_passed_as_arguments() {
        let cfg = EffectiveConfig {
            remote_base: "/srv/ralphus".to_string(),
            ..config()
        };
        let payload =
            r#"{"cwd":"/srv/ralphus/w","program":"git","args":["push"],"env":{"TOKEN":"s3'cret"}}"#;
        let script = run_script(payload, &cfg, "M", true).unwrap();
        assert!(
            script.starts_with("( export TOKEN='s3'\\''cret'; cd "),
            "{script}"
        );
        assert!(!script.contains("env TOKEN"), "{script}");
    }

    #[test]
    fn run_rejects_a_relative_cwd() {
        let err = run(
            "alice@host",
            r#"{"cwd":"rel","program":"git","args":["status"]}"#,
            &config(),
        )
        .unwrap_err();
        assert!(err.contains("absolute"), "{err}");
    }

    #[test]
    fn parent_dir_splits_on_the_last_slash() {
        assert_eq!(parent_dir("/a/b/c"), Some("/a/b"));
        assert_eq!(parent_dir("/a"), Some(""));
        assert_eq!(parent_dir("/a/b/"), Some("/a"));
    }

    #[test]
    fn split_run_output_recovers_stdout_and_exit_code() {
        let raw = format!("hello\nworld\n{RUN_EXIT_MARKER}0\n");
        let (stdout, code) = split_run_output(&raw).unwrap();
        assert_eq!(stdout, "hello\nworld");
        assert_eq!(code, 0);
    }

    #[test]
    fn split_run_output_recovers_a_nonzero_exit_code() {
        let raw = format!("fatal: not a git repository\n{RUN_EXIT_MARKER}128\n");
        let (stdout, code) = split_run_output(&raw).unwrap();
        assert_eq!(stdout, "fatal: not a git repository");
        assert_eq!(code, 128);
    }

    #[test]
    fn split_run_output_without_a_marker_is_an_actionable_error() {
        let err = split_run_output("connection reset\n").unwrap_err();
        assert!(err.contains("exit-code marker"), "{err}");
    }

    #[test]
    fn a_path_outside_the_configured_remote_root_is_refused() {
        let mut cfg = config();
        cfg.target_runner_config = Some(
            serde_json::json!({
                "mode": "installed",
                "command": "ralphus-runner",
                "remote_root": "/srv/ralphus",
            })
            .to_string(),
        );
        let err = read_file("alice@host", r#"{"path":"/etc/passwd"}"#, &cfg).unwrap_err();
        assert!(err.contains("outside configured remote_root"), "{err}");
    }

    #[test]
    fn a_path_under_the_configured_remote_root_passes_scoping() {
        let mut cfg = config();
        cfg.target_runner_config = Some(
            serde_json::json!({
                "mode": "installed",
                "command": "ralphus-runner",
                "remote_root": "/srv/ralphus",
            })
            .to_string(),
        );
        // Fails on the (unreachable) ssh connection, not the scoping check --
        // proves the request passed validation before it ever attempted the
        // network.
        let err = read_file(
            "alice@host",
            r#"{"path":"/srv/ralphus/projects/x/repository/README.md"}"#,
            &cfg,
        )
        .unwrap_err();
        assert!(!err.contains("outside configured remote_root"), "{err}");
    }
}
