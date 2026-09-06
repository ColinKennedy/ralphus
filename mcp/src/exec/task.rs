//! Mirrors `ralphus_cli::commands::task::dispatch` -- see `exec/mod.rs`'s
//! module doc for the "why a mirror, not a reuse" rationale.

use ralphus_cli::client::DaemonClient;
use ralphus_cli::commands::env;
use ralphus_cli::commands::task::{self, TaskCommand};
use ralphus_cli::selector::squad_view_uri;
use serde_json::json;

use super::{ExecResult, usage};

pub fn execute(cmd: TaskCommand, client: &DaemonClient) -> ExecResult {
    match cmd {
        TaskCommand::Help | TaskCommand::UsageError(_) => Err(usage("no such tool")),
        TaskCommand::Env { selector, scope } => {
            let resolved = task::resolve_scoped(client, &selector, "task")?;
            Ok(client.env_view(&env::task_path(&resolved, &scope))?)
        }
        TaskCommand::Show { selector } => {
            let resolved = task::resolve_scoped(client, &selector, "task")?;
            let squad = client.squad(&resolved.squad_id)?;
            let uri = squad_view_uri(&squad, &resolved);
            Ok(task::with_uri(
                squad["tasks"][resolved.task_idx as usize].clone(),
                uri,
            ))
        }
        TaskCommand::SetStatus { selector, state } => {
            let resolved = task::resolve_scoped(client, &selector, "task")?;
            Ok(client.set_status(
                &resolved.squad_id,
                &state,
                "task",
                resolved.task_idx,
                -1,
                -1,
                "",
            )?)
        }
        TaskCommand::RestartProof { selector, from } => {
            let resolved = task::resolve_scoped(client, &selector, "task")?;
            Ok(client.restart_task_proof(&resolved.squad_id, resolved.task_idx, from)?)
        }
        TaskCommand::Edit {
            selector,
            name,
            project,
            model,
        } => {
            let resolved = task::resolve_scoped(client, &selector, "task")?;
            Ok(client.edit_task(
                &resolved.squad_id,
                resolved.task_idx,
                name.as_deref(),
                project.as_deref(),
                model.as_deref(),
            )?)
        }
    }
    .map(|v| if v.is_null() { json!({}) } else { v })
}
