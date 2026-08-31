//! Mirrors `ralphus_cli::commands::squad::dispatch`.

use ralphus_cli::client::DaemonClient;
use ralphus_cli::commands::CommandError;
use ralphus_cli::commands::squad::SquadCommand;
use ralphus_cli::commands::task::with_uri;
use ralphus_cli::selector::{ResolvedSelector, squad_view_uri};

use super::{ExecResult, usage};

pub fn execute(cmd: SquadCommand, client: &DaemonClient) -> ExecResult {
    match cmd {
        SquadCommand::Help | SquadCommand::UsageError(_) => Err(usage("no such tool")),
        SquadCommand::List { status, name, sort } => {
            Ok(client.tasks(status.as_deref(), name.as_deref(), sort.as_deref())?)
        }
        SquadCommand::Show { squad_id } => {
            let squad = client.squad(&squad_id)?;
            let resolved = ResolvedSelector {
                kind: "squad".to_string(),
                squad_id: squad_id.clone(),
                task_idx: 0,
                cell_idx: -1,
                proof_idx: -1,
                proof_scope: String::new(),
            };
            let uri = squad_view_uri(&squad, &resolved);
            Ok(with_uri(squad, uri))
        }
        SquadCommand::Logs { squad_id } => Ok(client.squad_logs(&squad_id)?),
        SquadCommand::Timeline { squad_id, write } => {
            let timeline = client.squad_timeline(&squad_id)?;
            if let Some(path) = &write {
                let text = timeline["text"].as_str().unwrap_or_default();
                std::fs::write(path, text)
                    .map_err(|e| CommandError::Usage(format!("could not write {path}: {e}")))?;
            }
            Ok(timeline)
        }
        SquadCommand::SetStatus { squad_id, state } => {
            Ok(client.set_status(&squad_id, &state, "squad", 0, -1, -1, "")?)
        }
        SquadCommand::Restart { squad_id } => Ok(client.restart_squad(&squad_id)?),
        SquadCommand::Retry { squad_id } => Ok(client.retry_squad(&squad_id)?),
        SquadCommand::Activate { squad_id } => Ok(client.activate_squad(&squad_id)?),
        SquadCommand::Cancel { squad_id } => Ok(client.cancel(&squad_id)?),
        SquadCommand::Delete { squad_id, yes } => {
            // See `exec::exec_clear`'s doc comment -- MCP has no stdin
            // confirmation channel, so `yes=true` is required outright.
            if !yes {
                return Err(usage(
                    "delete is destructive and requires yes=true (there is no interactive \
                     confirmation prompt over MCP)",
                ));
            }
            Ok(client.delete_squad(&squad_id)?)
        }
        SquadCommand::Rename { squad_id, label } => Ok(client.edit_squad(&squad_id, &label)?),
        SquadCommand::Edit { squad_id, label } => {
            Ok(client.edit_squad(&squad_id, label.as_deref().unwrap_or_default())?)
        }
    }
}
