//! RAL-473: pure helpers for a DB-backed agent profile's environment table.
//!
//! A profile's `env` is an ordered list of [`AgentEnvEntry`] rows, each
//! either a literal `Set` value or a `Link` -- a named indirection that
//! resolves against *this same profile's* other rows first, falling back to
//! the daemon process environment for a name the profile itself does not
//! define. `Link` replaces the old TOML `from_env`, but unlike `from_env` (a
//! single hop straight to the process environment) a `Link` can chain
//! through other rows in the same profile, so the chain must be checked for
//! cycles before it is ever resolved. Links never cross profile boundaries --
//! there is no such thing as a `Link` into a different named profile.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// How one [`AgentEnvEntry`]'s `value` should be interpreted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentEnvKind {
    /// `value` is the literal environment-variable value.
    Set,
    /// `value` is the *name* of another environment variable to resolve --
    /// first checked against this profile's own other entries, then the
    /// daemon process environment.
    Link,
}

/// One row of a profile's environment table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentEnvEntry {
    pub key: String,
    pub kind: AgentEnvKind,
    pub value: String,
}

/// A `Link` chain that resolves back to one of its own keys.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkCycle {
    /// The keys involved in the cycle, in traversal order, e.g.
    /// `["A", "B", "A"]` for `A -> B -> A`.
    pub keys: Vec<String>,
}

impl std::fmt::Display for LinkCycle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.keys.join(" -> "))
    }
}

/// Walks every `Link` entry's chain and reports the first cycle found (`None`
/// if the graph is acyclic). Only edges between two keys *both* defined in
/// `env` count -- a `Link` naming a key this profile does not define is a
/// leaf that falls back to the process environment at resolution time, not a
/// graph edge, so it can never participate in a cycle.
///
/// Must be called (and any cycle rejected) before a profile is persisted --
/// see `Store::upsert_agent_profile`.
#[must_use]
pub fn detect_link_cycle(env: &[AgentEnvEntry]) -> Option<LinkCycle> {
    let by_key: BTreeMap<&str, &AgentEnvEntry> = env.iter().map(|e| (e.key.as_str(), e)).collect();

    #[derive(PartialEq)]
    enum Mark {
        Visiting,
        Done,
    }
    let mut marks: BTreeMap<&str, Mark> = BTreeMap::new();

    fn visit<'a>(
        key: &'a str,
        by_key: &BTreeMap<&'a str, &'a AgentEnvEntry>,
        marks: &mut BTreeMap<&'a str, Mark>,
        path: &mut Vec<String>,
    ) -> Option<LinkCycle> {
        if let Some(pos) = path.iter().position(|k| k == key) {
            let mut keys: Vec<String> = path[pos..].to_vec();
            keys.push(key.to_string());
            return Some(LinkCycle { keys });
        }
        if marks.get(key) == Some(&Mark::Done) {
            return None;
        }
        let entry = by_key.get(key)?;
        if entry.kind != AgentEnvKind::Link {
            marks.insert(key, Mark::Done);
            return None;
        }
        let Some(&target) = by_key.get(entry.value.as_str()) else {
            marks.insert(key, Mark::Done);
            return None;
        };
        marks.insert(key, Mark::Visiting);
        path.push(key.to_string());
        let target_key = target.key.as_str();
        let result = visit(target_key, by_key, marks, path);
        path.pop();
        marks.insert(key, Mark::Done);
        result
    }

    for key in by_key.keys() {
        if marks.contains_key(key) {
            continue;
        }
        let mut path = Vec::new();
        if let Some(cycle) = visit(key, &by_key, &mut marks, &mut path) {
            return Some(cycle);
        }
    }
    None
}

/// Resolves every entry in `env` to its final literal value. Callers must
/// have already run [`detect_link_cycle`] and rejected any cycle -- this
/// function assumes an acyclic graph and does not itself guard against
/// infinite recursion.
///
/// `process_env` is injected (rather than reading `std::env::var` directly)
/// so tests can supply a fixed fallback environment.
pub fn resolve_agent_env(
    env: &[AgentEnvEntry],
    process_env: impl Fn(&str) -> Option<String>,
) -> BTreeMap<String, String> {
    let by_key: BTreeMap<&str, &AgentEnvEntry> = env.iter().map(|e| (e.key.as_str(), e)).collect();

    fn resolve_one(
        entry: &AgentEnvEntry,
        by_key: &BTreeMap<&str, &AgentEnvEntry>,
        process_env: &impl Fn(&str) -> Option<String>,
    ) -> Option<String> {
        match entry.kind {
            AgentEnvKind::Set => Some(entry.value.clone()),
            AgentEnvKind::Link => match by_key.get(entry.value.as_str()) {
                Some(target) => resolve_one(target, by_key, process_env),
                None => process_env(&entry.value),
            },
        }
    }

    let mut out = BTreeMap::new();
    for entry in env {
        if let Some(value) = resolve_one(entry, &by_key, &process_env) {
            out.insert(entry.key.clone(), value);
        }
    }
    out
}

/// The resolved value of every `Link` entry in `env` -- these are treated as
/// secret-shaped for RAL-264 redaction purposes, mirroring the old
/// `from_env` behavior in `agent_profiles.rs`. `Set` values are excluded:
/// they are literal, plaintext-in-the-database values, exactly like a
/// literal TOML value was never treated as secret.
#[must_use]
pub fn secret_values(
    env: &[AgentEnvEntry],
    process_env: impl Fn(&str) -> Option<String>,
) -> Vec<String> {
    let resolved = resolve_agent_env(env, process_env);
    env.iter()
        .filter(|e| e.kind == AgentEnvKind::Link)
        .filter_map(|e| resolved.get(&e.key).cloned())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(key: &str, value: &str) -> AgentEnvEntry {
        AgentEnvEntry {
            key: key.to_string(),
            kind: AgentEnvKind::Set,
            value: value.to_string(),
        }
    }
    fn link(key: &str, target: &str) -> AgentEnvEntry {
        AgentEnvEntry {
            key: key.to_string(),
            kind: AgentEnvKind::Link,
            value: target.to_string(),
        }
    }

    #[test]
    fn acyclic_graph_reports_no_cycle() {
        let env = vec![set("A", "1"), link("B", "A"), link("C", "B")];
        assert!(detect_link_cycle(&env).is_none());
    }

    #[test]
    fn link_to_undefined_name_is_not_a_cycle() {
        let env = vec![link("A", "SOME_PROCESS_VAR")];
        assert!(detect_link_cycle(&env).is_none());
    }

    #[test]
    fn direct_self_link_is_a_cycle() {
        let env = vec![link("A", "A")];
        let cycle = detect_link_cycle(&env).expect("cycle");
        assert_eq!(cycle.keys, vec!["A", "A"]);
    }

    #[test]
    fn two_step_cycle_is_detected() {
        let env = vec![link("A", "B"), link("B", "A")];
        let cycle = detect_link_cycle(&env).expect("cycle");
        assert!(cycle.keys.contains(&"A".to_string()));
        assert!(cycle.keys.contains(&"B".to_string()));
    }

    #[test]
    fn cycle_downstream_of_an_acyclic_prefix_is_still_found() {
        let env = vec![
            set("ROOT", "1"),
            link("A", "B"),
            link("B", "C"),
            link("C", "A"),
        ];
        let cycle = detect_link_cycle(&env).expect("cycle");
        assert!(cycle.keys.len() >= 3);
    }

    #[test]
    fn resolves_set_values_literally() {
        let env = vec![set("A", "hello")];
        let resolved = resolve_agent_env(&env, |_| None);
        assert_eq!(resolved.get("A"), Some(&"hello".to_string()));
    }

    #[test]
    fn resolves_link_within_profile() {
        let env = vec![set("A", "hello"), link("B", "A")];
        let resolved = resolve_agent_env(&env, |_| None);
        assert_eq!(resolved.get("B"), Some(&"hello".to_string()));
    }

    #[test]
    fn resolves_link_chain_within_profile() {
        let env = vec![set("A", "hello"), link("B", "A"), link("C", "B")];
        let resolved = resolve_agent_env(&env, |_| None);
        assert_eq!(resolved.get("C"), Some(&"hello".to_string()));
    }

    #[test]
    fn link_falls_back_to_process_env_when_name_undefined_in_profile() {
        let env = vec![link("A", "PROCESS_VAR")];
        let resolved = resolve_agent_env(&env, |name| {
            (name == "PROCESS_VAR").then(|| "from-process".to_string())
        });
        assert_eq!(resolved.get("A"), Some(&"from-process".to_string()));
    }

    #[test]
    fn link_to_undefined_name_with_no_process_fallback_is_omitted() {
        let env = vec![link("A", "MISSING")];
        let resolved = resolve_agent_env(&env, |_| None);
        assert!(!resolved.contains_key("A"));
    }

    #[test]
    fn secret_values_includes_only_link_resolutions() {
        let env = vec![set("A", "plain"), link("B", "SECRET_VAR")];
        let secrets = secret_values(&env, |name| {
            (name == "SECRET_VAR").then(|| "sk-super-secret".to_string())
        });
        assert_eq!(secrets, vec!["sk-super-secret".to_string()]);
    }
}
