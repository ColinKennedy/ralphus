//! Mirrors `ralphus_cli::commands::preset::dispatch`.

use ralphus_cli::client::DaemonClient;
use ralphus_cli::commands::preset::PresetCommand;

use super::{ExecResult, usage};

pub fn execute(cmd: PresetCommand, client: &DaemonClient) -> ExecResult {
    match cmd {
        PresetCommand::Help | PresetCommand::UsageError(_) => Err(usage("no such tool")),
        PresetCommand::Register {
            name,
            system_prompt,
            system_prompt_position,
            maximum_context,
            auto_compact_threshold,
            maximum_tool_output_tokens,
        } => Ok(client.register_preset(
            &name,
            system_prompt.as_deref(),
            system_prompt_position.as_deref(),
            maximum_context,
            auto_compact_threshold,
            maximum_tool_output_tokens,
        )?),
        PresetCommand::List => Ok(client.list_presets()?),
        PresetCommand::Get { name } => Ok(client.get_preset(&name)?),
        PresetCommand::Deregister { name } => Ok(client.deregister_preset(&name)?),
    }
}
