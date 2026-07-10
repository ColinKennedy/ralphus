//! ralphus daemon binary entry point.

use std::process::ExitCode;

use ralphus_daemon::{
    Command, DEFAULT_MAX_CONCURRENT, DEFAULT_PORT, default_db_path, parse_args, server, usage,
    validate_file,
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
        Command::Serve => {
            if let Err(e) = ralphus_auth::check_license() {
                eprintln!("Authorization error: {e}");
                return ExitCode::FAILURE;
            }
            let daemon_cfg = ralphus_daemon::config::load_daemon_config();
            ralphus_daemon::logging::init(
                daemon_cfg.log_path.as_deref(),
                daemon_cfg.log_level.as_deref(),
            );
            let db = default_db_path();
            let addr = ("127.0.0.1", DEFAULT_PORT);
            ralphus_daemon::logging::write_line(
                ralphus_daemon::logging::LogLevel::INFO,
                &format!(
                    "ralphus-daemon serving on http://127.0.0.1:{DEFAULT_PORT} (db: {})",
                    db.display()
                ),
            );
            match server::serve(addr, &db, DEFAULT_MAX_CONCURRENT) {
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
