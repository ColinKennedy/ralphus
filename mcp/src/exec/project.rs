//! Mirrors `ralphus_cli::commands::project::dispatch`.

use ralphus_cli::client::DaemonClient;
use ralphus_cli::commands::project::ProjectCommand;

use super::{ExecResult, usage};

pub fn execute(cmd: ProjectCommand, client: &DaemonClient) -> ExecResult {
    match cmd {
        ProjectCommand::Help | ProjectCommand::UsageError(_) => Err(usage("no such tool")),
        ProjectCommand::Git {
            path,
            name,
            description,
        } => {
            let target = ralphus_core::expand_home(&path);
            let target =
                ralphus_core::strip_verbatim_prefix(target.canonicalize().unwrap_or(target));
            let target_str = target.to_string_lossy().to_string();
            Ok(client.register_project(&name, &target_str, &description, "git")?)
        }
        ProjectCommand::List { short: _ } => Ok(client.list_projects()?),
        ProjectCommand::Get { name } => Ok(client.get_project(&name)?),
    }
}
