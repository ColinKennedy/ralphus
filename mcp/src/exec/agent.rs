//! Mirrors `ralphus_cli::commands::agent::dispatch` -- the `agent list` leaf
//! never touches the daemon (reads `.ralphus.toml` directly), hence no
//! `DaemonClient` use there, but `agent profile ...`/`agent backend-command
//! ...` (RAL-473) are daemon-backed, administrative DB-profile management.

use ralphus_cli::agents::KNOWN_AGENTS;
use ralphus_cli::client::DaemonClient;
use ralphus_cli::commands::agent::{
    AgentBackendCommandCommand, AgentCapabilityCommand, AgentCommand, AgentProfileCommand,
    AgentProfileSummary, load_agent_profile_summaries,
};
use serde_json::json;

use super::{ExecResult, usage};

pub fn execute(cmd: AgentCommand, client: &DaemonClient) -> ExecResult {
    match cmd {
        AgentCommand::Help | AgentCommand::UsageError(_) => Err(usage("no such tool")),
        AgentCommand::List => {
            let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
            let builtins: Vec<_> = KNOWN_AGENTS
                .iter()
                .map(|a| {
                    json!({
                        "name": a.name,
                        "aliases": a.aliases,
                        "models": a.models,
                        "default_model": a.default_model,
                        "description": a.description,
                    })
                })
                .collect();
            let profiles: Vec<AgentProfileSummary> = load_agent_profile_summaries(&cwd);
            let profiles: Vec<_> = profiles
                .iter()
                .map(|p| json!({"name": p.name, "backend": p.backend}))
                .collect();
            Ok(json!({"builtin": builtins, "profiles": profiles}))
        }
        AgentCommand::Profile(c) => execute_profile(c, client),
        AgentCommand::BackendCommand(c) => execute_backend_command(c, client),
        AgentCommand::Capability(c) => execute_capability(c, client),
    }
}

fn execute_profile(cmd: AgentProfileCommand, client: &DaemonClient) -> ExecResult {
    match cmd {
        AgentProfileCommand::Help | AgentProfileCommand::UsageError(_) => {
            Err(usage("no such tool"))
        }
        AgentProfileCommand::List => Ok(client.list_agent_profiles()?),
        AgentProfileCommand::Get { name } => Ok(client.get_agent_profile(&name)?),
        AgentProfileCommand::Create {
            name,
            backend,
            executable,
            model,
            env,
            thinking_capable,
        } => Ok(client.create_agent_profile(
            &name,
            &backend,
            executable.as_deref(),
            model.as_deref(),
            &env,
            thinking_capable,
        )?),
        AgentProfileCommand::Update {
            name,
            backend,
            executable,
            model,
            env,
            thinking_capable,
        } => Ok(client.update_agent_profile(
            &name,
            &backend,
            executable.as_deref(),
            model.as_deref(),
            &env,
            thinking_capable,
        )?),
        AgentProfileCommand::Delete { name, force } => {
            Ok(client.delete_agent_profile(&name, force)?)
        }
    }
}

fn execute_backend_command(cmd: AgentBackendCommandCommand, client: &DaemonClient) -> ExecResult {
    match cmd {
        AgentBackendCommandCommand::Help | AgentBackendCommandCommand::UsageError(_) => {
            Err(usage("no such tool"))
        }
        AgentBackendCommandCommand::List => Ok(client.list_agent_backend_commands()?),
        AgentBackendCommandCommand::Set { backend, command } => {
            Ok(client.set_agent_backend_command(&backend, &command)?)
        }
        AgentBackendCommandCommand::Reset { backend } => {
            Ok(client.reset_agent_backend_command(&backend)?)
        }
        AgentBackendCommandCommand::Profiles { backend } => {
            Ok(client.list_agent_profiles_for_backend(&backend)?)
        }
    }
}

fn execute_capability(cmd: AgentCapabilityCommand, client: &DaemonClient) -> ExecResult {
    use AgentCapabilityCommand as C;
    match cmd {
        C::Help | C::UsageError(_) => Err(usage("no such tool")),
        C::List => Ok(client.list_capabilities()?),
        C::Save {
            kind,
            name,
            body,
            replace,
        } => Ok(client.save_capability(&kind, &name, body, replace)?),
        C::Remove { kind, name } => Ok(client.remove_capability(&kind, &name)?),
        C::Check { kind, name } => Ok(client.check_capability(&kind, &name)?),
        C::Import => Ok(client.import_capabilities()?),
        C::Profile { profile } => Ok(client.get_profile_capabilities(&profile)?),
        C::Toggle {
            profile,
            kind,
            name,
            enabled,
        } => Ok(client.set_profile_capability(&profile, &kind, &name, enabled)?),
        C::Delivery {
            profile,
            mode,
            args,
        } => Ok(client.set_profile_capability_delivery(&profile, &mode, &args)?),
        C::ToolPolicy {
            profile,
            server,
            tool,
            allowed,
        } => Ok(client.set_profile_tool_policy(&profile, &server, &tool, allowed)?),
    }
}
