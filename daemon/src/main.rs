//! ralphus daemon binary entry point.
//!
//! This is a CLI: its stdout IS the product (version string, usage text, stop
//! confirmations), so the workspace-wide `clippy::print_stdout = "deny"` is
//! relaxed here. That lint guards the daemon<->runner JSON contract carried on
//! the *runner subprocess's* stdout; nothing in this file writes to that
//! channel. Log output still goes to stderr — see AGENTS.md's Logging Policy.
#![allow(clippy::print_stdout)]

use std::process::ExitCode;
use std::time::Duration;

use ralphus_daemon::{
    Command, DEFAULT_MAX_CONCURRENT, default_db_path, parse_args, server, usage, validate_file,
};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match parse_args(&args) {
        Command::Version => {
            println!("ralphus-daemon {}", ralphus_core::version());
            ExitCode::SUCCESS
        }
        Command::Help => {
            print!("{}", usage());
            ExitCode::SUCCESS
        }
        Command::Validate(file) => {
            if file.is_empty() {
                eprintln!("usage: ralphus-daemon validate <file>");
                return ExitCode::FAILURE;
            }
            if validate_file(&file) {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        Command::Mux(mux_args) => {
            let command_line = match ralphus_daemon::tmux::resolve_tmux_program() {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("error: {e}");
                    return ExitCode::FAILURE;
                }
            };
            // `command_line` may be a bare program or a program plus leading
            // arguments (e.g. `RALPHUS_TMUX_CMD="wsl.exe tmux"`) -- go
            // through `Tmux` just for its program()/prefix_args() split
            // rather than duplicating that parsing here.
            let tmux = ralphus_daemon::tmux::Tmux::from_program(command_line);
            match std::process::Command::new(tmux.program())
                .args(tmux.prefix_args())
                .args(&mux_args)
                .status()
            {
                Ok(status) => {
                    if status.success() {
                        ExitCode::SUCCESS
                    } else {
                        ExitCode::FAILURE
                    }
                }
                Err(e) => {
                    eprintln!("error: could not run '{}': {e}", tmux.program());
                    ExitCode::FAILURE
                }
            }
        }
        Command::Stop { port, auto_cancel } => {
            let url = format!("http://127.0.0.1:{port}/api/daemon/shutdown");
            let body = serde_json::json!({ "auto_cancel": auto_cancel }).to_string();
            match ureq::post(&url)
                .timeout(Duration::from_secs(10))
                .set("Content-Type", "application/json")
                .send_string(&body)
            {
                Ok(resp) => {
                    let text = resp.into_string().unwrap_or_default();
                    println!("daemon on port {port} is stopping: {text}");
                    ExitCode::SUCCESS
                }
                Err(ureq::Error::Transport(e)) => {
                    println!("no daemon reachable on 127.0.0.1:{port} ({e}) — nothing to stop");
                    ExitCode::SUCCESS
                }
                Err(ureq::Error::Status(code, resp)) => {
                    let text = resp.into_string().unwrap_or_default();
                    eprintln!("error: daemon returned {code}: {text}");
                    ExitCode::FAILURE
                }
            }
        }
        Command::Serve { port, db } => {
            if let Err(e) = ralphus_auth::check_license() {
                eprintln!("Authorization error: {e}");
                return ExitCode::FAILURE;
            }
            // Held for the rest of `main` (RAL-99): confines every subprocess
            // this daemon spawns, transitively, to one Job Object so ending
            // the daemon from Task Manager takes the whole tree with it.
            // Must stay a named binding, not `let _ = ...` — dropping it
            // early self-terminates the daemon (kill-on-close).
            let _job_guard = ralphus_daemon::jobobject::confine_process_tree();
            let daemon_cfg = ralphus_daemon::config::load_daemon_config();
            ralphus_daemon::logging::init(
                daemon_cfg.log_path.as_deref(),
                daemon_cfg.log_level.as_deref(),
            );
            let db = db.unwrap_or_else(default_db_path);
            let addr = ("127.0.0.1", port);
            ralphus_daemon::logging::write_line(
                ralphus_daemon::logging::LogLevel::INFO,
                &format!(
                    "ralphus-daemon serving on http://127.0.0.1:{port} (db: {})",
                    db.display()
                ),
            );
            let otel_provider = ralphus_daemon::otel::init("ralphus-daemon");
            let result = server::serve(addr, &db, DEFAULT_MAX_CONCURRENT);
            ralphus_daemon::otel::shutdown(otel_provider);
            match result {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    ralphus_daemon::logging::write_line(
                        ralphus_daemon::logging::LogLevel::ERROR,
                        &format!("ralphus-daemon failed: {e}"),
                    );
                    ExitCode::FAILURE
                }
            }
        }
    }
}
