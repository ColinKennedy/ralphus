//! Mirrors `ralphus_cli::commands::project::dispatch`.

use ralphus_cli::client::DaemonClient;
use ralphus_cli::commands::project::{ProjectCommand, ProjectForkCommand};

use super::{ExecResult, usage};

pub fn execute(cmd: ProjectCommand, client: &DaemonClient) -> ExecResult {
    match cmd {
        ProjectCommand::Help | ProjectCommand::UsageError(_) => Err(usage("no such tool")),
        ProjectCommand::Git {
            path,
            clone_url,
            clear_url,
            name,
            description,
            match_pr_branch_name,
        } => {
            let target = ralphus_core::expand_home(&path);
            let target =
                ralphus_core::strip_verbatim_prefix(target.canonicalize().unwrap_or(target));
            let target_str = target.to_string_lossy().to_string();
            Ok(client.register_project(
                &name,
                &target_str,
                &description,
                "git",
                clone_url.as_deref(),
                clear_url,
                match_pr_branch_name,
            )?)
        }
        ProjectCommand::List { short: _ } => Ok(client.list_projects()?),
        ProjectCommand::Get { name } => Ok(client.get_project(&name)?),
        ProjectCommand::Fork(cmd) => exec_fork(cmd, client),
    }
}

fn exec_fork(cmd: ProjectForkCommand, client: &DaemonClient) -> ExecResult {
    match cmd {
        ProjectForkCommand::Help | ProjectForkCommand::UsageError(_) => Err(usage("no such tool")),
        ProjectForkCommand::Add {
            project,
            url,
            user,
            remote_name,
            owner,
        } => Ok(client.add_project_fork(
            &project,
            user.as_deref().unwrap_or(""),
            &url,
            remote_name.as_deref(),
            owner.as_deref(),
        )?),
        ProjectForkCommand::List { project, .. } => Ok(match &project {
            Some(p) => client.list_project_forks(p)?,
            None => client.list_all_project_forks()?,
        }),
        ProjectForkCommand::Set {
            project,
            user,
            url,
            remote_name,
            owner,
        } => Ok(client.set_project_fork(
            &project,
            user.as_deref().unwrap_or(""),
            url.as_deref(),
            remote_name.as_deref(),
            owner.as_deref(),
        )?),
        ProjectForkCommand::Remove { project, user } => {
            Ok(client.remove_project_fork(&project, user.as_deref().unwrap_or(""))?)
        }
    }
}
