//! Mirrors `ralphus_cli::commands::proof::dispatch`.

use ralphus_cli::client::DaemonClient;
use ralphus_cli::commands::env;
use ralphus_cli::commands::proof::{self, ProofCommand};
use ralphus_cli::selector::squad_view_uri;

use super::{ExecResult, usage};

pub fn execute(cmd: ProofCommand, client: &DaemonClient) -> ExecResult {
    match cmd {
        ProofCommand::Help | ProofCommand::UsageError(_) => Err(usage("no such tool")),
        ProofCommand::Env { selector } => {
            let resolved = proof::resolve_scoped(client, &selector, "proof")?;
            Ok(client.env_view(&env::proof_path(&resolved))?)
        }
        ProofCommand::Show { selector } => {
            let resolved = proof::resolve_scoped(client, &selector, "proof")?;
            let squad = client.squad(&resolved.squad_id)?;
            let uri = squad_view_uri(&squad, &resolved);
            Ok(proof::with_uri(
                proof::proof_step_for(&squad, &resolved),
                uri,
            ))
        }
        ProofCommand::SetStatus { selector, state } => {
            let resolved = proof::resolve_scoped(client, &selector, "proof")?;
            Ok(client.set_status(
                &resolved.squad_id,
                &state,
                "proof",
                resolved.task_idx,
                resolved.cell_idx,
                resolved.proof_idx,
                &resolved.proof_scope,
            )?)
        }
        ProofCommand::Restart { selector } => {
            let resolved = proof::resolve_scoped(client, &selector, "proof")?;
            if resolved.proof_scope == "cell" {
                Ok(client.restart_cell_proof(
                    &resolved.squad_id,
                    resolved.task_idx,
                    resolved.cell_idx,
                    resolved.proof_idx,
                )?)
            } else {
                Ok(client.restart_task_proof(
                    &resolved.squad_id,
                    resolved.task_idx,
                    resolved.proof_idx,
                )?)
            }
        }
        ProofCommand::Edit {
            selector,
            model,
            maximum_tool_output_tokens,
        } => {
            let resolved = proof::resolve_scoped(client, &selector, "proof")?;
            Ok(client.edit_proof(
                &resolved.squad_id,
                resolved.task_idx,
                &resolved.proof_scope,
                resolved.cell_idx,
                resolved.proof_idx,
                model.as_deref(),
                maximum_tool_output_tokens.as_deref(),
            )?)
        }
    }
}
