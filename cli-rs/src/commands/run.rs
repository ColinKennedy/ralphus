//! `ralphus run <subcommand>`, ported from `cli/src/ralphus/__main__.py`'s
//! `run` group (list/show/logs/timeline/set-status/restart/retry/activate/
//! cancel/delete/rename/edit).

use serde_json::Value;

use crate::args::GlobalOpts;
use crate::commands::{CommandError, emit, run_and_report};
use crate::flags::Scanner;

#[derive(Debug, Clone)]
pub enum RunCommand {
    Help,
    List {
        status: Option<String>,
        name: Option<String>,
        sort: Option<String>,
    },
    Show {
        run_id: String,
    },
    Logs {
        run_id: String,
    },
    Timeline {
        run_id: String,
        write: Option<String>,
    },
    SetStatus {
        run_id: String,
        state: String,
    },
    Restart {
        run_id: String,
    },
    Retry {
        run_id: String,
    },
    Activate {
        run_id: String,
    },
    Cancel {
        run_id: String,
    },
    Delete {
        run_id: String,
        yes: bool,
    },
    Rename {
        run_id: String,
        label: String,
    },
    Edit {
        run_id: String,
        label: Option<String>,
    },
    UsageError(String),
}

#[must_use]
pub fn parse(args: &[String]) -> RunCommand {
    let mut scanner = Scanner::new(&args[1.min(args.len())..]);
    match args.first().map(String::as_str) {
        None => RunCommand::Help,
        Some("list") => {
            let status = scanner.take_value("--status").ok().flatten();
            let name = scanner.take_value("--name").ok().flatten();
            let sort = scanner.take_value("--sort").ok().flatten();
            RunCommand::List { status, name, sort }
        }
        Some("show") => with_run_id(scanner, |run_id| RunCommand::Show { run_id }),
        Some("logs") => with_run_id(scanner, |run_id| RunCommand::Logs { run_id }),
        Some("timeline") => {
            let write = scanner.take_value("--write").ok().flatten();
            with_run_id(scanner, |run_id| RunCommand::Timeline { run_id, write })
        }
        Some("set-status") => {
            let rest = scanner.remaining();
            match (rest.first(), rest.get(1)) {
                (Some(run_id), Some(state)) => RunCommand::SetStatus {
                    run_id: run_id.clone(),
                    state: state.clone(),
                },
                _ => RunCommand::UsageError("set-status requires <run_id> <state>".to_string()),
            }
        }
        Some("restart") => with_run_id(scanner, |run_id| RunCommand::Restart { run_id }),
        Some("retry") => with_run_id(scanner, |run_id| RunCommand::Retry { run_id }),
        Some("activate") => with_run_id(scanner, |run_id| RunCommand::Activate { run_id }),
        Some("cancel") => with_run_id(scanner, |run_id| RunCommand::Cancel { run_id }),
        Some("delete") => {
            let yes = scanner.take_bool("--yes");
            with_run_id(scanner, |run_id| RunCommand::Delete { run_id, yes })
        }
        Some("rename") => {
            let rest = scanner.remaining();
            match (rest.first(), rest.get(1)) {
                (Some(run_id), Some(label)) => RunCommand::Rename {
                    run_id: run_id.clone(),
                    label: label.clone(),
                },
                _ => RunCommand::UsageError("rename requires <run_id> <label>".to_string()),
            }
        }
        Some("edit") => {
            let label = scanner.take_value("--label").ok().flatten();
            with_run_id(scanner, |run_id| RunCommand::Edit { run_id, label })
        }
        Some(other) => RunCommand::UsageError(format!("unknown run subcommand: {other}")),
    }
}

fn with_run_id(scanner: Scanner, make: impl FnOnce(String) -> RunCommand) -> RunCommand {
    match scanner.remaining().into_iter().next() {
        Some(run_id) => make(run_id),
        None => RunCommand::UsageError("missing required <run_id> argument".to_string()),
    }
}

#[must_use]
pub fn dispatch(cmd: RunCommand, opts: &GlobalOpts) -> i32 {
    let client = opts.client();
    match cmd {
        RunCommand::Help => {
            println!(
                "ralphus run <list|show|logs|timeline|set-status|restart|retry|activate|cancel|delete|rename|edit>"
            );
            0
        }
        RunCommand::UsageError(m) => {
            println!("usage error: {m}");
            2
        }
        RunCommand::List { status, name, sort } => run_and_report(opts, None, || {
            let board = client.tasks(status.as_deref(), name.as_deref(), sort.as_deref())?;
            emit(opts, &board, crate::commands::misc::render_run_list);
            Ok(())
        }),
        RunCommand::Show { run_id } => run_and_report(opts, None, || {
            let run = client.run(&run_id)?;
            let resolved = crate::selector::ResolvedSelector {
                kind: "run".to_string(),
                run_id: run_id.clone(),
                task_idx: 0,
                session_idx: -1,
                verify_idx: -1,
                verify_scope: String::new(),
            };
            let uri = crate::selector::run_view_uri(&run, &resolved);
            let mut with_uri = run.clone();
            if let Value::Object(map) = &mut with_uri {
                let mut ordered = serde_json::Map::new();
                ordered.insert("uri".to_string(), Value::String(uri));
                ordered.extend(map.clone());
                *map = ordered;
            }
            emit(opts, &with_uri, render_run_detail);
            Ok(())
        }),
        RunCommand::Logs { run_id } => run_and_report(opts, None, || {
            let events = client.run_logs(&run_id)?;
            emit(opts, &events, render_events);
            Ok(())
        }),
        RunCommand::Timeline { run_id, write } => run_and_report(opts, None, || {
            let timeline = client.run_timeline(&run_id)?;
            if let Some(path) = &write {
                let text = timeline["text"].as_str().unwrap_or_default();
                std::fs::write(path, text)
                    .map_err(|e| CommandError::Usage(format!("could not write {path}: {e}")))?;
            }
            emit(opts, &timeline, render_timeline);
            Ok(())
        }),
        RunCommand::SetStatus { run_id, state } => run_and_report(opts, None, || {
            let result = client.set_status(&run_id, &state, "run", 0, -1, -1, "")?;
            emit(opts, &result, |_| println!("{run_id} -> {state}"));
            Ok(())
        }),
        RunCommand::Restart { run_id } => run_and_report(opts, None, || {
            let result = client.restart_run(&run_id)?;
            emit(opts, &result, render_dirtied);
            Ok(())
        }),
        RunCommand::Retry { run_id } => run_and_report(opts, None, || {
            let result = client.retry_run(&run_id)?;
            emit(opts, &result, |r| println!("{run_id} -> {}", r["state"]));
            Ok(())
        }),
        RunCommand::Activate { run_id } => run_and_report(opts, None, || {
            let result = client.activate_run(&run_id)?;
            emit(opts, &result, |r| println!("{run_id} -> {}", r["state"]));
            Ok(())
        }),
        RunCommand::Cancel { run_id } => run_and_report(opts, None, || {
            let result = client.cancel(&run_id)?;
            emit(opts, &result, |r| println!("{run_id} -> {}", r["state"]));
            Ok(())
        }),
        RunCommand::Delete { run_id, yes } => {
            if !yes && !confirm(&format!("Delete {run_id}? This cannot be undone. [y/N] ")) {
                println!("aborted");
                return 1;
            }
            run_and_report(opts, None, || {
                let result = client.delete_run(&run_id)?;
                emit(opts, &result, |r| println!("{run_id} -> {}", r["state"]));
                Ok(())
            })
        }
        RunCommand::Rename { run_id, label } => run_and_report(opts, None, || {
            let result = client.edit_run(&run_id, &label)?;
            emit(opts, &result, |_| println!("{run_id} renamed to '{label}'"));
            Ok(())
        }),
        RunCommand::Edit { run_id, label } => run_and_report(opts, None, || {
            let result = client.edit_run(&run_id, label.as_deref().unwrap_or_default())?;
            emit(opts, &result, |_| println!("{run_id} updated"));
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

fn render_run_detail(run: &Value) {
    let mut kv: Vec<(&str, String)> = Vec::new();
    if let Some(uri) = run["uri"].as_str() {
        kv.push(("uri", uri.to_string()));
    }
    kv.push(("id", run["id"].as_str().unwrap_or_default().to_string()));
    kv.push((
        "label",
        run["label"].as_str().unwrap_or_default().to_string(),
    ));
    kv.push((
        "state",
        run["state"].as_str().unwrap_or_default().to_string(),
    ));
    crate::output::print_kv(&kv);

    for (ti, task) in run["tasks"].as_array().into_iter().flatten().enumerate() {
        println!("\n[{ti}] task {}  {}", task["name"], task["state"]);
        for (vi, v) in task["verify"].as_array().into_iter().flatten().enumerate() {
            println!("    verify/{vi}  {}  {}", v["kind"], v["state"]);
        }
        for (si, s) in task["sessions"]
            .as_array()
            .into_iter()
            .flatten()
            .enumerate()
        {
            let name = s["name"]
                .as_str()
                .or_else(|| s["id"].as_str())
                .unwrap_or_default();
            println!(
                "    [{si}] session {name}  {}  agent={} model={}",
                s["state"],
                s["agent"],
                s["model"].as_str().unwrap_or("-")
            );
            for (vi, v) in s["verify"].as_array().into_iter().flatten().enumerate() {
                println!("        verify/{vi}  {}  {}", v["kind"], v["state"]);
            }
        }
    }
    let reviews = run["reviews"].as_array().cloned().unwrap_or_default();
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
        "run {}: {} events (truncated={}, gaps_possible={}), {} terminal-log refs, {} tasks, {} sessions",
        meta["run_id"],
        meta["event_count"],
        meta["truncated"],
        meta["gaps_possible"],
        meta["terminal_log_count"],
        meta["task_count"],
        meta["session_count"]
    );
    println!("written to: {}", timeline["file_path"]);
    println!();
    print!("{}", timeline["text"].as_str().unwrap_or_default());
}

fn render_dirtied(result: &Value) {
    println!("state: {}", result["state"]);
    let dirtied = result["dirtied"].as_array().cloned().unwrap_or_default();
    if !dirtied.is_empty() {
        println!("dirtied downstream runs:");
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
            RunCommand::List { status, sort, .. } => {
                assert_eq!(status.as_deref(), Some("done,failed"));
                assert_eq!(sort.as_deref(), Some("name"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parses_set_status_positional_pair() {
        match parse(&v(&["set-status", "run-1", "done"])) {
            RunCommand::SetStatus { run_id, state } => {
                assert_eq!(run_id, "run-1");
                assert_eq!(state, "done");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn missing_run_id_is_usage_error() {
        matches!(parse(&v(&["show"])), RunCommand::UsageError(_));
    }

    #[test]
    fn delete_parses_yes_flag() {
        match parse(&v(&["delete", "run-1", "--yes"])) {
            RunCommand::Delete { run_id, yes } => {
                assert_eq!(run_id, "run-1");
                assert!(yes);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }
}
