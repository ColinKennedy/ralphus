//! ralphus daemon library.
//!
//! The daemon is the only process that touches the SQLite database; the CLI and
//! librarian reach it exclusively through its HTTP/JSON API (see
//! `docs/daemon-api.md`). Splitting the logic into a library (with a thin
//! `main.rs` binary on top) keeps it unit-testable.

pub mod agent_access;
pub mod agent_profiles;
pub mod cancel;
pub mod cartographer;
pub mod channel;
pub mod chat_client;
pub mod config;
pub mod entity_uri;
pub mod events;
pub mod forge;
pub mod ghost;
pub mod guardian;
pub mod guardian_merge;
pub mod jobobject;
pub mod logging;
pub mod machines;
pub mod mailbox;
pub mod otel;
pub mod plan;
pub mod pr;
pub mod procreg;
pub mod proof;
pub mod redact;
pub mod remote_runner;
pub mod resources;
pub mod reviews;
pub mod runner;
pub mod scheduler;
pub mod server;
pub(crate) mod short_paths;
pub(crate) mod stash;
pub mod store;
pub mod summary_worker;
pub mod terminal_log;
pub mod timeline;
pub mod tmux;
pub mod token;
pub mod users;
pub mod vcs;
pub mod workspace;
pub mod worktrees;

use std::path::PathBuf;

/// Default port the daemon's HTTP API listens on.
pub const DEFAULT_PORT: u16 = 7890;

/// Default maximum number of concurrently running squads.
pub const DEFAULT_MAX_CONCURRENT: i64 = 12;

/// Overrides the host the daemon's HTTP listener binds to.
pub const BIND_ADDR_ENV: &str = "RALPHUS_BIND_ADDR";

/// Resolve the host the daemon's HTTP listener binds to: `env_override`
/// (read from [`BIND_ADDR_ENV`] by the caller) when non-empty, else the
/// bare-subprocess default `127.0.0.1`.
///
/// The container execution mode (RAL-225) sets `RALPHUS_BIND_ADDR=0.0.0.0`
/// so a `docker run -p`/compose published port can reach the listener from
/// outside the container's network namespace -- binding only `127.0.0.1`
/// inside a container is unreachable from the host despite the port
/// mapping. Every other setup must keep the `127.0.0.1` default: binding
/// `0.0.0.0` there would expose the daemon's unauthenticated HTTP API on
/// every network interface, not just loopback.
#[must_use]
pub fn resolve_bind_host(env_override: Option<&str>) -> String {
    match env_override.map(str::trim) {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => "127.0.0.1".to_string(),
    }
}

/// Resolve the daemon's state directory (`~/.ralphus`), creating it if needed.
///
/// Falls back to `./.ralphus` when no home directory is set. Delegates to
/// `ralphus_core::state_dir()` so the librarian (which does not depend on this
/// crate) can resolve the same path — see that function's doc comment
/// (including the RAL-230 owner-only permission tightening it performs).
#[must_use]
pub fn state_dir() -> PathBuf {
    ralphus_core::state_dir()
}

/// Path to the daemon's SQLite database.
#[must_use]
pub fn default_db_path() -> PathBuf {
    state_dir().join("tasks.db")
}

/// Path to the daemon's bearer-token file (RAL-219). See
/// `ralphus_core::daemon_token_path()` and `docs/daemon-api.md`'s
/// "Authentication" section.
#[must_use]
pub fn token_path() -> PathBuf {
    ralphus_core::daemon_token_path()
}

/// Command-line action parsed from the daemon's arguments.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HelpCommand {
    Serve,
    Validate,
    Mux,
    Stop,
    License,
    Version,
}

impl HelpCommand {
    #[cfg(test)]
    fn name(self) -> &'static str {
        match self {
            Self::Serve => "serve",
            Self::Validate => "validate",
            Self::Mux => "mux",
            Self::Stop => "stop",
            Self::License => "license",
            Self::Version => "version",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Print version and exit.
    Version,
    /// Print the embedded LICENSE text and exit.
    License,
    /// Print usage and exit.
    Help(Option<HelpCommand>),
    /// Run the daemon (serve the HTTP API).
    Serve {
        /// Port the HTTP API binds to on `127.0.0.1` (see [`resolve_bind_host`]
        /// for the `RALPHUS_BIND_ADDR`-overridable host).
        port: u16,
        /// SQLite database path. Defaults to `default_db_path()` when unset.
        /// Explicit only (RAL-164) -- multiple instances (e.g. one per git
        /// worktree) must each be given a distinct `--db` to get real
        /// isolation; nothing here auto-picks a path.
        db: Option<PathBuf>,
    },
    /// Validate a task TOML file offline (no server, no database).
    Validate(String),
    /// Forward arguments raw to the resolved tmux binary (RAL-102). CLI-only —
    /// never exposed over the HTTP API.
    Mux(Vec<String>),
    /// Ask a running daemon to kill every process it has spawned (cells,
    /// proofs, reviews, chats, summaries — everything) and exit.
    Stop {
        /// Port the target daemon's HTTP API is listening on.
        port: u16,
        /// When true, also mark every in-flight squad/guardian as `cancelled`
        /// in the store instead of leaving them for crash-recovery to
        /// resume on the next `serve()`.
        auto_cancel: bool,
    },
}

/// Parse daemon CLI arguments (excluding the program name).
///
/// Supports `serve [--port N]`. Unknown or missing subcommands fall back to
/// `Help` so the binary always has a well-defined, non-panicking behavior.
#[must_use]
pub fn parse_args(args: &[String]) -> Command {
    if let Some(command) = requested_help(args) {
        return Command::Help(command);
    }
    let Some(first) = args.first().map(String::as_str) else {
        return Command::Help(None);
    };
    if matches!(first, "--version" | "-V") {
        return Command::Version;
    }
    match registered_command(first) {
        Some(HelpCommand::Version) => Command::Version,
        Some(HelpCommand::License) => Command::License,
        Some(HelpCommand::Serve) => {
            let tail = &args[1..];
            let port = parse_port_flag(tail).unwrap_or(DEFAULT_PORT);
            let db = parse_db_flag(tail);
            Command::Serve { port, db }
        }
        Some(HelpCommand::Validate) => Command::Validate(args.get(1).cloned().unwrap_or_default()),
        Some(HelpCommand::Mux) => Command::Mux(args[1..].to_vec()),
        Some(HelpCommand::Stop) => {
            let tail = &args[1..];
            let port = parse_port_flag(tail).unwrap_or(DEFAULT_PORT);
            let auto_cancel = tail.iter().any(|a| a == "--auto-cancel");
            Command::Stop { port, auto_cancel }
        }
        None => Command::Help(None),
    }
}

fn registered_command(name: &str) -> Option<HelpCommand> {
    match name {
        "serve" => Some(HelpCommand::Serve),
        "validate" => Some(HelpCommand::Validate),
        "mux" => Some(HelpCommand::Mux),
        "stop" => Some(HelpCommand::Stop),
        "license" => Some(HelpCommand::License),
        "version" => Some(HelpCommand::Version),
        _ => None,
    }
}

#[cfg(test)]
const HELP_COMMANDS: &[HelpCommand] = &[
    HelpCommand::Serve,
    HelpCommand::Validate,
    HelpCommand::Mux,
    HelpCommand::Stop,
    HelpCommand::License,
    HelpCommand::Version,
];

fn requested_help(args: &[String]) -> Option<Option<HelpCommand>> {
    let before_separator: Vec<&String> =
        args.iter().take_while(|arg| arg.as_str() != "--").collect();
    if !before_separator
        .iter()
        .any(|arg| matches!(arg.as_str(), "--help" | "-h"))
    {
        return None;
    }
    let command = before_separator
        .first()
        .and_then(|arg| registered_command(arg.as_str()));
    Some(command)
}

/// Extract `--port <N>` from the argument tail, if present and valid.
fn parse_port_flag(tail: &[String]) -> Option<u16> {
    let mut it = tail.iter();
    while let Some(arg) = it.next() {
        if arg == "--port" {
            return it.next().and_then(|v| v.parse::<u16>().ok());
        }
    }
    None
}

/// Extract `--db <path>` from the argument tail, if present.
fn parse_db_flag(tail: &[String]) -> Option<PathBuf> {
    let mut it = tail.iter();
    while let Some(arg) = it.next() {
        if arg == "--db" {
            return it.next().map(PathBuf::from);
        }
    }
    None
}

/// The usage string shown for `help` / unknown commands.
#[must_use]
pub fn usage() -> String {
    format!(
        "ralphus-daemon {}\n\nUSAGE:\n    ralphus-daemon <COMMAND>\n\nCOMMANDS:\n    serve [--port {DEFAULT_PORT}] [--db <path>]   Run the daemon and serve the HTTP/JSON API\n    validate <file>   Validate a task TOML file offline (no server needed)\n    mux <args...>     Forward arguments raw to tmux (CLI-only; never exposed over HTTP)\n    stop [--port {DEFAULT_PORT}] [--auto-cancel]   Kill every process the daemon spawned and exit\n    license           Print the embedded LICENSE text\n    version           Print version and exit\n    help              Print this message\n\nOPTIONS:\n    -h, --help        Print help and exit\n",
        ralphus_core::version()
    )
}

/// Detailed help for one registered daemon command.
#[must_use]
pub fn command_usage(command: Option<HelpCommand>) -> String {
    let Some(command) = command else {
        return usage();
    };
    let (summary, invocation, details) = match command {
        HelpCommand::Serve => (
            "Run the daemon and serve the HTTP/JSON API.",
            "ralphus-daemon serve [--port <integer>] [--db <path>]",
            format!(
                "    --port <integer>    TCP port to bind (default {DEFAULT_PORT})\n    --db <path>        SQLite database path\n"
            ),
        ),
        HelpCommand::Validate => (
            "Validate one task TOML file offline.",
            "ralphus-daemon validate <file>",
            "    <file>              Task TOML path to validate\n".to_string(),
        ),
        HelpCommand::Mux => (
            "Forward arguments to the configured tmux-compatible program.",
            "ralphus-daemon mux <args...>",
            "    <args...>           Arguments forwarded verbatim to tmux\n".to_string(),
        ),
        HelpCommand::Stop => (
            "Ask a running daemon to stop.",
            "ralphus-daemon stop [--port <integer>] [--auto-cancel]",
            format!(
                "    --port <integer>    Daemon TCP port (default {DEFAULT_PORT})\n    --auto-cancel       Cancel in-flight work before shutdown\n"
            ),
        ),
        HelpCommand::License => (
            "Print the embedded LICENSE text.",
            "ralphus-daemon license",
            String::new(),
        ),
        HelpCommand::Version => (
            "Print the daemon version.",
            "ralphus-daemon version",
            String::new(),
        ),
    };
    format!(
        "{invocation} -- {summary}\n\nUSAGE:\n    {invocation}\n\nARGUMENTS AND OPTIONS:\n{details}    -h, --help         Print help and exit\n"
    )
}

/// Validate a task TOML file with the core validator, printing findings.
/// Returns `true` when the file is valid. This needs no server or database.
///
/// Writes findings to stdout because this is `ralphus-daemon validate`'s CLI
/// output — the product of the command, not a log line — hence the local
/// opt-out from the workspace `clippy::print_stdout = "deny"`.
#[must_use]
#[allow(clippy::print_stdout)]
pub fn validate_file(path: &str) -> bool {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("error: could not read {path}: {e}");
            return false;
        }
    };
    let report = ralphus_core::validate::validate_toml(&text);
    let loc = |line: Option<u32>| line.map_or(String::new(), |n| format!(" [line {n}]"));
    for w in &report.warnings {
        println!("warning{}: {}", loc(w.line), w.message);
    }
    for e in &report.errors {
        eprintln!("error{}: {}", loc(e.line), e.message);
    }
    if report.is_ok() {
        println!("valid");
        true
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn parses_version_flags() {
        assert_eq!(parse_args(&args(&["--version"])), Command::Version);
        assert_eq!(parse_args(&args(&["-V"])), Command::Version);
        assert_eq!(parse_args(&args(&["version"])), Command::Version);
    }

    #[test]
    fn parses_license() {
        assert_eq!(parse_args(&args(&["license"])), Command::License);
    }

    #[test]
    fn parses_serve() {
        assert_eq!(
            parse_args(&args(&["serve"])),
            Command::Serve {
                port: DEFAULT_PORT,
                db: None
            }
        );
    }

    #[test]
    fn serve_honors_port_flag() {
        assert_eq!(
            parse_args(&args(&["serve", "--port", "9000"])),
            Command::Serve {
                port: 9000,
                db: None
            }
        );
    }

    #[test]
    fn serve_ignores_bad_port() {
        assert_eq!(
            parse_args(&args(&["serve", "--port", "notaport"])),
            Command::Serve {
                port: DEFAULT_PORT,
                db: None
            }
        );
    }

    #[test]
    fn serve_honors_db_flag() {
        assert_eq!(
            parse_args(&args(&["serve", "--db", "C:/tmp/tasks-9000.db"])),
            Command::Serve {
                port: DEFAULT_PORT,
                db: Some(PathBuf::from("C:/tmp/tasks-9000.db"))
            }
        );
    }

    #[test]
    fn serve_honors_port_and_db_flags_together() {
        assert_eq!(
            parse_args(&args(&[
                "serve",
                "--port",
                "9000",
                "--db",
                "C:/tmp/tasks-9000.db"
            ])),
            Command::Serve {
                port: 9000,
                db: Some(PathBuf::from("C:/tmp/tasks-9000.db"))
            }
        );
    }

    #[test]
    fn parses_validate_with_path() {
        assert_eq!(
            parse_args(&args(&["validate", "task.toml"])),
            Command::Validate("task.toml".to_string())
        );
        assert_eq!(
            parse_args(&args(&["validate"])),
            Command::Validate(String::new())
        );
    }

    #[test]
    fn parses_mux_with_trailing_args() {
        assert_eq!(
            parse_args(&args(&["mux", "capture-pane", "-p", "-t", "foo"])),
            Command::Mux(vec![
                "capture-pane".to_string(),
                "-p".to_string(),
                "-t".to_string(),
                "foo".to_string(),
            ])
        );
        assert_eq!(parse_args(&args(&["mux"])), Command::Mux(vec![]));
    }

    #[test]
    fn validate_file_missing_is_false() {
        assert!(!validate_file("/no/such/ralphus/file.toml"));
    }

    #[test]
    fn unknown_and_empty_fall_back_to_help() {
        assert_eq!(parse_args(&args(&[])), Command::Help(None));
        assert_eq!(parse_args(&args(&["wat"])), Command::Help(None));
    }

    #[test]
    fn parses_stop_with_defaults() {
        assert_eq!(
            parse_args(&args(&["stop"])),
            Command::Stop {
                port: DEFAULT_PORT,
                auto_cancel: false
            }
        );
    }

    #[test]
    fn stop_honors_port_and_auto_cancel_flags() {
        assert_eq!(
            parse_args(&args(&["stop", "--port", "9000", "--auto-cancel"])),
            Command::Stop {
                port: 9000,
                auto_cancel: true
            }
        );
        assert_eq!(
            parse_args(&args(&["stop", "--auto-cancel", "--port", "9000"])),
            Command::Stop {
                port: 9000,
                auto_cancel: true
            }
        );
    }

    #[test]
    fn usage_mentions_serve() {
        assert!(usage().contains("serve"));
    }

    #[test]
    fn usage_mentions_stop() {
        assert!(usage().contains("stop"));
    }

    #[test]
    fn usage_mentions_license() {
        assert!(usage().contains("license"));
    }

    #[test]
    fn help_precedes_every_daemon_command_argument() {
        for command in HELP_COMMANDS {
            assert_eq!(
                parse_args(&args(&[command.name(), "--bad", "--help"])),
                Command::Help(Some(*command))
            );
            let text = command_usage(Some(*command));
            assert!(text.contains("-h, --help"));
            assert!(text.contains("USAGE:"));
        }
    }

    #[test]
    fn help_after_separator_is_forwarded() {
        assert_eq!(
            parse_args(&args(&["mux", "--", "--help"])),
            Command::Mux(args(&["--", "--help"]))
        );
    }

    #[test]
    fn bind_host_defaults_to_loopback() {
        assert_eq!(resolve_bind_host(None), "127.0.0.1");
        assert_eq!(resolve_bind_host(Some("")), "127.0.0.1");
        assert_eq!(resolve_bind_host(Some("   ")), "127.0.0.1");
    }

    #[test]
    fn bind_host_honors_explicit_override() {
        assert_eq!(resolve_bind_host(Some("0.0.0.0")), "0.0.0.0");
        assert_eq!(resolve_bind_host(Some("  0.0.0.0  ")), "0.0.0.0");
    }
}
