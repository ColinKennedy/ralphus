//! `ralphus task <subcommand>`, ported from `cli/src/ralphus/__main__.py`'s
//! `task` group (show/set-status/restart-proof/edit). `task show-tutor` is
//! intentionally not ported here -- it is a plain, argument-free reprint of
//! the Task TOML schema reference (`_cmd_show_tutor` -> `print(TASK_TUTOR)`),
//! already available from the top-level `ralphus tutor` command
//! (`Command::TutorShow` in `commands/mod.rs`, backed by
//! `crate::tutor::task_tutor()`); duplicating that text under a second
//! subcommand name adds no behavior worth a second leaf here.

use serde_json::Value;

use crate::args::GlobalOpts;
use crate::client::DaemonClient;
use crate::commands::{CommandError, emit, run_and_report};
use crate::flags::Scanner;
use crate::selector::{ResolvedSelector, SelectorError, resolve_squad_selector, squad_view_uri};

#[derive(Debug, Clone)]
pub enum TaskCommand {
    Help,
    Show {
        selector: String,
    },
    SetStatus {
        selector: String,
        state: String,
    },
    RestartProof {
        selector: String,
        from: i64,
    },
    Edit {
        selector: String,
        name: Option<String>,
        project: Option<String>,
        model: Option<String>,
    },
    /// RAL-324: read-only listing of this task's resolved environment.
    /// `scope` is `task` (what its cells inherit) or `proof` (what its
    /// task-scoped proof steps inherit).
    Env {
        selector: String,
        scope: String,
    },
    UsageError(String),
}

#[must_use]
pub fn parse(args: &[String]) -> TaskCommand {
    let mut scanner = Scanner::new(&args[1.min(args.len())..]);
    match args.first().map(String::as_str) {
        None | Some("help" | "--help" | "-h") => TaskCommand::Help,
        Some("show") => with_selector(scanner, |selector| TaskCommand::Show { selector }),
        Some("env") => {
            match crate::commands::env::take_scope(&mut scanner, "task", &["task", "proof"]) {
                Ok(scope) => {
                    with_selector(scanner, |selector| TaskCommand::Env { selector, scope })
                }
                Err(message) => TaskCommand::UsageError(message),
            }
        }
        Some("set-status") => {
            let rest = scanner.remaining();
            match (rest.first(), rest.get(1)) {
                (Some(selector), Some(state)) => TaskCommand::SetStatus {
                    selector: selector.clone(),
                    state: state.clone(),
                },
                _ => TaskCommand::UsageError("set-status requires <selector> <state>".to_string()),
            }
        }
        Some("restart-proof") => match scanner.take_parsed::<i64>("--from") {
            Ok(Some(from)) => with_selector(scanner, |selector| TaskCommand::RestartProof {
                selector,
                from,
            }),
            Ok(None) => {
                TaskCommand::UsageError("restart-proof requires --from <index>".to_string())
            }
            Err(e) => TaskCommand::UsageError(e.0),
        },
        Some("edit") => {
            let name = scanner.take_value("--name").ok().flatten();
            let project = scanner.take_value("--project").ok().flatten();
            let model = scanner.take_value("--model").ok().flatten();
            with_selector(scanner, |selector| TaskCommand::Edit {
                selector,
                name,
                project,
                model,
            })
        }
        Some(other) => TaskCommand::UsageError(format!("unknown task subcommand: {other}")),
    }
}

fn with_selector(scanner: Scanner, make: impl FnOnce(String) -> TaskCommand) -> TaskCommand {
    match scanner.remaining().into_iter().next() {
        Some(selector) => make(selector),
        None => TaskCommand::UsageError("missing required <selector> argument".to_string()),
    }
}

/// Resolves `selector`, mapping a kind mismatch to the same "'<selector>' is
/// a X selector, not a Y" message `_resolve_selector_or_none` prints in
/// Python -- ported as a plain `Result` since `run_and_report`/`CommandError`
/// already own the print-and-exit-code step here, rather than each handler
/// printing inline the way the Python version does.
pub fn resolve_scoped(
    client: &DaemonClient,
    selector: &str,
    want_kind: &str,
) -> Result<ResolvedSelector, CommandError> {
    let resolved = resolve_squad_selector(client, selector)?;
    if resolved.kind != want_kind {
        return Err(CommandError::Selector(SelectorError(format!(
            "'{selector}' is a {} selector, not a {want_kind}",
            resolved.kind
        ))));
    }
    Ok(resolved)
}

pub fn with_uri(mut payload: Value, uri: String) -> Value {
    if let Value::Object(map) = &mut payload {
        let mut ordered = serde_json::Map::new();
        ordered.insert("uri".to_string(), Value::String(uri));
        ordered.extend(map.clone());
        *map = ordered;
    }
    payload
}

#[must_use]
pub fn dispatch(cmd: TaskCommand, opts: &GlobalOpts) -> i32 {
    let client = opts.client();
    match cmd {
        TaskCommand::Help => {
            println!(
                "{}",
                crate::help_map::command_help(&["task"]).expect("task help exists")
            );
            0
        }
        TaskCommand::UsageError(m) => {
            println!("usage error: {m}");
            2
        }
        TaskCommand::Env { selector, scope } => run_and_report(opts, None, || {
            let resolved = resolve_scoped(&client, &selector, "task")?;
            let view = client.env_view(&crate::commands::env::task_path(&resolved, &scope))?;
            emit(opts, &view, crate::commands::env::render);
            Ok(())
        }),
        TaskCommand::Show { selector } => run_and_report(opts, None, || {
            let resolved = resolve_scoped(&client, &selector, "task")?;
            let squad = client.squad(&resolved.squad_id)?;
            let uri = squad_view_uri(&squad, &resolved);
            let task = with_uri(squad["tasks"][resolved.task_idx as usize].clone(), uri);
            emit(opts, &task, render_task_detail);
            Ok(())
        }),
        TaskCommand::SetStatus { selector, state } => run_and_report(opts, None, || {
            let resolved = resolve_scoped(&client, &selector, "task")?;
            let result = client.set_status(
                &resolved.squad_id,
                &state,
                "task",
                resolved.task_idx,
                -1,
                -1,
                "",
            )?;
            emit(opts, &result, |_| println!("{selector} -> {state}"));
            Ok(())
        }),
        TaskCommand::RestartProof { selector, from } => run_and_report(opts, None, || {
            let resolved = resolve_scoped(&client, &selector, "task")?;
            let result = client.restart_task_proof(&resolved.squad_id, resolved.task_idx, from)?;
            emit(opts, &result, render_dirtied);
            Ok(())
        }),
        TaskCommand::Edit {
            selector,
            name,
            project,
            model,
        } => run_and_report(opts, None, || {
            let resolved = resolve_scoped(&client, &selector, "task")?;
            let result = client.edit_task(
                &resolved.squad_id,
                resolved.task_idx,
                name.as_deref(),
                project.as_deref(),
                model.as_deref(),
            )?;
            emit(opts, &result, |_| println!("{selector} updated"));
            Ok(())
        }),
    }
}

fn render_task_detail(t: &Value) {
    let depends_on = t["depends_on"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "-".to_string());
    crate::output::print_kv(&[
        ("uri", t["uri"].as_str().unwrap_or_default().to_string()),
        ("name", t["name"].as_str().unwrap_or_default().to_string()),
        (
            "project",
            t["project"].as_str().unwrap_or_default().to_string(),
        ),
        ("state", t["state"].as_str().unwrap_or_default().to_string()),
        ("model", t["model"].as_str().unwrap_or_default().to_string()),
        ("depends_on", depends_on),
    ]);
    for (vi, v) in t["proof"].as_array().into_iter().flatten().enumerate() {
        println!("  proof/{vi}  {}  {}", v["kind"], v["state"]);
    }
    for (si, s) in t["cells"].as_array().into_iter().flatten().enumerate() {
        let name = s["name"]
            .as_str()
            .or_else(|| s["id"].as_str())
            .unwrap_or_default();
        println!("  [{si}] cell {name}  {}", s["state"]);
    }
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
    fn parses_show_with_selector() {
        match parse(&v(&["show", "squad-1/build"])) {
            TaskCommand::Show { selector } => assert_eq!(selector, "squad-1/build"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parses_env_with_a_default_and_an_explicit_scope() {
        match parse(&v(&["env", "squad-1/build"])) {
            TaskCommand::Env { selector, scope } => {
                assert_eq!(selector, "squad-1/build");
                assert_eq!(scope, "task");
            }
            other => panic!("unexpected: {other:?}"),
        }
        match parse(&v(&["env", "squad-1/build", "--scope", "proof"])) {
            TaskCommand::Env { scope, .. } => assert_eq!(scope, "proof"),
            other => panic!("unexpected: {other:?}"),
        }
        matches!(
            parse(&v(&["env", "squad-1/build", "--scope", "cell"])),
            TaskCommand::UsageError(_)
        );
    }

    #[test]
    fn parses_set_status_positional_pair() {
        match parse(&v(&["set-status", "squad-1/build", "done"])) {
            TaskCommand::SetStatus { selector, state } => {
                assert_eq!(selector, "squad-1/build");
                assert_eq!(state, "done");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parses_restart_proof_with_from_flag() {
        match parse(&v(&["restart-proof", "squad-1/build", "--from", "2"])) {
            TaskCommand::RestartProof { selector, from } => {
                assert_eq!(selector, "squad-1/build");
                assert_eq!(from, 2);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn restart_proof_requires_from_flag() {
        matches!(
            parse(&v(&["restart-proof", "squad-1/build"])),
            TaskCommand::UsageError(_)
        );
    }

    #[test]
    fn parses_edit_with_optional_flags() {
        match parse(&v(&[
            "edit",
            "squad-1/build",
            "--name",
            "new-name",
            "--project",
            "proj",
            "--model",
            "gpt-5",
        ])) {
            TaskCommand::Edit {
                selector,
                name,
                project,
                model,
            } => {
                assert_eq!(selector, "squad-1/build");
                assert_eq!(name.as_deref(), Some("new-name"));
                assert_eq!(project.as_deref(), Some("proj"));
                assert_eq!(model.as_deref(), Some("gpt-5"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn missing_selector_is_usage_error() {
        matches!(parse(&v(&["show"])), TaskCommand::UsageError(_));
    }

    #[test]
    fn bare_task_is_help() {
        matches!(parse(&v(&[])), TaskCommand::Help);
    }
}
