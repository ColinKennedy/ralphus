//! `ralphus-fleet` command line.
//!
//! ```text
//! ralphus-fleet serve [--config PATH]
//! ralphus-fleet list  [--config PATH]
//! ralphus-fleet add <host> --remote-root PATH [--user U] [--name N]
//!                   [--os OS] [--arch ARCH] [--label L]... [--config PATH]
//! ralphus-fleet add --outpost --name N --token-env VAR --remote-root PATH
//!                   [--os OS] [--arch ARCH] [--label L]... [--config PATH]
//! ```
//!
//! The configuration defaults to `ralphus-fleet.toml` in the current
//! directory. The service token is read from the variable its `token_env`
//! names; it is never written to the configuration.

use std::io::Write as _;
use std::path::PathBuf;
use std::process::ExitCode;

use ralphus_fleet::allocations::Allocations;
use ralphus_fleet::config::{FleetConfig, MachineConfig, RunnerConfig};
use ralphus_fleet::server::{Service, run};

const USAGE: &str = "usage: ralphus-fleet <serve|list|add> [--config PATH]\n\
    \x20 serve                 serve the allocation API and Machine API\n\
    \x20 list                  show the configured machines and current allocations\n\
    \x20 add <host> --remote-root PATH [--user U] [--name N] [--os OS] [--arch ARCH] [--label L]...\n\
    \x20                       add a machine reachable over SSH\n\
    \x20 add --outpost --name N --token-env VAR --remote-root PATH [--os OS] [--arch ARCH] [--label L]...\n\
    \x20                       add a machine that connects with ralphus-outpost";

/// Pull `--name value` out of `args`.
fn take(args: &mut Vec<String>, name: &str) -> Result<Option<String>, String> {
    let Some(at) = args.iter().position(|a| a == name) else {
        return Ok(None);
    };
    if at + 1 >= args.len() {
        return Err(format!("{name} needs a value"));
    }
    let value = args.remove(at + 1);
    args.remove(at);
    Ok(Some(value))
}

fn config_path(args: &mut Vec<String>) -> Result<PathBuf, String> {
    Ok(take(args, "--config")?.map_or_else(|| PathBuf::from("ralphus-fleet.toml"), PathBuf::from))
}

fn serve(mut args: Vec<String>) -> Result<(), String> {
    let path = config_path(&mut args)?;
    let config = FleetConfig::load(&path)?;
    let token = std::env::var(&config.token_env)
        .ok()
        .filter(|t| !t.trim().is_empty())
        .ok_or_else(|| {
            format!(
                "{} is not set; set it to the token ralphus will present (the daemon's \
                 [machine.providers.<name>] token_env must hold the same value)",
                config.token_env
            )
        })?;
    let allocations = Allocations::load(config.state_path(&path))?;
    let base_url = config
        .public_url
        .clone()
        .unwrap_or_else(|| default_base_url(&config.listen));
    run(Service::new(config, base_url, token, allocations))
}

/// `http://<listen>`, except that a wildcard address (`0.0.0.0`, `[::]`),
/// which no client can connect to, is given as loopback: set `public_url`
/// when ralphus runs on another host.
fn default_base_url(listen: &str) -> String {
    let (host, port) = listen.rsplit_once(':').unwrap_or((listen, ""));
    let host = match host {
        "0.0.0.0" | "[::]" | "::" => "127.0.0.1",
        other => other,
    };
    if port.is_empty() {
        format!("http://{host}")
    } else {
        format!("http://{host}:{port}")
    }
}

#[allow(clippy::print_stdout)] // the listing is this command's product
fn list(mut args: Vec<String>) -> Result<(), String> {
    let path = config_path(&mut args)?;
    let config = FleetConfig::load(&path)?;
    let allocations = Allocations::load(config.state_path(&path))?;
    let mut out = std::io::stdout().lock();
    for machine in &config.machines {
        let name = machine.display_name();
        let reach = if machine.is_outpost() {
            "outpost".to_string()
        } else {
            format!("ssh {}", machine.ssh_uri())
        };
        let _ = writeln!(
            out,
            "{name}  {reach}  {}/{}  labels [{}]  allocations {}",
            machine.os.as_deref().unwrap_or("?"),
            machine.arch.as_deref().unwrap_or("?"),
            machine.labels.join(", "),
            allocations.load_of(name)
        );
        for allocation in allocations.all().into_iter().filter(|a| a.machine == name) {
            let _ = writeln!(
                out,
                "    {}{} {}",
                allocation.id,
                if allocation.idle { " (idle)" } else { "" },
                allocation.scope
            );
        }
    }
    if config.machines.is_empty() {
        let _ = writeln!(out, "no machines; add one with `ralphus-fleet add <host>`");
    }
    Ok(())
}

fn add(mut args: Vec<String>) -> Result<(), String> {
    let path = config_path(&mut args)?;
    let remote_root = take(&mut args, "--remote-root")?
        .ok_or_else(|| "add needs --remote-root (where projects go on the machine)".to_string())?;
    let user = take(&mut args, "--user")?;
    let name = take(&mut args, "--name")?;
    let outpost = match args.iter().position(|a| a == "--outpost") {
        Some(at) => {
            args.remove(at);
            true
        }
        None => false,
    };
    let token_env = take(&mut args, "--token-env")?;
    let os = take(&mut args, "--os")?;
    let arch = take(&mut args, "--arch")?;
    let mut labels = Vec::new();
    while let Some(label) = take(&mut args, "--label")? {
        labels.push(label);
    }
    let host = if outpost {
        if !args.is_empty() {
            return Err(format!("add --outpost takes no <host>\n{USAGE}"));
        }
        if name.is_none() || token_env.is_none() {
            return Err(format!(
                "add --outpost needs --name and --token-env\n{USAGE}"
            ));
        }
        String::new()
    } else {
        if token_env.is_some() {
            return Err("--token-env is only for --outpost machines".to_string());
        }
        let [host] = args.as_slice() else {
            return Err(format!("add needs exactly one <host>\n{USAGE}"));
        };
        host.clone()
    };
    let shown = name.clone().unwrap_or_else(|| host.clone());
    let mut config = if path.exists() {
        FleetConfig::load(&path)?
    } else {
        FleetConfig::parse("token_env = \"RALPHUS_FLEET_TOKEN\"\n", "defaults")?
    };
    config.machines.push(MachineConfig {
        name,
        host,
        transport: outpost.then(|| ralphus_fleet::config::TRANSPORT_OUTPOST.to_string()),
        outpost_token_env: token_env,
        user,
        os,
        arch,
        labels,
        remote_root,
        max_allocations: None,
        runner: RunnerConfig::default(),
        agents: std::collections::BTreeMap::new(),
    });
    // Validate the result before writing it.
    let text = toml::to_string_pretty(&config).map_err(|e| e.to_string())?;
    FleetConfig::parse(&text, &path.display().to_string())?;
    config.save(&path)?;
    eprintln!("ralphus-fleet: added {shown} to {}", path.display());
    Ok(())
}

fn main() -> ExitCode {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    }
    let command = args.remove(0);
    let result = match command.as_str() {
        "serve" => serve(args),
        "list" => list(args),
        "add" => add(args),
        "help" | "--help" | "-h" => {
            eprintln!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        other => Err(format!("unknown command {other:?}\n{USAGE}")),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("ralphus-fleet: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::default_base_url;

    #[test]
    fn a_wildcard_listen_address_is_advertised_as_loopback() {
        assert_eq!(default_base_url("0.0.0.0:7900"), "http://127.0.0.1:7900");
        assert_eq!(default_base_url("[::]:7900"), "http://127.0.0.1:7900");
        assert_eq!(default_base_url("10.0.0.5:7900"), "http://10.0.0.5:7900");
    }
}
