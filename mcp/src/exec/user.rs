//! Mirrors `ralphus_cli::commands::user::dispatch`.

use ralphus_cli::client::DaemonClient;
use ralphus_cli::commands::user::UserCommand;

use super::{ExecResult, usage};

pub fn execute(cmd: UserCommand, client: &DaemonClient) -> ExecResult {
    match cmd {
        UserCommand::Help | UserCommand::UsageError(_) => Err(usage("no such tool")),
        UserCommand::SetForgeToken { user, host, token } => {
            Ok(client.set_user_forge_token(&user, &host, &token)?)
        }
        UserCommand::ListForgeTokens { user } => Ok(client.list_user_forge_tokens(&user)?),
        UserCommand::DeleteForgeToken { user, host } => {
            Ok(client.delete_user_forge_token(&user, &host)?)
        }
    }
}
