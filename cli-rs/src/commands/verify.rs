//! `ralphus verify <subcommand>`, ported from `cli/src/ralphus/__main__.py`'s
//! `verify` group (show/set-status/restart).

use serde_json::Value;

use crate::args::GlobalOpts;
use crate::client::DaemonClient;
use crate::commands::{CommandError, emit, run_and_report};
use crate::flags::Scanner;
use crate::selector::{ResolvedSelector, SelectorError, resolve_run_selector, run_view_uri};

#[derive(Debug, Clone)]
pub enum VerifyCommand {
    Help,
    Show { selector: String },
    SetStatus { selector: String, state: String },
    Restart { selector: String },
    UsageError(String),
}

#[must_use]
pub fn parse(args: &[String]) -> VerifyCommand {
    let scanner = Scanner::new(&args[1.min(args.len())..]);
    match args.first().map(String::as_str) {
        None => VerifyCommand::Help,
        Some("show") => with_selector(scanner, |selector| VerifyCommand::Show { selector }),
        Some("set-status") => {
            let rest = scanner.remaining();
            match (rest.first(), rest.get(1)) {
                (Some(selector), Some(state)) => VerifyCommand::SetStatus {
                    selector: selector.clone(),
                    state: state.clone(),
                },
                _ => {
                    VerifyCommand::UsageError("set-status requires <selector> <state>".to_string())
                }
            }
        }
        Some("restart") => with_selector(scanner, |selector| VerifyCommand::Restart { selector }),
        Some(other) => VerifyCommand::UsageError(format!("unknown verify subcommand: {other}")),
    }
}

fn with_selector(scanner: Scanner, make: impl FnOnce(String) -> VerifyCommand) -> VerifyCommand {
    match scanner.remaining().into_iter().next() {
        Some(selector) => make(selector),
        None => VerifyCommand::UsageError("missing required <selector> argument".to_string()),
    }
}

/// Mirrors `_resolve_selector_or_none`'s kind check; see `task.rs`'s twin for
/// why this returns a `Result` instead of printing inline.
fn resolve_scoped(
    client: &DaemonClient,
    selector: &str,
    want_kind: &str,
) -> Result<ResolvedSelector, CommandError> {
    let resolved = resolve_run_selector(client, selector)?;
    if resolved.kind != want_kind {
        return Err(CommandError::Selector(SelectorError(format!(
            "'{selector}' is a {} selector, not a {want_kind}",
            resolved.kind
        ))));
    }
    Ok(resolved)
}

fn with_uri(mut payload: Value, uri: String) -> Value {
    if let Value::Object(map) = &mut payload {
        let mut ordered = serde_json::Map::new();
        ordered.insert("uri".to_string(), Value::String(uri));
        ordered.extend(map.clone());
        *map = ordered;
    }
    payload
}

/// Indexes the verify step `resolved` addresses out of a `/api/runs/{id}`
/// view -- ported from Python's `_verify_step_for`: `task["verify"][idx]`
/// when `verify_scope == "task"`, else `session["verify"][idx]`.
fn verify_step_for(run: &Value, resolved: &ResolvedSelector) -> Value {
    let task = &run["tasks"][resolved.task_idx as usize];
    if resolved.verify_scope == "session" {
        task["sessions"][resolved.session_idx as usize]["verify"][resolved.verify_idx as usize]
            .clone()
    } else {
        task["verify"][resolved.verify_idx as usize].clone()
    }
}

#[must_use]
pub fn dispatch(cmd: VerifyCommand, opts: &GlobalOpts) -> i32 {
    let client = opts.client();
    match cmd {
        VerifyCommand::Help => {
            println!("ralphus verify <show|set-status|restart>");
            0
        }
        VerifyCommand::UsageError(m) => {
            println!("usage error: {m}");
            2
        }
        VerifyCommand::Show { selector } => run_and_report(opts, None, || {
            let resolved = resolve_scoped(&client, &selector, "verify")?;
            let run = client.run(&resolved.run_id)?;
            let uri = run_view_uri(&run, &resolved);
            let step = with_uri(verify_step_for(&run, &resolved), uri);
            let scope = resolved.verify_scope.clone();
            emit(opts, &step, |v| render_verify_detail(v, &scope));
            Ok(())
        }),
        VerifyCommand::SetStatus { selector, state } => run_and_report(opts, None, || {
            let resolved = resolve_scoped(&client, &selector, "verify")?;
            let result = client.set_status(
                &resolved.run_id,
                &state,
                "verify",
                resolved.task_idx,
                resolved.session_idx,
                resolved.verify_idx,
                &resolved.verify_scope,
            )?;
            emit(opts, &result, |_| println!("{selector} -> {state}"));
            Ok(())
        }),
        VerifyCommand::Restart { selector } => run_and_report(opts, None, || {
            let resolved = resolve_scoped(&client, &selector, "verify")?;
            let result = if resolved.verify_scope == "session" {
                client.restart_session_verify(
                    &resolved.run_id,
                    resolved.task_idx,
                    resolved.session_idx,
                    resolved.verify_idx,
                )?
            } else {
                client.restart_task_verify(
                    &resolved.run_id,
                    resolved.task_idx,
                    resolved.verify_idx,
                )?
            };
            emit(opts, &result, render_dirtied);
            Ok(())
        }),
    }
}

fn render_verify_detail(v: &Value, scope: &str) {
    crate::output::print_kv(&[
        ("uri", v["uri"].as_str().unwrap_or_default().to_string()),
        ("kind", v["kind"].as_str().unwrap_or_default().to_string()),
        ("state", v["state"].as_str().unwrap_or_default().to_string()),
        ("scope", scope.to_string()),
        ("agent", v["agent"].as_str().unwrap_or_default().to_string()),
        ("model", v["model"].as_str().unwrap_or_default().to_string()),
        ("spec", v["spec"].to_string()),
    ]);
    if let Some(output) = v["output"].as_str().filter(|o| !o.is_empty()) {
        println!("\noutput:");
        println!("{output}");
    }
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
    fn parses_show_with_selector() {
        match parse(&v(&["show", "run-1/build/verify/0"])) {
            VerifyCommand::Show { selector } => assert_eq!(selector, "run-1/build/verify/0"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parses_set_status_positional_pair() {
        match parse(&v(&["set-status", "run-1/build/verify/0", "done"])) {
            VerifyCommand::SetStatus { selector, state } => {
                assert_eq!(selector, "run-1/build/verify/0");
                assert_eq!(state, "done");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parses_restart() {
        match parse(&v(&["restart", "run-1/build/verify/0"])) {
            VerifyCommand::Restart { selector } => assert_eq!(selector, "run-1/build/verify/0"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn missing_selector_is_usage_error() {
        matches!(parse(&v(&["show"])), VerifyCommand::UsageError(_));
    }

    #[test]
    fn bare_verify_is_help() {
        matches!(parse(&v(&[])), VerifyCommand::Help);
    }

    #[test]
    fn verify_step_for_task_scope() {
        let run = serde_json::json!({
            "tasks": [{"verify": [{"kind": "command"}, {"kind": "prompt"}], "sessions": []}]
        });
        let resolved = ResolvedSelector {
            kind: "verify".to_string(),
            run_id: "run-1".to_string(),
            task_idx: 0,
            session_idx: -1,
            verify_idx: 1,
            verify_scope: "task".to_string(),
        };
        assert_eq!(verify_step_for(&run, &resolved)["kind"], "prompt");
    }

    #[test]
    fn verify_step_for_session_scope() {
        let run = serde_json::json!({
            "tasks": [{"verify": [], "sessions": [{"verify": [{"kind": "command"}, {"kind": "prompt"}]}]}]
        });
        let resolved = ResolvedSelector {
            kind: "verify".to_string(),
            run_id: "run-1".to_string(),
            task_idx: 0,
            session_idx: 0,
            verify_idx: 1,
            verify_scope: "session".to_string(),
        };
        assert_eq!(verify_step_for(&run, &resolved)["kind"], "prompt");
    }
}
