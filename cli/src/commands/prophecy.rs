//! `ralphus prophecy <subcommand>`: read-side access to the prophecy
//! subsystem (`docs/prophecy-design.md`) -- a durable, append-only record of
//! what an agent learned while it worked. Phase 1 only: `list`/`show`. No
//! write path exists here -- phase 6's `prophecy record` is explicitly
//! deferred (blocked on the credential question, RAL-252/RAL-225 territory).

use serde_json::Value;

use crate::args::GlobalOpts;
use crate::client::ProphecyFilters;
use crate::flags::Scanner;

use super::{emit, run_and_report};

#[derive(Debug, Clone)]
pub enum ProphecyCommand {
    List {
        entity_uri: Option<String>,
        squad_id: Option<String>,
        guardian_id: Option<String>,
        limit: i64,
        offset: i64,
    },
    Show {
        entity_uri: String,
    },
    Help,
    UsageError(String),
}

#[must_use]
pub fn parse(args: &[String]) -> ProphecyCommand {
    match args.first().map(String::as_str) {
        Some("show") => {
            let scanner = Scanner::new(&args[1..]);
            match scanner.remaining().into_iter().next() {
                Some(entity_uri) => ProphecyCommand::Show { entity_uri },
                None => ProphecyCommand::UsageError(
                    "prophecy show requires an <entity-uri> argument".to_string(),
                ),
            }
        }
        Some("help" | "--help" | "-h") => ProphecyCommand::Help,
        Some("list") => parse_list(&args[1..]),
        None => parse_list(args),
        Some(other) => ProphecyCommand::UsageError(format!("unknown prophecy subcommand: {other}")),
    }
}

fn parse_list(rest: &[String]) -> ProphecyCommand {
    let mut scanner = Scanner::new(rest);
    let entity_uri = scanner.take_value("--entity").ok().flatten();
    let squad_id = scanner.take_value("--squad").ok().flatten();
    let guardian_id = scanner.take_value("--guardian").ok().flatten();
    let limit = scanner
        .take_parsed::<i64>("--limit")
        .ok()
        .flatten()
        .unwrap_or(100);
    let offset = scanner
        .take_parsed::<i64>("--offset")
        .ok()
        .flatten()
        .unwrap_or(0);
    ProphecyCommand::List {
        entity_uri,
        squad_id,
        guardian_id,
        limit,
        offset,
    }
}

#[must_use]
pub fn dispatch(cmd: ProphecyCommand, opts: &GlobalOpts) -> i32 {
    match cmd {
        ProphecyCommand::List {
            entity_uri,
            squad_id,
            guardian_id,
            limit,
            offset,
        } => {
            let client = opts.client();
            run_and_report(opts, None, || {
                let rows = client.list_prophecies(ProphecyFilters {
                    entity_uri: entity_uri.as_deref(),
                    squad_id: squad_id.as_deref(),
                    guardian_id: guardian_id.as_deref(),
                    limit,
                    offset,
                })?;
                emit(opts, &rows, render_prophecy_rows);
                Ok(())
            })
        }
        ProphecyCommand::Show { entity_uri } => {
            let client = opts.client();
            run_and_report(opts, None, || {
                let rows = client.show_prophecy(&entity_uri)?;
                emit(opts, &rows, render_prophecy_rows);
                Ok(())
            })
        }
        ProphecyCommand::Help => {
            println!(
                "{}",
                crate::help_map::command_help(&["prophecy"]).expect("prophecy help exists")
            );
            0
        }
        ProphecyCommand::UsageError(m) => {
            println!("usage error: {m}");
            2
        }
    }
}

fn render_prophecy_rows(rows: &Value) {
    let rows = rows.as_array().cloned().unwrap_or_default();
    if rows.is_empty() {
        println!("no prophecies recorded");
        return;
    }
    for row in &rows {
        println!(
            "[{}] {} attempt={} kind={}",
            row["created_at_ms"],
            row["entity_uri"].as_str().unwrap_or_default(),
            row["attempt"],
            row["kind"].as_str().unwrap_or_default()
        );
        println!("    {}", row["body"].as_str().unwrap_or_default());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn bare_prophecy_behaves_like_list() {
        assert!(matches!(parse(&[]), ProphecyCommand::List { .. }));
    }

    #[test]
    fn parses_list_explicitly_with_filters() {
        match parse(&v(&["list", "--guardian", "guardian-1", "--limit", "5"])) {
            ProphecyCommand::List {
                guardian_id, limit, ..
            } => {
                assert_eq!(guardian_id.as_deref(), Some("guardian-1"));
                assert_eq!(limit, 5);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parses_show_with_entity_uri() {
        match parse(&v(&["show", "cell:squad-1:0:0"])) {
            ProphecyCommand::Show { entity_uri } => {
                assert_eq!(entity_uri, "cell:squad-1:0:0");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn show_without_entity_uri_is_usage_error() {
        assert!(matches!(
            parse(&v(&["show"])),
            ProphecyCommand::UsageError(_)
        ));
    }

    #[test]
    fn unknown_subcommand_is_usage_error() {
        assert!(matches!(
            parse(&v(&["bogus"])),
            ProphecyCommand::UsageError(_)
        ));
    }
}
