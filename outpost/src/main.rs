//! `ralphus-outpost` command line.
//!
//! ```text
//! ralphus-outpost connect <fleet-url> --name <machine> --token-env <VAR> --remote-root <path>
//! ralphus-outpost runner <args...>      the built-in ralphus-runner (jobs run through it)
//! ralphus-outpost --version
//! ```
//!
//! The pairing secret is read from the environment variable `--token-env`
//! names; it is never accepted on the command line.

use std::process::ExitCode;
use std::sync::atomic::AtomicBool;

const USAGE: &str = "usage: ralphus-outpost connect <fleet-url> --name <machine> --token-env <VAR> --remote-root <path>\n\
    \x20 connect   connect out to ralphus-fleet and serve this machine's Machine API\n\
    \x20           --name         this machine's name in the fleet's configuration\n\
    \x20           --token-env    environment variable holding the pairing secret\n\
    \x20           --remote-root  where projects and jobs live on this machine\n\
    \x20 runner    the built-in ralphus-runner (used by jobs)\n\
    \x20 --version print the version";

fn take(args: &mut Vec<String>, flag: &str) -> Result<Option<String>, String> {
    let Some(at) = args.iter().position(|a| a == flag) else {
        return Ok(None);
    };
    if at + 1 >= args.len() {
        return Err(format!("{flag} needs a value"));
    }
    let value = args.remove(at + 1);
    args.remove(at);
    Ok(Some(value))
}

fn connect(mut args: Vec<String>) -> Result<(), String> {
    if args
        .iter()
        .any(|a| a == "--token" || a.starts_with("--token="))
    {
        return Err(
            "the pairing secret is never taken on the command line (other users can read it \
             there); put it in an environment variable and pass --token-env <VAR>"
                .to_string(),
        );
    }
    let name = take(&mut args, "--name")?.ok_or("connect needs --name <machine>")?;
    let token_env = take(&mut args, "--token-env")?.ok_or("connect needs --token-env <VAR>")?;
    let remote_root =
        take(&mut args, "--remote-root")?.ok_or("connect needs --remote-root <path>")?;
    let [fleet_url] = args.as_slice() else {
        return Err(format!("connect needs exactly one <fleet-url>\n{USAGE}"));
    };
    let secret = std::env::var(&token_env)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            format!("{token_env} is not set; set it to this machine's pairing secret")
        })?;
    let exe =
        std::env::current_exe().map_err(|e| format!("could not find this executable: {e}"))?;
    let options = ralphus_outpost::Options {
        fleet_url: fleet_url.trim_end_matches('/').to_string(),
        name,
        secret,
        remote_root,
        runner: vec![exe.to_string_lossy().into_owned(), "runner".to_string()],
    };
    ralphus_outpost::run(&options, &AtomicBool::new(false))
}

fn main() -> ExitCode {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    }
    let command = args.remove(0);
    let result = match command.as_str() {
        "connect" => connect(args),
        "runner" => return ralphus_runner::cli::main_with_args(&args),
        "--version" | "version" => {
            eprintln!("ralphus-outpost {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        "help" | "--help" | "-h" => {
            eprintln!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        other => Err(format!("unknown command {other:?}\n{USAGE}")),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("ralphus-outpost: {e}");
            ExitCode::FAILURE
        }
    }
}
