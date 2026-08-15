//! ralphus librarian binary entry point.

// This is a CLI: its stdout IS the product (the version string), so the
// workspace-wide `clippy::print_stdout = "deny"` is relaxed here. Log output
// still goes to stderr — see AGENTS.md's Logging Policy.
#![allow(clippy::print_stdout)]

use std::process::ExitCode;

use ralphus_librarian::{Command, DEFAULT_DAEMON_URL, parse_args, server, usage};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match parse_args(&args) {
        Command::Version => {
            println!("ralphus-librarian {}", ralphus_core::version());
            ExitCode::SUCCESS
        }
        Command::Help => {
            print!("{}", usage());
            ExitCode::SUCCESS
        }
        Command::Serve { port } => {
            if let Err(e) = ralphus_auth::check_license() {
                eprintln!("Authorization error: {e}");
                return ExitCode::FAILURE;
            }
            let daemon_url = std::env::var("RALPHUS_DAEMON_URL")
                .unwrap_or_else(|_| DEFAULT_DAEMON_URL.to_string());
            eprintln!(
                "ralphus-librarian serving on http://127.0.0.1:{port} (daemon: {daemon_url})"
            );
            let otel_provider = ralphus_librarian::otel::init("ralphus-librarian");
            let result = server::serve(port, &daemon_url);
            ralphus_librarian::otel::shutdown(otel_provider);
            match result {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("librarian failed: {e}");
                    ExitCode::FAILURE
                }
            }
        }
    }
}
