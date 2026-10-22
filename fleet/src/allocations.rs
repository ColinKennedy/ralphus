//! Allocations: which machine serves which scope of ralphus work, and the
//! short-lived tokens for each allocation's Machine API.
//!
//! Allocations are kept in a state file so ralphus can re-read them after
//! either side restarts. Tokens never are: after a restart every allocation
//! answers `401` until ralphus renews it.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::{FleetConfig, MachineConfig};

/// How long an allocation token is valid.
pub const TOKEN_LIFETIME_MS: i64 = 60 * 60 * 1000;

/// What ralphus asked for.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct Requirements {
    #[serde(default)]
    pub os: Option<String>,
    #[serde(default)]
    pub arch: Option<String>,
    #[serde(default)]
    pub labels: Vec<String>,
    /// Agent backends the work needs installed and logged in on the machine
    /// (`claude-code`, `codex`, ...). Optional.
    #[serde(default)]
    pub agents: Vec<String>,
}

/// One allocation, as stored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Allocation {
    pub id: String,
    /// [`MachineConfig::display_name`] of the machine serving it.
    pub machine: String,
    /// The `scope` ralphus sent (`squad_id` or `review_id`, and `key`).
    pub scope: Value,
    #[serde(default)]
    pub idle: bool,
    pub created_at_ms: i64,
}

/// A machine's os and arch, configured or probed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Platform {
    pub os: Option<String>,
    pub arch: Option<String>,
}

/// Why an otherwise-matching machine cannot take work now (its readiness
/// report has a blocking problem), and what fixes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotReady {
    pub reason: String,
    pub remedy: String,
}

/// A refusal to allocate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub reason_code: &'static str,
    pub error: String,
    pub remediation: String,
}

#[must_use]
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// A fresh random token or id: `bytes` random bytes as hex.
///
/// # Errors
/// The OS refused randomness.
pub fn random_hex(bytes: usize) -> Result<String, String> {
    let mut buf = vec![0u8; bytes];
    getrandom::getrandom(&mut buf).map_err(|e| format!("no randomness: {e}"))?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

/// Every allocation, plus the tokens valid for each.
#[derive(Debug, Default)]
pub struct Allocations {
    path: Option<PathBuf>,
    by_id: BTreeMap<String, Allocation>,
    /// Allocation id → `(token, expires_at_ms)`, newest last. The previous
    /// token stays valid until it expires so a call already in flight when
    /// ralphus renews is not refused.
    tokens: BTreeMap<String, Vec<(String, i64)>>,
}

impl Allocations {
    /// Load the allocations kept at `path` (none if it does not exist).
    ///
    /// # Errors
    /// An unreadable or corrupt state file.
    pub fn load(path: PathBuf) -> Result<Self, String> {
        let by_id = match std::fs::read_to_string(&path) {
            Ok(text) => {
                let list: Vec<Allocation> = serde_json::from_str(&text)
                    .map_err(|e| format!("{} is corrupt: {e}", path.display()))?;
                list.into_iter().map(|a| (a.id.clone(), a)).collect()
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(e) => return Err(format!("could not read {}: {e}", path.display())),
        };
        Ok(Self {
            path: Some(path),
            by_id,
            tokens: BTreeMap::new(),
        })
    }

    fn save(&self) -> Result<(), String> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let list: Vec<&Allocation> = self.by_id.values().collect();
        let text = serde_json::to_string_pretty(&list).map_err(|e| e.to_string())?;
        std::fs::write(path, text).map_err(|e| format!("could not write {}: {e}", path.display()))
    }

    #[must_use]
    pub fn get(&self, id: &str) -> Option<&Allocation> {
        self.by_id.get(id)
    }

    #[must_use]
    pub fn all(&self) -> Vec<&Allocation> {
        self.by_id.values().collect()
    }

    /// How many allocations `machine` serves.
    #[must_use]
    pub fn load_of(&self, machine: &str) -> usize {
        self.by_id.values().filter(|a| a.machine == machine).count()
    }

    /// Issue a fresh token for `id`, returning it and its expiry.
    ///
    /// # Errors
    /// No randomness.
    pub fn issue_token(&mut self, id: &str) -> Result<(String, i64), String> {
        let token = random_hex(32)?;
        let expires = now_ms() + TOKEN_LIFETIME_MS;
        let now = now_ms();
        let tokens = self.tokens.entry(id.to_string()).or_default();
        tokens.retain(|(_, at)| *at > now);
        tokens.push((token.clone(), expires));
        // The newest token and the one before it; older ones go.
        if tokens.len() > 2 {
            tokens.drain(..tokens.len() - 2);
        }
        Ok((token, expires))
    }

    /// Whether `token` is valid for allocation `id` now.
    #[must_use]
    pub fn token_valid(&self, id: &str, token: &str) -> bool {
        let now = now_ms();
        self.tokens.get(id).is_some_and(|tokens| {
            tokens
                .iter()
                .any(|(t, at)| *at > now && constant_time_eq(t.as_bytes(), token.as_bytes()))
        })
    }

    /// The allocation already serving `scope`, if any.
    #[must_use]
    pub fn for_scope(&self, scope: &Value) -> Option<&Allocation> {
        self.by_id.values().find(|a| a.scope == *scope)
    }

    /// Record a new allocation on `machine` for `scope`.
    ///
    /// # Errors
    /// No randomness, or the state file could not be written.
    pub fn insert(&mut self, machine: &str, scope: Value) -> Result<Allocation, String> {
        let allocation = Allocation {
            id: format!("alloc-{}", random_hex(6)?),
            machine: machine.to_string(),
            scope,
            idle: false,
            created_at_ms: now_ms(),
        };
        self.by_id.insert(allocation.id.clone(), allocation.clone());
        self.save()?;
        Ok(allocation)
    }

    /// Set the idle hint.
    ///
    /// # Errors
    /// The state file could not be written.
    pub fn set_idle(&mut self, id: &str, idle: bool) -> Result<bool, String> {
        let Some(allocation) = self.by_id.get_mut(id) else {
            return Ok(false);
        };
        allocation.idle = idle;
        self.save()?;
        Ok(true)
    }

    /// Hand an allocation back.
    ///
    /// # Errors
    /// The state file could not be written.
    pub fn remove(&mut self, id: &str) -> Result<bool, String> {
        let removed = self.by_id.remove(id).is_some();
        self.tokens.remove(id);
        if removed {
            self.save()?;
        }
        Ok(removed)
    }
}

/// Compare two byte strings without stopping at the first difference.
#[must_use]
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Why `machine` (with `platform`) cannot serve `wanted`, or `None`.
#[must_use]
pub fn mismatch(
    machine: &MachineConfig,
    platform: &Platform,
    wanted: &Requirements,
) -> Option<String> {
    let name = machine.display_name();
    if let Some(os) = &wanted.os {
        match &platform.os {
            Some(have) if have == os => {}
            Some(have) => return Some(format!("{name} runs {have}, not {os}")),
            None => return Some(format!("{name}'s os is unknown (it did not answer)")),
        }
    }
    if let Some(arch) = &wanted.arch {
        match &platform.arch {
            Some(have) if have == arch => {}
            Some(have) => return Some(format!("{name} is {have}, not {arch}")),
            None => return Some(format!("{name}'s arch is unknown (it did not answer)")),
        }
    }
    let missing: Vec<&String> = wanted
        .labels
        .iter()
        .filter(|l| !machine.labels.contains(l))
        .collect();
    if !missing.is_empty() {
        return Some(format!(
            "{name} has no label {}",
            missing
                .iter()
                .map(|l| format!("{l:?}"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    None
}

/// Pick the machine to serve `wanted`: the least loaded one that matches, is
/// not in `not_ready`, and has room, in configuration order on a tie.
///
/// # Errors
/// Why no machine can, listing each one's reason (and, for machines that
/// are not ready, what fixes them).
pub fn choose<'a>(
    config: &'a FleetConfig,
    platforms: &BTreeMap<String, Platform>,
    allocations: &Allocations,
    wanted: &Requirements,
    not_ready: &BTreeMap<String, NotReady>,
) -> Result<&'a MachineConfig, Refusal> {
    if config.machines.is_empty() {
        return Err(Refusal {
            reason_code: "no_matching_machine",
            error: "this fleet has no machines".to_string(),
            remediation: "add one with `ralphus-fleet add <host>`".to_string(),
        });
    }
    let mut reasons = Vec::new();
    let mut remedies = Vec::new();
    let mut matching = Vec::new();
    for machine in &config.machines {
        let platform = platforms
            .get(machine.display_name())
            .cloned()
            .unwrap_or_default();
        match mismatch(machine, &platform, wanted) {
            Some(reason) => reasons.push(reason),
            None => match not_ready.get(machine.display_name()) {
                Some(blocked) => {
                    reasons.push(blocked.reason.clone());
                    remedies.push(blocked.remedy.clone());
                }
                None => matching.push(machine),
            },
        }
    }
    if matching.is_empty() {
        return Err(Refusal {
            reason_code: "no_matching_machine",
            error: format!("no machine matches: {}", reasons.join("; ")),
            remediation: if remedies.is_empty() {
                "relax the request's os/arch/labels, or add a matching machine to the fleet"
                    .to_string()
            } else {
                remedies.join("; ")
            },
        });
    }
    let with_room: Vec<&MachineConfig> = matching
        .into_iter()
        .filter(|m| {
            m.max_allocations
                .is_none_or(|max| allocations.load_of(m.display_name()) < max)
        })
        .collect();
    with_room
        .into_iter()
        .min_by_key(|m| allocations.load_of(m.display_name()))
        .ok_or_else(|| Refusal {
            reason_code: "capacity",
            error: "every matching machine is at its max_allocations".to_string(),
            remediation: "wait for running work to finish, or raise max_allocations".to_string(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> FleetConfig {
        FleetConfig::parse(
            "token_env = \"T\"\n\
             [[machine]]\nhost = \"lin\"\nos = \"linux\"\nremote_root = \"/r\"\nmax_allocations = 1\n\
             [[machine]]\nhost = \"gpu\"\nos = \"linux\"\nlabels = [\"gpu\"]\nremote_root = \"/r\"\n\
             [[machine]]\nhost = \"win\"\nos = \"windows\"\nremote_root = \"C:/r\"\n",
            "f",
        )
        .unwrap()
    }

    fn platforms(config: &FleetConfig) -> BTreeMap<String, Platform> {
        config
            .machines
            .iter()
            .map(|m| {
                (
                    m.display_name().to_string(),
                    Platform {
                        os: m.os.clone(),
                        arch: Some("x86_64".to_string()),
                    },
                )
            })
            .collect()
    }

    fn wants(os: Option<&str>, labels: &[&str]) -> Requirements {
        Requirements {
            os: os.map(str::to_string),
            arch: None,
            labels: labels.iter().map(|l| (*l).to_string()).collect(),
            agents: Vec::new(),
        }
    }

    #[test]
    fn requirements_pick_a_matching_machine_and_spread_load() {
        let config = config();
        let platforms = platforms(&config);
        let mut allocations = Allocations::default();
        let windows = choose(
            &config,
            &platforms,
            &allocations,
            &wants(Some("windows"), &[]),
            &BTreeMap::new(),
        );
        assert_eq!(windows.unwrap().host, "win");
        let gpu = choose(
            &config,
            &platforms,
            &allocations,
            &wants(None, &["gpu"]),
            &BTreeMap::new(),
        );
        assert_eq!(gpu.unwrap().host, "gpu");

        let first = choose(
            &config,
            &platforms,
            &allocations,
            &wants(Some("linux"), &[]),
            &BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(first.host, "lin");
        allocations.insert("lin", Value::Null).unwrap();
        // `lin` is full, so the other linux machine serves the next request.
        let second = choose(
            &config,
            &platforms,
            &allocations,
            &wants(Some("linux"), &[]),
            &BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(second.host, "gpu");
    }

    #[test]
    fn a_refusal_says_why_each_machine_does_not_fit() {
        let config = config();
        let refusal = choose(
            &config,
            &platforms(&config),
            &Allocations::default(),
            &wants(Some("macos"), &[]),
            &BTreeMap::new(),
        )
        .expect_err("no mac");
        assert_eq!(refusal.reason_code, "no_matching_machine");
        assert!(
            refusal.error.contains("win runs windows, not macos"),
            "{}",
            refusal.error
        );
    }

    #[test]
    fn a_machine_that_is_not_ready_is_refused_with_its_fix() {
        let config = config();
        let not_ready = BTreeMap::from([(
            "win".to_string(),
            NotReady {
                reason: "win is not ready: Claude Code is not logged in".to_string(),
                remedy: "run `claude auth login` on win".to_string(),
            },
        )]);
        let refusal = choose(
            &config,
            &platforms(&config),
            &Allocations::default(),
            &wants(Some("windows"), &[]),
            &not_ready,
        )
        .expect_err("win is not ready");
        assert!(refusal.error.contains("not logged in"), "{}", refusal.error);
        assert_eq!(refusal.remediation, "run `claude auth login` on win");
    }

    #[test]
    fn a_renewed_token_keeps_the_previous_one_valid_until_it_expires() {
        let mut allocations = Allocations::default();
        let a = allocations.insert("lin", Value::Null).unwrap();
        let (old, _) = allocations.issue_token(&a.id).unwrap();
        let (new, _) = allocations.issue_token(&a.id).unwrap();
        assert!(allocations.token_valid(&a.id, &old));
        assert!(allocations.token_valid(&a.id, &new));
        assert!(!allocations.token_valid(&a.id, "guess"));
        let (newest, _) = allocations.issue_token(&a.id).unwrap();
        assert!(
            !allocations.token_valid(&a.id, &old),
            "only two tokens stay valid"
        );
        assert!(allocations.token_valid(&a.id, &newest));
    }

    #[test]
    fn allocations_survive_a_restart_but_their_tokens_do_not() {
        let path = std::env::temp_dir().join(format!("fleet-state-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let mut allocations = Allocations::load(path.clone()).unwrap();
        let a = allocations
            .insert("lin", serde_json::json!({"squad_id": "s"}))
            .unwrap();
        let (token, _) = allocations.issue_token(&a.id).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains(&token), "tokens are never written down");

        let reloaded = Allocations::load(path.clone()).unwrap();
        assert_eq!(reloaded.get(&a.id), Some(&a));
        assert!(!reloaded.token_valid(&a.id, &token));
        let _ = std::fs::remove_file(&path);
    }
}
