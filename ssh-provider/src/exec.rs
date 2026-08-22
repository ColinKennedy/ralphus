//! The `exec` verb: sync source to the remote host, run one cell there
//! synchronously, and return its result (RAL-200).
//!
//! Scope is deliberately narrow -- see the crate root docs. `exec` always
//! replies with `result` (never a `handle`), so nothing on the daemon side
//! needs to poll `status`/`stream`/`cancel` for this provider; it simply
//! blocks until the remote `ralphus-runner` finishes.
//!
//! ## Why the local `cwd` from the cell spec isn't used remotely as-is
//!
//! The daemon hands this provider the same [`crate::config`]-independent
//! `RunnerSpec` JSON it would send a local runner, `cwd` included -- but that
//! `cwd` is a path on the **daemon's own host** (its worktree checkout). This
//! function derives a deterministic remote workspace directory
//! ([`crate::config::remote_workspace_dir`]), syncs the local `cwd`'s
//! contents there, rewrites `cwd` in the spec to that remote path, and only
//! then pipes the spec to the remote `ralphus-runner` -- which reads `cwd`
//! straight out of the spec for its own workspace, not its process's own
//! working directory (`cli/src/ralphus/runner/execute.py::Workspace.create`).

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};

use serde_json::Value;

use crate::config::{
    self, DEFAULT_CONNECT_TIMEOUT_SECS, DEFAULT_REMOTE_BASE, DEFAULT_REMOTE_RUNNER_CMD,
};
use crate::ssh;
use crate::transport::{self, HostOs, Transport};
use crate::uri::{self, SshTarget};

/// Everything about `exec` that comes from the environment, gathered once so
/// the orchestration logic below takes plain values instead of reading env
/// vars itself (keeps [`run`] testable by construction, even though the
/// process-spawning it does still requires a live target for end-to-end
/// coverage -- see `tests/exec.rs`).
pub struct EffectiveConfig {
    pub remote_base: String,
    pub extra_excludes: Vec<String>,
    pub connect_timeout_secs: u32,
    pub remote_runner_cmd: String,
}

impl EffectiveConfig {
    /// Read from the process environment, applying the documented defaults
    /// (`crate::config` module docs) for anything unset.
    #[must_use]
    pub fn from_env() -> Self {
        Self {
            remote_base: std::env::var("RALPHUS_SSH_REMOTE_BASE")
                .unwrap_or_else(|_| DEFAULT_REMOTE_BASE.to_string()),
            extra_excludes: std::env::var("RALPHUS_SSH_EXCLUDE")
                .map(|raw| config::parse_exclude_list(&raw))
                .unwrap_or_default(),
            connect_timeout_secs: std::env::var("RALPHUS_SSH_CONNECT_TIMEOUT_SECS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(DEFAULT_CONNECT_TIMEOUT_SECS),
            remote_runner_cmd: std::env::var("RALPHUS_SSH_REMOTE_RUNNER_CMD")
                .unwrap_or_else(|_| DEFAULT_REMOTE_RUNNER_CMD.to_string()),
        }
    }
}

/// Run `exec`: parse `uri`, sync `spec_json`'s `cwd` to the remote host, run
/// the remote `ralphus-runner` against the rewritten spec, and return its
/// `CellResult` JSON.
///
/// `RALPHUS_EVENT:` lines (including `llm-invoke` usage events) seen on the
/// remote invocation's stderr are forwarded verbatim to *this process's own*
/// stderr as they arrive -- not buffered until the end -- because the daemon
/// scrapes this process's stderr live while it waits on us, and the whole
/// point of forwarding `llm-invoke` events promptly is that the daemon's live
/// cost-cap kill can act on them mid-run rather than only after we exit.
///
/// # Errors
/// Returns an actionable message on any transport, sync, or protocol
/// failure. Never returns `Err` for a cell that ran and merely failed --
/// that comes back as `Ok(json with status: "failed")`, per the contract's
/// distinction between an infrastructure failure and a normal task outcome.
pub fn run(uri: &str, spec_json: &str, config: &EffectiveConfig) -> Result<Value, String> {
    let target = uri::parse(uri).map_err(|e| e.to_string())?;

    let spec: Value = serde_json::from_str(spec_json)
        .map_err(|e| format!("could not parse the cell spec on stdin: {e}"))?;
    let Value::Object(mut spec_obj) = spec else {
        return Err("the cell spec on stdin must be a JSON object".to_string());
    };
    let local_dir = spec_obj
        .get("cwd")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| "the cell spec's \"cwd\" field must be a non-empty string".to_string())?
        .to_string();

    let remote_dir = config::remote_workspace_dir(&config.remote_base, &local_dir);

    sync_source(&target, &local_dir, &remote_dir, config)?;

    spec_obj.insert("cwd".to_string(), Value::String(remote_dir.clone()));
    // Never meaningful to the remote runner (it ignores unknown keys anyway,
    // `CellSpec.from_json` reads named fields only) -- dropped so a remote
    // machine value referencing itself can never even look plausible.
    spec_obj.remove("machine");
    let remote_spec_json = serde_json::to_string(&Value::Object(spec_obj))
        .map_err(|e| format!("could not re-serialize the rewritten cell spec: {e}"))?;

    run_remote_cell(&target, &remote_dir, &remote_spec_json, config)
}

/// Sync `local_dir`'s contents onto `target:remote_dir`, choosing the
/// transport per [`transport::choose_transport`].
fn sync_source(
    target: &SshTarget,
    local_dir: &str,
    remote_dir: &str,
    config: &EffectiveConfig,
) -> Result<(), String> {
    let excludes = transport::merge_excludes(transport::DEFAULT_EXCLUDES, &config.extra_excludes);
    let host_os = transport::local_os();
    let rsync_available = host_os == HostOs::Unix && program_on_path("rsync");
    match transport::choose_transport(host_os, rsync_available) {
        Transport::Rsync => sync_via_rsync(target, local_dir, remote_dir, &excludes, config),
        Transport::TarSsh => sync_via_tar_ssh(target, local_dir, remote_dir, &excludes, config),
    }
}

/// Whether `program` can be spawned at all -- used only to decide whether
/// `rsync` is worth attempting; never called on the connectivity-sensitive
/// path itself, so a false negative just means an extra, harmless fallback to
/// `tar | ssh`.
fn program_on_path(program: &str) -> bool {
    Command::new(program)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn sync_via_rsync(
    target: &SshTarget,
    local_dir: &str,
    remote_dir: &str,
    excludes: &[String],
    config: &EffectiveConfig,
) -> Result<(), String> {
    let ssh_args = ssh::non_interactive_args(config.connect_timeout_secs);
    let args = transport::rsync_args(
        local_dir,
        &target.target_string(),
        remote_dir,
        excludes,
        "ssh",
        &ssh_args,
    );
    let out = Command::new("rsync")
        .args(&args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("could not run rsync: {e}"))?;
    if out.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    Err(ssh::interpret_failure("rsync", out.status.code(), &stderr))
}

fn sync_via_tar_ssh(
    target: &SshTarget,
    local_dir: &str,
    remote_dir: &str,
    excludes: &[String],
    config: &EffectiveConfig,
) -> Result<(), String> {
    let mut tar_child = Command::new("tar")
        .args(transport::tar_create_args(local_dir, excludes))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("could not run tar to archive {local_dir:?}: {e}"))?;
    let tar_stdout = tar_child
        .stdout
        .take()
        .expect("tar spawned with a piped stdout");
    // Drain tar's stderr on its own thread, concurrently with ssh below --
    // otherwise a `tar` chatty enough to fill its stderr pipe (e.g. many
    // exclude-pattern warnings) would block writing to it forever, since
    // nothing would read it until after ssh had already finished.
    let tar_stderr_handle = tar_child.stderr.take().map(|mut stderr| {
        std::thread::spawn(move || {
            use std::io::Read;
            let mut buf = String::new();
            let _ = stderr.read_to_string(&mut buf);
            buf
        })
    });

    let remote_cmd = transport::remote_tar_extract_command(remote_dir);
    let ssh_args = ssh::command_args(
        &target.target_string(),
        config.connect_timeout_secs,
        &remote_cmd,
    );
    let ssh_out = Command::new("ssh")
        .args(&ssh_args)
        .stdin(Stdio::from(tar_stdout))
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("could not run ssh to extract onto {target}: {e}"))?;

    let tar_status = tar_child
        .wait()
        .map_err(|e| format!("could not wait on tar: {e}"))?;
    let tar_stderr = tar_stderr_handle
        .and_then(|h| h.join().ok())
        .unwrap_or_default();

    if !ssh_out.status.success() {
        let stderr = String::from_utf8_lossy(&ssh_out.stderr);
        return Err(ssh::interpret_failure(
            "ssh",
            ssh_out.status.code(),
            &stderr,
        ));
    }
    if !tar_status.success() {
        return Err(ssh::interpret_failure(
            "tar",
            tar_status.code(),
            &tar_stderr,
        ));
    }
    Ok(())
}

/// SSH to `target`, pipe `remote_spec_json` into the remote `ralphus-runner`,
/// and return its parsed `CellResult` JSON.
///
/// The remote command deliberately still `cd`s into `remote_dir` before
/// invoking the runner even though `cwd` is also carried in the spec itself
/// -- cheap, and keeps the remote process's own working directory sane for
/// anything (a relative-path tool invocation, a core dump) that might assume
/// it, beyond what the spec's `cwd` field alone covers.
fn run_remote_cell(
    target: &SshTarget,
    remote_dir: &str,
    remote_spec_json: &str,
    config: &EffectiveConfig,
) -> Result<Value, String> {
    let remote_cmd = format!(
        "cd {} && {}",
        transport::shell_quote_single(remote_dir),
        config.remote_runner_cmd
    );
    let ssh_args = ssh::command_args(
        &target.target_string(),
        config.connect_timeout_secs,
        &remote_cmd,
    );
    let mut child: Child = Command::new("ssh")
        .args(&ssh_args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("could not run ssh to reach {target}: {e}"))?;

    if let Some(mut stdin) = child.stdin.take() {
        // A remote runner that never reads stdin would otherwise deadlock us
        // on a full pipe; treat the write failing as non-fatal and let the
        // exit status/stdout decide, mirroring the daemon's own
        // `ProviderRunner::invoke_with`.
        let _ = stdin.write_all(remote_spec_json.as_bytes());
    }

    // Forward `RALPHUS_EVENT:` lines to our own stderr as they arrive -- see
    // this module's docs on why that must happen live, not at the end.
    let stderr_handle = child.stderr.take().map(|stderr| {
        std::thread::spawn(move || {
            let mut tail = String::new();
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                if let Some(rest) = line.strip_prefix(crate::EVENT_MARKER) {
                    eprintln!("{}{rest}", crate::EVENT_MARKER);
                } else if tail.len() < 4096 {
                    tail.push_str(&line);
                    tail.push('\n');
                }
            }
            tail
        })
    });

    let out = child
        .wait_with_output()
        .map_err(|e| format!("could not wait on ssh: {e}"))?;
    let stderr_tail = stderr_handle
        .and_then(|h| h.join().ok())
        .unwrap_or_default();

    let stdout = String::from_utf8_lossy(&out.stdout);
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        // No JSON at all -- the remote `ralphus-runner` never produced a
        // result, which is a transport/invocation problem (ssh itself
        // failing, or the remote command not found), not a task outcome.
        let base = ssh::interpret_failure("ssh", out.status.code(), &stderr_tail);
        return Err(format!("remote ralphus-runner produced no output: {base}"));
    }
    serde_json::from_str::<Value>(trimmed).map_err(|e| {
        format!(
            "remote ralphus-runner produced unparseable output: {e} ({})",
            truncate(trimmed, 300)
        )
    })
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
            remote_base: DEFAULT_REMOTE_BASE.to_string(),
            extra_excludes: vec![],
            connect_timeout_secs: DEFAULT_CONNECT_TIMEOUT_SECS,
            remote_runner_cmd: DEFAULT_REMOTE_RUNNER_CMD.to_string(),
        }
    }

    #[test]
    fn a_malformed_uri_fails_before_touching_the_spec() {
        let err = run("not a valid uri", "{}", &config()).unwrap_err();
        assert!(err.contains("whitespace"), "{err}");
    }

    #[test]
    fn a_spec_missing_cwd_fails_with_an_actionable_message() {
        let err = run("alice@host", r#"{"squad_id":"r1"}"#, &config()).unwrap_err();
        assert!(err.contains("cwd"), "{err}");
    }

    #[test]
    fn a_non_object_spec_is_rejected() {
        let err = run("alice@host", "[1,2,3]", &config()).unwrap_err();
        assert!(err.contains("JSON object"), "{err}");
    }

    #[test]
    fn invalid_json_on_stdin_is_rejected() {
        let err = run("alice@host", "not json", &config()).unwrap_err();
        assert!(err.contains("parse"), "{err}");
    }
}
