//! ralphus librarian library.
//!
//! The librarian serves the web UI and proxies/queries the daemon's HTTP API.
//! It never touches the database directly and never starts the daemon on its
//! own (a future flag will make that opt-in). Keeping the logic in a library
//! makes the argument parsing and daemon-URL resolution unit-testable.

pub mod config;
pub mod otel;
pub mod server;

/// Default port the librarian listens on.
pub const DEFAULT_PORT: u16 = 7474;

/// Default base URL of the daemon API the librarian talks to.
pub const DEFAULT_DAEMON_URL: &str = "http://127.0.0.1:7890";

/// Overrides the host the librarian's HTTP listener binds to.
pub const BIND_ADDR_ENV: &str = "RALPHUS_BIND_ADDR";

/// Resolve the host the librarian's HTTP listener binds to: `env_override`
/// (read from [`BIND_ADDR_ENV`] by the caller) when non-empty, else the
/// bare-subprocess default `127.0.0.1`. Mirrors
/// `ralphus_daemon::resolve_bind_host` -- see its doc comment for why the
/// default must stay `127.0.0.1` outside the container execution mode
/// (RAL-225).
#[must_use]
pub fn resolve_bind_host(env_override: Option<&str>) -> String {
    match env_override.map(str::trim) {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => "127.0.0.1".to_string(),
    }
}

/// Command-line action parsed from the librarian's arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Print version and exit.
    Version,
    /// Print the embedded LICENSE text and exit.
    License,
    /// Print usage and exit.
    Help,
    /// Serve the web UI on the given port. Not yet implemented in this phase.
    Serve { port: u16 },
}

/// Parse librarian CLI arguments (excluding the program name).
///
/// Supports `serve [--port N]`. Unknown input falls back to `Help`.
#[must_use]
pub fn parse_args(args: &[String]) -> Command {
    match args.first().map(String::as_str) {
        Some("--version" | "-V" | "version") => Command::Version,
        Some("license") => Command::License,
        Some("serve") => {
            let port = parse_port_flag(&args[1..]).unwrap_or(DEFAULT_PORT);
            Command::Serve { port }
        }
        _ => Command::Help,
    }
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

/// The usage string shown for `help` / unknown commands.
#[must_use]
pub fn usage() -> String {
    format!(
        "ralphus-librarian {}\n\nUSAGE:\n    ralphus-librarian <COMMAND>\n\nCOMMANDS:\n    serve [--port {DEFAULT_PORT}]   Run the board web server\n    license           Print the embedded LICENSE text\n    version           Print version and exit\n    help              Print this message\n\nThe librarian renders the daemon's state at {DEFAULT_DAEMON_URL}.\nIt fails gracefully if the daemon is not running.\n",
        ralphus_core::version()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn serve_uses_default_port() {
        assert_eq!(
            parse_args(&args(&["serve"])),
            Command::Serve { port: DEFAULT_PORT }
        );
    }

    #[test]
    fn serve_honors_port_flag() {
        assert_eq!(
            parse_args(&args(&["serve", "--port", "9000"])),
            Command::Serve { port: 9000 }
        );
    }

    #[test]
    fn serve_ignores_bad_port() {
        assert_eq!(
            parse_args(&args(&["serve", "--port", "notaport"])),
            Command::Serve { port: DEFAULT_PORT }
        );
    }

    #[test]
    fn version_and_help() {
        assert_eq!(parse_args(&args(&["-V"])), Command::Version);
        assert_eq!(parse_args(&args(&["license"])), Command::License);
        assert_eq!(parse_args(&args(&[])), Command::Help);
        assert!(usage().contains("librarian"));
        assert!(usage().contains("license"));
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

    #[test]
    fn help_precedes_every_librarian_command_argument() {
        for command in HELP_COMMANDS {
            assert_eq!(
                parse_args(&args(&[command.name(), "--bad", "--help"])),
                Command::Help(Some(*command))
            );
            assert!(command_usage(Some(*command)).contains("-h, --help"));
        }
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
