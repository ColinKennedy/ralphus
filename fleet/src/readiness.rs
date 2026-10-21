//! A machine's **readiness report**: what an outpost found when it checked
//! its machine on connecting (git, the agents the fleet expects, long paths,
//! disk space), each problem with what to do about it.
//!
//! The outpost builds it (`ralphus-outpost`'s `readiness` module) and sends
//! it in a sealed poll; the fleet keeps the latest one per machine, shows it
//! in `ralphus-fleet list`, and refuses to allocate a machine with a
//! [blocking](Report::blocking) problem. Reports never carry a secret: an
//! agent's login check says logged in or not, never who as.

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
}

/// Everything an outpost checked, at one moment.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Report {
    pub checked_at_ms: i64,
    pub checks: Vec<Check>,
}

impl Report {
    /// The checks that make the machine unable to serve work needing
    /// `required_agents`: git missing, or a required agent that cannot start
    /// or is not logged in.
    #[must_use]
    pub fn blocking(&self, required_agents: &[String]) -> Vec<&Check> {
        self.checks
            .iter()
            .filter(|check| check.status == FAIL)
            .filter(|check| {
                check.id == CHECK_GIT
                    || required_agents.iter().any(|agent| {
                        check.id == format!("{CHECK_AGENT}{agent}")
                            || check.id == format!("{CHECK_AGENT_LOGIN}{agent}")
                    })
            })
            .collect()
    }

    /// The checks that did not pass.
    #[must_use]
    pub fn problems(&self) -> Vec<&Check> {
        self.checks.iter().filter(|c| !c.passed()).collect()
    }

    /// `ready`, or how many problems and how many of them block.
    #[must_use]
    pub fn summary(&self, required_agents: &[String]) -> String {
        let problems = self.problems().len();
        let blocking = self.blocking(required_agents).len();
        match (problems, blocking) {
            (0, _) => "ready".to_string(),
            (n, 0) => format!("ready, {n} warning(s)"),
            (_, b) => format!("NOT READY ({b} blocking)"),
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
                Check::new("agent-login:claude-code", FAIL, "not logged in")
                    .with_remedy("run `claude auth login` on the machine"),
                Check::new("agent:codex", FAIL, "codex not found"),
                Check::new(CHECK_LONG_PATHS, WARN, "LongPathsEnabled is 0"),
            ],
        }
    }

    #[test]
    fn only_failures_of_git_or_a_required_agent_block() {
        let r = report();
        assert!(r.blocking(&[]).is_empty(), "no agent required");
        let blocking = r.blocking(&["claude-code".to_string()]);
        assert_eq!(blocking.len(), 1);
        assert_eq!(blocking[0].id, "agent-login:claude-code");
        assert_eq!(r.blocking(&["codex".to_string()])[0].id, "agent:codex");
        assert_eq!(r.problems().len(), 3);
        assert_eq!(r.summary(&[]), "ready, 3 warning(s)");
        assert_eq!(
            r.summary(&["claude-code".to_string()]),
            "NOT READY (1 blocking)"
        );
        let mut no_git = report();
        no_git.checks[0].status = FAIL.to_string();
        assert_eq!(no_git.blocking(&[]).len(), 1);
    }
}
