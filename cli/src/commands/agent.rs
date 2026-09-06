//! `ralphus agent <subcommand>`, ported from `cli/src/ralphus/__main__.py`'s
//! `agent` group. There is only one real subcommand (`list`); bare
//! `ralphus agent` behaves the same way
//! (`p_agent.set_defaults(func=_cmd_agent_list)` in the Python source,
//! mirroring bare `ralphus queue`/`ralphus machine`). Purely informational --
//! merges the static registry in `crate::agents` with the current project's
//! `[agent.profiles.*]` entries (RAL-270), interleaved alphabetically by
//! name. Reads `.ralphus.toml` directly (like `crate::config::load_config`)
//! rather than the daemon's `GET /api/agents` -- unlike that endpoint, this
//! command never resolves `env`/`executable`, so it can't fail just because
//! a profile's `from_env` variable isn't set in the CLI's own shell, and
//! (per `cli/tests/cli_integration.rs::agent_list_needs_no_daemon_at_all`)
//! it must keep working with no daemon reachable at all.

use std::path::{Path, PathBuf};

use crate::args::GlobalOpts;

/// One `[agent.profiles.<name>]` entry, just enough of it to list alongside
/// a built-in backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentProfileSummary {
    pub name: String,
    pub backend: String,
}

/// Reads `[agent.profiles.*]` from the current project's `.ralphus.toml`,
/// found by walking up from `cwd` the same way
/// `ralphus_daemon::agent_profiles` resolves it at submit/run time. Returns
/// an empty list -- never an error -- when there is no project config, the
/// file fails to parse, or an entry is missing `backend`: this command is a
/// discoverability aid, not a validator (`ralphus validate`/`ralphus submit`
/// already enforce the real schema and resolve `env`).
#[must_use]
pub fn load_agent_profile_summaries(cwd: &Path) -> Vec<AgentProfileSummary> {
    let Some(path) = ralphus_daemon::config::find_project_config(cwd) else {
        return Vec::new();
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    let Ok(raw) = text.parse::<toml::Table>() else {
        return Vec::new();
    };
    let Some(profiles) = raw
        .get("agent")
        .and_then(toml::Value::as_table)
        .and_then(|agent| agent.get("profiles"))
        .and_then(toml::Value::as_table)
    else {
        return Vec::new();
    };
    let mut out: Vec<AgentProfileSummary> = profiles
        .iter()
        .filter_map(|(name, value)| {
            let backend = value.as_table()?.get("backend")?.as_str()?;
            Some(AgentProfileSummary {
                name: name.clone(),
                backend: backend.to_string(),
            })
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

#[derive(Debug, Clone)]
enum AgentRow {
    Builtin(&'static crate::agents::AgentInfo),
    Profile(AgentProfileSummary),
}

impl AgentRow {
    fn name(&self) -> &str {
        match self {
            AgentRow::Builtin(a) => a.name,
            AgentRow::Profile(p) => &p.name,
        }
    }
}

/// Built-in backends and the current project's custom agent profiles,
/// interleaved into one list sorted alphabetically by name -- not grouped
/// into separate built-in/custom sections (RAL-270).
fn build_agent_rows(cwd: &Path) -> Vec<AgentRow> {
    let mut rows: Vec<AgentRow> = crate::agents::KNOWN_AGENTS
        .iter()
        .map(AgentRow::Builtin)
        .collect();
    rows.extend(
        load_agent_profile_summaries(cwd)
            .into_iter()
            .map(AgentRow::Profile),
    );
    rows.sort_by(|a, b| a.name().cmp(b.name()));
    rows
}

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
/// actually accepts, not a ralphus-side validation rule. Merged with the
/// current project's custom `[agent.profiles.*]` entries, if any (RAL-270).
fn render_agent_list() {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    for row in build_agent_rows(&cwd) {
        match row {
            AgentRow::Builtin(a) => {
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
            AgentRow::Profile(p) => {
                println!("{:<24} backend: {}", p.name, p.backend);
                println!("    Custom agent profile from .ralphus.toml.");
            }
        }
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

    fn tempdir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ralphus-cli-agent-list-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("time")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("create tempdir");
        dir
    }

    #[test]
    fn load_agent_profile_summaries_empty_with_no_project_config() {
        let cwd = tempdir("no-config");
        assert!(load_agent_profile_summaries(&cwd).is_empty());
        std::fs::remove_dir_all(&cwd).ok();
    }

    #[test]
    fn load_agent_profile_summaries_reads_profiles_sorted_by_name() {
        let cwd = tempdir("with-profiles");
        std::fs::write(
            cwd.join(".ralphus.toml"),
            r#"
[agent.profiles.openrouter-deepseek]
backend = "claude-code"

[agent.profiles.abacus]
backend = "raw"
executable = "abacus-runner"
"#,
        )
        .expect("write project config");

        let summaries = load_agent_profile_summaries(&cwd);
        assert_eq!(
            summaries,
            vec![
                AgentProfileSummary {
                    name: "abacus".to_string(),
                    backend: "raw".to_string(),
                },
                AgentProfileSummary {
                    name: "openrouter-deepseek".to_string(),
                    backend: "claude-code".to_string(),
                },
            ]
        );
        std::fs::remove_dir_all(&cwd).ok();
    }

    #[test]
    fn load_agent_profile_summaries_skips_entries_missing_backend() {
        let cwd = tempdir("missing-backend");
        std::fs::write(
            cwd.join(".ralphus.toml"),
            "[agent.profiles.broken]\nexecutable = \"x\"\n",
        )
        .expect("write project config");

        assert!(load_agent_profile_summaries(&cwd).is_empty());
        std::fs::remove_dir_all(&cwd).ok();
    }

    #[test]
    fn build_agent_rows_interleaves_profiles_with_builtins_alphabetically() {
        let cwd = tempdir("interleaved");
        std::fs::write(
            cwd.join(".ralphus.toml"),
            r#"
[agent.profiles.abacus]
backend = "raw"
executable = "abacus-runner"

[agent.profiles.zzz-custom]
backend = "codex"
"#,
        )
        .expect("write project config");

        let rows = build_agent_rows(&cwd);
        let names: Vec<&str> = rows.iter().map(AgentRow::name).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(names, sorted, "rows must be sorted alphabetically by name");
        assert!(names.contains(&"abacus"));
        assert!(names.contains(&"zzz-custom"));
        assert!(names.contains(&"claude-code"));
        std::fs::remove_dir_all(&cwd).ok();
    }
}
