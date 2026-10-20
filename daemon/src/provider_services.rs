//! Machine provider services (provider contract v2) declared in the global
//! `config.toml` as `[machine.providers.<name>]` -- see
//! [`docs/machine-provider-service.md`](../../docs/machine-provider-service.md).
//!
//! Validation is strict on shape and soft on availability: an unknown key, a
//! non-http(s) `url`, an invalid `token_env` name, an invalid provider name, or
//! one named `local` (reserved for the built-in provider) is a configuration
//! error; a token variable that is unset is not -- that provider is reported
//! unavailable when it is used ([`ProviderService::token`]) and only work
//! requesting it fails.
//!
//! Like `[machine.targets.*]`, this is read from global scope only: which
//! services may hand out machines is a property of the daemon, not of a
//! project.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// One configured provider service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderService {
    /// The `[machine.providers.<name>]` key -- what a task file's `provider`
    /// names.
    pub name: String,
    /// Base URL of the service.
    pub url: String,
    /// Environment variable holding the service's bearer token.
    pub token_env: String,
}

impl ProviderService {
    /// The service's bearer token, read from [`Self::token_env`] now.
    ///
    /// # Errors
    /// A remediated message when the variable is unset or empty: the provider
    /// is unavailable until it is set (and the daemon restarted, if the
    /// variable comes from its environment).
    pub fn token(&self) -> Result<String, String> {
        std::env::var(&self.token_env)
            .ok()
            .filter(|t| !t.trim().is_empty())
            .ok_or_else(|| {
                format!(
                    "provider service {:?} is unavailable: its token variable {} is not set; set \
                     it in the daemon's environment and restart the daemon",
                    self.name, self.token_env
                )
            })
    }
}

#[derive(Debug, Default, Deserialize)]
struct ProvidersFile {
    #[serde(default)]
    machine: Option<MachineTable>,
}

#[derive(Debug, Default, Deserialize)]
struct MachineTable {
    #[serde(default)]
    providers: BTreeMap<String, RawProvider>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawProvider {
    url: String,
    token_env: String,
}

/// Whether `name` is a valid environment variable name.
fn is_env_var_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Whether `url` is an `http://` or `https://` URL with a host.
fn is_http_url(url: &str) -> bool {
    ["http://", "https://"].iter().any(|scheme| {
        url.strip_prefix(scheme)
            .and_then(|rest| rest.split(['/', '?', '#']).next())
            .is_some_and(|host| !host.is_empty())
    })
}

/// Parse and validate the `[machine.providers.*]` tables in `text`, in the
/// order their headers appear (`origin` names the file in errors).
fn parse_provider_services(text: &str, origin: &str) -> Result<Vec<ProviderService>, String> {
    let parsed: ProvidersFile =
        toml::from_str(text).map_err(|e| format!("could not parse {origin}: {e}"))?;
    let mut services = Vec::new();
    for (name, raw) in parsed.machine.unwrap_or_default().providers {
        if name.eq_ignore_ascii_case(ralphus_core::schema::LOCAL_MACHINE) {
            return Err(format!(
                "{origin}: [machine.providers.{name}] is reserved -- `local` is the built-in \
                 provider for the daemon's own host and cannot be declared"
            ));
        }
        if !ralphus_core::schema::is_valid_provider_name(&name) {
            return Err(format!(
                "{origin}: provider name {name:?} must be letters, digits, '_' and '-', at least \
                 {} characters",
                ralphus_core::schema::MIN_MACHINE_SCHEME_LEN
            ));
        }
        if !is_http_url(raw.url.trim()) {
            return Err(format!(
                "{origin}: [machine.providers.{name}] url {:?} must be an http:// or https:// URL",
                raw.url
            ));
        }
        if !is_env_var_name(raw.token_env.trim()) {
            return Err(format!(
                "{origin}: [machine.providers.{name}] token_env {:?} must be an environment \
                 variable name",
                raw.token_env
            ));
        }
        services.push(ProviderService {
            name,
            url: raw.url.trim().trim_end_matches('/').to_string(),
            token_env: raw.token_env.trim().to_string(),
        });
    }
    // Declaration order: where each provider's header sits in the text. A
    // provider written without its own header keeps its relative name order
    // after those that have one.
    let position = |name: &str| {
        text.find(&format!("[machine.providers.{name}]"))
            .unwrap_or(usize::MAX)
    };
    services.sort_by_key(|s| position(&s.name));
    Ok(services)
}

/// `$RALPHUS_CONFIGURATION_PATH` entries, in order (same convention as
/// [`crate::machine_targets`]).
fn configuration_path_entries(configuration_path_env: Option<&str>) -> Vec<PathBuf> {
    let Some(raw) = configuration_path_env else {
        return Vec::new();
    };
    std::env::split_paths(raw).collect()
}

fn read_file(path: &Path) -> Result<Vec<ProviderService>, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("could not read {}: {e}", path.display()))?;
    parse_provider_services(&text, &path.display().to_string())
}

/// Every configured provider service, in declaration order: the global
/// `config.toml`, then `$RALPHUS_CONFIGURATION_PATH` entries (a later file's
/// provider of the same name replaces the earlier one in place; new ones are
/// appended).
///
/// # Errors
/// The first configuration error found.
pub fn load_provider_services() -> Result<Vec<ProviderService>, String> {
    let raw = std::env::var("RALPHUS_CONFIGURATION_PATH").ok();
    let mut files: Vec<PathBuf> = crate::config::global_config_path()
        .filter(|p| p.is_file())
        .into_iter()
        .collect();
    files.extend(
        configuration_path_entries(raw.as_deref())
            .into_iter()
            .filter(|p| p.is_file()),
    );
    let mut merged: Vec<ProviderService> = Vec::new();
    for file in files {
        for service in read_file(&file)? {
            match merged.iter_mut().find(|s| s.name == service.name) {
                Some(existing) => *existing = service,
                None => merged.push(service),
            }
        }
    }
    Ok(merged)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn providers_load_in_declaration_order() {
        let text = "[machine.providers.zeta]\nurl = \"http://127.0.0.1:7900/\"\ntoken_env = \"Z_TOKEN\"\n\
                    [machine.providers.alpha]\nurl = \"https://fleet.example\"\ntoken_env = \"A_TOKEN\"\n";
        let services = parse_provider_services(text, "config.toml").unwrap();
        let names: Vec<&str> = services.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["zeta", "alpha"]);
        assert_eq!(services[0].url, "http://127.0.0.1:7900");
    }

    #[test]
    fn a_misshapen_provider_is_a_configuration_error() {
        for (text, needle) in [
            (
                "[machine.providers.fleet]\nurl = \"ftp://x\"\ntoken_env = \"T\"\n",
                "http:// or https://",
            ),
            (
                "[machine.providers.fleet]\nurl = \"http://x\"\ntoken_env = \"not a var\"\n",
                "environment variable name",
            ),
            (
                "[machine.providers.fleet]\nurl = \"http://x\"\ntoken_env = \"T\"\ncolor = \"red\"\n",
                "unknown field",
            ),
            (
                "[machine.providers.local]\nurl = \"http://x\"\ntoken_env = \"T\"\n",
                "reserved",
            ),
            (
                "[machine.providers.x]\nurl = \"http://x\"\ntoken_env = \"T\"\n",
                "at least",
            ),
            (
                "[machine.providers.fleet]\nurl = \"http://x\"\n",
                "token_env",
            ),
        ] {
            let err = parse_provider_services(text, "config.toml").expect_err(text);
            assert!(err.contains(needle), "{text}: {err}");
        }
    }

    #[test]
    fn an_unset_token_makes_the_provider_unavailable_not_invalid() {
        let services = parse_provider_services(
            "[machine.providers.fleet]\nurl = \"http://x\"\ntoken_env = \"RALPHUS_TEST_UNSET_FLEET_TOKEN_9A\"\n",
            "config.toml",
        )
        .expect("shape is valid");
        let err = services[0].token().expect_err("unset");
        assert!(err.contains("unavailable"), "{err}");
        assert!(err.contains("RALPHUS_TEST_UNSET_FLEET_TOKEN_9A"), "{err}");
    }

    #[test]
    fn other_machine_tables_do_not_disturb_providers() {
        let text = "[machine]\nstrict = true\n[machine.targets.box]\nmachine = \"ssh:box\"\n\
                    remote_root = \"/srv\"\n[machine.providers.fleet]\nurl = \"http://x\"\ntoken_env = \"T\"\n";
        assert_eq!(parse_provider_services(text, "c").unwrap().len(), 1);
    }
}
