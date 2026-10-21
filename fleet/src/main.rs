//! `ralphus-fleet` command line.
//!
//! ```text
//! ralphus-fleet serve [--config PATH]
//! ralphus-fleet list  [--config PATH]
//! ralphus-fleet add <host> --remote-root PATH [--user U] [--name N]
//!                   [--os OS] [--arch ARCH] [--label L]... [--require-agent A]... [--config PATH]
//! ralphus-fleet add --outpost --name N --remote-root PATH [--token-env VAR]
//!                   [--os OS] [--arch ARCH] [--label L]... [--require-agent A]... [--config PATH]
//! ralphus-fleet pair <machine> [--config PATH]
//! ralphus-fleet revoke <machine> [--config PATH]
//! ```
//!
//! The configuration defaults to `ralphus-fleet.toml` in the current
//! directory. The service token is read from the variable its `token_env`
//! names; it is never written to the configuration.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use ralphus_fleet::allocations::Allocations;
use ralphus_fleet::config::{FleetConfig, MachineConfig, RunnerConfig};
use ralphus_fleet::outpost_keys;
use ralphus_fleet::outpost_registry::Registry;
use ralphus_fleet::outposts::Outposts;
use ralphus_fleet::secret_store::OsStore;
use ralphus_fleet::server::{Service, run};
use ralphus_fleet::{fleet_identity, readiness};

const USAGE: &str = "usage: ralphus-fleet <serve|list|add|pair|revoke> [--config PATH]\n\
    \x20 serve                 serve the allocation API and Machine API\n\
    \x20 list                  show the machines, their readiness and current allocations\n\
    \x20 add <host> --remote-root PATH [--user U] [--name N] [--os OS] [--arch ARCH] [--label L]...\n\
    \x20                       [--require-agent A]...  add a machine reachable over SSH\n\
    \x20 add --outpost --name N --remote-root PATH [--token-env VAR] [--os OS] [--arch ARCH]\n\
    \x20                       [--label L]... [--require-agent A]...\n\
    \x20                       add a machine that connects with ralphus-outpost\n\
    \x20 pair <machine>        print a one-time code for `ralphus-outpost install --code`\n\
    \x20 revoke <machine>      forget a paired machine's credential; it can no longer connect";

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

/// Where the fleet keeps its identity key when the OS store is unreachable.
fn identity_fallback_dir(config_path: &Path) -> PathBuf {
    config_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
        .join(".ralphus-fleet-secrets")
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
    let registry = Registry::new(config.registry_path(&path));
    let mut outposts = Outposts::new(&config, |variable| std::env::var(variable).ok())
        .with_registry(registry.clone());
    if config.machines.iter().any(|m| m.is_outpost()) {
        let has_pairings = registry
            .load()?
            .machines
            .values()
            .any(|e| e.public_key.is_some());
        let identity = fleet_identity::load_or_create(
            &OsStore::new(fleet_identity::SERVICE),
            &identity_fallback_dir(&path),
            &fleet_identity::key_for(registry.path()),
            has_pairings,
        )?;
        eprintln!(
            "ralphus-fleet: identity {} (outposts pair with this key)",
            outpost_keys::fingerprint(&identity.public_key())
        );
        outposts = outposts.with_identity(identity);
    }
    run(Service::with_outposts(
        config,
        base_url,
        token,
        allocations,
        outposts,
    ))
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
    let registry = Registry::new(config.registry_path(&path)).load()?;
    let mut out = std::io::stdout().lock();
    for machine in &config.machines {
        let name = machine.display_name();
        let entry = registry.machines.get(name);
        let reach = if machine.is_outpost() {
            match entry.and_then(|e| e.public_key.as_deref()) {
                Some(key) => format!("outpost, paired {}", outpost_keys::fingerprint(key)),
                None if machine.outpost_token_env.is_some() => {
                    "outpost, pairing secret".to_string()
                }
                None => "outpost, NOT PAIRED (run `ralphus-fleet pair`)".to_string(),
            }
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
        if machine.is_outpost() {
            write_readiness(&mut out, machine, entry.and_then(|e| e.readiness.as_ref()));
        }
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

fn write_readiness(
    out: &mut impl std::io::Write,
    machine: &MachineConfig,
    report: Option<&readiness::Report>,
) {
    let Some(report) = report else {
        let _ = writeln!(
            out,
            "    readiness: not reported yet (it reports on connecting)"
        );
        return;
    };
    let name = machine.display_name();
    let required: Vec<String> = machine
        .required_agents
        .iter()
        .filter_map(|a| readiness::canonical_agent(a).ok())
        .map(str::to_string)
        .collect();
    let _ = writeln!(out, "    readiness: {}", report.summary(name, &required));
    let agents = report.agents();
    if !agents.is_empty() {
        let states: Vec<String> = agents
            .iter()
            .map(|(agent, state)| {
                let state = match state {
                    readiness::AgentState::Ready => "ready",
                    readiness::AgentState::LoginUnclear => "installed, login unclear",
                    readiness::AgentState::NotInstalled(_) => "not installed",
                    readiness::AgentState::NotLoggedIn => "NOT LOGGED IN",
                    readiness::AgentState::NotChecked => "not checked",
                };
                format!("{agent} {state}")
            })
            .collect();
        let _ = writeln!(out, "      agents: {}", states.join(", "));
    }
    // Fixes for what blocks the required agents, and for any installed
    // agent that is not logged in.
    let mut fix_for: Vec<String> = required;
    fix_for.extend(
        agents
            .iter()
            .filter(|(_, s)| *s == readiness::AgentState::NotLoggedIn)
            .map(|(a, _)| a.clone()),
    );
    fix_for.sort();
    fix_for.dedup();
    for blocker in report.blockers(name, &fix_for) {
        let _ = writeln!(out, "      fix: {}", blocker.fix);
    }
    for check in report.machine_problems() {
        let _ = writeln!(out, "      {} {}: {}", check.status, check.id, check.detail);
        if let Some(remedy) = &check.remedy {
            let _ = writeln!(out, "        fix: {remedy}");
        }
    }
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
    let mut required_agents = Vec::new();
    while let Some(agent) = take(&mut args, "--require-agent")? {
        required_agents.push(agent);
    }
    let host = if outpost {
        if !args.is_empty() {
            return Err(format!("add --outpost takes no <host>\n{USAGE}"));
        }
        if name.is_none() {
            return Err(format!("add --outpost needs --name\n{USAGE}"));
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
    let paired = outpost && token_env.is_none();
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
        required_agents,
    });
    // Validate the result before writing it.
    let text = toml::to_string_pretty(&config).map_err(|e| e.to_string())?;
    FleetConfig::parse(&text, &path.display().to_string())?;
    config.save(&path)?;
    eprintln!("ralphus-fleet: added {shown} to {}", path.display());
    if paired {
        eprintln!(
            "ralphus-fleet: pair it with `ralphus-fleet pair {shown}` (restart `ralphus-fleet \
             serve` first so it serves the new machine)"
        );
    }
    Ok(())
}

/// The outpost machine `name` in `config`.
fn outpost_machine<'a>(config: &'a FleetConfig, name: &str) -> Result<&'a MachineConfig, String> {
    let machine = config
        .machines
        .iter()
        .find(|m| m.display_name() == name)
        .ok_or_else(|| format!("no machine {name:?} in the configuration"))?;
    if !machine.is_outpost() {
        return Err(format!(
            "{name:?} is reached over SSH; only outpost machines pair (add one with \
             `ralphus-fleet add --outpost --name {name} --remote-root <path>`)"
        ));
    }
    Ok(machine)
}

#[allow(clippy::print_stdout)] // the code is this command's product
fn pair(mut args: Vec<String>) -> Result<(), String> {
    let path = config_path(&mut args)?;
    let [name] = args.as_slice() else {
        return Err(format!("pair needs exactly one <machine>\n{USAGE}"));
    };
    let config = FleetConfig::load(&path)?;
    let machine = outpost_machine(&config, name)?;
    let registry = Registry::new(config.registry_path(&path));
    let code = registry.start_pairing(machine.display_name())?;
    let url = config.public_url.clone().unwrap_or_else(|| {
        let port = config.listen.rsplit_once(':').map_or("7900", |(_, p)| p);
        format!("http://<this host's address>:{port}")
    });
    let mut out = std::io::stdout().lock();
    let _ = writeln!(
        out,
        "Pairing code for {name} (single use, valid for {} minutes):\n\n    {code}\n\n\
         On {name}, run:\n\n    ralphus-outpost install --fleet {url} --code {code}\n",
        outpost_keys::PAIRING_TTL_MS / 60_000
    );
    if config.public_url.is_none() {
        let _ = writeln!(
            out,
            "(use the URL {name} reaches this fleet at; set public_url in {} to print it here)",
            path.display()
        );
    }
    Ok(())
}

fn revoke(mut args: Vec<String>) -> Result<(), String> {
    let path = config_path(&mut args)?;
    let [name] = args.as_slice() else {
        return Err(format!("revoke needs exactly one <machine>\n{USAGE}"));
    };
    let config = FleetConfig::load(&path)?;
    let machine = outpost_machine(&config, name)?;
    let registry = Registry::new(config.registry_path(&path));
    if registry.revoke(machine.display_name())? {
        eprintln!(
            "ralphus-fleet: revoked {name}; its outpost is refused from its next poll. Pair it \
             again with `ralphus-fleet pair {name}`"
        );
    } else {
        eprintln!("ralphus-fleet: {name} was not paired");
    }
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
        "pair" => pair(args),
        "revoke" => revoke(args),
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
    use super::*;

    #[test]
    fn a_wildcard_listen_address_is_advertised_as_loopback() {
        assert_eq!(default_base_url("0.0.0.0:7900"), "http://127.0.0.1:7900");
        assert_eq!(default_base_url("[::]:7900"), "http://127.0.0.1:7900");
        assert_eq!(default_base_url("10.0.0.5:7900"), "http://10.0.0.5:7900");
    }

    #[test]
    fn list_shows_what_to_fix_on_a_machine_that_is_not_ready() {
        let config = FleetConfig::parse(
            "token_env = \"T\"\n[[machine]]\nname = \"winbox\"\ntransport = \"outpost\"\n\
             remote_root = \"C:/r\"\nrequired_agents = [\"claude-code\"]\n",
            "f",
        )
        .unwrap();
        let report = readiness::Report {
            checked_at_ms: 1,
            checks: vec![
                readiness::Check::new("git", readiness::PASS, "git 2.45"),
                readiness::Check::new("agent:claude-code", readiness::PASS, "claude can start"),
                readiness::Check::new("agent-login:claude-code", readiness::FAIL, "not logged in")
                    .with_remedy("run `claude auth login` on the machine"),
            ],
        };
        let mut out = Vec::new();
        write_readiness(&mut out, &config.machines[0], Some(&report));
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("NOT READY (1 blocking)"), "{text}");
        assert!(text.contains("agents: claude-code NOT LOGGED IN"), "{text}");
        assert!(
            text.contains(
                "fix: Claude Code on winbox isn't logged in — on that machine run `claude`, then \
                 `/login`"
            ),
            "{text}"
        );

        assert!(
            !text.contains("git 2.45"),
            "passing checks stay quiet: {text}"
        );
    }
}
