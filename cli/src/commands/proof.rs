//! `ralphus proof <subcommand>`, ported from `cli/src/ralphus/__main__.py`'s
//! `verify` group (show/set-status/restart).

use serde_json::Value;

use crate::args::GlobalOpts;
use crate::client::DaemonClient;
use crate::commands::{CommandError, emit, run_and_report};
use crate::flags::Scanner;
use crate::selector::{ResolvedSelector, SelectorError, resolve_squad_selector, squad_view_uri};

#[derive(Debug, Clone)]
pub enum ProofCommand {
    Help,
    Show {
        selector: String,
    },
    SetStatus {
        selector: String,
        state: String,
    },
    Restart {
        selector: String,
    },
    Edit {
        selector: String,
        model: Option<String>,
    },
    /// RAL-324: read-only listing of this proof step's resolved environment.
    Env {
        selector: String,
    },
    UsageError(String),
}

#[must_use]
pub fn parse(args: &[String]) -> ProofCommand {
    let mut scanner = Scanner::new(&args[1.min(args.len())..]);
    match args.first().map(String::as_str) {
        None | Some("help" | "--help" | "-h") => ProofCommand::Help,
        Some("show") => with_selector(scanner, |selector| ProofCommand::Show { selector }),
        Some("env") => with_selector(scanner, |selector| ProofCommand::Env { selector }),
        Some("set-status") => {
            let rest = scanner.remaining();
            match (rest.first(), rest.get(1)) {
                (Some(selector), Some(state)) => ProofCommand::SetStatus {
                    selector: selector.clone(),
                    state: state.clone(),
                },
                _ => ProofCommand::UsageError("set-status requires <selector> <state>".to_string()),
            }
        }
        Some("restart") => with_selector(scanner, |selector| ProofCommand::Restart { selector }),
        Some("edit") => {
            let model = scanner.take_value("--model").ok().flatten();
            with_selector(scanner, |selector| ProofCommand::Edit { selector, model })
        }
        Some(other) => ProofCommand::UsageError(format!("unknown proof subcommand: {other}")),
    }
}

fn with_selector(scanner: Scanner, make: impl FnOnce(String) -> ProofCommand) -> ProofCommand {
    match scanner.remaining().into_iter().next() {
        Some(selector) => make(selector),
        None => ProofCommand::UsageError("missing required <selector> argument".to_string()),
    }
}

/// Mirrors `_resolve_selector_or_none`'s kind check; see `task.rs`'s twin for
/// why this returns a `Result` instead of printing inline.
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

/// Indexes the proof step `resolved` addresses out of a `/api/squads/{id}`
/// view -- ported from Python's `_verify_step_for`: `task["proof"][idx]`
/// when `proof_scope == "task"`, else `cell["proof"][idx]`.
pub fn proof_step_for(squad: &Value, resolved: &ResolvedSelector) -> Value {
    let task = &squad["tasks"][resolved.task_idx as usize];
    if resolved.proof_scope == "cell" {
        task["cells"][resolved.cell_idx as usize]["proof"][resolved.proof_idx as usize].clone()
    } else {
        task["proof"][resolved.proof_idx as usize].clone()
    }
}

#[must_use]
pub fn dispatch(cmd: ProofCommand, opts: &GlobalOpts) -> i32 {
    let client = opts.client();
    match cmd {
        ProofCommand::Help => {
            println!(
                "{}",
                crate::help_map::command_help(&["proof"]).expect("proof help exists")
            );
            0
        }
        ProofCommand::UsageError(m) => {
            println!("usage error: {m}");
            2
        }
        ProofCommand::Env { selector } => run_and_report(opts, None, || {
            let resolved = resolve_scoped(&client, &selector, "proof")?;
            let view = client.env_view(&crate::commands::env::proof_path(&resolved))?;
            emit(opts, &view, crate::commands::env::render);
            Ok(())
        }),
        ProofCommand::Show { selector } => run_and_report(opts, None, || {
            let resolved = resolve_scoped(&client, &selector, "proof")?;
            let squad = client.squad(&resolved.squad_id)?;
            let uri = squad_view_uri(&squad, &resolved);
            let step = with_uri(proof_step_for(&squad, &resolved), uri);
            let scope = resolved.proof_scope.clone();
            emit(opts, &step, |v| render_proof_detail(v, &scope));
            Ok(())
        }),
        ProofCommand::SetStatus { selector, state } => run_and_report(opts, None, || {
            let resolved = resolve_scoped(&client, &selector, "proof")?;
            let result = client.set_status(
                &resolved.squad_id,
                &state,
                "proof",
                resolved.task_idx,
                resolved.cell_idx,
                resolved.proof_idx,
                &resolved.proof_scope,
            )?;
            emit(opts, &result, |_| println!("{selector} -> {state}"));
            Ok(())
        }),
        ProofCommand::Restart { selector } => run_and_report(opts, None, || {
            let resolved = resolve_scoped(&client, &selector, "proof")?;
            let result = if resolved.proof_scope == "cell" {
                client.restart_cell_proof(
                    &resolved.squad_id,
                    resolved.task_idx,
                    resolved.cell_idx,
                    resolved.proof_idx,
                )?
            } else {
                client.restart_task_proof(
                    &resolved.squad_id,
                    resolved.task_idx,
                    resolved.proof_idx,
                )?
            };
            emit(opts, &result, render_dirtied);
            Ok(())
        }),
        ProofCommand::Edit { selector, model } => run_and_report(opts, None, || {
            let resolved = resolve_scoped(&client, &selector, "proof")?;
            let result = client.edit_proof(
                &resolved.squad_id,
                resolved.task_idx,
                &resolved.proof_scope,
                resolved.cell_idx,
                resolved.proof_idx,
                model.as_deref(),
            )?;
            emit(opts, &result, |_| println!("{selector} updated"));
            Ok(())
        }),
    }
}

fn render_proof_detail(v: &Value, scope: &str) {
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
    fn parses_env_with_selector() {
        match parse(&v(&["env", "squad-1/build/proof/0"])) {
            ProofCommand::Env { selector } => assert_eq!(selector, "squad-1/build/proof/0"),
            other => panic!("unexpected: {other:?}"),
        }
        matches!(parse(&v(&["env"])), ProofCommand::UsageError(_));
    }

    #[test]
    fn parses_show_with_selector() {
        match parse(&v(&["show", "squad-1/build/proof/0"])) {
            ProofCommand::Show { selector } => assert_eq!(selector, "squad-1/build/proof/0"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parses_set_status_positional_pair() {
        match parse(&v(&["set-status", "squad-1/build/proof/0", "done"])) {
            ProofCommand::SetStatus { selector, state } => {
                assert_eq!(selector, "squad-1/build/proof/0");
                assert_eq!(state, "done");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parses_restart() {
        match parse(&v(&["restart", "squad-1/build/proof/0"])) {
            ProofCommand::Restart { selector } => assert_eq!(selector, "squad-1/build/proof/0"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parses_edit_with_model_flag() {
        match parse(&v(&["edit", "squad-1/build/proof/0", "--model", "gpt-5"])) {
            ProofCommand::Edit { selector, model } => {
                assert_eq!(selector, "squad-1/build/proof/0");
                assert_eq!(model.as_deref(), Some("gpt-5"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn missing_selector_is_usage_error() {
        matches!(parse(&v(&["show"])), ProofCommand::UsageError(_));
    }

    #[test]
    fn bare_proof_is_help() {
        matches!(parse(&v(&[])), ProofCommand::Help);
    }

    #[test]
    fn proof_step_for_task_scope() {
        let squad = serde_json::json!({
            "tasks": [{"proof": [{"kind": "command"}, {"kind": "prompt"}], "cells": []}]
        });
        let resolved = ResolvedSelector {
            kind: "proof".to_string(),
            squad_id: "squad-1".to_string(),
            task_idx: 0,
            cell_idx: -1,
            proof_idx: 1,
            proof_scope: "task".to_string(),
        };
        assert_eq!(proof_step_for(&squad, &resolved)["kind"], "prompt");
    }

    #[test]
    fn proof_step_for_cell_scope() {
        let squad = serde_json::json!({
            "tasks": [{"proof": [], "cells": [{"proof": [{"kind": "command"}, {"kind": "prompt"}]}]}]
        });
        let resolved = ResolvedSelector {
            kind: "proof".to_string(),
            squad_id: "squad-1".to_string(),
            task_idx: 0,
            cell_idx: 0,
            proof_idx: 1,
            proof_scope: "cell".to_string(),
        };
        assert_eq!(proof_step_for(&squad, &resolved)["kind"], "prompt");
    }
}
