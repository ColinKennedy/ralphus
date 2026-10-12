//! Mirrors `ralphus_cli::commands::forge::dispatch`: plain `DaemonClient`
//! calls whose `Value` is returned as-is.

use ralphus_cli::client::DaemonClient;
use ralphus_cli::commands::forge::ForgeCommand;
use serde_json::json;

use super::{ExecResult, usage};

pub fn execute(cmd: ForgeCommand, client: &DaemonClient) -> ExecResult {
    match cmd {
        ForgeCommand::PoolList => Ok(client.list_forge_pool()?),
        ForgeCommand::PoolAdd {
            kind,
            host,
            label,
            token,
        } => Ok(client.add_forge_pool_token(&kind, &host, &label, &token)?),
        ForgeCommand::PoolRemove { kind, host, label } => {
            client.remove_forge_pool_token(&kind, &host, &label)?;
            Ok(json!({"removed": true}))
        }
        ForgeCommand::Help | ForgeCommand::UsageError(_) => Err(usage("no such tool")),
    }
}
