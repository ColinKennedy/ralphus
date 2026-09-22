//! `ralphus user <subcommand>` (RAL-338 follow-up): a ralphus user's own
//! personal access token for one forge host, so `route_worktree_to_submitter_fork`'s
//! git-credential helper can push/fetch as that user without any per-host
//! SSH key/config setup.

use serde_json::Value;

use crate::args::GlobalOpts;
use crate::flags::Scanner;

use super::{emit, run_and_report};

#[derive(Debug)]
pub enum UserCommand {
    Help,
    SetForgeToken {
        user: String,
        host: String,
        token: String,
    },
    ListForgeTokens {
        user: String,
    },
    DeleteForgeToken {
        user: String,
        host: String,
    },
    UsageError(String),
}

pub fn parse(args: &[String]) -> UserCommand {
    let scanner = Scanner::new(args);
    let tail = scanner.remaining();
    match tail.first().map(String::as_str) {
        Some("set-forge-token") => parse_set_forge_token(&tail[1..]),
        Some("list-forge-tokens") => parse_list_forge_tokens(&tail[1..]),
        Some("delete-forge-token") => parse_delete_forge_token(&tail[1..]),
        Some(other) => UserCommand::UsageError(format!("unknown user subcommand: {other}")),
        None => UserCommand::Help,
    }
}

fn parse_set_forge_token(args: &[String]) -> UserCommand {
    let mut scanner = Scanner::new(args);
    let host = match scanner.take_value("--host") {
        Ok(v) => v,
        Err(e) => return UserCommand::UsageError(e.0),
    };
    let token = match scanner.take_value("--token") {
        Ok(v) => v,
        Err(e) => return UserCommand::UsageError(e.0),
    };
    let (Some(host), Some(token)) = (host, token) else {
        return UserCommand::UsageError(
            "user set-forge-token requires --host and --token".to_string(),
        );
    };
    let Some(user) = scanner.remaining().into_iter().next() else {
        return UserCommand::UsageError(
            "user set-forge-token requires a <user> argument".to_string(),
        );
    };
    UserCommand::SetForgeToken { user, host, token }
}

fn parse_list_forge_tokens(args: &[String]) -> UserCommand {
    let scanner = Scanner::new(args);
    let Some(user) = scanner.remaining().into_iter().next() else {
        return UserCommand::UsageError(
            "user list-forge-tokens requires a <user> argument".to_string(),
        );
    };
    UserCommand::ListForgeTokens { user }
}

fn parse_delete_forge_token(args: &[String]) -> UserCommand {
    let scanner = Scanner::new(args);
    let rest = scanner.remaining();
    let mut it = rest.into_iter();
    let (Some(user), Some(host)) = (it.next(), it.next()) else {
        return UserCommand::UsageError(
            "user delete-forge-token requires <user> and <host> arguments".to_string(),
        );
    };
    UserCommand::DeleteForgeToken { user, host }
}

pub fn dispatch(cmd: UserCommand, opts: &GlobalOpts) -> i32 {
    match cmd {
        UserCommand::Help => {
            println!(
                "{}",
                crate::help_map::command_help(&["user"]).expect("user help exists")
            );
            0
        }
        UserCommand::UsageError(m) => {
            println!("usage error: {m}");
            2
        }
        UserCommand::SetForgeToken { user, host, token } => run_and_report(opts, None, || {
            let client = opts.client();
            let record = client.set_user_forge_token(&user, &host, &token)?;
            emit(opts, &record, |_| {
                println!("forge token set for user {user:?} host {host:?}");
            });
            Ok(())
        }),
        UserCommand::ListForgeTokens { user } => run_and_report(opts, None, || {
            let client = opts.client();
            let payload = client.list_user_forge_tokens(&user)?;
            emit(opts, &payload, render_forge_tokens);
            Ok(())
        }),
        UserCommand::DeleteForgeToken { user, host } => run_and_report(opts, None, || {
            let client = opts.client();
            client.delete_user_forge_token(&user, &host)?;
            emit(opts, &serde_json::json!({"removed": true}), |_| {
                println!("removed forge token for user {user:?} host {host:?}");
            });
            Ok(())
        }),
    }
}

fn render_forge_tokens(payload: &Value) {
    let tokens = payload["tokens"].as_array().cloned().unwrap_or_default();
    if tokens.is_empty() {
        println!("no forge tokens configured");
        return;
    }
    for t in tokens {
        println!("{}", t["host"].as_str().unwrap_or_default());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn parses_set_forge_token() {
        match parse(&v(&[
            "set-forge-token",
            "alice",
            "--host",
            "gitlab.com",
            "--token",
            "glpat-x",
        ])) {
            UserCommand::SetForgeToken { user, host, token } => {
                assert_eq!(user, "alice");
                assert_eq!(host, "gitlab.com");
                assert_eq!(token, "glpat-x");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn set_forge_token_requires_host_token_and_user() {
        assert!(matches!(
            parse(&v(&["set-forge-token", "alice", "--host", "gitlab.com"])),
            UserCommand::UsageError(_)
        ));
        assert!(matches!(
            parse(&v(&[
                "set-forge-token",
                "--host",
                "gitlab.com",
                "--token",
                "t"
            ])),
            UserCommand::UsageError(_)
        ));
    }

    #[test]
    fn parses_list_forge_tokens() {
        match parse(&v(&["list-forge-tokens", "alice"])) {
            UserCommand::ListForgeTokens { user } => assert_eq!(user, "alice"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parses_delete_forge_token() {
        match parse(&v(&["delete-forge-token", "alice", "gitlab.com"])) {
            UserCommand::DeleteForgeToken { user, host } => {
                assert_eq!(user, "alice");
                assert_eq!(host, "gitlab.com");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn no_subcommand_is_help_and_unknown_is_usage_error() {
        assert!(matches!(parse(&v(&[])), UserCommand::Help));
        assert!(matches!(parse(&v(&["bogus"])), UserCommand::UsageError(_)));
    }
}
