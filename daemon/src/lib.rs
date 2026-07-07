//! ralphus daemon library.
//!
//! The daemon is the only process that touches the SQLite database; the CLI and
//! librarian reach it exclusively through its HTTP/JSON API (see
//! `docs/daemon-api.md`). Splitting the logic into a library (with a thin
//! `main.rs` binary on top) keeps it unit-testable.

pub mod cancel;
pub mod chat_client;
pub mod config;
pub mod guardian;
pub mod guardian_merge;
pub mod plan;
pub mod procreg;
pub mod resources;
pub mod reviews;
pub mod runner;
pub mod scheduler;
pub mod server;
pub mod store;
pub mod verify;

use std::path::PathBuf;

/// Default port the daemon's HTTP API listens on.
pub const DEFAULT_PORT: u16 = 7890;

/// Default maximum number of concurrently running runs.
pub const DEFAULT_MAX_CONCURRENT: i64 = 12;

/// Resolve the daemon's state directory (`~/.ralphus`), creating it if needed.
///
/// Falls back to `./.ralphus` when no home directory is set.
#[must_use]
pub fn state_dir() -> PathBuf {
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map_or_else(|| PathBuf::from("."), PathBuf::from);
    let dir = home.join(".ralphus");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// Path to the daemon's SQLite database.
#[must_use]
pub fn default_db_path() -> PathBuf {
    state_dir().join("tasks.db")
}

/// Command-line action parsed from the daemon's arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Print version and exit.
    Version,
    /// Print usage and exit.
    Help,
    /// Run the daemon (serve the HTTP API).
    Serve,
    /// Validate a task TOML file offline (no server, no database).
    Validate(String),
}

/// Parse daemon CLI arguments (excluding the program name).
///
/// Unknown or missing subcommands fall back to `Help` so the binary always has
/// a well-defined, non-panicking behavior.
#[must_use]
pub fn parse_args(args: &[String]) -> Command {
    match args.first().map(String::as_str) {
        Some("--version" | "-V" | "version") => Command::Version,
        Some("serve") => Command::Serve,
        Some("validate") => Command::Validate(args.get(1).cloned().unwrap_or_default()),
        _ => Command::Help,
    }
}

/// The usage string shown for `help` / unknown commands.
#[must_use]
pub fn usage() -> String {
    format!(
        "ralphus-daemon {}\n\nUSAGE:\n    ralphus-daemon <COMMAND>\n\nCOMMANDS:\n    serve             Run the daemon and serve the HTTP/JSON API\n    validate <file>   Validate a task TOML file offline (no server needed)\n    version           Print version and exit\n    help              Print this message\n",
        ralphus_core::version()
    )
}

/// Validate a task TOML file with the core validator, printing findings.
/// Returns `true` when the file is valid. This needs no server or database.
#[must_use]
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
    fn parses_serve() {
        assert_eq!(parse_args(&args(&["serve"])), Command::Serve);
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
    fn validate_file_missing_is_false() {
        assert!(!validate_file("/no/such/ralphus/file.toml"));
    }

    #[test]
    fn unknown_and_empty_fall_back_to_help() {
        assert_eq!(parse_args(&args(&[])), Command::Help);
        assert_eq!(parse_args(&args(&["wat"])), Command::Help);
    }

    #[test]
    fn usage_mentions_serve() {
        assert!(usage().contains("serve"));
    }
}
