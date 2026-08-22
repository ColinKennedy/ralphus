//! `ralphus queue <subcommand>`, ported from `cli/src/ralphus/__main__.py`'s
//! `queue` group (list/reorder/set-position/set-status). Bare `ralphus queue`
//! behaves like `queue list` (`p_queue.set_defaults(func=_cmd_queue_list)` in
//! the Python source).

use serde_json::Value;

use crate::args::GlobalOpts;
use crate::client::DaemonClient;
use crate::commands::{CommandError, emit, run_and_report};
use crate::flags::Scanner;
use crate::selector::{self, ResolvedSelector, SelectorError};

#[derive(Debug, Clone)]
pub enum QueueCommand {
    List {
        all: bool,
    },
    Reorder {
        paths: Vec<String>,
    },
    SetPosition {
        paths: Vec<String>,
        to: i64,
        relative: bool,
    },
    SetStatus {
        path: String,
        state: String,
    },
    UsageError(String),
}

#[must_use]
pub fn parse(args: &[String]) -> QueueCommand {
    let mut scanner = Scanner::new(&args[1.min(args.len())..]);
    match args.first().map(String::as_str) {
        None => QueueCommand::List { all: false },
        Some("list") => {
            let all = scanner.take_bool("--all");
            QueueCommand::List { all }
        }
        Some("reorder") => {
            let paths = scanner.remaining();
            if paths.is_empty() {
                QueueCommand::UsageError("reorder requires at least one <path>".to_string())
            } else {
                QueueCommand::Reorder { paths }
            }
        }
        Some("set-position") => match parse_set_position(scanner) {
            Ok(cmd) => cmd,
            Err(e) => QueueCommand::UsageError(e),
        },
        Some("set-status") => {
            let rest = scanner.remaining();
            match (rest.first(), rest.get(1)) {
                (Some(path), Some(state)) => QueueCommand::SetStatus {
                    path: path.clone(),
                    state: state.clone(),
                },
                _ => QueueCommand::UsageError("set-status requires <path> <state>".to_string()),
            }
        }
        Some(other) => QueueCommand::UsageError(format!("unknown queue subcommand: {other}")),
    }
}

fn parse_set_position(mut scanner: Scanner) -> Result<QueueCommand, String> {
    let relative = scanner.take_bool("--relative");
    let to = scanner
        .take_parsed::<i64>("--to")
        .map_err(|e| e.0)?
        .ok_or_else(|| "set-position requires --to <n>".to_string())?;
    let paths = scanner.remaining();
    if paths.is_empty() {
        return Err("set-position requires at least one <path>".to_string());
    }
    Ok(QueueCommand::SetPosition {
        paths,
        to,
        relative,
    })
}

#[must_use]
pub fn dispatch(cmd: QueueCommand, opts: &GlobalOpts) -> i32 {
    let client = opts.client();
    match cmd {
        QueueCommand::UsageError(m) => {
            println!("usage error: {m}");
            2
        }
        QueueCommand::List { all } => run_and_report(opts, None, || {
            let result = client.queue()?;
            emit(opts, &result, |r| render_queue_list(r, all));
            Ok(())
        }),
        QueueCommand::Reorder { paths } => run_and_report(opts, None, || {
            let normalized = normalize_queue_paths(&client, &paths)?;
            let result = client.queue_reorder(&normalized)?;
            emit(opts, &result, |r| print_queue_order(&r["order"]));
            Ok(())
        }),
        QueueCommand::SetPosition {
            paths,
            to,
            relative,
        } => run_and_report(opts, None, || {
            let normalized = normalize_queue_paths(&client, &paths)?;
            let result = client.queue_set_position(&normalized, to, !relative)?;
            emit(opts, &result, |r| print_queue_order(&r["order"]));
            Ok(())
        }),
        QueueCommand::SetStatus { path, state } => run_and_report(opts, None, || {
            let parsed = resolve_queue_path(&client, &path)?;
            let result = client.set_status(
                &parsed.run_id,
                &state,
                &parsed.kind,
                parsed.task_idx,
                parsed.session_idx,
                parsed.verify_idx,
                &parsed.verify_scope,
            )?;
            emit(opts, &result, |_| println!("set {path} -> {state}"));
            Ok(())
        }),
    }
}

/// Set-status fields for one queue item -- mirrors Python's `_PathParts`
/// TypedDict (`_parse_queue_path`/`_resolve_queue_path`).
struct PathParts {
    kind: String,
    run_id: String,
    task_idx: i64,
    session_idx: i64,
    verify_idx: i64,
    verify_scope: String,
}

/// Parses a queue item path (`run`, `run/t<ti>/s<si>`, `run/t<ti>/s<si>/v<vi>`,
/// or `run/t<ti>/tv<vi>`) into set-status fields -- ports Python's
/// `_parse_queue_path` (mirrors the daemon's own grammar for this shape).
fn parse_queue_path(path: &str) -> Option<PathParts> {
    let segs: Vec<&str> = path.split('/').collect();
    match segs.as_slice() {
        [run] => Some(PathParts {
            kind: "run".to_string(),
            run_id: (*run).to_string(),
            task_idx: 0,
            session_idx: -1,
            verify_idx: -1,
            verify_scope: String::new(),
        }),
        [run, t, s] => {
            let ti = t.strip_prefix('t')?.parse::<i64>().ok()?;
            if let Some(vi) = s.strip_prefix("tv") {
                Some(PathParts {
                    kind: "verify".to_string(),
                    run_id: (*run).to_string(),
                    task_idx: ti,
                    session_idx: -1,
                    verify_idx: vi.parse::<i64>().ok()?,
                    verify_scope: "task".to_string(),
                })
            } else {
                let si = s.strip_prefix('s')?.parse::<i64>().ok()?;
                Some(PathParts {
                    kind: "session".to_string(),
                    run_id: (*run).to_string(),
                    task_idx: ti,
                    session_idx: si,
                    verify_idx: -1,
                    verify_scope: String::new(),
                })
            }
        }
        [run, t, s, v] => {
            let ti = t.strip_prefix('t')?.parse::<i64>().ok()?;
            let si = s.strip_prefix('s')?.parse::<i64>().ok()?;
            let vi = v.strip_prefix('v')?.parse::<i64>().ok()?;
            Some(PathParts {
                kind: "verify".to_string(),
                run_id: (*run).to_string(),
                task_idx: ti,
                session_idx: si,
                verify_idx: vi,
                verify_scope: "session".to_string(),
            })
        }
        _ => None,
    }
}

/// Resolves `path` -- a RAL-155 URI or a positional queue item path -- to
/// set-status fields, ports Python's `_resolve_queue_path`.
fn resolve_queue_path(client: &DaemonClient, path: &str) -> Result<PathParts, CommandError> {
    if ralphus_core::uri::looks_like_uri(path) {
        let resolved = selector::resolve_run_selector(client, path)?;
        return Ok(PathParts {
            kind: resolved.kind,
            run_id: resolved.run_id,
            task_idx: resolved.task_idx,
            session_idx: resolved.session_idx,
            verify_idx: resolved.verify_idx,
            verify_scope: resolved.verify_scope,
        });
    }
    parse_queue_path(path)
        .ok_or_else(|| CommandError::Usage(format!("could not parse item path '{path}'")))
}

/// Renders a resolved selector as the daemon's own queue-item path -- ports
/// Python's `_queue_path_for`. A task has no queue item of its own (only its
/// sessions/verify steps do), so that case is an error rather than a silently
/// wrong path.
fn queue_path_for(resolved: &ResolvedSelector) -> Result<String, SelectorError> {
    match resolved.kind.as_str() {
        "run" => Ok(resolved.run_id.clone()),
        "session" => Ok(format!(
            "{}/t{}/s{}",
            resolved.run_id, resolved.task_idx, resolved.session_idx
        )),
        "verify" if resolved.verify_scope == "task" => Ok(format!(
            "{}/t{}/tv{}",
            resolved.run_id, resolved.task_idx, resolved.verify_idx
        )),
        "verify" => Ok(format!(
            "{}/t{}/s{}/v{}",
            resolved.run_id, resolved.task_idx, resolved.session_idx, resolved.verify_idx
        )),
        other => Err(SelectorError(format!(
            "a task has no queue item of its own -- address one of its sessions or verify steps \
             instead (got a {other} selector)"
        ))),
    }
}

/// Converts any RAL-155 URI in `paths` to the daemon's queue-item path,
/// leaving an already-positional path untouched -- ports Python's
/// `_normalize_queue_paths`.
fn normalize_queue_paths(
    client: &DaemonClient,
    paths: &[String],
) -> Result<Vec<String>, CommandError> {
    paths
        .iter()
        .map(|p| {
            if ralphus_core::uri::looks_like_uri(p) {
                let resolved = selector::resolve_run_selector(client, p)?;
                Ok(queue_path_for(&resolved)?)
            } else {
                Ok(p.clone())
            }
        })
        .collect()
}

fn print_queue_order(order: &Value) {
    println!("new queue order:");
    for (i, p) in order.as_array().into_iter().flatten().enumerate() {
        println!("  {i:>3}  {}", p.as_str().unwrap_or_default());
    }
}

/// Formats a queue rank roughly like Python's `f"{rank:g}"` -- whole numbers
/// print without a trailing `.0`, everything else prints as-is.
fn format_rank(rank: f64) -> String {
    if rank.fract() == 0.0 && rank.abs() < 1e15 {
        format!("{}", rank as i64)
    } else {
        format!("{rank}")
    }
}

fn render_queue_list(result: &Value, show_all: bool) {
    let items = result["items"].as_array().cloned().unwrap_or_default();
    let shown: Vec<&Value> = items
        .iter()
        .filter(|i| show_all || i["readiness"].as_str() == Some("ready"))
        .collect();
    if shown.is_empty() {
        let hint = if show_all {
            ""
        } else {
            " ready to run (use --all to include blocked items)"
        };
        println!("no queued work{hint}");
        return;
    }
    for i in &shown {
        let rank_s = i["queue_rank"]
            .as_f64()
            .map_or_else(|| "-".to_string(), format_rank);
        let blocked: Vec<&str> = i["blocked_by"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        let tail = if blocked.is_empty() {
            String::new()
        } else {
            format!("  <- {}", blocked.join(", "))
        };
        let readiness = i["readiness"].as_str().unwrap_or("None");
        println!(
            "  [{rank_s:>4}] {readiness:<9} {}  {}{tail}",
            i["path"].as_str().unwrap_or_default(),
            i["name"].as_str().unwrap_or_default()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn bare_queue_behaves_like_list() {
        matches!(parse(&[]), QueueCommand::List { all: false });
    }

    #[test]
    fn parses_list_all_flag() {
        match parse(&v(&["list", "--all"])) {
            QueueCommand::List { all } => assert!(all),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parses_reorder_paths() {
        match parse(&v(&["reorder", "run-1/t0/s1", "run-1/t0/s2"])) {
            QueueCommand::Reorder { paths } => {
                assert_eq!(paths, v(&["run-1/t0/s1", "run-1/t0/s2"]));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn reorder_requires_at_least_one_path() {
        matches!(parse(&v(&["reorder"])), QueueCommand::UsageError(_));
    }

    #[test]
    fn parses_set_position_with_to_and_relative() {
        match parse(&v(&["set-position", "run-1", "--to", "3", "--relative"])) {
            QueueCommand::SetPosition {
                paths,
                to,
                relative,
            } => {
                assert_eq!(paths, v(&["run-1"]));
                assert_eq!(to, 3);
                assert!(relative);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn set_position_requires_to_flag() {
        matches!(
            parse(&v(&["set-position", "run-1"])),
            QueueCommand::UsageError(_)
        );
    }

    #[test]
    fn parses_set_status_positional_pair() {
        match parse(&v(&["set-status", "run-1/t0/s1", "ignored"])) {
            QueueCommand::SetStatus { path, state } => {
                assert_eq!(path, "run-1/t0/s1");
                assert_eq!(state, "ignored");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn set_status_requires_both_positionals() {
        matches!(
            parse(&v(&["set-status", "run-1"])),
            QueueCommand::UsageError(_)
        );
    }

    #[test]
    fn unknown_subcommand_is_usage_error() {
        matches!(parse(&v(&["bogus"])), QueueCommand::UsageError(_));
    }

    #[test]
    fn parse_queue_path_bare_run() {
        let parts = parse_queue_path("run-1").unwrap();
        assert_eq!(parts.kind, "run");
        assert_eq!(parts.run_id, "run-1");
        assert_eq!(parts.task_idx, 0);
        assert_eq!(parts.session_idx, -1);
    }

    #[test]
    fn parse_queue_path_session() {
        let parts = parse_queue_path("run-1/t0/s2").unwrap();
        assert_eq!(parts.kind, "session");
        assert_eq!(parts.task_idx, 0);
        assert_eq!(parts.session_idx, 2);
    }

    #[test]
    fn parse_queue_path_task_verify() {
        let parts = parse_queue_path("run-1/t0/tv3").unwrap();
        assert_eq!(parts.kind, "verify");
        assert_eq!(parts.verify_scope, "task");
        assert_eq!(parts.verify_idx, 3);
    }

    #[test]
    fn parse_queue_path_session_verify() {
        let parts = parse_queue_path("run-1/t0/s2/v3").unwrap();
        assert_eq!(parts.kind, "verify");
        assert_eq!(parts.verify_scope, "session");
        assert_eq!(parts.session_idx, 2);
        assert_eq!(parts.verify_idx, 3);
    }

    #[test]
    fn parse_queue_path_rejects_malformed() {
        assert!(parse_queue_path("run-1/bad/s2").is_none());
    }

    #[test]
    fn queue_path_for_run() {
        let resolved = ResolvedSelector {
            kind: "run".to_string(),
            run_id: "run-1".to_string(),
            task_idx: 0,
            session_idx: -1,
            verify_idx: -1,
            verify_scope: String::new(),
        };
        assert_eq!(queue_path_for(&resolved).unwrap(), "run-1");
    }

    #[test]
    fn queue_path_for_task_errors() {
        let resolved = ResolvedSelector {
            kind: "task".to_string(),
            run_id: "run-1".to_string(),
            task_idx: 0,
            session_idx: -1,
            verify_idx: -1,
            verify_scope: String::new(),
        };
        assert!(queue_path_for(&resolved).is_err());
    }

    #[test]
    fn format_rank_drops_trailing_zero() {
        assert_eq!(format_rank(3.0), "3");
        assert_eq!(format_rank(3.5), "3.5");
    }
}
