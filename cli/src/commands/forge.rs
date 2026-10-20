//! `ralphus forge pool <list|add|remove>`: the daemon-wide poll-token pool
//! (RAL-635). Pool tokens are used only for read-only forge polling (CI
//! watch, PR cache refresh, base-state reads); every write still uses the
//! acting user's own token. Tokens are never printed back.

use serde_json::Value;

use crate::args::GlobalOpts;
use crate::flags::Scanner;

use super::{emit, run_and_report};

#[derive(Debug, Clone)]
pub enum ForgeCommand {
    PoolList,
    PoolAdd {
        kind: String,
        host: String,
        label: String,
        token: crate::args::Secret,
    },
    PoolRemove {
        kind: String,
        host: String,
        label: String,
    },
    Help,
    UsageError(String),
}

/// The public host a pool entry of `kind` defaults to.
fn default_host(kind: &str) -> &'static str {
    if kind.eq_ignore_ascii_case("gitlab") {
        "gitlab.com"
    } else {
        "github.com"
    }
}

#[must_use]
pub fn parse(args: &[String]) -> ForgeCommand {
    match args.first().map(String::as_str) {
        Some("pool") => parse_pool(&args[1..]),
        Some("help" | "--help" | "-h") | None => ForgeCommand::Help,
        Some(other) => ForgeCommand::UsageError(format!("unknown forge subcommand: {other}")),
    }
}

fn parse_pool(args: &[String]) -> ForgeCommand {
    match args.first().map(String::as_str) {
        Some("list") => ForgeCommand::PoolList,
        Some("add") => parse_add(&args[1..]),
        Some("remove") => parse_remove(&args[1..]),
        Some("help" | "--help" | "-h") | None => ForgeCommand::Help,
        Some(other) => ForgeCommand::UsageError(format!("unknown forge pool subcommand: {other}")),
    }
}

fn parse_add(args: &[String]) -> ForgeCommand {
    let mut scanner = Scanner::new(args);
    let host = match scanner.take_value("--host") {
        Ok(v) => v,
        Err(e) => return ForgeCommand::UsageError(e.0),
    };
    let token = match scanner.take_value("--token") {
        Ok(v) => v,
        Err(e) => return ForgeCommand::UsageError(e.0),
    };
    let Some(token) = token else {
        return ForgeCommand::UsageError("forge pool add requires --token".to_string());
    };
    let mut rest = scanner.remaining().into_iter();
    let (Some(kind), Some(label)) = (rest.next(), rest.next()) else {
        return ForgeCommand::UsageError(
            "forge pool add requires <kind> and <label> arguments".to_string(),
        );
    };
    let host = host.unwrap_or_else(|| default_host(&kind).to_string());
    ForgeCommand::PoolAdd {
        kind,
        host,
        label,
        token: crate::args::Secret::new(token),
    }
}

fn parse_remove(args: &[String]) -> ForgeCommand {
    let mut scanner = Scanner::new(args);
    let host = match scanner.take_value("--host") {
        Ok(v) => v,
        Err(e) => return ForgeCommand::UsageError(e.0),
    };
    let mut rest = scanner.remaining().into_iter();
    let (Some(kind), Some(label)) = (rest.next(), rest.next()) else {
        return ForgeCommand::UsageError(
            "forge pool remove requires <kind> and <label> arguments".to_string(),
        );
    };
    let host = host.unwrap_or_else(|| default_host(&kind).to_string());
    ForgeCommand::PoolRemove { kind, host, label }
}

#[must_use]
pub fn dispatch(cmd: ForgeCommand, opts: &GlobalOpts) -> i32 {
    match cmd {
        ForgeCommand::PoolList => run_and_report(opts, None, || {
            let payload = opts.client().list_forge_pool()?;
            emit(opts, &payload, render_pool);
            Ok(())
        }),
        ForgeCommand::PoolAdd {
            kind,
            host,
            label,
            token,
        } => run_and_report(opts, None, || {
            let record =
                opts.client()
                    .add_forge_pool_token(&kind, &host, &label, token.expose())?;
            emit(opts, &record, |_| {
                println!("added {kind} poll token {label:?} for {host}");
            });
            Ok(())
        }),
        ForgeCommand::PoolRemove { kind, host, label } => run_and_report(opts, None, || {
            let payload = opts
                .client()
                .remove_forge_pool_token(&kind, &host, &label)?;
            emit(opts, &payload, |_| {
                println!("removed {kind} poll token {label:?} for {host}");
            });
            Ok(())
        }),
        ForgeCommand::Help => {
            println!(
                "{}",
                crate::help_map::command_help(&["forge"]).expect("forge help exists")
            );
            0
        }
        ForgeCommand::UsageError(m) => {
            println!("usage error: {m}");
            2
        }
    }
}

fn render_pool(payload: &Value) {
    let tokens = payload["tokens"].as_array().cloned().unwrap_or_default();
    if tokens.is_empty() {
        println!("no poll tokens configured");
        return;
    }
    for t in tokens {
        println!(
            "{}  {}  {}  {}",
            t["kind"].as_str().unwrap_or_default(),
            t["host"].as_str().unwrap_or_default(),
            t["label"].as_str().unwrap_or_default(),
            t["masked"].as_str().unwrap_or_default()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn add_defaults_the_host_from_the_kind() {
        match parse(&v(&[
            "pool", "add", "gitlab", "ci-bot", "--token", "glpat-x",
        ])) {
            cmd @ ForgeCommand::PoolAdd { .. } => {
                // `main` logs every parsed command with `{:?}`.
                let logged = format!("{cmd:?}");
                assert!(!logged.contains("glpat-x"), "{logged}");
                let ForgeCommand::PoolAdd {
                    host, label, token, ..
                } = cmd
                else {
                    unreachable!()
                };
                assert_eq!(host, "gitlab.com");
                assert_eq!(label, "ci-bot");
                assert_eq!(token.expose(), "glpat-x");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn add_takes_a_self_hosted_host() {
        match parse(&v(&[
            "pool", "add", "github", "bot", "--host", "ghe.corp", "--token", "t",
        ])) {
            ForgeCommand::PoolAdd { host, .. } => assert_eq!(host, "ghe.corp"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn add_requires_a_token_and_positionals() {
        assert!(matches!(
            parse(&v(&["pool", "add", "github", "bot"])),
            ForgeCommand::UsageError(_)
        ));
        assert!(matches!(
            parse(&v(&["pool", "add", "--token", "t"])),
            ForgeCommand::UsageError(_)
        ));
    }

    #[test]
    fn remove_and_list_parse() {
        assert!(matches!(
            parse(&v(&["pool", "remove", "github", "bot"])),
            ForgeCommand::PoolRemove { host, .. } if host == "github.com"
        ));
        assert!(matches!(
            parse(&v(&["pool", "list"])),
            ForgeCommand::PoolList
        ));
    }
}
