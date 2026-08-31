//! Mirrors `ralphus_cli::commands::queue::dispatch`.

use ralphus_cli::client::DaemonClient;
use ralphus_cli::commands::queue::{self, QueueCommand};
use serde_json::json;

use super::{ExecResult, usage};

pub fn execute(cmd: QueueCommand, client: &DaemonClient) -> ExecResult {
    match cmd {
        QueueCommand::Help | QueueCommand::UsageError(_) => Err(usage("no such tool")),
        QueueCommand::List { all: _ } => Ok(client.queue()?),
        QueueCommand::Reorder { paths } => {
            let normalized = queue::normalize_queue_paths(client, &paths)?;
            Ok(client.queue_reorder(&normalized)?)
        }
        QueueCommand::SetPosition {
            paths,
            to,
            relative,
        } => {
            let normalized = queue::normalize_queue_paths(client, &paths)?;
            Ok(client.queue_set_position(&normalized, to, !relative)?)
        }
        QueueCommand::SetStatus { path, state } => {
            let parsed = queue::resolve_queue_path(client, &path)?;
            Ok(client.set_status(
                &parsed.squad_id,
                &state,
                &parsed.kind,
                parsed.task_idx,
                parsed.cell_idx,
                parsed.proof_idx,
                &parsed.proof_scope,
            )?)
        }
    }
    .map(|v| if v.is_null() { json!({}) } else { v })
}
