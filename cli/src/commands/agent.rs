//! `ralphus agent <subcommand>`, ported from `cli/src/ralphus/__main__.py`'s
//! `agent` group and extended by RAL-460's `profile` subgroup. Bare
//! `ralphus agent` behaves like `agent list`
//! (`p_agent.set_defaults(func=_cmd_agent_list)` in the Python source,
//! mirroring bare `ralphus queue`/`ralphus machine`).
//!
//! `agent list` is purely informational and never touches the daemon --
//! built-in backends only now (RAL-460 moved agent profiles into the
//! daemon's own store, so there is no `.ralphus.toml` left to read them
//! from); this keeps `cli/tests/cli_integration.rs::agent_list_needs_no_daemon_at_all`
//! true. `agent profile ...` is the new daemon-backed CRUD surface for
//! managing those stored profiles -- mirrors `cli/src/commands/machine.rs`'s
//! register/list/get/remove shape.

use serde_json::Value;

use crate::args::GlobalOpts;
use crate::commands::CommandError;
use crate::flags::{Scanner, UsageError};

#[derive(Debug, Clone)]
pub enum AgentCommand {
    List,
    Help,
    Profile(ProfileCommand),
    UsageError(String),
}

/// `ralphus agent profile <subcommand>` -- daemon-backed CRUD over the
/// `agent_profiles` store (RAL-460). Bare `ralphus agent profile` behaves
/// like `profile list`, mirroring `agent`/`machine`/`queue`'s own bare-form
/// convention.
#[derive(Debug, Clone)]
pub enum ProfileCommand {
    List,
    Show {
        name: String,
    },
    Register {
        name: String,
        backend: String,
        executable: Option<String>,
        default_model: Option<String>,
        /// `(key, value)` literal env entries from repeated `--env KEY=VALUE`.
        env: Vec<(String, String)>,
        /// `(key, target_var)` link env entries from repeated
        /// `--env-link KEY=TARGET_VAR`.
        env_link: Vec<(String, String)>,
    },
    /// Changes a **locked** (built-in-backend) row's `executable` -- the
    /// only field such a row can have changed on it. A separate verb from
    /// `Register` rather than an overload, mirroring the daemon API's own
    /// split (`POST /api/agent-profiles` vs
    /// `POST /api/agent-profiles/{name}/executable`).
    SetExecutable {
        name: String,
        executable: String,
    },
    Remove {
        name: String,
    },
    Help,
    UsageError(String),
}

#[must_use]
pub fn parse(args: &[String]) -> AgentCommand {
    match args.first().map(String::as_str) {
        None | Some("list") => AgentCommand::List,
        Some("help" | "--help" | "-h") => AgentCommand::Help,
        Some("profile") => AgentCommand::Profile(parse_profile(&args[1..])),
        Some(other) => AgentCommand::UsageError(format!("unknown agent subcommand: {other}")),
    }
}

fn parse_profile(args: &[String]) -> ProfileCommand {
    let mut scanner = Scanner::new(&args[1.min(args.len())..]);
    match args.first().map(String::as_str) {
        None | Some("list") => ProfileCommand::List,
        Some("help" | "--help" | "-h") => ProfileCommand::Help,
        Some("show") => with_name(scanner, |name| ProfileCommand::Show { name }, "show"),
        Some("remove") => with_name(scanner, |name| ProfileCommand::Remove { name }, "remove"),
        Some("register") => match parse_register(&mut scanner) {
            Ok(cmd) => cmd,
            Err(e) => ProfileCommand::UsageError(e.0),
        },
        Some("set-executable") => match parse_set_executable(scanner) {
            Ok(cmd) => cmd,
            Err(e) => ProfileCommand::UsageError(e.0),
        },
        Some(other) => {
            ProfileCommand::UsageError(format!("unknown agent profile subcommand: {other}"))
        }
    }
}

fn with_name(
    scanner: Scanner,
    make: impl FnOnce(String) -> ProfileCommand,
    subcmd: &str,
) -> ProfileCommand {
    match scanner.remaining().into_iter().next() {
        Some(name) => make(name),
        None => ProfileCommand::UsageError(format!("{subcmd} requires a <name> argument")),
    }
}

/// Splits a repeated `--env`/`--env-link` value on its first `=` --
/// `KEY=VALUE`/`KEY=TARGET_VAR`. Rejects a missing `=` or an empty key
/// up front rather than sending a malformed pair to the daemon.
fn split_env_pair(raw: &str, flag: &str) -> Result<(String, String), UsageError> {
    let Some((key, value)) = raw.split_once('=') else {
        return Err(UsageError(format!("{flag} expects KEY=VALUE, got {raw:?}")));
    };
    if key.is_empty() {
        return Err(UsageError(format!(
            "{flag} expects a non-empty KEY before '=', got {raw:?}"
        )));
    }
    Ok((key.to_string(), value.to_string()))
}

fn parse_register(scanner: &mut Scanner) -> Result<ProfileCommand, UsageError> {
    let name = scanner.take_value("--name")?;
    let backend = scanner.take_value("--backend")?;
    let executable = scanner.take_value("--executable")?;
    let default_model = scanner.take_value("--default-model")?;
    let env = scanner
        .take_repeated("--env")?
        .iter()
        .map(|raw| split_env_pair(raw, "--env"))
        .collect::<Result<Vec<_>, _>>()?;
    let env_link = scanner
        .take_repeated("--env-link")?
        .iter()
        .map(|raw| split_env_pair(raw, "--env-link"))
        .collect::<Result<Vec<_>, _>>()?;
    let Some(name) = name else {
        return Err(UsageError(
            "agent profile register requires --name".to_string(),
        ));
    };
    let Some(backend) = backend else {
        return Err(UsageError(
            "agent profile register requires --backend".to_string(),
        ));
    };
    Ok(ProfileCommand::Register {
        name,
        backend,
        executable,
        default_model,
        env,
        env_link,
    })
}

fn parse_set_executable(mut scanner: Scanner) -> Result<ProfileCommand, UsageError> {
    let executable = scanner.take_value("--executable")?;
    let Some(name) = scanner.remaining().into_iter().next() else {
        return Err(UsageError(
            "agent profile set-executable requires a <name> argument".to_string(),
        ));
    };
    let Some(executable) = executable else {
        return Err(UsageError(
            "agent profile set-executable requires --executable".to_string(),
        ));
    };
    Ok(ProfileCommand::SetExecutable { name, executable })
}

#[must_use]
pub fn dispatch(cmd: AgentCommand, opts: &GlobalOpts) -> i32 {
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
        AgentCommand::Profile(cmd) => dispatch_profile(cmd, opts),
        AgentCommand::UsageError(m) => {
            println!("usage error: {m}");
            2
        }
    }
}

#[must_use]
fn dispatch_profile(cmd: ProfileCommand, opts: &GlobalOpts) -> i32 {
    let client = opts.client();
    match cmd {
        ProfileCommand::Help => {
            println!(
                "{}",
                crate::help_map::command_help(&["agent", "profile"]).expect("help exists")
            );
            0
        }
        ProfileCommand::UsageError(m) => {
            println!("usage error: {m}");
            2
        }
        ProfileCommand::List => match client.list_agent_profiles() {
            Ok(payload) => {
                render_profile_list(&payload);
                0
            }
            Err(e) => {
                CommandError::Daemon(e).print(false, None);
                2
            }
        },
        ProfileCommand::Show { name } => match client.get_agent_profile(&name) {
            Ok(p) => {
                render_profile_detail(&p);
                0
            }
            Err(e) => {
                CommandError::Daemon(e).print(false, None);
                2
            }
        },
        ProfileCommand::Register {
            name,
            backend,
            executable,
            default_model,
            env,
            env_link,
        } => match client.register_agent_profile(
            &name,
            &backend,
            executable.as_deref(),
            default_model.as_deref(),
            &env,
            &env_link,
        ) {
            Ok(_) => {
                println!("registered agent profile \"{name}\" -> backend {backend}");
                0
            }
            Err(e) => {
                CommandError::Daemon(e).print(false, None);
                1
            }
        },
        ProfileCommand::SetExecutable { name, executable } => {
            match client.set_agent_profile_executable(&name, &executable) {
                Ok(_) => {
                    println!("agent profile \"{name}\" executable -> {executable}");
                    0
                }
                Err(e) => {
                    CommandError::Daemon(e).print(false, None);
                    1
                }
            }
        }
        ProfileCommand::Remove { name } => match client.deregister_agent_profile(&name) {
            Ok(_) => {
                println!("removed agent profile \"{name}\"");
                0
            }
            Err(e) => {
                CommandError::Daemon(e).print(false, None);
                1
            }
        },
    }
}

fn render_profile_list(payload: &Value) {
    let profiles = payload["profiles"].as_array().cloned().unwrap_or_default();
    if profiles.is_empty() {
        println!("no agent profiles registered");
        return;
    }
    for p in &profiles {
        let locked = p["locked"].as_bool().unwrap_or(false);
        let name = p["name"].as_str().unwrap_or_default();
        let backend = p["backend"].as_str().unwrap_or_default();
        let flag = if locked { " (built-in, read-only)" } else { "" };
        println!("{name:<24} backend: {backend}{flag}");
        if let Some(model) = p["default_model"].as_str() {
            println!("    default model: {model}");
        }
        if let Some(exe) = p["executable"].as_str() {
            println!("    executable: {exe}");
        }
    }
    if let Some(backends) = payload["available_backends"].as_array() {
        let names: Vec<&str> = backends.iter().filter_map(Value::as_str).collect();
        println!();
        println!("available backends: {}", names.join(", "));
    }
}

fn render_profile_detail(p: &Value) {
    println!("name:       {}", p["name"].as_str().unwrap_or_default());
    println!("backend:    {}", p["backend"].as_str().unwrap_or_default());
    println!("locked:     {}", p["locked"].as_bool().unwrap_or(false));
    if let Some(exe) = p["executable"].as_str() {
        println!("executable: {exe}");
    }
    if let Some(model) = p["default_model"].as_str() {
        println!("model:      {model}");
    }
    let env = p["env"].as_array().cloned().unwrap_or_default();
    if !env.is_empty() {
        println!("env:");
        for e in &env {
            let key = e["key"].as_str().unwrap_or_default();
            let kind = e["kind"].as_str().unwrap_or_default();
            let value = e["value"].as_str().unwrap_or_default();
            println!("    {key} ({kind}) = {value}");
        }
    }
}

#[must_use]
pub fn build_agent_rows() -> Vec<&'static crate::agents::AgentInfo> {
    crate::agents::KNOWN_AGENTS.iter().collect()
}

/// Ports Python's `_cmd_agent_list`: a hand-maintained, purely informational
/// catalog -- ralphus itself does not enforce a model allow-list for any
/// agent; a fixed model list here reflects what the underlying CLI/API
/// actually accepts, not a ralphus-side validation rule. Built-in backends
/// only now (RAL-460) -- use `ralphus agent profile list` for stored custom
/// profiles.
fn render_agent_list() {
    for a in build_agent_rows() {
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
    println!();
    println!(
        "Custom agent profiles are managed with `ralphus agent profile ...` (requires a daemon)."
    );
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

    #[test]
    fn bare_agent_profile_behaves_like_list() {
        match parse(&v(&["profile"])) {
            AgentCommand::Profile(ProfileCommand::List) => {}
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parses_profile_show_and_remove() {
        match parse(&v(&["profile", "show", "my-profile"])) {
            AgentCommand::Profile(ProfileCommand::Show { name }) => assert_eq!(name, "my-profile"),
            other => panic!("unexpected: {other:?}"),
        }
        match parse(&v(&["profile", "remove", "my-profile"])) {
            AgentCommand::Profile(ProfileCommand::Remove { name }) => {
                assert_eq!(name, "my-profile");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn profile_show_and_remove_require_a_name() {
        matches!(
            parse(&v(&["profile", "show"])),
            AgentCommand::Profile(ProfileCommand::UsageError(_))
        );
        matches!(
            parse(&v(&["profile", "remove"])),
            AgentCommand::Profile(ProfileCommand::UsageError(_))
        );
    }

    #[test]
    fn parses_profile_register_with_required_flags() {
        match parse(&v(&[
            "profile",
            "register",
            "--name",
            "my-openrouter",
            "--backend",
            "claude-code",
        ])) {
            AgentCommand::Profile(ProfileCommand::Register {
                name,
                backend,
                executable,
                default_model,
                env,
                env_link,
            }) => {
                assert_eq!(name, "my-openrouter");
                assert_eq!(backend, "claude-code");
                assert_eq!(executable, None);
                assert_eq!(default_model, None);
                assert!(env.is_empty());
                assert!(env_link.is_empty());
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parses_profile_register_with_repeated_env_and_env_link() {
        match parse(&v(&[
            "profile",
            "register",
            "--name",
            "my-openrouter",
            "--backend",
            "pi",
            "--executable",
            "/usr/bin/pi",
            "--default-model",
            "openrouter/glm",
            "--env",
            "FEATURE_FLAG=enabled",
            "--env-link",
            "OPENROUTER_API_KEY=MY_TOKEN_VAR",
        ])) {
            AgentCommand::Profile(ProfileCommand::Register {
                executable,
                default_model,
                env,
                env_link,
                ..
            }) => {
                assert_eq!(executable, Some("/usr/bin/pi".to_string()));
                assert_eq!(default_model, Some("openrouter/glm".to_string()));
                assert_eq!(
                    env,
                    vec![("FEATURE_FLAG".to_string(), "enabled".to_string())]
                );
                assert_eq!(
                    env_link,
                    vec![("OPENROUTER_API_KEY".to_string(), "MY_TOKEN_VAR".to_string())]
                );
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn register_requires_name_and_backend() {
        matches!(
            parse(&v(&["profile", "register", "--name", "x"])),
            AgentCommand::Profile(ProfileCommand::UsageError(_))
        );
        matches!(
            parse(&v(&["profile", "register", "--backend", "pi"])),
            AgentCommand::Profile(ProfileCommand::UsageError(_))
        );
    }

    #[test]
    fn register_rejects_a_malformed_env_pair() {
        matches!(
            parse(&v(&[
                "profile",
                "register",
                "--name",
                "x",
                "--backend",
                "pi",
                "--env",
                "NOEQUALSSIGN",
            ])),
            AgentCommand::Profile(ProfileCommand::UsageError(_))
        );
    }

    #[test]
    fn parses_set_executable() {
        match parse(&v(&[
            "profile",
            "set-executable",
            "claude-code",
            "--executable",
            "my-claude-fork",
        ])) {
            AgentCommand::Profile(ProfileCommand::SetExecutable { name, executable }) => {
                assert_eq!(name, "claude-code");
                assert_eq!(executable, "my-claude-fork");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn set_executable_requires_name_and_flag() {
        matches!(
            parse(&v(&["profile", "set-executable", "--executable", "x"])),
            AgentCommand::Profile(ProfileCommand::UsageError(_))
        );
        matches!(
            parse(&v(&["profile", "set-executable", "claude-code"])),
            AgentCommand::Profile(ProfileCommand::UsageError(_))
        );
    }

    #[test]
    fn unknown_profile_subcommand_is_usage_error() {
        matches!(
            parse(&v(&["profile", "bogus"])),
            AgentCommand::Profile(ProfileCommand::UsageError(_))
        );
    }
}
