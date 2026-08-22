//! The `ping` verb: confirm the machine is reachable and ready, doing no
//! work (RAL-200).
//!
//! Cheap by contract (`docs/machine-providers.md`) -- the daemon calls this
//! from `POST /api/machines/{scheme}/check` (the board's Machines tab "Check"
//! button), not as part of running anything. For this provider, "reachable
//! and ready" means exactly one thing: a non-interactive `ssh` round trip
//! succeeds, which is also the single best test of whether `exec` will be
//! able to run at all -- the same auth/host-key setup gates both.

use std::process::{Command, Stdio};

use crate::exec::EffectiveConfig;
use crate::ssh;
use crate::uri;

/// A marker echoed back by the remote shell so a successful-looking exit
/// status can't be confused with, say, a login-shell MOTD swallowing the
/// actual command.
const PING_MARKER: &str = "ralphus-ssh-provider-ping-ok";

/// Confirm `uri` is reachable over non-interactive `ssh`.
///
/// # Errors
/// Any failure to reach the machine, turned into an actionable message by
/// [`ssh::interpret_failure`].
pub fn run(uri: &str, config: &EffectiveConfig) -> Result<Option<String>, String> {
    let target = uri::parse(uri).map_err(|e| e.to_string())?;
    let ssh_args = ssh::command_args(
        &target.target_string(),
        config.connect_timeout_secs,
        &format!("echo {PING_MARKER}"),
    );
    let out = Command::new("ssh")
        .args(&ssh_args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("could not run ssh to reach {target}: {e}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(ssh::interpret_failure("ssh", out.status.code(), &stderr));
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    if !stdout.contains(PING_MARKER) {
        return Err(format!(
            "ssh to {target} exited successfully but did not echo back the \
             expected marker -- the remote shell may be doing something \
             unusual on login (a banner, an interactive profile script); got: {:?}",
            stdout.trim()
        ));
    }
    Ok(Some(format!("reachable via ssh as {target}")))
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
        }
    }

    #[test]
    fn a_malformed_uri_fails_before_spawning_ssh() {
        let err = run("bad uri with spaces", &config()).unwrap_err();
        assert!(err.contains("whitespace"), "{err}");
    }
}
