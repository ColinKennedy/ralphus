//! Target-aware runner readiness and content-addressed upload (RAL-355 Phase 5).

use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::exec::EffectiveConfig;
use crate::ssh;
use crate::transport::shell_quote_single;
use crate::uri::SshTarget;

#[derive(Debug, Clone, Deserialize)]
pub struct RunnerPolicy {
    pub mode: String,
    pub command: String,
    #[serde(default)]
    pub artifacts: BTreeMap<String, String>,
    pub remote_root: String,
}

pub fn policy_from_env(raw: Option<&str>) -> Result<Option<RunnerPolicy>, String> {
    raw.map(|value| {
        serde_json::from_str(value)
            .map_err(|e| format!("could not parse RALPHUS_TARGET_RUNNER_CONFIG: {e}"))
    })
    .transpose()
}

/// Ensure the configured runner is ready and return the exact remote command.
pub fn ensure(
    target: &SshTarget,
    policy: &RunnerPolicy,
    config: &EffectiveConfig,
) -> Result<String, String> {
    match policy.mode.as_str() {
        "installed" => verify_installed(target, &policy.command, config),
        "upload" => upload(target, policy, config),
        other => Err(format!(
            "unsupported target runner mode {other:?}; expected \"installed\" or \"upload\""
        )),
    }
}

fn verify_installed(
    target: &SshTarget,
    command: &str,
    config: &EffectiveConfig,
) -> Result<String, String> {
    if command.trim().is_empty() {
        return Err("installed runner command is empty".to_string());
    }
    let probe = format!("{command} --version");
    let output = ssh_output(target, &probe, config, None)?;
    let version = output.lines().next().unwrap_or_default().trim();
    if !version.starts_with("ralphus-runner ") {
        return Err(format!(
            "configured runner command {command:?} returned unexpected version output {version:?}"
        ));
    }
    Ok(command.to_string())
}

fn probe_target_triple(target: &SshTarget, config: &EffectiveConfig) -> Result<String, String> {
    let output = ssh_output(target, "uname -s; uname -m", config, None)?;
    let mut lines = output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty());
    let os = lines.next().unwrap_or_default();
    let arch = lines.next().unwrap_or_default();
    match (os, arch) {
        ("Linux", "x86_64" | "amd64") => Ok("x86_64-unknown-linux-musl".to_string()),
        ("Linux", "aarch64" | "arm64") => Ok("aarch64-unknown-linux-musl".to_string()),
        _ => Err(format!(
            "remote OS/architecture {os:?}/{arch:?} has no supported runner target mapping"
        )),
    }
}

fn upload(
    target: &SshTarget,
    policy: &RunnerPolicy,
    config: &EffectiveConfig,
) -> Result<String, String> {
    let triple = probe_target_triple(target, config)?;
    let artifact = policy.artifacts.get(&triple).ok_or_else(|| {
        let configured = policy
            .artifacts
            .keys()
            .cloned()
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "no runner artifact is configured for remote target {triple}; configured targets: {}",
            if configured.is_empty() {
                "none"
            } else {
                &configured
            }
        )
    })?;
    let bytes = std::fs::read(artifact).map_err(|e| {
        format!(
            "could not read runner artifact {}: {e}",
            Path::new(artifact).display()
        )
    })?;
    let checksum = format!("{:x}", Sha256::digest(&bytes));
    let runner_dir = format!("{}/runners/{triple}/{checksum}", policy.remote_root);
    let destination = format!("{runner_dir}/ralphus-runner");
    let q_dir = shell_quote_single(&runner_dir);
    let q_dest = shell_quote_single(&destination);
    let q_sum = shell_quote_single(&checksum);
    let script = format!(
        "set -eu; umask 077; mkdir -p {q_dir}; tmp={q_dest}.tmp.$$; cat > \"$tmp\"; actual=$(sha256sum \"$tmp\" | awk '{{print $1}}'); [ \"$actual\" = {q_sum} ] || {{ rm -f \"$tmp\"; echo 'runner checksum mismatch after upload' >&2; exit 1; }}; chmod 755 \"$tmp\"; if [ -e {q_dest} ]; then existing=$(sha256sum {q_dest} | awk '{{print $1}}'); rm -f \"$tmp\"; [ \"$existing\" = {q_sum} ] || {{ echo 'existing content-addressed runner checksum mismatch' >&2; exit 1; }}; else mv \"$tmp\" {q_dest}; fi; {q_dest} --version"
    );
    let output = ssh_output(target, &script, config, Some(&bytes))?;
    let version = output.lines().next().unwrap_or_default().trim();
    if !version.starts_with("ralphus-runner ") {
        return Err(format!(
            "uploaded runner returned unexpected version output {version:?}"
        ));
    }
    Ok(destination)
}

fn ssh_output(
    target: &SshTarget,
    remote_command: &str,
    config: &EffectiveConfig,
    stdin: Option<&[u8]>,
) -> Result<String, String> {
    let args = ssh::command_args(
        &target.target_string(),
        config.connect_timeout_secs,
        remote_command,
        config.ssh_config_file.as_deref(),
    );
    let mut child = Command::new("ssh")
        .args(args)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("could not run ssh to reach {target}: {e}"))?;
    if let (Some(bytes), Some(mut pipe)) = (stdin, child.stdin.take()) {
        pipe.write_all(bytes)
            .map_err(|e| format!("could not upload runner artifact to {target}: {e}"))?;
    }
    let output = child
        .wait_with_output()
        .map_err(|e| format!("could not wait on ssh to {target}: {e}"))?;
    if !output.status.success() {
        return Err(ssh::interpret_failure(
            "ssh",
            output.status.code(),
            &String::from_utf8_lossy(&output.stderr),
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_target_runner_policy() {
        let policy = policy_from_env(Some(
            r#"{"mode":"upload","command":"ralphus-runner","artifacts":{"x86_64-unknown-linux-musl":"/tmp/runner"},"remote_root":"/srv/ralphus"}"#,
        ))
        .unwrap()
        .unwrap();
        assert_eq!(policy.mode, "upload");
        assert_eq!(policy.artifacts.len(), 1);
    }

    #[test]
    fn absent_policy_remains_backward_compatible() {
        assert!(policy_from_env(None).unwrap().is_none());
    }
}
