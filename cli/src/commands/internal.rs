//! `ralphus internal <subcommand>` (RAL-338 follow-up): machine-invoked
//! interfaces with no interactive/task-file use, never a command a human or
//! a task file calls directly. Excluded from MCP tool generation entirely
//! (see `mcp/src/exclusions.rs`) -- there is nothing here with a
//! request/response shape a tool caller could sensibly invoke.

use crate::args::GlobalOpts;

#[derive(Debug)]
pub enum InternalCommand {
    /// `git config --worktree credential.helper "!ralphus internal
    /// fork-credential-helper"` (set by `route_worktree_to_submitter_fork`)
    /// points git at this. `action` is whichever of git's credential-helper
    /// verbs (`get`/`store`/`erase`) git invoked it with.
    ForkCredentialHelper {
        action: String,
    },
    UsageError(String),
}

pub fn parse(args: &[String]) -> InternalCommand {
    match args.first().map(String::as_str) {
        Some("fork-credential-helper") => InternalCommand::ForkCredentialHelper {
            action: args.get(1).cloned().unwrap_or_default(),
        },
        Some(other) => InternalCommand::UsageError(format!("unknown internal subcommand: {other}")),
        None => InternalCommand::UsageError("internal requires a subcommand".to_string()),
    }
}

pub fn dispatch(cmd: InternalCommand, opts: &GlobalOpts) -> i32 {
    match cmd {
        InternalCommand::UsageError(m) => {
            println!("usage error: {m}");
            2
        }
        InternalCommand::ForkCredentialHelper { action } => {
            cmd_fork_credential_helper(opts, &action)
        }
    }
}

/// Implements git's credential-helper protocol (`get` only -- `store`/
/// `erase` are no-ops, since the daemon's DB is the only place a token is
/// ever written; git still calls them, so they must exit `0` rather than
/// error). Reads `ralphus.worktree-id`/`ralphus.worktree-grant` from
/// whichever worktree this process's cwd is inside (set once by
/// `route_worktree_to_submitter_fork` at materialization time), fetches the
/// matching token from the daemon, and prints git's expected
/// `username=`/`password=` response.
///
/// Deliberately resolves the daemon through `opts.client()` -- the same
/// `--daemon-url`/`$RALPHUS_DAEMON_URL`/default resolution every other CLI
/// command already uses -- rather than any address hard-coded here, since
/// the daemon's host/port is free to change independently of any one
/// worktree's git config.
///
/// Best-effort throughout: a worktree with no grant configured, or a daemon
/// call that fails, both exit `0` printing nothing -- git then falls back to
/// whatever ambient auth already exists (or fails the push with its own
/// error), exactly as if this helper had never been installed. A stray
/// non-zero exit here would make git treat this helper's absence of an
/// answer as a hard failure instead of "try something else."
fn cmd_fork_credential_helper(opts: &GlobalOpts, action: &str) -> i32 {
    // Consume (and discard) git's request block on stdin -- a series of
    // `key=value` lines terminated by a blank line/EOF. Required even though
    // unused: git writes it whether or not this helper reads it, and leaving
    // it undrained is the kind of thing that can wedge a pipe.
    for line in std::io::stdin().lines() {
        match line {
            Ok(l) if !l.is_empty() => continue,
            _ => break,
        }
    }
    if action != "get" {
        return 0;
    }
    let Some(worktree_id) = git_config_value("ralphus.worktree-id") else {
        return 0;
    };
    let Some(grant) = git_config_value("ralphus.worktree-grant") else {
        return 0;
    };
    let client = opts.client();
    let Ok(payload) = client.fetch_fork_credential(&worktree_id, &grant) else {
        return 0;
    };
    let Some(token) = payload["token"].as_str() else {
        return 0;
    };
    println!("username=oauth2");
    println!("password={token}");
    0
}

/// `git config --get <key>` against the current working directory's
/// repository -- this process's cwd is whichever worktree git invoked it
/// from, exactly the worktree `ralphus.worktree-id`/`-grant` were stamped
/// onto.
fn git_config_value(key: &str) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(["config", "--get", key])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if value.is_empty() { None } else { Some(value) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn parses_fork_credential_helper_with_action() {
        match parse(&v(&["fork-credential-helper", "get"])) {
            InternalCommand::ForkCredentialHelper { action } => assert_eq!(action, "get"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn unknown_subcommand_is_a_usage_error() {
        assert!(matches!(
            parse(&v(&["bogus"])),
            InternalCommand::UsageError(_)
        ));
        assert!(matches!(parse(&v(&[])), InternalCommand::UsageError(_)));
    }
}
