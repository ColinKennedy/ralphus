//! `ralphus squad <subcommand>`, ported from `cli/src/ralphus/__main__.py`'s
//! `run` group (list/show/logs/timeline/set-status/restart/retry/activate/
//! cancel/delete/rename/edit).

use serde_json::Value;

use crate::args::GlobalOpts;
use crate::commands::{CommandError, emit, run_and_report};
use crate::flags::Scanner;

#[derive(Debug, Clone)]
pub enum SquadCommand {
    Help,
    List {
        status: Option<String>,
        name: Option<String>,
        sort: Option<String>,
    },
    Show {
        squad_id: String,
    },
    Logs {
        squad_id: String,
    },
    Timeline {
        squad_id: String,
        write: Option<String>,
    },
    SetStatus {
        squad_id: String,
        state: String,
    },
    Restart {
        squad_id: String,
    },
    Retry {
        squad_id: String,
    },
    Activate {
        squad_id: String,
    },
    Cancel {
        squad_id: String,
    },
    Delete {
        squad_id: String,
        yes: bool,
    },
    Rename {
        squad_id: String,
        label: String,
    },
    Edit {
        squad_id: String,
        label: Option<String>,
    },
    /// RAL-324: read-only listing of this squad's resolved environment.
    Env {
        squad_id: String,
    },
    UsageError(String),
}

#[must_use]
pub fn parse(args: &[String]) -> SquadCommand {
    let mut scanner = Scanner::new(&args[1.min(args.len())..]);
    match args.first().map(String::as_str) {
        None | Some("help" | "--help" | "-h") => SquadCommand::Help,
        Some("list") => {
            let status = scanner.take_value("--status").ok().flatten();
            let name = scanner.take_value("--name").ok().flatten();
            let sort = scanner.take_value("--sort").ok().flatten();
            SquadCommand::List { status, name, sort }
        }
        Some("show") => with_squad_id(scanner, |squad_id| SquadCommand::Show { squad_id }),
        Some("env") => with_squad_id(scanner, |squad_id| SquadCommand::Env { squad_id }),
        Some("logs") => with_squad_id(scanner, |squad_id| SquadCommand::Logs { squad_id }),
        Some("timeline") => {
            let write = scanner.take_value("--write").ok().flatten();
            with_squad_id(scanner, |squad_id| SquadCommand::Timeline {
                squad_id,
                write,
            })
        }
        Some("set-status") => {
            let rest = scanner.remaining();
            match (rest.first(), rest.get(1)) {
                (Some(squad_id), Some(state)) => SquadCommand::SetStatus {
                    squad_id: squad_id.clone(),
                    state: state.clone(),
                },
                _ => SquadCommand::UsageError("set-status requires <squad_id> <state>".to_string()),
            }
        }
        Some("restart") => with_squad_id(scanner, |squad_id| SquadCommand::Restart { squad_id }),
        Some("retry") => with_squad_id(scanner, |squad_id| SquadCommand::Retry { squad_id }),
        Some("activate") => with_squad_id(scanner, |squad_id| SquadCommand::Activate { squad_id }),
        Some("cancel") => with_squad_id(scanner, |squad_id| SquadCommand::Cancel { squad_id }),
        Some("delete") => {
            let yes = scanner.take_bool("--yes");
            with_squad_id(scanner, |squad_id| SquadCommand::Delete { squad_id, yes })
        }
        Some("rename") => {
            let rest = scanner.remaining();
            match (rest.first(), rest.get(1)) {
                (Some(squad_id), Some(label)) => SquadCommand::Rename {
                    squad_id: squad_id.clone(),
                    label: label.clone(),
                },
                _ => SquadCommand::UsageError("rename requires <squad_id> <label>".to_string()),
            }
        }
        Some("edit") => {
            let label = scanner.take_value("--label").ok().flatten();
            with_squad_id(scanner, |squad_id| SquadCommand::Edit { squad_id, label })
        }
        Some(other) => SquadCommand::UsageError(format!("unknown squad subcommand: {other}")),
    }
}

fn with_squad_id(scanner: Scanner, make: impl FnOnce(String) -> SquadCommand) -> SquadCommand {
    match scanner.remaining().into_iter().next() {
        Some(squad_id) => make(squad_id),
        None => SquadCommand::UsageError("missing required <squad_id> argument".to_string()),
    }
}

#[must_use]
pub fn dispatch(cmd: SquadCommand, opts: &GlobalOpts) -> i32 {
    let client = opts.client();
    match cmd {
        SquadCommand::Help => {
            println!(
                "{}",
                crate::help_map::command_help(&["squad"]).expect("squad help exists")
            );
            0
        }
        SquadCommand::UsageError(m) => {
            println!("usage error: {m}");
            2
        }
        SquadCommand::List { status, name, sort } => run_and_report(opts, None, || {
            let board = client.tasks(status.as_deref(), name.as_deref(), sort.as_deref())?;
            emit(opts, &board, crate::commands::misc::render_squad_list);
            Ok(())
        }),
        SquadCommand::Env { squad_id } => {
            crate::commands::env::show(opts, format!("/api/squads/{squad_id}/env"))
        }
        SquadCommand::Show { squad_id } => run_and_report(opts, None, || {
            let squad = client.squad(&squad_id)?;
            let resolved = crate::selector::ResolvedSelector {
                kind: "squad".to_string(),
                squad_id: squad_id.clone(),
                task_idx: 0,
                cell_idx: -1,
                proof_idx: -1,
                proof_scope: String::new(),
            };
            let uri = crate::selector::squad_view_uri(&squad, &resolved);
            let mut with_uri = squad.clone();
            if let Value::Object(map) = &mut with_uri {
                let mut ordered = serde_json::Map::new();
                ordered.insert("uri".to_string(), Value::String(uri));
                ordered.extend(map.clone());
                *map = ordered;
            }
            emit(opts, &with_uri, render_squad_detail);
            Ok(())
        }),
        SquadCommand::Logs { squad_id } => run_and_report(opts, None, || {
            let events = client.squad_logs(&squad_id)?;
            emit(opts, &events, render_events);
            Ok(())
        }),
        SquadCommand::Timeline { squad_id, write } => run_and_report(opts, None, || {
            let timeline = client.squad_timeline(&squad_id)?;
            if let Some(path) = &write {
                let text = timeline["text"].as_str().unwrap_or_default();
                std::fs::write(path, text)
                    .map_err(|e| CommandError::Usage(format!("could not write {path}: {e}")))?;
            }
            emit(opts, &timeline, render_timeline);
            Ok(())
        }),
        SquadCommand::SetStatus { squad_id, state } => run_and_report(opts, None, || {
            let result = client.set_status(&squad_id, &state, "squad", 0, -1, -1, "")?;
            emit(opts, &result, |_| println!("{squad_id} -> {state}"));
            Ok(())
        }),
        SquadCommand::Restart { squad_id } => run_and_report(opts, None, || {
            let result = client.restart_squad(&squad_id)?;
            emit(opts, &result, render_dirtied);
            Ok(())
        }),
        SquadCommand::Retry { squad_id } => run_and_report(opts, None, || {
            let result = client.retry_squad(&squad_id)?;
            emit(opts, &result, |r| println!("{squad_id} -> {}", r["state"]));
            Ok(())
        }),
        SquadCommand::Activate { squad_id } => run_and_report(opts, None, || {
            let result = client.activate_squad(&squad_id)?;
            emit(opts, &result, |r| println!("{squad_id} -> {}", r["state"]));
            Ok(())
        }),
        SquadCommand::Cancel { squad_id } => run_and_report(opts, None, || {
            let result = client.cancel(&squad_id)?;
            emit(opts, &result, |r| println!("{squad_id} -> {}", r["state"]));
            Ok(())
        }),
        SquadCommand::Delete { squad_id, yes } => {
            if !yes && !confirm(&format!("Delete {squad_id}? This cannot be undone. [y/N] ")) {
                println!("aborted");
                return 1;
            }
            run_and_report(opts, None, || {
                let result = client.delete_squad(&squad_id)?;
                emit(opts, &result, |r| println!("{squad_id} -> {}", r["state"]));
                Ok(())
            })
        }
        SquadCommand::Rename { squad_id, label } => run_and_report(opts, None, || {
            let result = client.edit_squad(&squad_id, &label)?;
            emit(opts, &result, |_| {
                println!("{squad_id} renamed to '{label}'")
            });
            Ok(())
        }),
        SquadCommand::Edit { squad_id, label } => run_and_report(opts, None, || {
            let result = client.edit_squad(&squad_id, label.as_deref().unwrap_or_default())?;
            emit(opts, &result, |_| println!("{squad_id} updated"));
            Ok(())
        }),
    }
}

fn confirm(prompt: &str) -> bool {
    use std::io::Write as _;
    print!("{prompt}");
    let _ = std::io::stdout().flush();
    let mut answer = String::new();
    let _ = std::io::stdin().read_line(&mut answer);
    matches!(answer.trim().to_lowercase().as_str(), "y" | "yes")
}

fn render_squad_detail(squad: &Value) {
    let mut kv: Vec<(&str, String)> = Vec::new();
    if let Some(uri) = squad["uri"].as_str() {
        kv.push(("uri", uri.to_string()));
    }
    kv.push(("id", squad["id"].as_str().unwrap_or_default().to_string()));
    kv.push((
        "label",
        squad["label"].as_str().unwrap_or_default().to_string(),
    ));
    kv.push((
        "state",
        squad["state"].as_str().unwrap_or_default().to_string(),
    ));
    crate::output::print_kv(&kv);

    for (ti, task) in squad["tasks"].as_array().into_iter().flatten().enumerate() {
        println!("\n[{ti}] task {}  {}", task["name"], task["state"]);
        for (vi, v) in task["proof"].as_array().into_iter().flatten().enumerate() {
            println!("    proof/{vi}  {}  {}", v["kind"], v["state"]);
        }
        for (si, s) in task["cells"].as_array().into_iter().flatten().enumerate() {
            let name = s["name"]
                .as_str()
                .or_else(|| s["id"].as_str())
                .unwrap_or_default();
            println!(
                "    [{si}] cell {name}  {}  agent={} model={}",
                s["state"],
                s["agent"],
                s["model"].as_str().unwrap_or("-")
            );
            for (vi, v) in s["proof"].as_array().into_iter().flatten().enumerate() {
                println!("        proof/{vi}  {}  {}", v["kind"], v["state"]);
            }
        }
    }
    let reviews = squad["reviews"].as_array().cloned().unwrap_or_default();
    if !reviews.is_empty() {
        println!("\nreviews:");
        for r in &reviews {
            println!("  {}  {}  {}", r["id"], r["name"], r["status"]);
        }
    }
}

fn render_events(events: &Value) {
    let events = events.as_array().cloned().unwrap_or_default();
    if events.is_empty() {
        println!("no events");
        return;
    }
    for e in &events {
        let ref_str = e["ref"]
            .as_str()
            .map(|r| format!(" {r}"))
            .unwrap_or_default();
        println!("[{}] {}{ref_str}: {}", e["at_ms"], e["scope"], e["message"]);
    }
}

fn render_timeline(timeline: &Value) {
    let meta = &timeline["meta"];
    println!(
        "squad {}: {} events (truncated={}, gaps_possible={}), {} terminal-log refs, {} tasks, {} cells",
        meta["squad_id"],
        meta["event_count"],
        meta["truncated"],
        meta["gaps_possible"],
        meta["terminal_log_count"],
        meta["task_count"],
        meta["cell_count"]
    );
    println!("written to: {}", timeline["file_path"]);
    println!();
    print!("{}", timeline["text"].as_str().unwrap_or_default());
}

fn render_dirtied(result: &Value) {
    println!("state: {}", result["state"]);
    let dirtied = result["dirtied"].as_array().cloned().unwrap_or_default();
    if !dirtied.is_empty() {
        println!("dirtied downstream squads:");
        for d in &dirtied {
            println!("  {d}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn parses_list_with_flags() {
        match parse(&v(&["list", "--status", "done,failed", "--sort", "name"])) {
            SquadCommand::List { status, sort, .. } => {
                assert_eq!(status.as_deref(), Some("done,failed"));
                assert_eq!(sort.as_deref(), Some("name"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parses_env_with_squad_id() {
        match parse(&v(&["env", "squad-1"])) {
            SquadCommand::Env { squad_id } => assert_eq!(squad_id, "squad-1"),
            other => panic!("unexpected: {other:?}"),
        }
        matches!(parse(&v(&["env"])), SquadCommand::UsageError(_));
    }

    #[test]
    fn parses_set_status_positional_pair() {
        match parse(&v(&["set-status", "squad-1", "done"])) {
            SquadCommand::SetStatus { squad_id, state } => {
                assert_eq!(squad_id, "squad-1");
                assert_eq!(state, "done");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn missing_squad_id_is_usage_error() {
        matches!(parse(&v(&["show"])), SquadCommand::UsageError(_));
    }

    #[test]
    fn delete_parses_yes_flag() {
        match parse(&v(&["delete", "squad-1", "--yes"])) {
            SquadCommand::Delete { squad_id, yes } => {
                assert_eq!(squad_id, "squad-1");
                assert!(yes);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }
}
