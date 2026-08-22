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
pub mod machine;
pub mod misc;
pub mod project;
pub mod queue;
pub mod quick_start;
pub mod review;
pub mod run;
pub mod session;
pub mod show;
pub mod task;
pub mod verify;

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

/// The parsed command-line action. Group-only nodes (e.g. bare `ralphus
/// run`) are represented by that group enum's own `Help` variant so every
/// node in the tree has well-defined, non-panicking behavior.
#[derive(Debug)]
pub enum Command {
    Help,
    Validate {
        files: Vec<String>,
    },
    Submit(misc::SubmitArgs),
    Status {
        run_id: Option<String>,
        concurrency: bool,
    },
    Resources,
    Graph {
        run_id: Option<String>,
        dot: bool,
        all: bool,
    },
    Get {
        uri: String,
        field: Option<String>,
    },
    Cartographer(misc::CartographerArgs),
    History {
        run_id: String,
    },
    Listen {
        selector: String,
        until: String,
        timeout: Option<f64>,
    },
    RetryRun {
        run_id: String,
    },
    Clear(misc::ClearArgs),
    Check(misc::CheckArgs),
    Completion,
    Configuration,
    Task(task::TaskCommand),
    TutorShow,
    Session(session::SessionCommand),
    Verify(verify::VerifyCommand),
    Review(review::ReviewCommand),
    Queue(queue::QueueCommand),
    /// `ralphus initialize git [--path P]`: enables git rerere+autoupdate in
    /// the target repository (defaults to cwd). No other `initialize`
    /// subcommand exists in the source today.
    InitializeGit {
        path: Option<String>,
    },
    Project(project::ProjectCommand),
    Machine(machine::MachineCommand),
    Agent(agent::AgentCommand),
    Show(show::ShowCommand),
    Run(run::RunCommand),
    QuickStart(quick_start::QuickStartCommand),
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
        Some("validate") => Command::Validate {
            files: scanner.remaining(),
        },
        Some("submit") => misc::parse_submit(&mut scanner),
        Some("status") => {
            let concurrency = scanner.take_bool("--concurrency");
            Command::Status {
                run_id: scanner.remaining().into_iter().next(),
                concurrency,
            }
        }
        Some("resources") => Command::Resources,
        Some("graph") => {
            let dot = scanner.take_bool("--dot");
            let all = scanner.take_bool("--all");
            Command::Graph {
                run_id: scanner.remaining().into_iter().next(),
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
        Some("history") => with_run_id(scanner, |run_id| Command::History { run_id }),
        Some("listen") => misc::parse_listen(&mut scanner),
        Some("retry") => with_run_id(scanner, |run_id| Command::RetryRun { run_id }),
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
                _ => Command::UsageError("initialize: expected 'git' subcommand".to_string()),
            }
        }
        Some("task") => Command::Task(task::parse(&scanner.remaining())),
        Some("session") => Command::Session(session::parse(&scanner.remaining())),
        Some("verify") => Command::Verify(verify::parse(&scanner.remaining())),
        Some("review") => Command::Review(review::parse(&scanner.remaining())),
        Some("queue") => Command::Queue(queue::parse(&scanner.remaining())),
        Some("project") => Command::Project(project::parse(&scanner.remaining())),
        Some("machine") => Command::Machine(machine::parse(&scanner.remaining())),
        Some("agent") => Command::Agent(agent::parse(&scanner.remaining())),
        Some("show") => Command::Show(show::parse(&scanner.remaining())),
        Some("run") => Command::Run(run::parse(&scanner.remaining())),
        Some("quick-start") => Command::QuickStart(quick_start::parse(&scanner.remaining())),
        Some(other) => Command::UsageError(format!("unknown command: {other}")),
    }
}

fn with_run_id(scanner: Scanner, make: impl FnOnce(String) -> Command) -> Command {
    match scanner.remaining().into_iter().next() {
        Some(run_id) => make(run_id),
        None => Command::UsageError("missing required <run_id> argument".to_string()),
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
        Command::UsageError(m) => {
            println!("usage error: {m}");
            2
        }
        Command::Validate { files } => misc::cmd_validate(opts, &files),
        Command::Submit(args) => misc::cmd_submit(opts, args),
        Command::Status {
            run_id,
            concurrency,
        } => misc::cmd_status(opts, run_id, concurrency),
        Command::Resources => misc::cmd_resources(opts),
        Command::Graph { run_id, dot, all } => misc::cmd_graph(opts, run_id, dot, all),
        Command::Get { uri, field } => misc::cmd_get(opts, &uri, field.as_deref()),
        Command::Cartographer(args) => misc::cmd_cartographer(opts, args),
        Command::History { run_id } => misc::cmd_history(opts, &run_id),
        Command::Listen {
            selector,
            until,
            timeout,
        } => misc::cmd_listen(opts, &selector, &until, timeout),
        Command::RetryRun { run_id } => misc::cmd_retry(opts, &run_id),
        Command::Clear(args) => misc::cmd_clear(opts, args),
        Command::Check(args) => misc::cmd_check(opts, args),
        Command::Completion => misc::cmd_completion(),
        Command::Configuration => misc::cmd_configuration(opts),
        Command::TutorShow => {
            println!("{}", crate::tutor::task_tutor());
            0
        }
        Command::Task(c) => task::dispatch(c, opts),
        Command::Session(c) => session::dispatch(c, opts),
        Command::Verify(c) => verify::dispatch(c, opts),
        Command::Review(c) => review::dispatch(c, opts),
        Command::Queue(c) => queue::dispatch(c, opts),
        Command::InitializeGit { path } => misc::cmd_initialize_git(path),
        Command::Project(c) => project::dispatch(c, opts),
        Command::Machine(c) => machine::dispatch(c, opts),
        Command::Agent(c) => agent::dispatch(c, opts),
        Command::Show(c) => show::dispatch(c, opts),
        Command::Run(c) => run::dispatch(c, opts),
        Command::QuickStart(c) => quick_start::dispatch(c),
    }
}

#[must_use]
pub fn usage() -> String {
    "ralphus -- see docs/cli-reference.md for the full subcommand list.\nUSAGE:\n    ralphus <COMMAND> [ARGS...]\n".to_string()
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
    fn history_requires_run_id() {
        matches!(parse_args(&v(&["history"])), Command::UsageError(_));
        matches!(
            parse_args(&v(&["history", "run-1"])),
            Command::History { .. }
        );
    }
}
