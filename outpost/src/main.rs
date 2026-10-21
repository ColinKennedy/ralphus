//! `ralphus-outpost` command line.
//!
//! ```text
//! ralphus-outpost install --fleet <url> --code <code> [--remote-root <path>] [--service]
//!                         [--dry-run <dir>] [--home <dir>]
//! ralphus-outpost status    [--home <dir>]
//! ralphus-outpost uninstall [--home <dir>]
//! ralphus-outpost serve     [--home <dir>]   what the installed autostart runs
//! ralphus-outpost connect <fleet-url> --name <machine> --token-env <VAR> --remote-root <path>
//! ralphus-outpost runner <args...>          the built-in ralphus-runner (jobs run through it)
//! ralphus-outpost --version
//! ```
//!
//! `install` pairs with a one-time code and keeps the machine credential in
//! the OS's encrypted per-user store. `connect` authenticates with a
//! pairing secret read from the environment variable `--token-env` names.
//! Neither a credential nor a secret is ever accepted on the command line.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use ralphus_fleet::secret_store::OsStore;
use ralphus_outpost::settings::{CREDENTIAL_SERVICE, Home, default_home};
use ralphus_outpost::{Auth, Options, install, log, logfile};

const USAGE: &str = "usage: ralphus-outpost <install|status|uninstall|serve|connect|runner> ...\n\
    \x20 install --fleet <url> --code <code> [--remote-root <path>] [--service] [--dry-run <dir>]\n\
    \x20           pair with the fleet (code from `ralphus-fleet pair <machine>`), keep the\n\
    \x20           credential in the OS store and start automatically from now on\n\
    \x20           --service   Windows: start at boot whether or not you are logged on (elevated);\n\
    \x20                       Linux: also `loginctl enable-linger` (start at boot)\n\
    \x20           --dry-run   write the autostart definition to <dir> and change nothing\n\
    \x20 status    show the installed outpost: settings, credential, autostart, connection, log\n\
    \x20 uninstall stop it, remove the autostart and forget the credential\n\
    \x20 serve     run the installed outpost (what the autostart runs)\n\
    \x20 connect <fleet-url> --name <machine> --token-env <VAR> --remote-root <path>\n\
    \x20           run once with a pairing secret from an environment variable\n\
    \x20 runner    the built-in ralphus-runner (used by jobs)\n\
    \x20 every command but connect/runner takes --home <dir> (default: the per-user app data dir)\n\
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

fn take_switch(args: &mut Vec<String>, flag: &str) -> bool {
    match args.iter().position(|a| a == flag) {
        Some(at) => {
            args.remove(at);
            true
        }
        None => false,
    }
}

fn refuse_secret_flags(args: &[String]) -> Result<(), String> {
    if args.iter().any(|a| {
        a == "--token"
            || a.starts_with("--token=")
            || a == "--credential"
            || a.starts_with("--credential=")
    }) {
        return Err(
            "secrets are never taken on the command line (other users can read it there); pair \
             with `install --code` or put a pairing secret in an environment variable and pass \
             --token-env <VAR>"
                .to_string(),
        );
    }
    Ok(())
}

fn home(args: &mut Vec<String>) -> Result<Home, String> {
    Ok(Home::new(match take(args, "--home")? {
        Some(dir) => PathBuf::from(dir),
        None => default_home()?,
    }))
}

fn no_more(args: &[String], command: &str) -> Result<(), String> {
    if args.is_empty() {
        Ok(())
    } else {
        Err(format!("{command}: unexpected {args:?}\n{USAGE}"))
    }
}

fn builtin_runner() -> Result<Vec<String>, String> {
    let exe =
        std::env::current_exe().map_err(|e| format!("could not find this executable: {e}"))?;
    Ok(vec![
        exe.to_string_lossy().into_owned(),
        "runner".to_string(),
    ])
}

fn install_command(mut args: Vec<String>) -> Result<(), String> {
    refuse_secret_flags(&args)?;
    let home = home(&mut args)?;
    let fleet_url = take(&mut args, "--fleet")?.ok_or("install needs --fleet <url>")?;
    let code = take(&mut args, "--code")?.ok_or(
        "install needs --code <code> (run `ralphus-fleet pair <machine>` on the fleet host)",
    )?;
    let remote_root = take(&mut args, "--remote-root")?;
    let dry_run = take(&mut args, "--dry-run")?.map(PathBuf::from);
    let service = take_switch(&mut args, "--service");
    no_more(&args, "install")?;
    install::install(&install::InstallOptions {
        fleet_url,
        code,
        remote_root,
        service,
        dry_run,
        home,
    })
}

/// Run the installed outpost until the process is stopped. A credential
/// that cannot be read yet (a login keyring still locked after boot) is
/// retried rather than fatal, so the autostart does not give up.
fn serve(mut args: Vec<String>) -> Result<(), String> {
    let home = home(&mut args)?;
    no_more(&args, "serve")?;
    logfile::open(&home.log_file())?;
    let settings = home.load()?.ok_or_else(|| {
        format!(
            "not installed (no {}); run `ralphus-outpost install --fleet <url> --code <code>`",
            home.settings_file().display()
        )
    })?;
    let os = OsStore::new(CREDENTIAL_SERVICE);
    let credential = loop {
        match home.load_credential(&os, settings.credential_store) {
            Ok(credential) => break credential,
            Err(e) => {
                log(&format!(
                    "could not read the machine credential: {e}; retrying in 30 s"
                ));
                std::thread::sleep(Duration::from_secs(30));
            }
        }
    };
    log(&format!(
        "starting: {:?} for {} (version {})",
        settings.name,
        settings.fleet_url,
        env!("CARGO_PKG_VERSION")
    ));
    let options = Options {
        fleet_url: settings.fleet_url,
        name: settings.name,
        auth: Auth::Paired {
            machine: Arc::clone(&credential.machine),
            fleet_public_key: credential.fleet_public_key,
        },
        remote_root: settings.remote_root,
        runner: builtin_runner()?,
        runner_cache: Some(home.dir.join("runners")),
        check_readiness: true,
        status_file: Some(home.status_file()),
    };
    ralphus_outpost::run(&options, &AtomicBool::new(false))
}

fn connect(mut args: Vec<String>) -> Result<(), String> {
    refuse_secret_flags(&args)?;
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
    let options = Options {
        fleet_url: fleet_url.trim_end_matches('/').to_string(),
        name,
        auth: Auth::Secret(secret),
        remote_root,
        runner: builtin_runner()?,
        runner_cache: None,
        check_readiness: true,
        status_file: None,
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
        "install" => install_command(args),
        "uninstall" => home(&mut args).and_then(|home| {
            no_more(&args, "uninstall")?;
            install::uninstall(&home)
        }),
        "status" => home(&mut args).and_then(|home| {
            no_more(&args, "status")?;
            install::status(&home)
        }),
        "serve" => serve(args),
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
