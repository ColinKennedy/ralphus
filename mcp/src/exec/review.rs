//! Mirrors `ralphus_cli::commands::review::dispatch` and its five nested
//! subgroup dispatchers (`base`/`pr`/`branch`/`checks`/`action`) -- by far
//! the widest command tree in the CLI, see that module's own doc comment.
//!
//! `review branch terminal` / `review checks terminal` print a local resume
//! command instead of calling the daemon's terminal-spawning endpoint,
//! exactly like `exec::cell`'s `CellCommand::Terminal` -- see that module.

use std::collections::BTreeMap;

use ralphus_cli::client::{DaemonClient, GuardianSettings};
use ralphus_cli::commands::CommandError;
use ralphus_cli::commands::env;
use ralphus_cli::commands::review::{
    self, GuardianEnvArgs, ReviewActionCommand, ReviewBranchCommand, ReviewChecksCommand,
    ReviewCommand, ReviewPrCommand, ReviewUpstreamCommand,
};
use ralphus_cli::commands::task::with_uri;
use ralphus_cli::selector::{
    DEFAULT_REVIEW_LIST_HINT, ResolvedGuardianSelector, SelectorError, guardian_view_uri,
    resolve_guardian_selector,
};
use serde_json::{Value, json};

use super::{ExecResult, usage};

#[derive(Clone, Copy)]
enum GuardianEnvSection {
    Build,
    ManualChecks,
}

pub fn execute(cmd: ReviewCommand, client: &DaemonClient) -> ExecResult {
    match cmd {
        ReviewCommand::Help | ReviewCommand::UsageError(_) => Err(usage("no such tool")),
        ReviewCommand::List { status, pr_ready } => {
            let guardians = client.guardian_list()?;
            let mut list = guardians.as_array().cloned().unwrap_or_default();
            if let Some(status) = &status {
                let wanted: Vec<String> = status
                    .split(',')
                    .map(|s| s.trim().to_lowercase())
                    .filter(|s| !s.is_empty())
                    .collect();
                list.retain(|g| {
                    wanted.contains(&g["status"].as_str().unwrap_or_default().to_lowercase())
                });
            }
            if pr_ready {
                list.retain(review::is_pr_ready);
            }
            Ok(Value::Array(list))
        }
        ReviewCommand::Show { selector } => {
            let resolved = resolve_guardian_selector(client, &selector, DEFAULT_REVIEW_LIST_HINT)?;
            let guardian = client.guardian_get(&resolved.guardian_id)?;
            let uri = guardian_view_uri(&guardian, Some(&resolved));
            Ok(with_uri(guardian, uri))
        }
        ReviewCommand::Logs { selector } => {
            let resolved = resolve_guardian_selector(client, &selector, DEFAULT_REVIEW_LIST_HINT)?;
            Ok(client.guardian_logs(&resolved.guardian_id)?)
        }
        ReviewCommand::Status { selector } => {
            let resolved = resolve_guardian_selector(client, &selector, DEFAULT_REVIEW_LIST_HINT)?;
            Ok(client.guardian_get(&resolved.guardian_id)?)
        }
        ReviewCommand::Worktrees { selector } => {
            let resolved = resolve_guardian_selector(client, &selector, DEFAULT_REVIEW_LIST_HINT)?;
            Ok(client.guardian_get(&resolved.guardian_id)?)
        }
        ReviewCommand::Create {
            name,
            base_branch,
            git_root,
            checks,
            skip_auto_build,
            skip_worktrees,
            review_type,
        } => {
            let checks_vec: Vec<String> = checks
                .as_deref()
                .unwrap_or("")
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect();
            Ok(client.guardian_create(
                &name,
                &base_branch,
                &git_root,
                Some(&checks_vec),
                skip_auto_build,
                skip_worktrees,
                review_type.as_deref(),
            )?)
        }
        ReviewCommand::Rename { selector, name } => {
            let resolved = resolve_guardian_selector(client, &selector, DEFAULT_REVIEW_LIST_HINT)?;
            Ok(client.guardian_rename(&resolved.guardian_id, &name)?)
        }
        ReviewCommand::Cancel { selector } => {
            let resolved = resolve_guardian_selector(client, &selector, DEFAULT_REVIEW_LIST_HINT)?;
            Ok(client.guardian_cancel(&resolved.guardian_id)?)
        }
        ReviewCommand::Reopen { selector } => {
            let resolved = resolve_guardian_selector(client, &selector, DEFAULT_REVIEW_LIST_HINT)?;
            Ok(client.guardian_reopen(&resolved.guardian_id)?)
        }
        ReviewCommand::Delete { selector, yes } => {
            if !yes {
                return Err(usage(
                    "delete is destructive and requires yes=true (there is no interactive \
                     confirmation prompt over MCP)",
                ));
            }
            let resolved = resolve_guardian_selector(client, &selector, DEFAULT_REVIEW_LIST_HINT)?;
            Ok(client.guardian_delete(&resolved.guardian_id)?)
        }
        ReviewCommand::Settings {
            selector,
            skip_auto_build,
            skip_worktrees,
            resolver_agent,
            resolver_model,
            base_branch,
            auto_pr_feedback,
            proof_scope,
            skip_auto_clean,
            skip_base_updates,
            match_pr_branch_name,
        } => {
            let resolved = resolve_guardian_selector(client, &selector, DEFAULT_REVIEW_LIST_HINT)?;
            let settings = GuardianSettings {
                skip_auto_build,
                skip_worktrees,
                resolver_agent: resolver_agent.as_deref(),
                resolver_model: resolver_model.as_deref(),
                base_branch: base_branch.as_deref(),
                auto_pr_feedback,
                proof_scope: proof_scope.as_deref(),
                proof_skip_auto_clean: skip_auto_clean,
                skip_base_updates,
                match_pr_branch_name,
            };
            Ok(client.guardian_settings(&resolved.guardian_id, &settings)?)
        }
        ReviewCommand::Env { selector, scope } => {
            let resolved = resolve_guardian_selector(client, &selector, DEFAULT_REVIEW_LIST_HINT)?;
            let scope = scope.unwrap_or_else(|| env::default_review_scope(&resolved));
            Ok(client.env_view(&env::review_path(&resolved, &scope, &selector)?)?)
        }
        ReviewCommand::BuildEnv(args) => exec_guardian_env(client, args, GuardianEnvSection::Build),
        ReviewCommand::ManualChecksEnv(args) => {
            exec_guardian_env(client, args, GuardianEnvSection::ManualChecks)
        }
        ReviewCommand::Squash {
            selector,
            project,
            enabled,
        } => {
            let resolved = resolve_guardian_selector(client, &selector, DEFAULT_REVIEW_LIST_HINT)?;
            Ok(client.guardian_squash(&resolved.guardian_id, &project, enabled)?)
        }
        ReviewCommand::AddBranch { selector, branch } => {
            let resolved = resolve_guardian_selector(client, &selector, DEFAULT_REVIEW_LIST_HINT)?;
            Ok(client.guardian_add_branch(&resolved.guardian_id, &branch)?)
        }
        ReviewCommand::Reorder {
            selector,
            order,
            disable,
            enable,
        } => {
            let order_vec: Vec<String> = order
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect();
            let mut enabled_map = serde_json::Map::new();
            for b in disable.as_deref().unwrap_or("").split(',') {
                let b = b.trim();
                if !b.is_empty() {
                    enabled_map.insert(b.to_string(), Value::Bool(false));
                }
            }
            for b in enable.as_deref().unwrap_or("").split(',') {
                let b = b.trim();
                if !b.is_empty() {
                    enabled_map.insert(b.to_string(), Value::Bool(true));
                }
            }
            let resolved = resolve_guardian_selector(client, &selector, DEFAULT_REVIEW_LIST_HINT)?;
            Ok(client.guardian_arrange(
                &resolved.guardian_id,
                &order_vec,
                Some(&Value::Object(enabled_map)),
            )?)
        }
        ReviewCommand::Merge { selector } => {
            let resolved = resolve_guardian_selector(client, &selector, DEFAULT_REVIEW_LIST_HINT)?;
            Ok(client.guardian_merge(&resolved.guardian_id)?)
        }
        ReviewCommand::SyncPr { selector } => {
            let resolved = resolve_guardian_selector(client, &selector, DEFAULT_REVIEW_LIST_HINT)?;
            Ok(client.guardian_sync_pr(&resolved.guardian_id)?)
        }
        ReviewCommand::RestartMerge { selector } => {
            let resolved = resolve_guardian_selector(client, &selector, DEFAULT_REVIEW_LIST_HINT)?;
            Ok(client.guardian_cancel_and_merge(&resolved.guardian_id)?)
        }
        ReviewCommand::StopMerge { selector } => {
            let resolved = resolve_guardian_selector(client, &selector, DEFAULT_REVIEW_LIST_HINT)?;
            Ok(client.guardian_stop(&resolved.guardian_id)?)
        }
        ReviewCommand::ForceStart { selector } => {
            let resolved = resolve_guardian_selector(client, &selector, DEFAULT_REVIEW_LIST_HINT)?;
            Ok(client.guardian_force_start(&resolved.guardian_id)?)
        }
        ReviewCommand::Approve { selector } => {
            let resolved = resolve_guardian_selector(client, &selector, DEFAULT_REVIEW_LIST_HINT)?;
            Ok(client.guardian_approve(&resolved.guardian_id)?)
        }
        ReviewCommand::Feedback { selector, text } => {
            let resolved = resolve_branch(client, &selector)?;
            Ok(client.guardian_feedback(
                &resolved.guardian_id,
                resolved.branch_id.as_deref().unwrap_or_default(),
                &text,
            )?)
        }
        ReviewCommand::DismissReenable { selector } => {
            let resolved = resolve_branch(client, &selector)?;
            Ok(client.guardian_dismiss_reenable(
                &resolved.guardian_id,
                resolved.branch_id.as_deref().unwrap_or_default(),
            )?)
        }
        ReviewCommand::MoveBranch {
            selector,
            to_review,
        } => {
            let resolved = resolve_branch(client, &selector)?;
            let to_resolved =
                resolve_guardian_selector(client, &to_review, DEFAULT_REVIEW_LIST_HINT)?;
            Ok(client.guardian_move_branch(
                &resolved.guardian_id,
                resolved.branch_id.as_deref().unwrap_or_default(),
                &to_resolved.guardian_id,
            )?)
        }
        ReviewCommand::Upstream(c) => exec_upstream(c, client),
        ReviewCommand::Pr(c) => exec_pr(c, client),
        ReviewCommand::Branch(c) => exec_branch(c, client),
        ReviewCommand::Checks(c) => exec_checks(c, client),
        ReviewCommand::Action(c) => exec_action(c, client),
    }
}

/// Mirrors `review::resolve_branch`, but returns the error instead of
/// printing it and a hardcoded exit code (MCP has no print-and-exit path).
fn resolve_branch(
    client: &DaemonClient,
    selector: &str,
) -> Result<ResolvedGuardianSelector, CommandError> {
    let resolved = resolve_guardian_selector(client, selector, DEFAULT_REVIEW_LIST_HINT)?;
    if resolved.branch_id.is_none() {
        return Err(CommandError::Selector(SelectorError(format!(
            "'{selector}' does not name a branch (use guardian#branch)"
        ))));
    }
    Ok(resolved)
}

fn exec_guardian_env(
    client: &DaemonClient,
    args: GuardianEnvArgs,
    section: GuardianEnvSection,
) -> ExecResult {
    let GuardianEnvArgs {
        selector,
        set,
        unset,
        clear,
    } = args;
    let set_map = review::parse_environment_flags(&set, "set").map_err(usage)?;
    if set_map.is_empty() && unset.is_empty() && clear.is_empty() {
        return Err(usage("at least one of set/unset/clear is required"));
    }
    let resolved = resolve_guardian_selector(client, &selector, DEFAULT_REVIEW_LIST_HINT)?;
    let set_value = Value::Object(
        set_map
            .iter()
            .map(|(k, v)| (k.clone(), Value::String(v.clone())))
            .collect(),
    );
    let clear_opt = (!clear.is_empty()).then_some(clear.as_slice());
    let result = match section {
        GuardianEnvSection::Build => client.set_guardian_build_env(
            &resolved.guardian_id,
            Some(&set_value),
            Some(&unset),
            clear_opt,
        )?,
        GuardianEnvSection::ManualChecks => client.set_guardian_manual_checks_env(
            &resolved.guardian_id,
            Some(&set_value),
            Some(&unset),
            clear_opt,
        )?,
    };
    Ok(result)
}

fn exec_upstream(cmd: ReviewUpstreamCommand, client: &DaemonClient) -> ExecResult {
    match cmd {
        ReviewUpstreamCommand::Help | ReviewUpstreamCommand::UsageError(_) => {
            Err(usage("no such tool"))
        }
        ReviewUpstreamCommand::List { selector } => {
            let resolved = resolve_guardian_selector(client, &selector, DEFAULT_REVIEW_LIST_HINT)?;
            Ok(client.guardian_base_branches(&resolved.guardian_id)?)
        }
        ReviewUpstreamCommand::Set { selector, branch } => {
            let resolved = resolve_guardian_selector(client, &selector, DEFAULT_REVIEW_LIST_HINT)?;
            Ok(client.guardian_change_base(&resolved.guardian_id, &branch)?)
        }
    }
}

fn exec_pr(cmd: ReviewPrCommand, client: &DaemonClient) -> ExecResult {
    match cmd {
        ReviewPrCommand::Help | ReviewPrCommand::UsageError(_) => Err(usage("no such tool")),
        ReviewPrCommand::Submit {
            selector,
            position,
            combined,
            alias,
            title,
            description,
            use_worktree_branch_name,
        } => {
            let mut pr_spec = serde_json::Map::new();
            if let Some(alias) = &alias {
                pr_spec.insert("branch_alias".to_string(), Value::String(alias.clone()));
            }
            if let Some(title) = &title {
                pr_spec.insert("title".to_string(), Value::String(title.clone()));
            }
            if let Some(description) = &description {
                pr_spec.insert(
                    "description".to_string(),
                    Value::String(description.clone()),
                );
            }
            if let Some(use_worktree_branch_name) = use_worktree_branch_name {
                pr_spec.insert(
                    "use_worktree_branch_name".to_string(),
                    Value::Bool(use_worktree_branch_name),
                );
            }
            let resolved =
                resolve_guardian_selector(client, &selector, "ralphus review list --pr-ready")?;
            if !combined {
                let position = position.unwrap_or_default();
                let guardian = client.guardian_get(&resolved.guardian_id)?;
                let branches = guardian["branches"].as_array().cloned().unwrap_or_default();
                let found = branches
                    .iter()
                    .find(|b| b["position"].as_i64() == Some(position))
                    .ok_or_else(|| {
                        CommandError::Selector(SelectorError(format!(
                            "no branch at position {position} in this review"
                        )))
                    })?;
                pr_spec.insert("branch_id".to_string(), found["id"].clone());
            }
            Ok(client.guardian_submit_prs(&resolved.guardian_id, &[Value::Object(pr_spec)])?)
        }
        ReviewPrCommand::List { selector } => {
            let resolved = resolve_guardian_selector(client, &selector, DEFAULT_REVIEW_LIST_HINT)?;
            Ok(client.guardian_list_prs(&resolved.guardian_id)?)
        }
        ReviewPrCommand::Show { pr_id } => Ok(client.pr_get(&pr_id)?),
        ReviewPrCommand::Find {
            forge,
            repo,
            pr_number,
        } => Ok(client.pr_find(&forge, &repo, pr_number)?),
        ReviewPrCommand::Update {
            pr_id,
            pr_number,
            pr_url,
            branch_alias,
            state,
        } => Ok(client.pr_update(
            &pr_id,
            pr_number,
            pr_url.as_deref(),
            branch_alias.as_deref(),
            state.as_deref(),
        )?),
        ReviewPrCommand::Comments { pr_id } => Ok(client.pr_comments(&pr_id)?),
        ReviewPrCommand::PullFeedback { pr_id } => Ok(client.pr_action_feedback(&pr_id)?),
        ReviewPrCommand::PullFromPr { pr_id } => Ok(client.pr_pull_from_pr(&pr_id)?),
        ReviewPrCommand::Unlink { selector } => {
            let resolved = resolve_guardian_selector(client, &selector, DEFAULT_REVIEW_LIST_HINT)?;
            Ok(client.guardian_unlink_prs(&resolved.guardian_id)?)
        }
    }
}

fn exec_branch(cmd: ReviewBranchCommand, client: &DaemonClient) -> ExecResult {
    match cmd {
        ReviewBranchCommand::Help | ReviewBranchCommand::UsageError(_) => {
            Err(usage("no such tool"))
        }
        ReviewBranchCommand::Enable { selector } => {
            exec_branch_set_enabled(client, &selector, true)
        }
        ReviewBranchCommand::Disable { selector } => {
            exec_branch_set_enabled(client, &selector, false)
        }
        ReviewBranchCommand::Terminal { selector, mode } => {
            exec_branch_terminal(client, &selector, &mode)
        }
    }
}

fn exec_branch_set_enabled(client: &DaemonClient, selector: &str, enabled: bool) -> ExecResult {
    let resolved = resolve_branch(client, selector)?;
    let guardian = client.guardian_get(&resolved.guardian_id)?;
    let order: Vec<String> = guardian["branches"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|b| b["branch"].as_str().map(str::to_string))
        .collect();
    let mut enabled_map = serde_json::Map::new();
    if let Some(branch) = &resolved.branch {
        enabled_map.insert(branch.clone(), Value::Bool(enabled));
    }
    Ok(client.guardian_arrange(
        &resolved.guardian_id,
        &order,
        Some(&Value::Object(enabled_map)),
    )?)
}

fn exec_branch_terminal(client: &DaemonClient, selector: &str, mode: &str) -> ExecResult {
    let resolved = resolve_branch(client, selector)?;
    let guardian = client.guardian_get(&resolved.guardian_id)?;
    let branch = guardian["branches"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|b| b["id"].as_str() == resolved.branch_id.as_deref())
        .ok_or_else(|| {
            CommandError::Selector(SelectorError(format!(
                "no branch '{selector}' in this review"
            )))
        })?
        .clone();
    let agent_session_id = branch["resolver_agent_session_id"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    if agent_session_id.is_empty() {
        return Err(CommandError::Selector(SelectorError(format!(
            "no resolver_agent_session_id available for '{selector}' -- conflict resolution may \
             not have run yet"
        ))));
    }
    let cmd =
        review::agent_resume_command(guardian["resolver_agent"].as_str(), &agent_session_id, mode);
    let cwd = branch["worktree"]
        .as_str()
        .filter(|s| !s.is_empty())
        .unwrap_or("-")
        .to_string();
    Ok(json!({"cwd": cwd, "command": cmd.join(" "), "branch": branch}))
}

fn exec_checks(cmd: ReviewChecksCommand, client: &DaemonClient) -> ExecResult {
    match cmd {
        ReviewChecksCommand::Help | ReviewChecksCommand::UsageError(_) => {
            Err(usage("no such tool"))
        }
        ReviewChecksCommand::List { selector } => {
            let resolved = resolve_guardian_selector(client, &selector, DEFAULT_REVIEW_LIST_HINT)?;
            let guardian = client.guardian_get(&resolved.guardian_id)?;
            Ok(guardian["manual_commands"].clone())
        }
        ReviewChecksCommand::Run {
            selector,
            index,
            all,
            input,
        } => exec_checks_run(client, &selector, &index, all, &input),
        ReviewChecksCommand::Terminal { selector, mode } => {
            exec_checks_terminal(client, &selector, &mode)
        }
    }
}

fn exec_checks_run(
    client: &DaemonClient,
    selector: &str,
    index: &[i64],
    all: bool,
    input: &[(String, String)],
) -> ExecResult {
    let resolved = resolve_guardian_selector(client, selector, DEFAULT_REVIEW_LIST_HINT)?;
    let guardian = client.guardian_get(&resolved.guardian_id)?;
    let commands = guardian["manual_commands"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    if commands.is_empty() {
        return Err(usage(
            "no manual checks available (review may still be building)",
        ));
    }
    let indices: Vec<usize> = if all || index.is_empty() {
        (0..commands.len()).collect()
    } else {
        let bad: Vec<i64> = index
            .iter()
            .copied()
            .filter(|&i| i < 0 || i as usize >= commands.len())
            .collect();
        if !bad.is_empty() {
            return Err(usage(format!(
                "check index out of range: {bad:?} (have {})",
                commands.len()
            )));
        }
        index.iter().map(|&i| i as usize).collect()
    };
    let cwd = guardian["combined_worktree"]
        .as_str()
        .filter(|s| !s.is_empty())
        .or_else(|| guardian["git_root"].as_str())
        .unwrap_or_default()
        .to_string();
    let input_values = &guardian["input_values"];
    let mut resolved_commands: BTreeMap<usize, String> = BTreeMap::new();
    let mut all_missing: Vec<(usize, Vec<String>)> = Vec::new();
    for &i in &indices {
        let command = commands[i]["command"].as_str().unwrap_or_default();
        let (resolved_cmd, missing) =
            review::resolve_check_inputs(command, &commands[i], input_values, input);
        resolved_commands.insert(i, resolved_cmd);
        if !missing.is_empty() {
            all_missing.push((i, missing));
        }
    }
    if !all_missing.is_empty() {
        let detail = all_missing
            .iter()
            .map(|(i, names)| format!("[{i}] needs {names:?}"))
            .collect::<Vec<_>>()
            .join("; ");
        return Err(usage(format!(
            "missing required input value(s): {detail} (supply via input NAME=VALUE)"
        )));
    }
    let env_keys: Vec<String> = {
        let mut keys: Vec<String> = guardian["manual_checks_env"]
            .as_object()
            .into_iter()
            .flatten()
            .map(|(k, _)| k.clone())
            .collect();
        keys.sort();
        keys
    };
    Ok(json!({
        "cwd": cwd,
        "results": indices.iter().map(|&i| json!({
            "index": i, "command": resolved_commands[&i], "env_keys": env_keys,
        })).collect::<Vec<_>>(),
    }))
}

fn exec_checks_terminal(client: &DaemonClient, selector: &str, mode: &str) -> ExecResult {
    let resolved = resolve_guardian_selector(client, selector, DEFAULT_REVIEW_LIST_HINT)?;
    let guardian = client.guardian_get(&resolved.guardian_id)?;
    let agent_session_id = guardian["manual_commands_agent_session_id"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    if agent_session_id.is_empty() {
        return Err(usage(format!(
            "no manual_commands_agent_session_id available for '{selector}' -- manual-checks \
             generation may not have run yet"
        )));
    }
    let cwd = guardian["combined_worktree"]
        .as_str()
        .filter(|s| !s.is_empty())
        .or_else(|| guardian["git_root"].as_str())
        .unwrap_or("-")
        .to_string();
    let cmd = review::agent_resume_command(
        guardian["manual_commands_agent"].as_str(),
        &agent_session_id,
        mode,
    );
    Ok(json!({"cwd": cwd, "command": cmd.join(" "), "guardian_id": resolved.guardian_id}))
}

fn exec_action(cmd: ReviewActionCommand, client: &DaemonClient) -> ExecResult {
    match cmd {
        ReviewActionCommand::Help | ReviewActionCommand::UsageError(_) => {
            Err(usage("no such tool"))
        }
        ReviewActionCommand::List { selector } => {
            let resolved = resolve_guardian_selector(client, &selector, DEFAULT_REVIEW_LIST_HINT)?;
            let guardian = client.guardian_get(&resolved.guardian_id)?;
            Ok(guardian["action_hints"].clone())
        }
        ReviewActionCommand::Run {
            selector,
            index,
            input,
        } => exec_action_run(client, &selector, index, &input),
    }
}

fn exec_action_run(
    client: &DaemonClient,
    selector: &str,
    index: i64,
    input: &[(String, String)],
) -> ExecResult {
    let resolved = resolve_guardian_selector(client, selector, DEFAULT_REVIEW_LIST_HINT)?;
    let guardian = client.guardian_get(&resolved.guardian_id)?;
    let hints = guardian["action_hints"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    if index < 0 || index as usize >= hints.len() {
        return Err(usage(format!(
            "action hint index {index} out of range (have {})",
            hints.len()
        )));
    }
    let hint = &hints[index as usize];
    let command = hint["command"].as_str().unwrap_or_default();
    if command.is_empty() {
        return Err(usage("prompt-kind action hints cannot be run directly yet"));
    }
    let cwd = guardian["combined_worktree"]
        .as_str()
        .filter(|s| !s.is_empty())
        .or_else(|| guardian["git_root"].as_str())
        .unwrap_or_default()
        .to_string();
    let input_values = &guardian["input_values"];
    let (resolved_command, missing) =
        review::resolve_check_inputs(command, hint, input_values, input);
    if !missing.is_empty() {
        return Err(usage(format!(
            "missing required input value(s): {missing:?} (supply via input NAME=VALUE)"
        )));
    }
    let mut hint_with_command = hint.clone();
    if let Value::Object(map) = &mut hint_with_command {
        map.insert(
            "command".to_string(),
            Value::String(resolved_command.clone()),
        );
    }
    Ok(json!({"cwd": cwd, "hint": hint_with_command}))
}
