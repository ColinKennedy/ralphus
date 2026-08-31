//! Mirrors `ralphus_cli::commands::agent::dispatch` -- the `agent list`
//! leaf never touches the daemon (reads `.ralphus.toml` directly), hence no
//! `DaemonClient` parameter.

use ralphus_cli::agents::KNOWN_AGENTS;
use ralphus_cli::commands::agent::{
    AgentCommand, AgentProfileSummary, load_agent_profile_summaries,
};
use serde_json::json;

use super::{ExecResult, usage};

pub fn execute(cmd: AgentCommand) -> ExecResult {
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
    }
}
