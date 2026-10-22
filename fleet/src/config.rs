//! `ralphus-fleet.toml`: where the service listens, the environment variable
//! holding the bearer token ralphus must present, and the machines it may
//! hand out. Unknown keys are errors.
//!
//! ```toml
//! listen    = "127.0.0.1:7900"
//! token_env = "RALPHUS_FLEET_TOKEN"
//!
//! [[machine]]
//! host        = "wslbox"              # ssh host, `user@host` or ~/.ssh/config alias
//! os          = "linux"               # optional: probed when omitted
//! labels      = ["gpu"]
//! remote_root = "/home/me/ralphus-fleet"
//! [machine.runner]
//! mode     = "upload"
//! artifacts = { "linux-x86_64" = "C:/ralphus/linux/ralphus-runner" }
//! [machine.agents]
//! codex = "/home/me/.local/bin/codex"
//!
//! [[machine]]
//! name              = "winbox"            # reached through ralphus-outpost
//! transport         = "outpost"
//! outpost_token_env = "WINBOX_OUTPOST_TOKEN"
//! remote_root       = "C:/ralphus/fleet"
//! ```
//!
//! `transport` picks how the service reaches a machine: `ssh` (the default,
//! through `ralphus-ssh-provider`) or `outpost` (the machine runs
//! `ralphus-outpost connect`, which connects out to this service; see
//! `docs/outpost.md`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The whole configuration file.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FleetConfig {
    /// `host:port` to listen on.
    #[serde(default = "default_listen")]
    pub listen: String,
    /// The URL ralphus reaches this service at, when it differs from
    /// `http://<listen>` (a proxy, another host name); allocations' Machine
    /// API endpoints are under it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_url: Option<String>,
    /// Environment variable holding the token ralphus presents. The token
    /// itself is never written to this file.
    pub token_env: String,
    /// The `ralphus-ssh-provider` program that reaches machines; found next
    /// to this executable, then on `PATH`, when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh_provider: Option<String>,
    /// OpenSSH client config passed to every connection (`ssh -F`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh_config_file: Option<String>,
    /// Where allocations are kept across restarts (never their tokens);
    /// `<config>.state.json` when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_file: Option<String>,
    #[serde(default, rename = "machine")]
    pub machines: Vec<MachineConfig>,
}

fn default_listen() -> String {
    "127.0.0.1:7900".to_string()
}

/// One machine the service may hand out.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MachineConfig {
    /// How allocations and logs name it; `host` when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// SSH destination: a hostname, `user@host`, or a `~/.ssh/config` alias.
    /// Required for `ssh`; unused by `outpost`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub host: String,
    /// `ssh` (default) or `outpost`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport: Option<String>,
    /// `outpost` only: the environment variable (in this service's
    /// environment) holding the pairing secret the machine's outpost
    /// presents. The secret itself is never written to this file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outpost_token_env: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// `linux`, `windows` or `macos`; asked of the machine when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub os: Option<String>,
    /// `x86_64` or `aarch64`; asked of the machine when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arch: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<String>,
    /// Durable storage root on the machine, where projects are cloned.
    pub remote_root: String,
    /// At most this many allocations at once; unlimited when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_allocations: Option<usize>,
    #[serde(default)]
    pub runner: RunnerConfig,
    /// Agent name → executable path on the machine.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub agents: BTreeMap<String, String>,
}

impl MachineConfig {
    /// The name allocations and messages use.
    #[must_use]
    pub fn display_name(&self) -> &str {
        self.name.as_deref().unwrap_or(&self.host)
    }

    /// Whether the machine is reached through `ralphus-outpost`.
    #[must_use]
    pub fn is_outpost(&self) -> bool {
        self.transport.as_deref() == Some(TRANSPORT_OUTPOST)
    }

    /// `ssh` or `outpost`.
    #[must_use]
    pub fn transport_name(&self) -> &str {
        self.transport.as_deref().unwrap_or(TRANSPORT_SSH)
    }

    /// The `--uri` the SSH provider is given.
    #[must_use]
    pub fn ssh_uri(&self) -> String {
        match &self.user {
            Some(user) if !self.host.contains('@') => format!("{user}@{}", self.host),
            _ => self.host.clone(),
        }
    }
}

/// How `ralphus-runner` gets onto a machine (the SSH provider's target
/// runner policy).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RunnerConfig {
    /// `upload` (copy a runner from `artifacts`) or `installed` (use
    /// `command` as found on the machine).
    #[serde(default = "default_runner_mode")]
    pub mode: String,
    #[serde(default = "default_runner_command")]
    pub command: String,
    /// `<os>-<arch>` → runner binary on this host, for `upload`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub artifacts: BTreeMap<String, String>,
}

impl Default for RunnerConfig {
    fn default() -> Self {
        Self {
            mode: default_runner_mode(),
            command: default_runner_command(),
            artifacts: BTreeMap::new(),
        }
    }
}

fn default_runner_mode() -> String {
    "installed".to_string()
}

fn default_runner_command() -> String {
    "ralphus-runner".to_string()
}

const OSES: [&str; 3] = ["linux", "windows", "macos"];
/// The default transport.
pub const TRANSPORT_SSH: &str = "ssh";
/// The outbound-connection transport (`ralphus-outpost`).
pub const TRANSPORT_OUTPOST: &str = "outpost";

fn valid_env_name(name: &str) -> bool {
    !name.is_empty()
        && name.chars().all(|c| c == '_' || c.is_ascii_alphanumeric())
        && !name.starts_with(|c: char| c.is_ascii_digit())
}

/// Whether `root` can go unquoted on a Windows machine's command line (the
/// SSH transport's rule, `ssh-provider/src/windows.rs`).
fn windows_root_ok(root: &str) -> bool {
    let bytes = root.as_bytes();
    bytes.len() > 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'/' | b'\\')
        && root
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | ':' | '\\' | '/'))
        && !root.split(['/', '\\']).any(|p| p == "..")
}
const ARCHES: [&str; 2] = ["x86_64", "aarch64"];

impl FleetConfig {
    /// Parse and validate a configuration.
    ///
    /// # Errors
    /// The first problem found, naming `origin`.
    pub fn parse(text: &str, origin: &str) -> Result<Self, String> {
        let config: Self =
            toml::from_str(text).map_err(|e| format!("could not parse {origin}: {e}"))?;
        config.validate(origin)?;
        Ok(config)
    }

    /// Read and validate the configuration at `path`.
    ///
    /// # Errors
    /// An unreadable or invalid file.
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("could not read {}: {e}", path.display()))?;
        Self::parse(&text, &path.display().to_string())
    }

    fn validate(&self, origin: &str) -> Result<(), String> {
        if self.token_env.trim().is_empty() {
            return Err(format!(
                "{origin}: token_env must name an environment variable"
            ));
        }
        let mut names = Vec::new();
        for machine in &self.machines {
            let name = machine.display_name();
            match machine.transport_name() {
                TRANSPORT_SSH => {
                    if machine.host.trim().is_empty() {
                        return Err(format!("{origin}: a [[machine]] has an empty host"));
                    }
                    if machine.outpost_token_env.is_some() {
                        return Err(format!(
                            "{origin}: machine {name:?} sets outpost_token_env but its transport is ssh"
                        ));
                    }
                }
                TRANSPORT_OUTPOST => {
                    if machine.name.as_deref().is_none_or(|n| n.trim().is_empty()) {
                        return Err(format!(
                            "{origin}: an outpost [[machine]] needs a name (its outpost connects with --name)"
                        ));
                    }
                    if !machine
                        .outpost_token_env
                        .as_deref()
                        .is_some_and(valid_env_name)
                    {
                        return Err(format!(
                            "{origin}: outpost machine {name:?} needs outpost_token_env naming the \
                             environment variable that holds its pairing secret"
                        ));
                    }
                    if machine.runner != RunnerConfig::default() {
                        return Err(format!(
                            "{origin}: outpost machine {name:?} takes no [machine.runner]: the outpost \
                             carries its own runner"
                        ));
                    }
                }
                other => {
                    return Err(format!(
                        "{origin}: machine {name:?} transport {other:?} must be \"ssh\" or \"outpost\""
                    ));
                }
            }
            if names.contains(&name) {
                return Err(format!("{origin}: two machines are named {name:?}"));
            }
            names.push(name);
            if let Some(os) = machine.os.as_deref().filter(|os| !OSES.contains(os)) {
                return Err(format!(
                    "{origin}: machine {name:?} os {os:?} must be one of {OSES:?}"
                ));
            }
            if let Some(arch) = machine.arch.as_deref().filter(|a| !ARCHES.contains(a)) {
                return Err(format!(
                    "{origin}: machine {name:?} arch {arch:?} must be one of {ARCHES:?}"
                ));
            }
            if machine.remote_root.trim().is_empty() {
                return Err(format!("{origin}: machine {name:?} needs a remote_root"));
            }
            if machine.os.as_deref() == Some("windows")
                && !machine.is_outpost()
                && !windows_root_ok(&machine.remote_root)
            {
                return Err(format!(
                    "{origin}: Windows machine {name:?} remote_root {:?} must be a drive path made \
                     only of letters, digits and . _ - \\ / (no spaces), since it goes unquoted \
                     on the machine's command line",
                    machine.remote_root
                ));
            }
            if !["upload", "installed"].contains(&machine.runner.mode.as_str()) {
                return Err(format!(
                    "{origin}: machine {name:?} runner mode {:?} must be \"upload\" or \"installed\"",
                    machine.runner.mode
                ));
            }
        }
        Ok(())
    }

    /// Write the configuration back to `path`.
    ///
    /// # Errors
    /// An unwritable file.
    pub fn save(&self, path: &Path) -> Result<(), String> {
        let text = toml::to_string_pretty(self).map_err(|e| format!("could not encode: {e}"))?;
        std::fs::write(path, text).map_err(|e| format!("could not write {}: {e}", path.display()))
    }

    /// The state file for a configuration read from `config_path`.
    #[must_use]
    pub fn state_path(&self, config_path: &Path) -> PathBuf {
        self.state_file.as_ref().map_or_else(
            || {
                let mut name = config_path.as_os_str().to_owned();
                name.push(".state.json");
                PathBuf::from(name)
            },
            PathBuf::from,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_minimal_machine_parses_with_defaults() {
        let config = FleetConfig::parse(
            "token_env = \"T\"\n[[machine]]\nhost = \"box\"\nuser = \"me\"\nremote_root = \"/srv\"\n",
            "fleet.toml",
        )
        .unwrap();
        assert_eq!(config.listen, "127.0.0.1:7900");
        let machine = &config.machines[0];
        assert_eq!(machine.display_name(), "box");
        assert_eq!(machine.ssh_uri(), "me@box");
        assert_eq!(machine.runner.mode, "installed");
        assert_eq!(machine.transport_name(), "ssh");
    }

    #[test]
    fn an_outpost_machine_needs_no_host() {
        let config = FleetConfig::parse(
            "token_env = \"T\"\n[[machine]]\nname = \"winbox\"\ntransport = \"outpost\"\n\
             outpost_token_env = \"WINBOX_TOKEN\"\nremote_root = \"C:/Program Files/ralphus\"\n",
            "fleet.toml",
        )
        .unwrap();
        assert!(config.machines[0].is_outpost());
        assert_eq!(config.machines[0].display_name(), "winbox");
        let text = toml::to_string_pretty(&config).unwrap();
        assert!(!text.contains("host"), "{text}");
        FleetConfig::parse(&text, "round trip").unwrap();
    }

    #[test]
    fn bad_values_are_configuration_errors() {
        for (text, needle) in [
            ("token_env = \"T\"\ncolour = 1\n", "unknown field"),
            (
                "token_env = \"T\"\n[[machine]]\nhost = \"a\"\nos = \"beos\"\nremote_root = \"/r\"\n",
                "os",
            ),
            (
                "token_env = \"T\"\n[[machine]]\nhost = \"a\"\nremote_root = \"/r\"\n[[machine]]\nhost = \"a\"\nremote_root = \"/r\"\n",
                "two machines",
            ),
            (
                "token_env = \"T\"\n[[machine]]\nhost = \"a\"\n",
                "remote_root",
            ),
            (
                "token_env = \"T\"\n[[machine]]\nname = \"w\"\ntransport = \"outpost\"\nremote_root = \"/r\"\n",
                "outpost_token_env",
            ),
            (
                "token_env = \"T\"\n[[machine]]\ntransport = \"outpost\"\noutpost_token_env = \"W\"\nremote_root = \"/r\"\n",
                "needs a name",
            ),
            (
                "token_env = \"T\"\n[[machine]]\nname = \"w\"\ntransport = \"pigeon\"\nremote_root = \"/r\"\n",
                "transport",
            ),
            (
                "token_env = \"T\"\n[[machine]]\nname = \"w\"\ntransport = \"outpost\"\noutpost_token_env = \"W\"\nremote_root = \"/r\"\n[machine.runner]\nmode = \"upload\"\n",
                "carries its own runner",
            ),
            (
                "token_env = \"T\"\n[[machine]]\nhost = \"w\"\nos = \"windows\"\nremote_root = \"C:/Program Files/r\"\n",
                "no spaces",
            ),
        ] {
            let err = FleetConfig::parse(text, "f").expect_err(text);
            assert!(err.contains(needle), "{text}: {err}");
        }
    }
}
