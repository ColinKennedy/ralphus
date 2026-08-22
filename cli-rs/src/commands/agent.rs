//! `ralphus agent <subcommand>`, ported from `cli/src/ralphus/__main__.py`'s
//! `agent` group. There is only one real subcommand (`list`); bare
//! `ralphus agent` behaves the same way
//! (`p_agent.set_defaults(func=_cmd_agent_list)` in the Python source,
//! mirroring bare `ralphus queue`/`ralphus machine`). Purely informational --
//! reads the static registry in `crate::agents`, never touches the daemon.

use crate::args::GlobalOpts;

#[derive(Debug, Clone)]
pub enum AgentCommand {
    List,
    Help,
    UsageError(String),
}

#[must_use]
pub fn parse(args: &[String]) -> AgentCommand {
    match args.first().map(String::as_str) {
        None | Some("list") => AgentCommand::List,
        Some("help" | "--help" | "-h") => AgentCommand::Help,
        Some(other) => AgentCommand::UsageError(format!("unknown agent subcommand: {other}")),
    }
}

#[must_use]
pub fn dispatch(cmd: AgentCommand, _opts: &GlobalOpts) -> i32 {
    match cmd {
        AgentCommand::List => {
            render_agent_list();
            0
        }
        AgentCommand::Help => {
            println!(
                "{}",
                crate::help_map::command_help(&["agent"]).expect("agent help exists")
            );
            0
        }
        AgentCommand::UsageError(m) => {
            println!("usage error: {m}");
            2
        }
    }
}

/// Ports Python's `_cmd_agent_list`: a hand-maintained, purely informational
/// catalog -- ralphus itself does not enforce a model allow-list for any
/// agent; a fixed model list here reflects what the underlying CLI/API
/// actually accepts, not a ralphus-side validation rule.
fn render_agent_list() {
    for a in crate::agents::KNOWN_AGENTS {
        let label = if a.aliases.is_empty() {
            a.name.to_string()
        } else {
            format!("{} ({})", a.name, a.aliases.join(", "))
        };
        let models = match a.models {
            None => {
                let mut s = "<any model>".to_string();
                if let Some(default_model) = a.default_model {
                    s.push_str(&format!(" (default: {default_model})"));
                }
                s
            }
            Some(models) => models.join(", "),
        };
        println!("{label:<24} {models}");
        println!("    {}", a.description);
    }
    println!();
    println!("{}", crate::agents::OTHER_AGENTS_NOTE);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn bare_agent_behaves_like_list() {
        matches!(parse(&[]), AgentCommand::List);
    }

    #[test]
    fn parses_list_explicitly() {
        matches!(parse(&v(&["list"])), AgentCommand::List);
    }

    #[test]
    fn parses_help() {
        matches!(parse(&v(&["help"])), AgentCommand::Help);
        matches!(parse(&v(&["--help"])), AgentCommand::Help);
    }

    #[test]
    fn unknown_subcommand_is_usage_error() {
        matches!(parse(&v(&["bogus"])), AgentCommand::UsageError(_));
    }
}
