//! Non-interactive `ssh` invocation: the flags that make interactive auth
//! impossible by construction, plus turning a failed invocation into an
//! actionable message (RAL-200).
//!
//! A subprocess that hits a password or host-key prompt blocks forever and
//! looks like a stalled session, not an error -- there is no human at the
//! other end of a daemon-spawned provider to answer it. The flags built here
//! are **always** applied, with no configuration knob to turn them off: this
//! provider works purely on already-set-up key-based auth and a populated
//! `known_hosts`, by design.

/// Build the `ssh` arguments that come before the destination and remote
/// command: non-interactive flags, then `-T` (no pty -- see the module docs
/// on why stdout/stderr must stay separate channels).
///
/// # Why each flag
/// - `BatchMode=yes` -- disables every interactive prompt (password,
///   passphrase, "are you sure"); `ssh` fails immediately instead of hanging.
/// - `StrictHostKeyChecking=yes` -- an unrecognized host key fails the
///   connection outright rather than prompting "are you sure you want to
///   continue connecting?". The operator must `ssh-keyscan` the host into
///   `known_hosts` in advance.
/// - `PasswordAuthentication=no` / `KbdInteractiveAuthentication=no` --
///   belt-and-suspenders on top of `BatchMode`: even a misconfigured
///   `~/.ssh/config` that re-enables prompting for this host cannot make
///   these methods viable.
/// - `ConnectTimeout` -- bounds how long a genuinely unreachable host (as
///   opposed to one that would prompt) can block us.
/// - `-T` -- never allocate a pseudo-terminal. A remote shell with a pty
///   merges the child's stdout and stderr into one stream server-side before
///   `ssh` ever sees them, which is exactly how `RALPHUS_EVENT:` marker lines
///   would end up interleaved with regular output.
#[must_use]
pub fn non_interactive_args(connect_timeout_secs: u32) -> Vec<String> {
    vec![
        "-o".to_string(),
        "BatchMode=yes".to_string(),
        "-o".to_string(),
        "StrictHostKeyChecking=yes".to_string(),
        "-o".to_string(),
        "PasswordAuthentication=no".to_string(),
        "-o".to_string(),
        "KbdInteractiveAuthentication=no".to_string(),
        "-o".to_string(),
        format!("ConnectTimeout={connect_timeout_secs}"),
        "-T".to_string(),
    ]
}

/// Build a full `ssh <non-interactive flags> <target> -- <remote_command>`
/// argument vector (everything after the `ssh` program name itself).
#[must_use]
pub fn command_args(target: &str, connect_timeout_secs: u32, remote_command: &str) -> Vec<String> {
    let mut args = non_interactive_args(connect_timeout_secs);
    args.push(target.to_string());
    args.push("--".to_string());
    args.push(remote_command.to_string());
    args
}

/// Turn a failed transport invocation's exit code + captured stderr into an
/// actionable message, distinguishing the failure modes an operator can
/// actually act on. Pure and unit-testable without a live remote host.
///
/// `program` names whichever local program actually failed (`"ssh"` or
/// `"rsync"`) for the generic fallback message -- the ssh-specific substring
/// checks below still apply either way, since `rsync -e ssh` routes every
/// connection-level failure through the same `ssh` client underneath.
#[must_use]
pub fn interpret_failure(program: &str, exit_code: Option<i32>, stderr: &str) -> String {
    let lower = stderr.to_lowercase();
    if lower.contains("host key verification failed") || lower.contains("no matching host key") {
        return format!(
            "ssh host-key verification failed -- this host is not in known_hosts. \
             Run `ssh-keyscan -H <host> >> ~/.ssh/known_hosts` (verifying the \
             fingerprint out-of-band) before registering this machine. ssh said: {}",
            stderr.trim()
        );
    }
    if lower.contains("permission denied") {
        return format!(
            "ssh authentication failed (permission denied) -- this provider only \
             supports non-interactive key-based auth. Ensure a private key is \
             loaded (ssh-agent, or an IdentityFile in ~/.ssh/config for this \
             host) and its public key is in the remote's authorized_keys \
             (`ssh-copy-id <target>`). ssh said: {}",
            stderr.trim()
        );
    }
    if lower.contains("could not resolve hostname") || lower.contains("name or service not known") {
        return format!(
            "ssh could not resolve the host -- check the hostname/alias and \
             ~/.ssh/config. ssh said: {}",
            stderr.trim()
        );
    }
    if lower.contains("connection timed out") || lower.contains("operation timed out") {
        return format!(
            "ssh connection timed out -- the host is unreachable or a firewall is \
             blocking it. ssh said: {}",
            stderr.trim()
        );
    }
    if lower.contains("connection refused") {
        return format!(
            "ssh connection refused -- no sshd listening on the target host/port. \
             ssh said: {}",
            stderr.trim()
        );
    }
    match exit_code {
        Some(code) => format!("{program} exited with status {code}: {}", stderr.trim()),
        None => format!("{program} did not exit cleanly: {}", stderr.trim()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_interactive_args_always_carry_batch_mode_and_strict_host_key_checking() {
        let args = non_interactive_args(15);
        let joined = args.join(" ");
        assert!(joined.contains("BatchMode=yes"), "{joined}");
        assert!(joined.contains("StrictHostKeyChecking=yes"), "{joined}");
        assert!(joined.contains("PasswordAuthentication=no"), "{joined}");
        assert!(
            joined.contains("KbdInteractiveAuthentication=no"),
            "{joined}"
        );
        assert!(joined.contains("ConnectTimeout=15"), "{joined}");
        assert!(args.contains(&"-T".to_string()), "{joined}");
    }

    #[test]
    fn command_args_places_target_then_double_dash_then_the_remote_command() {
        let args = command_args("alice@host", 10, "cd /work && ralphus-runner");
        assert_eq!(args.last().unwrap(), "cd /work && ralphus-runner");
        assert_eq!(args[args.len() - 2], "--");
        assert_eq!(args[args.len() - 3], "alice@host");
    }

    #[test]
    fn unknown_host_key_message_mentions_known_hosts() {
        let msg = interpret_failure("ssh", Some(255), "Host key verification failed.\r\n");
        assert!(msg.contains("known_hosts"), "{msg}");
        assert!(msg.contains("ssh-keyscan"), "{msg}");
    }

    #[test]
    fn permission_denied_message_mentions_keys() {
        let msg = interpret_failure(
            "ssh",
            Some(255),
            "alice@host: Permission denied (publickey).\r\n",
        );
        assert!(msg.contains("ssh-copy-id") || msg.contains("key"), "{msg}");
    }

    #[test]
    fn unresolvable_host_message_is_actionable() {
        let msg = interpret_failure(
            "ssh",
            Some(255),
            "ssh: Could not resolve hostname nope: nodename nor servname provided, or not known\r\n",
        );
        assert!(msg.contains("resolve"), "{msg}");
    }

    #[test]
    fn connection_timeout_message_is_actionable() {
        let msg = interpret_failure(
            "ssh",
            Some(255),
            "ssh: connect to host 10.0.0.9 port 22: Connection timed out\r\n",
        );
        assert!(
            msg.contains("unreachable") || msg.contains("timed out"),
            "{msg}"
        );
    }

    #[test]
    fn connection_refused_message_is_actionable() {
        let msg = interpret_failure(
            "ssh",
            Some(255),
            "ssh: connect to host 10.0.0.9 port 22: Connection refused\r\n",
        );
        assert!(msg.contains("refused"), "{msg}");
    }

    #[test]
    fn an_unrecognized_failure_still_reports_exit_code_and_stderr() {
        let msg = interpret_failure("ssh", Some(1), "something else entirely went wrong");
        assert!(msg.contains('1'), "{msg}");
        assert!(msg.contains("something else entirely went wrong"), "{msg}");
    }

    #[test]
    fn generic_fallback_names_the_program_that_actually_failed() {
        let msg = interpret_failure("rsync", Some(23), "rsync: some partial transfer error");
        assert!(msg.starts_with("rsync exited"), "{msg}");
    }
}
