//! `ralphus prophecy <subcommand>` (design doc §10 Phase 1): the read side of
//! the durable, append-only prophecy log (`daemon/src/prophecy.rs`) -- what
//! an agent (or ralphus itself, e.g. a `guardian_merge.rs` conflict-
//! resolution decision) learned mid-work. Both leaves are pure queries over
//! `GET /api/prophecies`/`GET /api/prophecies/{id}`; nothing here writes.

use serde_json::Value;

use crate::args::GlobalOpts;
use crate::client::ProphecyFilters;
use crate::commands::{emit, run_and_report};
use crate::flags::Scanner;

#[derive(Debug, Clone)]
pub enum ProphecyCommand {
    List {
        entity: Option<String>,
        kind: Option<String>,
        q: Option<String>,
        limit: i64,
        offset: i64,
        ascending: bool,
    },
    Show {
        id: i64,
    },
    UsageError(String),
}

#[must_use]
pub fn parse(args: &[String]) -> ProphecyCommand {
    let mut scanner = Scanner::new(&args[1.min(args.len())..]);
    match args.first().map(String::as_str) {
        None | Some("list") => {
            let entity = match scanner.take_value("--entity") {
                Ok(v) => v,
                Err(e) => return ProphecyCommand::UsageError(e.0),
            };
            let kind = match scanner.take_value("--kind") {
                Ok(v) => v,
                Err(e) => return ProphecyCommand::UsageError(e.0),
            };
            let q = match scanner.take_value("--q") {
                Ok(v) => v,
                Err(e) => return ProphecyCommand::UsageError(e.0),
            };
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
            let ascending = scanner.take_bool("--ascending");
            ProphecyCommand::List {
                entity,
                kind,
                q,
                limit,
                offset,
                ascending,
            }
        }
        Some("show") => match scanner.remaining().into_iter().next() {
            Some(raw) => match raw.parse::<i64>() {
                Ok(id) => ProphecyCommand::Show { id },
                Err(_) => ProphecyCommand::UsageError(format!("prophecy show: not an id: {raw}")),
            },
            None => ProphecyCommand::UsageError("show requires a <id> argument".to_string()),
        },
        Some(other) => ProphecyCommand::UsageError(format!("unknown prophecy subcommand: {other}")),
    }
}

#[must_use]
pub fn dispatch(cmd: ProphecyCommand, opts: &GlobalOpts) -> i32 {
    let client = opts.client();
    match cmd {
        ProphecyCommand::UsageError(m) => {
            println!("usage error: {m}");
            2
        }
        ProphecyCommand::List {
            entity,
            kind,
            q,
            limit,
            offset,
            ascending,
        } => run_and_report(opts, None, || {
            let filters = ProphecyFilters {
                entity: entity.as_deref(),
                kind: kind.as_deref(),
                q: q.as_deref(),
                limit,
                offset,
                ascending,
            };
            let page = client.prophecies(filters)?;
            emit(opts, &page, render_prophecy_page);
            Ok(())
        }),
        ProphecyCommand::Show { id } => run_and_report(opts, None, || {
            let row = client.prophecy_get(id)?;
            emit(opts, &row, render_prophecy_row);
            Ok(())
        }),
    }
}

fn render_prophecy_page(page: &Value) {
    let rows = page["rows"].as_array().cloned().unwrap_or_default();
    if rows.is_empty() {
        println!("no matching prophecies");
        return;
    }
    for row in &rows {
        render_prophecy_row(row);
    }
    println!("({} of {} total)", rows.len(), page["total"]);
}

fn render_prophecy_row(row: &Value) {
    println!(
        "[{}] #{} {} attempt={} kind={}",
        row["created_at_ms"],
        row["id"],
        row["entity_uri"].as_str().unwrap_or_default(),
        row["attempt"],
        row["kind"].as_str().unwrap_or_default(),
    );
    println!("    {}", row["body"].as_str().unwrap_or_default());
}
