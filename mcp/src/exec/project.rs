//! Mirrors `ralphus_cli::commands::project::dispatch`.

use ralphus_cli::client::{DaemonClient, ProjectReviewSettingsPatch};
use ralphus_cli::commands::project::{
    ProjectCommand, ProjectForkCommand, ProjectReviewSettingsCommand,
};

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
        ProjectCommand::Remove { name } => Ok(client.remove_project(&name)?),
        ProjectCommand::Fork(cmd) => exec_fork(cmd, client),
        ProjectCommand::ReviewSettings(cmd) => exec_review_settings(cmd, client),
    }
}

fn exec_review_settings(cmd: ProjectReviewSettingsCommand, client: &DaemonClient) -> ExecResult {
    match cmd {
        ProjectReviewSettingsCommand::Help | ProjectReviewSettingsCommand::UsageError(_) => {
            Err(usage("no such tool"))
        }
        ProjectReviewSettingsCommand::Get { name } => {
            Ok(client.get_project_review_settings(&name)?)
        }
        ProjectReviewSettingsCommand::Set {
            name,
            resolver_agent,
            resolver_model,
            machine,
            maximum_budget_usd,
            clear_maximum_budget_usd,
            base_shift_maximum_rebuilds,
            clear_base_shift_maximum_rebuilds,
            proof_scope,
            skip_auto_clean,
            skip_worktrees,
            skip_base_updates,
            match_pr_branch_name,
            separate_pr_branch,
            dual_root_pr,
            auto_build,
            auto_submit_pr_stack,
            auto_fix_pr_errors,
            auto_fix_prompt_template,
            discourage_tests_during_auto_pull_request_fixes,
        } => {
            let patch = ProjectReviewSettingsPatch {
                default_resolver_agent: resolver_agent.as_deref(),
                default_resolver_model: resolver_model.as_deref(),
                default_machine: machine.as_deref(),
                default_maximum_budget_usd: maximum_budget_usd,
                clear_maximum_budget_usd,
                base_shift_maximum_rebuilds,
                clear_base_shift_maximum_rebuilds,
                default_proof_scope: proof_scope.as_deref(),
                verify_skip_auto_clean: skip_auto_clean,
                skip_worktrees,
                skip_base_updates,
                match_pr_branch_name,
                separate_pr_branch,
                dual_root_pr,
                auto_build: auto_build.as_deref(),
                auto_submit_pr_stack,
                auto_fix_pr_errors,
                auto_fix_prompt_template: auto_fix_prompt_template.as_deref(),
                discourage_tests_during_auto_pull_request_fixes,
            };
            Ok(client.set_project_review_settings(&name, &patch)?)
        }
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
