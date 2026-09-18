//! Mirrors `ralphus_cli::commands::agent::dispatch`. `AgentCommand::List`
//! never touches the daemon (built-in backends only, RAL-460); the `Profile`
//! subgroup is the new daemon-backed CRUD surface and needs a `DaemonClient`.

use ralphus_cli::agents::KNOWN_AGENTS;
use ralphus_cli::client::DaemonClient;
use ralphus_cli::commands::agent::{AgentCommand, ProfileCommand};
use serde_json::json;

use super::{ExecResult, usage};

pub fn execute(cmd: AgentCommand, client: &DaemonClient) -> ExecResult {
    match cmd {
        AgentCommand::Help | AgentCommand::UsageError(_) => Err(usage("no such tool")),
        AgentCommand::List => {
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
            Ok(json!({"builtin": builtins}))
        }
        AgentCommand::Profile(cmd) => execute_profile(cmd, client),
    }
}

fn execute_profile(cmd: ProfileCommand, client: &DaemonClient) -> ExecResult {
    match cmd {
        ProfileCommand::Help | ProfileCommand::UsageError(_) => Err(usage("no such tool")),
        ProfileCommand::List => Ok(client.list_agent_profiles()?),
        ProfileCommand::Show { name } => Ok(client.get_agent_profile(&name)?),
        ProfileCommand::Register {
            name,
            backend,
            executable,
            default_model,
            env,
            env_link,
        } => Ok(client.register_agent_profile(
            &name,
            &backend,
            executable.as_deref(),
            default_model.as_deref(),
            &env,
            &env_link,
        )?),
        ProfileCommand::SetExecutable { name, executable } => {
            Ok(client.set_agent_profile_executable(&name, &executable)?)
        }
        ProfileCommand::Remove { name } => Ok(client.deregister_agent_profile(&name)?),
    }
}
