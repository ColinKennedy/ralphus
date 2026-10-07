//! The `terminal` verb (RAL-355 Phase 10): a raw, bidirectional byte tunnel
//! to an interactive remote pty, for the Open Agent terminal relay.
//!
//! **This is the one verb in the whole contract that is not JSON
//! request/response.** Every other verb replies with exactly one JSON
//! envelope on stdout (`crate::protocol::reply`); this one does not call
//! that at all. From the moment `ssh -tt` connects, this process's own
//! stdin/stdout *are* the terminal byte stream -- read from this process's
//! stdin and write to the remote pty, read from the remote pty and write to
//! this process's stdout, until either side closes. A trailing JSON line
//! would land in the middle of a human's terminal session as garbled text,
//! so there is deliberately nothing to parse here: success or failure is
//! reported through this process's own exit code, which the daemon checks
//! after the relay ends, not through anything on stdout.
//!
//! No dynamic resize forwarding -- `--cols`/`--lines` set the *initial* size
//! only (via `COLUMNS`/`LINES` on the remote command), a disclosed
//! limitation rather than a silent one (see `REMOTE_IMPROVEMENTS.local.md`'s
//! Phase 10 notes): forwarding a live resize would need this process's own
//! `ssh` child to have a real local pty of its own so `ssh` can detect the
//! change and propagate it, which needs a pty-allocation dependency this
//! first version doesn't take on.

use std::io::{self, Read, Write};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;

use crate::exec::EffectiveConfig;
use crate::ssh;
use crate::transport::shell_quote_single;
use crate::uri;

const BUFFER_SIZE: usize = 4096;

/// The remote command string for a terminal session on a machine reached
/// directly (no container): `sh -c '<command>'`.
///
/// ssh hands its trailing argument to the remote account's login shell as one
/// command line. The command here is a whole script (`export ...; claude
/// --resume ...`), so it has to be an argument to `sh -c`; quoting it alone
/// makes the login shell look for a program named after the entire script and
/// exit 127.
fn plain_remote_command(sized_command: &str) -> String {
    format!("sh -c {}", shell_quote_single(sized_command))
}

/// Run the `terminal` verb: spawn `ssh -tt <target> <command>` (with
/// `COLUMNS`/`LINES` set to `cols`/`lines` for the remote shell) and relay
/// raw bytes between this process's own stdin/stdout and the remote pty
/// until either side closes.
///
/// # Errors
/// A malformed `uri`, or `ssh` itself failing to spawn. A remote command
/// that runs and then exits (the human ended their session, or the harness
/// crashed) is *not* an error -- that is a normal, successful end to a
/// terminal session, reported via the returned exit code.
pub fn run(
    uri: &str,
    command: &str,
    cols: u16,
    lines: u16,
    config: &EffectiveConfig,
) -> Result<i32, String> {
    let target = uri::parse(uri).map_err(|e| e.to_string())?;
    // A process started by `docker exec` outlives the ssh session that started
    // it, so in container mode the command carries an id the cleanup at the
    // bottom can find it by.
    let terminal_id = crate::container::active().map(|_| crate::container::new_terminal_id());
    let marker = terminal_id
        .as_deref()
        .map(|id| format!(" RALPHUS_TERMINAL={id}"))
        .unwrap_or_default();
    let sized_command = format!("export COLUMNS={cols} LINES={lines}{marker}; {command}",);
    let remote_command = if crate::container::active().is_some() {
        // `docker exec -it` is already a complete, correctly quoted shell
        // command for the host's login shell.
        crate::container::wrap_active(&target.target_string(), &sized_command, true)
    } else {
        plain_remote_command(&sized_command)
    };
    let args = ssh::pty_command_args(
        &target.target_string(),
        config.connect_timeout_secs,
        &remote_command,
        config.ssh_config_file.as_deref(),
    );
    // The quoted command above is itself passed as ssh's single trailing
    // argument (see `pty_command_args`'s doc comment) -- ssh hands it to the
    // remote account's login shell, which is what actually interprets the
    // quoting; nothing on this side re-parses it.
    let mut child = Command::new("ssh")
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        // Diagnostics only (connection errors, host-key warnings) -- never
        // part of the terminal byte stream itself, which is why this is
        // *not* merged into the piped stdout `-tt` already keeps separate
        // from the remote pty's own output.
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| format!("could not run ssh to reach {target}: {e}"))?;

    let mut child_stdin = child.stdin.take().expect("stdin piped at spawn");
    let mut child_stdout = child.stdout.take().expect("stdout piped at spawn");
    let child = Arc::new(Mutex::new(child));

    // This process's own stdin (bytes the daemon relays in from the WS
    // client) -> the remote pty. Runs on its own thread so it can block on
    // `read` independently of the child-stdout loop below, and is never joined:
    // when the remote side ends first this thread may still be parked in a
    // `read`, and the process exits right after `run` returns.
    let killer = Arc::clone(&child);
    thread::spawn(move || {
        let mut buf = [0_u8; BUFFER_SIZE];
        let mut stdin = io::stdin();
        loop {
            match stdin.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if child_stdin.write_all(&buf[..n]).is_err() {
                        break;
                    }
                }
            }
        }
        // The daemon closed our stdin (the client's WebSocket went away), so the
        // session is over. Closing ssh's stdin is not enough -- a pty session
        // keeps running without input -- so end ssh itself, which also ends the
        // output loop below. Leaving it to the daemon to kill this process
        // would orphan ssh on a host where killing a process does not take its
        // children with it.
        drop(child_stdin);
        if let Ok(mut ssh) = killer.lock() {
            let _ = ssh.kill();
        }
    });

    // The remote pty's output -> this process's own stdout (bytes the
    // daemon relays out to the WS client).
    let mut stdout = io::stdout();
    let mut buf = [0_u8; BUFFER_SIZE];
    loop {
        match child_stdout.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if stdout.write_all(&buf[..n]).is_err() || stdout.flush().is_err() {
                    break;
                }
            }
        }
    }

    let status = child
        .lock()
        .map_err(|_| "the ssh child lock was poisoned".to_string())?
        .wait()
        .map_err(|e| format!("could not wait on ssh: {e}"))?;
    if let Some(id) = terminal_id {
        // Best effort: the session is already over either way.
        if let Err(e) = crate::container::cleanup_terminal(uri, &id, config) {
            eprintln!("could not clean up the container terminal {id}: {e}");
        }
    }
    Ok(status.code().unwrap_or(-1))
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
    fn a_plain_terminal_command_is_run_by_sh_c_not_looked_up_as_a_program_name() {
        assert_eq!(
            plain_remote_command("export COLUMNS=80 LINES=24; claude --resume 'x'"),
            "sh -c 'export COLUMNS=80 LINES=24; claude --resume '\\''x'\\'''"
        );
    }

    #[test]
    fn a_malformed_uri_fails_before_spawning_ssh() {
        let err = run("not a valid uri", "claude", 80, 24, &config()).unwrap_err();
        assert!(err.contains("whitespace"), "{err}");
    }
}
