//! Mirrors `ralphus_cli::commands::cell::dispatch`. `cell open-agent` and
//! `cell remote-terminal` are excluded from the MCP tool surface (see
//! `crate::exclusions`) so their `CellCommand` arms are unreachable from a
//! real tool call, but are still handled (as a defensive error, not a
//! `panic!`/`unreachable!`) in case argv construction is ever wired up wrong.

use ralphus_cli::client::DaemonClient;
use ralphus_cli::commands::CommandError;
use ralphus_cli::commands::cell::{self, CellCommand};
use ralphus_cli::commands::env;
use ralphus_cli::selector::{SelectorError, squad_view_uri};
use serde_json::{Value, json};

use super::{ExecResult, usage};

pub fn execute(cmd: CellCommand, client: &DaemonClient) -> ExecResult {
    match cmd {
        CellCommand::Help | CellCommand::UsageError(_) => Err(usage("no such tool")),
        CellCommand::Env { selector, scope } => {
            let resolved = cell::resolve_scoped(client, &selector, "cell")?;
            Ok(client.env_view(&env::cell_path(&resolved, &scope))?)
        }
        CellCommand::Show { selector } => {
            let resolved = cell::resolve_scoped(client, &selector, "cell")?;
            let squad = client.squad(&resolved.squad_id)?;
            let uri = squad_view_uri(&squad, &resolved);
            Ok(cell::with_uri(
                squad["tasks"][resolved.task_idx as usize]["cells"][resolved.cell_idx as usize]
                    .clone(),
                uri,
            ))
        }
        CellCommand::Worktree { selector } => {
            let resolved = cell::resolve_scoped(client, &selector, "cell")?;
            let paths = client.squad_worktrees(&resolved.squad_id)?;
            let paths = paths.as_array().cloned().unwrap_or_default();
            paths
                .into_iter()
                .find(|p| {
                    p["task_idx"].as_i64() == Some(resolved.task_idx)
                        && p["cell_idx"].as_i64() == Some(resolved.cell_idx)
                })
                .ok_or_else(|| {
                    CommandError::Selector(SelectorError(format!(
                        "no worktree recorded for '{selector}'"
                    )))
                })
        }
        CellCommand::Reviews { selector } => {
            let resolved = cell::resolve_scoped(client, &selector, "cell")?;
            let squad = client.squad(&resolved.squad_id)?;
            let cell =
                &squad["tasks"][resolved.task_idx as usize]["cells"][resolved.cell_idx as usize];
            Ok(Value::Array(
                cell["reviews"].as_array().cloned().unwrap_or_default(),
            ))
        }
        CellCommand::SetStatus { selector, state } => {
            let resolved = cell::resolve_scoped(client, &selector, "cell")?;
            Ok(client.set_status(
                &resolved.squad_id,
                &state,
                "cell",
                resolved.task_idx,
                resolved.cell_idx,
                -1,
                "",
            )?)
        }
        CellCommand::Restart { selector } => {
            let resolved = cell::resolve_scoped(client, &selector, "cell")?;
            Ok(client.restart_cell(&resolved.squad_id, resolved.task_idx, resolved.cell_idx)?)
        }
        CellCommand::RestartProof { selector, from } => {
            let resolved = cell::resolve_scoped(client, &selector, "cell")?;
            Ok(client.restart_cell_proof(
                &resolved.squad_id,
                resolved.task_idx,
                resolved.cell_idx,
                from,
            )?)
        }
        CellCommand::Edit {
            selector,
            cwd,
            agent,
            model,
            prompt,
            command,
            auto_compact_threshold,
            maximum_tool_output_tokens,
            system_prompt,
        } => {
            let resolved = cell::resolve_scoped(client, &selector, "cell")?;
            Ok(client.edit_cell(
                &resolved.squad_id,
                resolved.task_idx,
                resolved.cell_idx,
                cwd.as_deref(),
                agent.as_deref(),
                model.as_deref(),
                prompt.as_deref(),
                command.as_deref(),
                auto_compact_threshold.as_deref(),
                maximum_tool_output_tokens.as_deref(),
                system_prompt.as_deref(),
            )?)
        }
        CellCommand::Terminal { selector, mode } => {
            let resolved = cell::resolve_scoped(client, &selector, "cell")?;
            let squad = client.squad(&resolved.squad_id)?;
            let cell_data = squad["tasks"][resolved.task_idx as usize]["cells"]
                [resolved.cell_idx as usize]
                .clone();
            let agent_session_id = cell_data["agent_session_id"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            if agent_session_id.is_empty() {
                return Err(CommandError::Selector(SelectorError(format!(
                    "no agent_session_id available for '{selector}' -- the cell may not have \
                     completed yet"
                ))));
            }
            let resume_cmd =
                cell::agent_resume_command(cell_data["agent"].as_str(), &agent_session_id, &mode);
            Ok(json!({
                "cwd": cell_data["cwd"],
                "command": resume_cmd.join(" "),
                "cell": cell_data,
            }))
        }
        CellCommand::OpenAgent { .. } => Err(usage(
            "cell open-agent is excluded from the MCP tool surface",
        )),
        CellCommand::RemoteTerminal { .. } => Err(usage(
            "cell remote-terminal is excluded from the MCP tool surface",
        )),
        CellCommand::ResumeAutomation { selector } => {
            let resolved = cell::resolve_scoped(client, &selector, "cell")?;
            Ok(client.resume_automation(
                &resolved.squad_id,
                resolved.task_idx,
                resolved.cell_idx,
            )?)
        }
    }
}
