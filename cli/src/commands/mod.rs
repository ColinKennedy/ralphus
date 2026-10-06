//! The `ralphus` CLI's command tree, ported from
//! `cli/src/ralphus/__main__.py`. Follows `daemon/src/lib.rs`'s existing
//! hand-rolled `Command` enum + `parse_args` convention (no `clap` anywhere
//! in this workspace) scaled up to ~105 leaf subcommands, organized into one
//! submodule per top-level command group.
//!
//! Every leaf command is a full 1:1 functional port of its Python handler --
//! same client call(s), same selector resolution, same rendering, same exit
//! codes. `author` (the agentic TOML-authoring loop) was deliberately not
//! ported and has no command here at all -- a decision, not a stub.

#![allow(clippy::print_stdout)] // This module's stdout IS the CLI's product.

pub mod agent;
pub mod cell;
pub mod env;
pub mod initialize;
pub mod internal;
pub mod machine;
pub mod mailbox;
pub mod mcp;
pub mod misc;
pub mod preset;
pub mod project;
pub mod proof;
pub mod prophecy;
pub mod queue;
pub mod quick_start;
pub mod review;
pub mod show;
pub mod squad;
pub mod task;
pub mod triage;
pub mod user;
pub mod waypoint;

use serde_json::Value;

use crate::args::GlobalOpts;
use crate::client::DaemonError;
use crate::flags::{Scanner, UsageError};
use crate::selector::SelectorError;

/// Exit-code convention (also printed by `ralphus --help`):
/// 0 ok, 1 domain error, 2 usage/local error, 3 not found, 4 conflict.
#[derive(Debug)]
pub enum CommandError {
    Daemon(DaemonError),
    Selector(SelectorError),
    Usage(String),
}

impl From<DaemonError> for CommandError {
    fn from(e: DaemonError) -> Self {
        Self::Daemon(e)
    }
}

impl From<SelectorError> for CommandError {
    fn from(e: SelectorError) -> Self {
        Self::Selector(e)
    }
}

impl From<UsageError> for CommandError {
    fn from(e: UsageError) -> Self {
        Self::Usage(e.0)
    }
}

impl CommandError {
    #[must_use]
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::Daemon(e) => crate::output::exit_code_for(e),
            Self::Selector(_) | Self::Usage(_) => 2,
        }
    }

    /// Prints this error, mirroring `_print_daemon_error`/
    /// `_print_selector_error`: a JSON envelope in `--json` mode, else a
    /// human line. A connection-level `DaemonError` (no status code) gets a
    /// "is the daemon running?" hint; a 404 with `not_found_hint` set
    /// suggests a follow-up listing command instead.
    pub fn print(&self, json: bool, not_found_hint: Option<&str>) {
        if json {
            let body = match self {
                Self::Daemon(e) => {
                    serde_json::json!({"error": {"message": e.message, "status_code": e.status_code}})
                }
                Self::Selector(e) => serde_json::json!({"error": {"message": e.0}}),
                Self::Usage(m) => serde_json::json!({"error": {"message": m}}),
            };
            println!(
                "{}",
                serde_json::to_string_pretty(&body).unwrap_or_default()
            );
            return;
        }
        match self {
            Self::Daemon(e) => {
                println!("error: {}", e.message);
                if e.status_code.is_none() {
                    println!("(is the daemon running? start it with: ralphus-daemon serve)");
                } else if e.status_code == Some(404) {
                    if let Some(hint) = not_found_hint {
                        println!("run '{hint}' to see candidates");
                    }
                }
            }
            Self::Selector(e) => println!("error: {}", e.0),
            Self::Usage(m) => println!("usage error: {m}"),
        }
    }
}

/// Runs `handler`, printing the error (if any) and returning the process
/// exit code -- the near-universal per-command shape ("open client -> call
/// -> emit or print error").
pub fn run_and_report(
    opts: &GlobalOpts,
    not_found_hint: Option<&str>,
    handler: impl FnOnce() -> Result<(), CommandError>,
) -> i32 {
    match handler() {
        Ok(()) => 0,
        Err(e) => {
            e.print(opts.json, not_found_hint);
            e.exit_code()
        }
    }
}

/// Emits a successful `Value` through the shared `--json`/human-rendering
/// convention.
pub fn emit(opts: &GlobalOpts, data: &Value, human: impl FnOnce(&Value)) {
    crate::output::emit(opts.json, data, human);
}

/// Human-readable rendering of one forge connectivity-check outcome
/// (RAL-523), shared by every check command: a check that ran and reported a
/// verdict is a successful command (exit 0) -- the verdict itself is the
/// payload, so `--json` consumers and scripts read `ok`/`status`/`detail`
/// rather than inferring anything from the exit code.
pub fn render_forge_check_outcome(outcome: &Value, subject: &str) {
    let ok = outcome["ok"].as_bool().unwrap_or(false);
    let status = outcome["status"].as_str().unwrap_or("error");
    let detail = outcome["detail"].as_str().unwrap_or_default();
    let mark = if ok { "✓" } else { "✗" };
    println!("{mark} {subject}: {status}");
    if let Some(identity) = outcome["identity"].as_str() {
        println!("  authenticated as: {identity}");
    }
    if !detail.is_empty() {
        println!("  {detail}");
    }
}

/// The parsed command-line action. Group-only nodes (e.g. bare `ralphus
/// run`) are represented by that group enum's own `Help` variant so every
/// node in the tree has well-defined, non-panicking behavior.
#[derive(Debug)]
pub enum Command {
    Help,
    License,
    Validate {
        files: Vec<String>,
    },
    Submit(misc::SubmitArgs),
    Status {
        squad_id: Option<String>,
        concurrency: bool,
    },
    Resources,
    Graph {
        squad_id: Option<String>,
        dot: bool,
        all: bool,
    },
    Get {
        uri: String,
        field: Option<String>,
    },
    Cartographer(misc::CartographerArgs),
    History {
        selector: String,
        type_filter: Option<String>,
    },
    Listen {
        selector: String,
        until: String,
        timeout: Option<f64>,
    },
    RetryRun {
        squad_id: String,
    },
    Clear(misc::ClearArgs),
    Check(misc::CheckArgs),
    /// RAL-416: `ralphus check catalog` -- lists every check
    /// `ralphus_core::health_catalog` knows about (id, section,
    /// applicability, cost tier, requirement level, impact) without running
    /// any probes. Read-only and instant, unlike `check health`.
    CheckCatalog,
    Completion,
    Configuration,
    Task(task::TaskCommand),
    TutorShow,
    Cell(cell::CellCommand),
    Proof(proof::ProofCommand),
    Prophecy(prophecy::ProphecyCommand),
    Review(review::ReviewCommand),
    Queue(queue::QueueCommand),
    Mcp(mcp::McpCommand),
    /// `ralphus initialize git [--path P]`: enables git rerere+autoupdate in
    /// the target repository (defaults to cwd).
    InitializeGit {
        path: Option<String>,
    },
    /// `ralphus initialize server` (RAL-501): the interactive, hidden new
    /// installation walkthrough -- deliberately absent from
    /// `help_map.rs`/generated help/the MCP tool surface (see
    /// `initialize/server.rs`'s module doc), reached only via the
    /// `resolved_path` exception in `help_map.rs`.
    InitializeServer {
        setup: Box<initialize::server::InitializeServerOptions>,
    },
    /// Set up an isolated daemon and submit a waypoint exercise suite.
    InitializeWaypoint {
        options: initialize::exercise::ExerciseOptions,
    },
    InitializeMailbox {
        options: initialize::exercise::ExerciseOptions,
    },
    InitializeMachine {
        options: initialize::exercise::ExerciseOptions,
    },
    InitializeTriage {
        options: initialize::exercise::ExerciseOptions,
    },
    InitializeReview {
        options: initialize::exercise::ExerciseOptions,
    },
    Project(project::ProjectCommand),
    Machine(machine::MachineCommand),
    Agent(agent::AgentCommand),
    Show(show::ShowCommand),
    Squad(squad::SquadCommand),
    Mailbox(mailbox::MailboxCommand),
    QuickStart(quick_start::QuickStartCommand),
    Triage(triage::TriageCommand),
    User(user::UserCommand),
    Waypoint(waypoint::WaypointCommand),
    Internal(internal::InternalCommand),
    Preset(preset::PresetCommand),
    UsageError(String),
}

/// Parses CLI arguments (excluding the program name and already-extracted
/// global `--json`/`--daemon-url` flags). Unknown/missing subcommands fall
/// back to `Help` so the binary never panics on a bad invocation.
#[must_use]
pub fn parse_args(args: &[String]) -> Command {
    let mut scanner = Scanner::new(&args[1.min(args.len())..]);
    match args.first().map(String::as_str) {
        None | Some("help" | "--help" | "-h") => Command::Help,
        Some("license") => Command::License,
        Some("validate") => Command::Validate {
            files: scanner.remaining(),
        },
        Some("submit") => misc::parse_submit(&mut scanner),
        Some("status") => {
            let concurrency = scanner.take_bool("--concurrency");
            Command::Status {
                squad_id: scanner.remaining().into_iter().next(),
                concurrency,
            }
        }
        Some("resources") => Command::Resources,
        Some("graph") => {
            let dot = scanner.take_bool("--dot");
            let all = scanner.take_bool("--all");
            Command::Graph {
                squad_id: scanner.remaining().into_iter().next(),
                dot,
                all,
            }
        }
        Some("get") => {
            let rest = scanner.remaining();
            match rest.first() {
                Some(uri) => Command::Get {
                    uri: uri.clone(),
                    field: rest.get(1).cloned(),
                },
                None => Command::UsageError("get requires a <selector> argument".to_string()),
            }
        }
        Some("cartographer") => misc::parse_cartographer(&mut scanner),
        Some("history") => {
            let type_filter = scanner.take_value("--type").ok().flatten();
            with_positional_arg(scanner, "selector", |selector| Command::History {
                selector,
                type_filter,
            })
        }
        Some("listen") => misc::parse_listen(&mut scanner),
        Some("retry") => with_positional_arg(scanner, "squad_id", |squad_id| Command::RetryRun {
            squad_id,
        }),
        Some("clear") => misc::parse_clear(&mut scanner),
        Some("check") => misc::parse_check(&mut scanner),
        Some("completion") => Command::Completion,
        Some("configuration") => Command::Configuration,
        Some("tutor") => Command::TutorShow,
        Some("initialize") => {
            let tail = scanner.remaining();
            match tail.first().map(String::as_str) {
                Some("git") => {
                    let mut inner = Scanner::new(&tail[1..]);
                    let path = inner.take_value("--path").ok().flatten();
                    Command::InitializeGit { path }
                }
                Some("server") => {
                    let mut inner = Scanner::new(&tail[1..]);
                    match parse_initialize_server(&mut inner) {
                        Ok(setup) if inner.remaining().is_empty() => Command::InitializeServer {
                            setup: Box::new(setup),
                        },
                        Ok(_) => Command::UsageError(
                            "initialize server: unexpected argument".to_string(),
                        ),
                        Err(error) => Command::UsageError(error.0),
                    }
                }
                Some(kind @ ("waypoint" | "mailbox" | "machine" | "triage" | "review")) => {
                    match parse_exercise_options(kind, &tail[1..]) {
                        Ok(options) => match kind {
                            "waypoint" => Command::InitializeWaypoint { options },
                            "mailbox" => Command::InitializeMailbox { options },
                            "machine" => Command::InitializeMachine { options },
                            "triage" => Command::InitializeTriage { options },
                            _ => Command::InitializeReview { options },
                        },
                        Err(error) => Command::UsageError(error),
                    }
                }
                _ => Command::UsageError(
                    "initialize: expected 'git', 'server', 'waypoint', 'mailbox', 'machine', 'triage', or 'review' subcommand"
                        .to_string(),
                ),
            }
        }
        Some("mcp") => Command::Mcp(mcp::parse(&scanner.remaining())),
        Some("task") => Command::Task(task::parse(&scanner.remaining())),
        Some("cell") => Command::Cell(cell::parse(&scanner.remaining())),
        Some("proof") => Command::Proof(proof::parse(&scanner.remaining())),
        Some("prophecy") => Command::Prophecy(prophecy::parse(&scanner.remaining())),
        Some("review") => Command::Review(review::parse(&scanner.remaining())),
        Some("queue") => Command::Queue(queue::parse(&scanner.remaining())),
        Some("project") => Command::Project(project::parse(&scanner.remaining())),
        Some("machine") => Command::Machine(machine::parse(&scanner.remaining())),
        Some("agent") => Command::Agent(agent::parse(&scanner.remaining())),
        Some("show") => Command::Show(show::parse(&scanner.remaining())),
        Some("squad") => Command::Squad(squad::parse(&scanner.remaining())),
        Some("mailbox") => Command::Mailbox(mailbox::parse(&scanner.remaining())),
        Some("quick-start") => Command::QuickStart(quick_start::parse(&scanner.remaining())),
        Some("triage") => Command::Triage(triage::parse(&scanner.remaining())),
        Some("user") => Command::User(user::parse(&scanner.remaining())),
        Some("waypoint") => Command::Waypoint(waypoint::parse(&scanner.remaining())),
        Some("internal") => Command::Internal(internal::parse(&scanner.remaining())),
        Some("preset") => Command::Preset(preset::parse(&scanner.remaining())),
        Some(other) => Command::UsageError(format!("unknown command: {other}")),
    }
}

fn parse_initialize_bool(scanner: &mut Scanner, name: &str) -> Result<Option<bool>, UsageError> {
    scanner
        .take_value(name)?
        .map_or(Ok(None), |value| match value.as_str() {
            "yes" | "true" => Ok(Some(true)),
            "no" | "false" => Ok(Some(false)),
            _ => Err(UsageError(format!(
                "{name}: expected yes or no, got '{value}'"
            ))),
        })
}

fn parse_sample_mode(scanner: &mut Scanner) -> Result<Option<String>, UsageError> {
    scanner
        .take_value("--sample-mode")?
        .map_or(Ok(None), |value| match value.as_str() {
            "agent" | "raw" => Ok(Some(value)),
            _ => Err(UsageError(format!(
                "--sample-mode: expected agent or raw, got '{value}'"
            ))),
        })
}

fn parse_forge_provider(scanner: &mut Scanner) -> Result<Option<String>, UsageError> {
    scanner
        .take_value("--forge-provider")?
        .map_or(Ok(None), |value| match value.as_str() {
            "github" | "gitlab" => Ok(Some(value)),
            _ => Err(UsageError(format!(
                "--forge-provider: expected github or gitlab, got '{value}'"
            ))),
        })
}

fn parse_forge_host(scanner: &mut Scanner) -> Result<Option<String>, UsageError> {
    scanner
        .take_value("--forge-host")?
        .map_or(Ok(None), |host| {
            initialize::server::validate_forge_host(&host)
                .map(|()| Some(host))
                .map_err(|error| UsageError(format!("--forge-host: {error}")))
        })
}

fn parse_project_fork_url(scanner: &mut Scanner) -> Result<Option<String>, UsageError> {
    scanner
        .take_value("--project-fork-url")?
        .map_or(Ok(None), |url| {
            url.starts_with("https://").then_some(url).ok_or_else(|| {
                UsageError(
                    "--project-fork-url: fork clone URLs must use HTTPS (for example https://github.com/owner/repo.git)"
                        .to_string(),
                )
            }).map(Some)
        })
}

/// The flags every guided exercise (`initialize waypoint|mailbox|machine|
/// triage|review`) takes. An unrecognized argument is an error, so a typo such
/// as `--remot` cannot silently run the local variant instead.
fn parse_exercise_options(
    kind: &str,
    args: &[String],
) -> Result<initialize::exercise::ExerciseOptions, String> {
    let mut inner = Scanner::new(args);
    let state_dir = inner
        .take_value("--state-dir")
        .map_err(|e| format!("initialize {kind}: {}", e.0))?;
    let remote = inner.take_bool("--remote");
    let stop = inner.take_bool("--stop");
    let rest = inner.remaining();
    if let Some(extra) = rest.first() {
        return Err(format!("initialize {kind}: unexpected argument {extra:?}"));
    }
    Ok(initialize::exercise::ExerciseOptions {
        state_dir,
        remote,
        stop,
    })
}

fn parse_initialize_server(
    scanner: &mut Scanner,
) -> Result<initialize::server::InitializeServerOptions, UsageError> {
    Ok(initialize::server::InitializeServerOptions {
        yes: scanner.take_bool("--yes"),
        answers_file: scanner
            .take_value("--answers-file")?
            .map(std::path::PathBuf::from),
        install_tmux: parse_initialize_bool(scanner, "--install-tmux")?,
        tmux_program: scanner.take_value("--tmux-program")?,
        setup_mcp: parse_initialize_bool(scanner, "--setup-mcp")?,
        mcp_hosts: scanner.take_repeated("--mcp-host")?,
        agent_logins: scanner.take_value("--agent-logins")?,
        register_project: parse_initialize_bool(scanner, "--register-project")?,
        project_name: scanner.take_value("--project-name")?,
        project_is_fork: parse_initialize_bool(scanner, "--project-is-fork")?,
        project_fork_url: parse_project_fork_url(scanner)?,
        project_url: scanner.take_value("--project-url")?,
        project_description: scanner.take_value("--project-description")?,
        bug_threshold: scanner.take_value("--bug-threshold")?,
        feature_threshold: scanner.take_value("--feature-threshold")?,
        investigation_threshold: scanner.take_value("--investigation-threshold")?,
        unclassified_threshold: scanner.take_value("--unclassified-threshold")?,
        review_auto_submit_pr_stack: parse_initialize_bool(
            scanner,
            "--review-auto-submit-pr-stack",
        )?,
        review_resolver_agent: scanner.take_value("--review-resolver-agent")?,
        require_forks: parse_initialize_bool(scanner, "--require-forks")?,
        fork_user: scanner.take_value("--fork-user")?,
        fork_url: scanner.take_value("--fork-url")?,
        forge_host: parse_forge_host(scanner)?,
        forge_token: scanner.take_value("--forge-token")?,
        create_admin: parse_initialize_bool(scanner, "--create-admin")?,
        admin_name: scanner.take_value("--admin-name")?,
        setup_forge_token: parse_initialize_bool(scanner, "--setup-forge-token")?,
        forge_provider: parse_forge_provider(scanner)?,
        submit_sample: parse_initialize_bool(scanner, "--submit-sample")?,
        sample_mode: parse_sample_mode(scanner)?,
        sample_agent: scanner.take_value("--sample-agent")?,
    })
}

fn with_positional_arg(
    scanner: Scanner,
    label: &str,
    make: impl FnOnce(String) -> Command,
) -> Command {
    match scanner.remaining().into_iter().next() {
        Some(value) => make(value),
        None => Command::UsageError(format!("missing required <{label}> argument")),
    }
}

/// Dispatches a parsed [`Command`], returning the process exit code.
#[must_use]
pub fn dispatch(cmd: Command, opts: &GlobalOpts) -> i32 {
    match cmd {
        Command::Help => {
            println!("{}", usage());
            0
        }
        Command::License => {
            print!("{}", ralphus_core::license::embedded_license());
            0
        }
        Command::UsageError(m) => {
            println!("usage error: {m}");
            2
        }
        Command::Validate { files } => misc::cmd_validate(opts, &files),
        Command::Submit(args) => misc::cmd_submit(opts, args),
        Command::Status {
            squad_id,
            concurrency,
        } => misc::cmd_status(opts, squad_id, concurrency),
        Command::Resources => misc::cmd_resources(opts),
        Command::Graph { squad_id, dot, all } => misc::cmd_graph(opts, squad_id, dot, all),
        Command::Get { uri, field } => misc::cmd_get(opts, &uri, field.as_deref()),
        Command::Cartographer(args) => misc::cmd_cartographer(opts, args),
        Command::History {
            selector,
            type_filter,
        } => misc::cmd_history(opts, &selector, type_filter.as_deref()),
        Command::Listen {
            selector,
            until,
            timeout,
        } => misc::cmd_listen(opts, &selector, &until, timeout),
        Command::RetryRun { squad_id } => misc::cmd_retry(opts, &squad_id),
        Command::Clear(args) => misc::cmd_clear(opts, args),
        Command::Check(args) => misc::cmd_check(opts, args),
        Command::Completion => misc::cmd_completion(),
        Command::Configuration => misc::cmd_configuration(opts),
        Command::CheckCatalog => misc::cmd_check_catalog(opts),
        Command::TutorShow => {
            let dir = std::env::current_dir().unwrap_or_default();
            println!("{}", crate::tutor::task_tutor_in(&dir));
            0
        }
        Command::Task(c) => task::dispatch(c, opts),
        Command::Cell(c) => cell::dispatch(c, opts),
        Command::Proof(c) => proof::dispatch(c, opts),
        Command::Prophecy(c) => prophecy::dispatch(c, opts),
        Command::Review(c) => review::dispatch(c, opts),
        Command::Queue(c) => queue::dispatch(c, opts),
        Command::Mcp(c) => mcp::dispatch(c),
        Command::InitializeGit { path } => misc::cmd_initialize_git(path),
        Command::InitializeServer { setup } => initialize::server::dispatch(opts, *setup),
        Command::InitializeWaypoint { options } => {
            initialize::exercise::run_logged("waypoint", || {
                initialize::waypoint::dispatch(&options)
            })
        }
        Command::InitializeMailbox { options } => {
            initialize::exercise::run_logged("mailbox", || initialize::mailbox::dispatch(&options))
        }
        Command::InitializeMachine { options } => {
            initialize::exercise::run_logged("machine", || initialize::machine::dispatch(&options))
        }
        Command::InitializeTriage { options } => {
            initialize::exercise::run_logged("triage", || initialize::triage::dispatch(&options))
        }
        Command::InitializeReview { options } => {
            initialize::exercise::run_logged("review", || initialize::review::dispatch(&options))
        }
        Command::Project(c) => project::dispatch(c, opts),
        Command::Machine(c) => machine::dispatch(c, opts),
        Command::Agent(c) => agent::dispatch(c, opts),
        Command::Show(c) => show::dispatch(c, opts),
        Command::Squad(c) => squad::dispatch(c, opts),
        Command::Mailbox(c) => mailbox::dispatch(c, opts),
        Command::QuickStart(c) => quick_start::dispatch(c, opts),
        Command::Triage(c) => triage::dispatch(c, opts),
        Command::User(c) => user::dispatch(c, opts),
        Command::Waypoint(c) => waypoint::dispatch(c, opts),
        Command::Internal(c) => internal::dispatch(c, opts),
        Command::Preset(c) => preset::dispatch(c, opts),
    }
}

#[must_use]
pub fn usage() -> String {
    crate::help_map::command_help(&[]).expect("root help exists")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn parses_validate_with_files() {
        match parse_args(&v(&["validate", "a.toml", "b.toml"])) {
            Command::Validate { files } => assert_eq!(files, v(&["a.toml", "b.toml"])),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn unknown_command_is_usage_error_not_panic() {
        matches!(parse_args(&v(&["bogus"])), Command::UsageError(_));
    }

    #[test]
    fn empty_args_is_help() {
        matches!(parse_args(&[]), Command::Help);
    }

    #[test]
    fn license_is_a_top_level_command() {
        matches!(parse_args(&v(&["license"])), Command::License);
    }

    #[test]
    fn history_requires_selector() {
        matches!(parse_args(&v(&["history"])), Command::UsageError(_));
        matches!(
            parse_args(&v(&["history", "squad-1"])),
            Command::History { .. }
        );
    }

    #[test]
    fn history_parses_type_filter() {
        match parse_args(&v(&[
            "history",
            "--type",
            "read glob",
            "squad-1/task/0/cell/0",
        ])) {
            Command::History {
                selector,
                type_filter: Some(type_filter),
            } => {
                assert_eq!(selector, "squad-1/task/0/cell/0");
                assert_eq!(type_filter, "read glob");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn initialize_server_parses_every_non_interactive_answer() {
        match parse_args(&v(&[
            "initialize",
            "server",
            "--install-tmux",
            "no",
            "--tmux-program",
            "",
            "--setup-mcp",
            "yes",
            "--mcp-host",
            "claude",
            "--register-project",
            "yes",
            "--project-name",
            "ralphus",
            "--project-is-fork",
            "yes",
            "--project-fork-url",
            "https://github.com/ColinKennedy/ralphus.git",
            "--project-url",
            "git@github.com:upstream/ralphus.git",
            "--project-description",
            "self-hosting",
            "--bug-threshold",
            "",
            "--feature-threshold",
            "5",
            "--investigation-threshold",
            "3",
            "--unclassified-threshold",
            "5",
            "--review-auto-submit-pr-stack",
            "yes",
            "--review-resolver-agent",
            "claude-code",
            "--require-forks",
            "yes",
            "--fork-user",
            "Ada",
            "--fork-url",
            "git@example.test:ada/ralphus.git",
            "--forge-host",
            "github.com",
            "--forge-token",
            "token",
            "--create-admin",
            "yes",
            "--admin-name",
            "Ada",
            "--setup-forge-token",
            "yes",
            "--forge-provider",
            "gitlab",
            "--submit-sample",
            "yes",
            "--sample-mode",
            "agent",
            "--sample-agent",
            "claude-code",
            "--agent-logins",
            "claude,codex",
        ])) {
            Command::InitializeServer { setup } => {
                assert_eq!(setup.agent_logins.as_deref(), Some("claude,codex"));
                assert_eq!(setup.install_tmux, Some(false));
                assert_eq!(setup.mcp_hosts, ["claude"]);
                assert_eq!(setup.project_name.as_deref(), Some("ralphus"));
                assert_eq!(setup.project_is_fork, Some(true));
                assert_eq!(
                    setup.project_fork_url.as_deref(),
                    Some("https://github.com/ColinKennedy/ralphus.git")
                );
                assert_eq!(
                    setup.project_url.as_deref(),
                    Some("git@github.com:upstream/ralphus.git")
                );
                assert_eq!(setup.forge_token.as_deref(), Some("token"));
                assert_eq!(setup.setup_forge_token, Some(true));
                assert_eq!(setup.forge_provider.as_deref(), Some("gitlab"));
                assert_eq!(setup.review_auto_submit_pr_stack, Some(true));
                assert_eq!(setup.review_resolver_agent.as_deref(), Some("claude-code"));
                assert_eq!(setup.sample_mode.as_deref(), Some("agent"));
                assert_eq!(setup.sample_agent.as_deref(), Some("claude-code"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn initialize_server_rejects_invalid_boolean_answer() {
        match parse_args(&v(&["initialize", "server", "--create-admin", "perhaps"])) {
            Command::UsageError(message) => assert!(message.contains("expected yes or no")),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn initialize_server_rejects_invalid_sample_mode() {
        match parse_args(&v(&["initialize", "server", "--sample-mode", "shell"])) {
            Command::UsageError(message) => assert!(message.contains("expected agent or raw")),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn initialize_server_rejects_invalid_forge_provider() {
        match parse_args(&v(&[
            "initialize",
            "server",
            "--forge-provider",
            "bitbucket",
        ])) {
            Command::UsageError(message) => assert!(message.contains("expected github or gitlab")),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn initialize_server_accepts_a_bare_forge_host() {
        match parse_args(&v(&["initialize", "server", "--forge-host", "gitlab.com"])) {
            Command::InitializeServer { setup } => {
                assert_eq!(setup.forge_host.as_deref(), Some("gitlab.com"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn initialize_server_rejects_forge_host_urls_and_www_prefixes() {
        for host in ["https://gitlab.com", "http://gitlab.com", "www.gitlab.com"] {
            match parse_args(&v(&["initialize", "server", "--forge-host", host])) {
                Command::UsageError(message) => assert!(message.contains("bare hostname")),
                other => panic!("unexpected: {other:?}"),
            }
        }
    }

    #[test]
    fn initialize_server_rejects_non_https_project_fork_url() {
        match parse_args(&v(&[
            "initialize",
            "server",
            "--project-fork-url",
            "git@github.com:owner/repo.git",
        ])) {
            Command::UsageError(message) => assert!(message.contains("must use HTTPS")),
            other => panic!("unexpected: {other:?}"),
        }
    }
}
