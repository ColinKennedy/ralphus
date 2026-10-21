//! A machine's **readiness report**: what an outpost found when it checked
//! its machine on connecting (git, its agents, long paths, disk space), each
//! problem with what to do about it.
//!
//! The outpost builds it (`ralphus-outpost`'s `readiness` module) and sends
//! it in a sealed poll; the fleet keeps the latest one per machine, shows it
//! in `ralphus-fleet list`, and refuses to allocate a machine that cannot
//! serve a request: git missing, or an agent the request (`requirements.agents`)
//! or the machine's `required_agents` needs that is not installed or not
//! logged in. Reports never carry a secret: an agent's login check says
//! logged in or not, never who as.

use serde::{Deserialize, Serialize};

/// The check passed.
pub const PASS: &str = "pass";
/// Something is off but work can run.
pub const WARN: &str = "warn";
/// Work that needs this will fail.
pub const FAIL: &str = "fail";

/// Check id: git is installed.
pub const CHECK_GIT: &str = "git";
/// Check id prefix: `agent:<name>` -- the agent's launcher can start.
pub const CHECK_AGENT: &str = "agent:";
/// Check id prefix: `agent-login:<name>` -- the agent is logged in.
pub const CHECK_AGENT_LOGIN: &str = "agent-login:";
/// Check id: Windows long paths are enabled.
pub const CHECK_LONG_PATHS: &str = "long-paths";
/// Check id: free space on the volume holding `remote_root`.
pub const CHECK_DISK: &str = "disk";

/// The agent backends that run as an installed CLI on the machine. Every
/// outpost checks these (and any other configured agent) when it connects.
pub const CLI_AGENTS: [&str; 3] = ["claude-code", "codex", "pi"];

/// The backend `name` (a built-in agent name or alias) stands for, or why it
/// is not one, listing the valid names.
///
/// # Errors
/// `name` is not in `ralphus_core::schema::RESERVED_AGENT_NAMES`.
pub fn canonical_agent(name: &str) -> Result<&'static str, String> {
    let canonical = match name {
        "claude-code" | "claude-cli" => "claude-code",
        "codex" | "codex-cli" => "codex",
        other => ralphus_core::schema::RESERVED_AGENT_NAMES
            .iter()
            .find(|known| **known == other)
            .copied()
            .unwrap_or_default(),
    };
    if canonical.is_empty() {
        return Err(format!(
            "unknown agent {name:?}; the agents are {}",
            ralphus_core::schema::RESERVED_AGENT_NAMES.join(", ")
        ));
    }
    Ok(canonical)
}

/// How people call `agent`.
#[must_use]
pub fn display_name(agent: &str) -> &str {
    match agent {
        "claude-code" => "Claude Code",
        "codex" => "Codex",
        "pi" => "Pi",
        other => other,
    }
}

/// What a person types on the machine to log `agent` in.
#[must_use]
pub fn login_instructions(agent: &str) -> String {
    match agent {
        "claude-code" => "run `claude`, then `/login`".to_string(),
        "codex" => "run `codex login`".to_string(),
        other => format!("log {other} in"),
    }
}

/// One check's outcome.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Check {
    pub id: String,
    /// [`PASS`], [`WARN`] or [`FAIL`].
    pub status: String,
    pub detail: String,
    /// What to do when it did not pass.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remedy: Option<String>,
}

impl Check {
    #[must_use]
    pub fn new(id: &str, status: &str, detail: impl Into<String>) -> Self {
        Self {
            id: id.to_string(),
            status: status.to_string(),
            detail: detail.into(),
            remedy: None,
        }
    }

    #[must_use]
    pub fn with_remedy(mut self, remedy: impl Into<String>) -> Self {
        self.remedy = Some(remedy.into());
        self
    }

    #[must_use]
    pub fn passed(&self) -> bool {
        self.status == PASS
    }

    /// The agent an `agent:` / `agent-login:` check is about.
    #[must_use]
    pub fn agent(&self) -> Option<&str> {
        self.id
            .strip_prefix(CHECK_AGENT_LOGIN)
            .or_else(|| self.id.strip_prefix(CHECK_AGENT))
    }
}

/// Whether a machine can run one agent, by its report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentState {
    /// It can start and is logged in (or has no login to check).
    Ready,
    /// It can start, but whether it is logged in could not be told.
    LoginUnclear,
    /// It cannot start (`detail` says why).
    NotInstalled(String),
    /// It can start but is not logged in.
    NotLoggedIn,
    /// The report does not cover it.
    NotChecked,
}

/// Everything an outpost checked, at one moment.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Report {
    pub checked_at_ms: i64,
    pub checks: Vec<Check>,
}

/// Why a machine cannot serve a request, and the plain-language fix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Blocker {
    pub reason: String,
    pub fix: String,
}

impl Report {
    fn check(&self, id: &str) -> Option<&Check> {
        self.checks.iter().find(|c| c.id == id)
    }

    /// Whether the machine can run `agent` (a canonical backend name).
    #[must_use]
    pub fn agent_state(&self, agent: &str) -> AgentState {
        let Some(launcher) = self.check(&format!("{CHECK_AGENT}{agent}")) else {
            return AgentState::NotChecked;
        };
        if !launcher.passed() {
            return AgentState::NotInstalled(launcher.detail.clone());
        }
        match self.check(&format!("{CHECK_AGENT_LOGIN}{agent}")) {
            None => AgentState::Ready,
            Some(login) if login.passed() => AgentState::Ready,
            Some(login) if login.status == FAIL => AgentState::NotLoggedIn,
            Some(_) => AgentState::LoginUnclear,
        }
    }

    /// What keeps `machine` from serving work that needs `agents`
    /// (canonical backend names): git missing, or one of the agents not
    /// installed or not logged in. A login that could not be told does not
    /// block.
    #[must_use]
    pub fn blockers(&self, machine: &str, agents: &[String]) -> Vec<Blocker> {
        let mut blockers = Vec::new();
        if let Some(git) = self.check(CHECK_GIT).filter(|c| c.status == FAIL) {
            blockers.push(Blocker {
                reason: format!("{machine} has no usable git ({})", git.detail),
                fix: format!(
                    "on {machine}: {}",
                    git.remedy.as_deref().unwrap_or("install git")
                ),
            });
        }
        for agent in agents {
            let name = display_name(agent);
            match self.agent_state(agent) {
                AgentState::Ready | AgentState::LoginUnclear => {}
                AgentState::NotLoggedIn => blockers.push(Blocker {
                    reason: format!("{name} on {machine} isn't logged in"),
                    // The reason already says what is wrong; the fix only
                    // says what to do, so the two read well side by side.
                    fix: format!("on {machine}, {}", login_instructions(agent)),
                }),
                AgentState::NotInstalled(detail) => blockers.push(Blocker {
                    reason: format!("{name} isn't installed on {machine} ({detail})"),
                    fix: format!(
                        "install {name} on {machine} for the user running ralphus-outpost (or \
                         set its path in the fleet's [machine.agents]), then restart \
                         ralphus-outpost"
                    ),
                }),
                AgentState::NotChecked => blockers.push(Blocker {
                    reason: format!("{machine} has not checked {name} yet"),
                    fix: format!(
                        "restart ralphus-outpost on {machine} so it checks {name} (an outpost \
                         older than this fleet does not)"
                    ),
                }),
            }
        }
        blockers
    }

    /// The checks that did not pass, agents aside (see [`Self::agent_state`]).
    #[must_use]
    pub fn machine_problems(&self) -> Vec<&Check> {
        self.checks
            .iter()
            .filter(|c| !c.passed() && c.agent().is_none())
            .collect()
    }

    /// The agents the report covers, each with its state.
    #[must_use]
    pub fn agents(&self) -> Vec<(String, AgentState)> {
        let mut names: Vec<String> = self
            .checks
            .iter()
            .filter(|c| c.id.starts_with(CHECK_AGENT) && !c.id.starts_with(CHECK_AGENT_LOGIN))
            .filter_map(|c| c.agent().map(str::to_string))
            .collect();
        names.dedup();
        names
            .into_iter()
            .map(|a| {
                let state = self.agent_state(&a);
                (a, state)
            })
            .collect()
    }

    /// `ready`, or how many problems block the machine's `required_agents`.
    #[must_use]
    pub fn summary(&self, machine: &str, required_agents: &[String]) -> String {
        let blocking = self.blockers(machine, required_agents).len();
        let warnings = self.machine_problems().len();
        match (blocking, warnings) {
            (0, 0) => "ready".to_string(),
            (0, n) => format!("ready, {n} warning(s)"),
            (b, _) => format!("NOT READY ({b} blocking)"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report() -> Report {
        Report {
            checked_at_ms: 1,
            checks: vec![
                Check::new(CHECK_GIT, PASS, "git 2.45"),
                Check::new("agent:claude-code", PASS, "claude found"),
                Check::new("agent-login:claude-code", FAIL, "not logged in"),
                Check::new("agent:codex", FAIL, "codex not found on PATH"),
                Check::new("agent:pi", PASS, "pi found"),
                Check::new(CHECK_LONG_PATHS, WARN, "LongPathsEnabled is 0"),
            ],
        }
    }

    fn agents(names: &[&str]) -> Vec<String> {
        names.iter().map(|a| (*a).to_string()).collect()
    }

    #[test]
    fn an_installed_agent_that_is_not_logged_in_blocks_with_a_plain_fix() {
        let r = report();
        assert!(r.blockers("winbox", &[]).is_empty(), "no agent needed");
        let blockers = r.blockers("winbox", &agents(&["claude-code"]));
        assert_eq!(blockers.len(), 1);
        assert_eq!(blockers[0].fix, "on winbox, run `claude`, then `/login`");
        assert_eq!(r.agent_state("claude-code"), AgentState::NotLoggedIn);
        assert!(matches!(
            r.agent_state("codex"),
            AgentState::NotInstalled(_)
        ));
        assert_eq!(r.agent_state("pi"), AgentState::Ready);
        assert_eq!(r.agent_state("ollama"), AgentState::NotChecked);
        assert!(
            r.blockers("winbox", &agents(&["codex"]))[0]
                .fix
                .starts_with("install Codex on winbox")
        );
        assert_eq!(r.summary("winbox", &[]), "ready, 1 warning(s)");
        assert_eq!(
            r.summary("winbox", &agents(&["claude-code", "codex"])),
            "NOT READY (2 blocking)"
        );
        let mut no_git = report();
        no_git.checks[0].status = FAIL.to_string();
        assert_eq!(no_git.blockers("winbox", &[]).len(), 1);
    }

    #[test]
    fn agent_names_are_checked_against_the_known_backends() {
        assert_eq!(canonical_agent("claude-cli").unwrap(), "claude-code");
        assert_eq!(canonical_agent("codex").unwrap(), "codex");
        assert_eq!(canonical_agent("ollama").unwrap(), "ollama");
        let err = canonical_agent("claude code").unwrap_err();
        assert!(err.contains("claude-code, claude-cli"), "{err}");
    }
}
