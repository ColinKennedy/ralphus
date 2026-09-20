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

use serde_json::Value;

use crate::args::GlobalOpts;
use crate::commands::CommandError;
use crate::flags::Scanner;

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
    /// `agent profile ...` (RAL-473): daemon-backed agent profile CRUD.
    /// Administrative -- most users only need `ralphus agent list`.
    Profile(AgentProfileCommand),
    /// `agent backend-command ...` (RAL-473, Q4): the global command
    /// override for a built-in backend (`claude-code`/`codex`/`pi`).
    /// Administrative -- editing it affects every profile using that
    /// backend.
    BackendCommand(AgentBackendCommandCommand),
    UsageError(String),
}

#[must_use]
pub fn parse(args: &[String]) -> AgentCommand {
    let scanner = Scanner::new(&args[1.min(args.len())..]);
    match args.first().map(String::as_str) {
        None | Some("list") => AgentCommand::List,
        Some("help" | "--help" | "-h") => AgentCommand::Help,
        Some("profile") => AgentCommand::Profile(parse_profile(&scanner.remaining())),
        Some("backend-command") => {
            AgentCommand::BackendCommand(parse_backend_command(&scanner.remaining()))
        }
        Some(other) => AgentCommand::UsageError(format!("unknown agent subcommand: {other}")),
    }
}

/// One `--set KEY=VALUE` or `--link KEY=TARGET` flag occurrence, parsed into
/// the `(key, kind, value)` triple `DaemonClient::create_agent_profile`/
/// `update_agent_profile` expect. `kind` is `"set"`/`"link"`, matching
/// `AgentEnvKind`'s `serde(rename_all = "snake_case")`.
fn parse_env_pairs(
    raw: Vec<String>,
    kind: &str,
    flag: &str,
) -> Result<Vec<(String, String, String)>, String> {
    raw.into_iter()
        .map(|entry| match entry.split_once('=') {
            Some((key, value)) if !key.is_empty() => {
                Ok((key.to_string(), kind.to_string(), value.to_string()))
            }
            _ => Err(format!("{flag} expects KEY=VALUE, got '{entry}'")),
        })
        .collect()
}

#[derive(Debug, Clone)]
pub enum AgentProfileCommand {
    Help,
    List,
    Get {
        name: String,
    },
    Create {
        name: String,
        backend: String,
        executable: Option<String>,
        model: Option<String>,
        env: Vec<(String, String, String)>,
    },
    Update {
        name: String,
        backend: String,
        executable: Option<String>,
        model: Option<String>,
        env: Vec<(String, String, String)>,
    },
    Delete {
        name: String,
        force: bool,
    },
    UsageError(String),
}

#[derive(Debug, Clone)]
pub enum AgentBackendCommandCommand {
    Help,
    List,
    Set {
        backend: String,
        command: String,
    },
    Reset {
        backend: String,
    },
    /// Blast-radius listing (Q4): which profiles use `backend`.
    Profiles {
        backend: String,
    },
    UsageError(String),
}

fn parse_profile(args: &[String]) -> AgentProfileCommand {
    let mut scanner = Scanner::new(&args[1.min(args.len())..]);
    match args.first().map(String::as_str) {
        None | Some("help" | "--help" | "-h") => AgentProfileCommand::Help,
        Some("list") => AgentProfileCommand::List,
        Some("get") => match scanner.remaining().into_iter().next() {
            Some(name) => AgentProfileCommand::Get { name },
            None => AgentProfileCommand::UsageError("get requires a <name> argument".to_string()),
        },
        Some("create") => parse_profile_save(&scanner.remaining(), false),
        Some("update") => parse_profile_save(&scanner.remaining(), true),
        Some("delete") => {
            let force = scanner.take_bool("--force");
            match scanner.remaining().into_iter().next() {
                Some(name) => AgentProfileCommand::Delete { name, force },
                None => {
                    AgentProfileCommand::UsageError("delete requires a <name> argument".to_string())
                }
            }
        }
        Some(other) => {
            AgentProfileCommand::UsageError(format!("unknown agent profile subcommand: {other}"))
        }
    }
}

fn parse_profile_save(args: &[String], is_update: bool) -> AgentProfileCommand {
    let mut scanner = Scanner::new(args);
    let backend = match scanner.take_value("--backend") {
        Ok(v) => v,
        Err(e) => return AgentProfileCommand::UsageError(e.0),
    };
    let executable = match scanner.take_value("--executable") {
        Ok(v) => v,
        Err(e) => return AgentProfileCommand::UsageError(e.0),
    };
    let model = match scanner.take_value("--model") {
        Ok(v) => v,
        Err(e) => return AgentProfileCommand::UsageError(e.0),
    };
    let sets = match scanner.take_repeated("--set") {
        Ok(v) => v,
        Err(e) => return AgentProfileCommand::UsageError(e.0),
    };
    let links = match scanner.take_repeated("--link") {
        Ok(v) => v,
        Err(e) => return AgentProfileCommand::UsageError(e.0),
    };
    let mut env = match parse_env_pairs(sets, "set", "--set") {
        Ok(v) => v,
        Err(e) => return AgentProfileCommand::UsageError(e),
    };
    match parse_env_pairs(links, "link", "--link") {
        Ok(v) => env.extend(v),
        Err(e) => return AgentProfileCommand::UsageError(e),
    }
    let Some(backend) = backend else {
        return AgentProfileCommand::UsageError(
            "create/update requires --backend <name>".to_string(),
        );
    };
    let Some(name) = scanner.remaining().into_iter().next() else {
        return AgentProfileCommand::UsageError("requires a <name> argument".to_string());
    };
    if is_update {
        AgentProfileCommand::Update {
            name,
            backend,
            executable,
            model,
            env,
        }
    } else {
        AgentProfileCommand::Create {
            name,
            backend,
            executable,
            model,
            env,
        }
    }
}

fn parse_backend_command(args: &[String]) -> AgentBackendCommandCommand {
    let mut scanner = Scanner::new(&args[1.min(args.len())..]);
    match args.first().map(String::as_str) {
        None | Some("help" | "--help" | "-h") => AgentBackendCommandCommand::Help,
        Some("list") => AgentBackendCommandCommand::List,
        Some("set") => {
            let command = match scanner.take_value("--command") {
                Ok(v) => v,
                Err(e) => return AgentBackendCommandCommand::UsageError(e.0),
            };
            let Some(command) = command else {
                return AgentBackendCommandCommand::UsageError(
                    "set requires --command \"<argv>\"".to_string(),
                );
            };
            match scanner.remaining().into_iter().next() {
                Some(backend) => AgentBackendCommandCommand::Set { backend, command },
                None => AgentBackendCommandCommand::UsageError(
                    "set requires a <backend> argument".to_string(),
                ),
            }
        }
        Some("reset") => match scanner.remaining().into_iter().next() {
            Some(backend) => AgentBackendCommandCommand::Reset { backend },
            None => AgentBackendCommandCommand::UsageError(
                "reset requires a <backend> argument".to_string(),
            ),
        },
        Some("profiles") => match scanner.remaining().into_iter().next() {
            Some(backend) => AgentBackendCommandCommand::Profiles { backend },
            None => AgentBackendCommandCommand::UsageError(
                "profiles requires a <backend> argument".to_string(),
            ),
        },
        Some(other) => AgentBackendCommandCommand::UsageError(format!(
            "unknown agent backend-command subcommand: {other}"
        )),
    }
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
        AgentCommand::Profile(c) => dispatch_profile(c, opts),
        AgentCommand::BackendCommand(c) => dispatch_backend_command(c, opts),
        AgentCommand::UsageError(m) => {
            println!("usage error: {m}");
            2
        }
    }
}

fn dispatch_profile(cmd: AgentProfileCommand, opts: &GlobalOpts) -> i32 {
    let client = opts.client();
    match cmd {
        AgentProfileCommand::Help => {
            println!(
                "{}",
                crate::help_map::command_help(&["agent", "profile"])
                    .expect("agent profile help exists")
            );
            0
        }
        AgentProfileCommand::UsageError(m) => {
            println!("usage error: {m}");
            2
        }
        AgentProfileCommand::List => match client.list_agent_profiles() {
            Ok(payload) => {
                render_agent_profile_list(&payload);
                0
            }
            Err(e) => {
                CommandError::Daemon(e).print(false, None);
                2
            }
        },
        AgentProfileCommand::Get { name } => match client.get_agent_profile(&name) {
            Ok(profile) => {
                render_agent_profile_detail(&profile);
                0
            }
            Err(e) => {
                CommandError::Daemon(e).print(false, None);
                2
            }
        },
        AgentProfileCommand::Create {
            name,
            backend,
            executable,
            model,
            env,
        } => match client.create_agent_profile(
            &name,
            &backend,
            executable.as_deref(),
            model.as_deref(),
            &env,
        ) {
            Ok(_) => {
                println!("created agent profile \"{name}\"");
                0
            }
            Err(e) => {
                CommandError::Daemon(e).print(false, None);
                1
            }
        },
        AgentProfileCommand::Update {
            name,
            backend,
            executable,
            model,
            env,
        } => match client.update_agent_profile(
            &name,
            &backend,
            executable.as_deref(),
            model.as_deref(),
            &env,
        ) {
            Ok(_) => {
                println!("updated agent profile \"{name}\"");
                0
            }
            Err(e) => {
                CommandError::Daemon(e).print(false, None);
                1
            }
        },
        AgentProfileCommand::Delete { name, force } => {
            match client.delete_agent_profile(&name, force) {
                Ok(_) => {
                    println!("removed agent profile \"{name}\"");
                    0
                }
                Err(e) => {
                    // Q9: a 409 here means the profile is still referenced by
                    // a stored squad/review; the message names `--force` as
                    // the way past it, though (a disclosed CLI-side gap) it
                    // does not enumerate the referencing IDs the way the web
                    // board's confirmation dialog will.
                    CommandError::Daemon(e).print(false, None);
                    1
                }
            }
        }
    }
}

fn dispatch_backend_command(cmd: AgentBackendCommandCommand, opts: &GlobalOpts) -> i32 {
    let client = opts.client();
    match cmd {
        AgentBackendCommandCommand::Help => {
            println!(
                "{}",
                crate::help_map::command_help(&["agent", "backend-command"])
                    .expect("agent backend-command help exists")
            );
            0
        }
        AgentBackendCommandCommand::UsageError(m) => {
            println!("usage error: {m}");
            2
        }
        AgentBackendCommandCommand::List => match client.list_agent_backend_commands() {
            Ok(payload) => {
                render_agent_backend_command_list(&payload);
                0
            }
            Err(e) => {
                CommandError::Daemon(e).print(false, None);
                2
            }
        },
        AgentBackendCommandCommand::Set { backend, command } => {
            match client.set_agent_backend_command(&backend, &command) {
                Ok(_) => {
                    println!(
                        "set \"{backend}\"'s command -- every profile using this backend picks it up on its next cell/proof run"
                    );
                    0
                }
                Err(e) => {
                    CommandError::Daemon(e).print(false, None);
                    1
                }
            }
        }
        AgentBackendCommandCommand::Reset { backend } => {
            match client.reset_agent_backend_command(&backend) {
                Ok(_) => {
                    println!("reset \"{backend}\" to its default command");
                    0
                }
                Err(e) => {
                    CommandError::Daemon(e).print(false, None);
                    1
                }
            }
        }
        AgentBackendCommandCommand::Profiles { backend } => {
            match client.list_agent_profiles_for_backend(&backend) {
                Ok(payload) => {
                    render_agent_profile_list(&payload);
                    0
                }
                Err(e) => {
                    CommandError::Daemon(e).print(false, None);
                    2
                }
            }
        }
    }
}

fn render_agent_profile_list(payload: &Value) {
    let profiles = payload["profiles"].as_array().cloned().unwrap_or_default();
    if profiles.is_empty() {
        println!("no agent profiles");
        return;
    }
    for p in &profiles {
        let mut label = format!(
            "{:<24} backend: {}",
            p["name"].as_str().unwrap_or_default(),
            p["backend"].as_str().unwrap_or_default()
        );
        if let Some(model) = p["model"].as_str() {
            label.push_str(&format!("  model: {model}"));
        }
        println!("{label}");
    }
}

/// Prints stored `Set`/`Link` rows exactly as the daemon returned them --
/// never a value resolved *through* a `Link` (Q7: resolved `Link` values are
/// never returned over the API at all).
fn render_agent_profile_detail(p: &Value) {
    println!("name:       {}", p["name"].as_str().unwrap_or_default());
    println!("backend:    {}", p["backend"].as_str().unwrap_or_default());
    if let Some(executable) = p["executable"].as_str() {
        println!("executable: {executable}");
    }
    if let Some(model) = p["model"].as_str() {
        println!("model:      {model}");
    }
    let env = p["env"].as_array().cloned().unwrap_or_default();
    if env.is_empty() {
        return;
    }
    println!("env:");
    for entry in &env {
        println!(
            "    {:<24} {:<6} {}",
            entry["key"].as_str().unwrap_or_default(),
            entry["kind"].as_str().unwrap_or_default(),
            entry["value"].as_str().unwrap_or_default()
        );
    }
}

fn render_agent_backend_command_list(payload: &Value) {
    let commands = payload["commands"].as_array().cloned().unwrap_or_default();
    if commands.is_empty() {
        println!("no backend command overrides (every backend runs its compiled-in default)");
        return;
    }
    for c in &commands {
        println!(
            "{:<16} {}",
            c["backend"].as_str().unwrap_or_default(),
            c["command"].as_str().unwrap_or_default()
        );
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

    #[test]
    fn parses_profile_list_and_get() {
        matches!(
            parse(&v(&["profile", "list"])),
            AgentCommand::Profile(AgentProfileCommand::List)
        );
        match parse(&v(&["profile", "get", "my-profile"])) {
            AgentCommand::Profile(AgentProfileCommand::Get { name }) => {
                assert_eq!(name, "my-profile");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn profile_get_requires_name() {
        matches!(
            parse(&v(&["profile", "get"])),
            AgentCommand::Profile(AgentProfileCommand::UsageError(_))
        );
    }

    #[test]
    fn parses_profile_create_with_sets_and_links() {
        match parse(&v(&[
            "profile",
            "create",
            "--backend",
            "claude-code",
            "--executable",
            "some-complex subcommand -- claude",
            "--model",
            "opus",
            "--set",
            "FOO=bar",
            "--link",
            "API_KEY=OTHER_KEY",
            "my-profile",
        ])) {
            AgentCommand::Profile(AgentProfileCommand::Create {
                name,
                backend,
                executable,
                model,
                env,
            }) => {
                assert_eq!(name, "my-profile");
                assert_eq!(backend, "claude-code");
                assert_eq!(
                    executable.as_deref(),
                    Some("some-complex subcommand -- claude")
                );
                assert_eq!(model.as_deref(), Some("opus"));
                assert_eq!(
                    env,
                    vec![
                        ("FOO".to_string(), "set".to_string(), "bar".to_string()),
                        (
                            "API_KEY".to_string(),
                            "link".to_string(),
                            "OTHER_KEY".to_string()
                        ),
                    ]
                );
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parses_profile_update() {
        match parse(&v(&[
            "profile",
            "update",
            "--backend",
            "codex",
            "my-profile",
        ])) {
            AgentCommand::Profile(AgentProfileCommand::Update { name, backend, .. }) => {
                assert_eq!(name, "my-profile");
                assert_eq!(backend, "codex");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn profile_create_requires_backend_and_name() {
        matches!(
            parse(&v(&["profile", "create", "my-profile"])),
            AgentCommand::Profile(AgentProfileCommand::UsageError(_))
        );
        matches!(
            parse(&v(&["profile", "create", "--backend", "codex"])),
            AgentCommand::Profile(AgentProfileCommand::UsageError(_))
        );
    }

    #[test]
    fn profile_create_rejects_malformed_env_entries() {
        matches!(
            parse(&v(&[
                "profile",
                "create",
                "--backend",
                "codex",
                "--set",
                "NOEQUALS",
                "my-profile"
            ])),
            AgentCommand::Profile(AgentProfileCommand::UsageError(_))
        );
    }

    #[test]
    fn parses_profile_delete_with_force() {
        match parse(&v(&["profile", "delete", "--force", "my-profile"])) {
            AgentCommand::Profile(AgentProfileCommand::Delete { name, force }) => {
                assert_eq!(name, "my-profile");
                assert!(force);
            }
            other => panic!("unexpected: {other:?}"),
        }
        match parse(&v(&["profile", "delete", "my-profile"])) {
            AgentCommand::Profile(AgentProfileCommand::Delete { name, force }) => {
                assert_eq!(name, "my-profile");
                assert!(!force);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn profile_delete_requires_name() {
        matches!(
            parse(&v(&["profile", "delete"])),
            AgentCommand::Profile(AgentProfileCommand::UsageError(_))
        );
    }

    #[test]
    fn unknown_profile_subcommand_is_usage_error() {
        matches!(
            parse(&v(&["profile", "bogus"])),
            AgentCommand::Profile(AgentProfileCommand::UsageError(_))
        );
    }

    #[test]
    fn bare_profile_is_help() {
        matches!(
            parse(&v(&["profile"])),
            AgentCommand::Profile(AgentProfileCommand::Help)
        );
    }

    #[test]
    fn parses_backend_command_list() {
        matches!(
            parse(&v(&["backend-command", "list"])),
            AgentCommand::BackendCommand(AgentBackendCommandCommand::List)
        );
    }

    #[test]
    fn parses_backend_command_set() {
        match parse(&v(&[
            "backend-command",
            "set",
            "--command",
            "some-complex subcommand -- claude",
            "claude-code",
        ])) {
            AgentCommand::BackendCommand(AgentBackendCommandCommand::Set { backend, command }) => {
                assert_eq!(backend, "claude-code");
                assert_eq!(command, "some-complex subcommand -- claude");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn backend_command_set_requires_command_and_backend() {
        matches!(
            parse(&v(&["backend-command", "set", "claude-code"])),
            AgentCommand::BackendCommand(AgentBackendCommandCommand::UsageError(_))
        );
        matches!(
            parse(&v(&["backend-command", "set", "--command", "x"])),
            AgentCommand::BackendCommand(AgentBackendCommandCommand::UsageError(_))
        );
    }

    #[test]
    fn parses_backend_command_reset_and_profiles() {
        match parse(&v(&["backend-command", "reset", "claude-code"])) {
            AgentCommand::BackendCommand(AgentBackendCommandCommand::Reset { backend }) => {
                assert_eq!(backend, "claude-code");
            }
            other => panic!("unexpected: {other:?}"),
        }
        match parse(&v(&["backend-command", "profiles", "claude-code"])) {
            AgentCommand::BackendCommand(AgentBackendCommandCommand::Profiles { backend }) => {
                assert_eq!(backend, "claude-code");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn backend_command_reset_and_profiles_require_backend() {
        matches!(
            parse(&v(&["backend-command", "reset"])),
            AgentCommand::BackendCommand(AgentBackendCommandCommand::UsageError(_))
        );
        matches!(
            parse(&v(&["backend-command", "profiles"])),
            AgentCommand::BackendCommand(AgentBackendCommandCommand::UsageError(_))
        );
    }

    #[test]
    fn unknown_backend_command_subcommand_is_usage_error() {
        matches!(
            parse(&v(&["backend-command", "bogus"])),
            AgentCommand::BackendCommand(AgentBackendCommandCommand::UsageError(_))
        );
    }
}
