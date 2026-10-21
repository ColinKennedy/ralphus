//! The outpost's readiness check: the same questions `ralphus-runner
//! preflight` and the daemon's health sweep ask, answered on this machine
//! when it connects, so `ralphus-fleet list` can say what to fix before any
//! work is sent (`ralphus_fleet::readiness` holds the report's shape).
//!
//! - **git**: installed, and which version.
//! - **agents** the fleet expects (`[machine.agents]`, `required_agents`):
//!   the launcher can start (`ralphus-runner preflight`'s check), and, for
//!   agents with a login probe (Claude Code, Codex), whether the agent is
//!   logged in. The status command's output is parsed, never forwarded: the
//!   report says logged in or not, never who as.
//! - **long paths** (Windows): `LongPathsEnabled`.
//! - **disk**: free space on the volume holding `remote_root`.

use std::time::Duration;

use ralphus_fleet::readiness::{
    CHECK_AGENT, CHECK_AGENT_LOGIN, CHECK_DISK, CHECK_GIT, Check, FAIL, PASS, Report, WARN,
};
use ralphus_runner::login_probe::{LoginProbe, LoginState, LoginStatus, StatusRun};
use ralphus_runner::version_probe::{self, VersionProbe};
use serde_json::{Map, Value};

/// Less free space than this on `remote_root`'s volume is a warning.
pub const LOW_DISK_BYTES: u64 = 5 * 1024 * 1024 * 1024;
const STATUS_TIMEOUT: Duration = Duration::from_secs(20);

/// Check this machine for `remote_root` and `agents` (agent name → its
/// configured path, or null for "as found on `PATH`").
#[must_use]
pub fn check(remote_root: &str, agents: Map<String, Value>) -> Report {
    let mut checks = vec![git_check(&version_probe::probe_version(
        "git",
        &["--version"],
        version_probe::DEFAULT_VERSION_PROBE_TIMEOUT,
        version_probe::first_version_token,
    ))];
    for (agent, path) in &agents {
        let executable = path.as_str().filter(|p| !p.trim().is_empty());
        let launcher = ralphus_runner::execute::preflight_agent(agent, executable, false);
        let can_start = launcher.is_ok();
        checks.push(agent_check(agent, launcher));
        if can_start {
            if let Some(probe) = login_probe_for(agent) {
                let program = executable.map_or_else(
                    || {
                        std::env::var(probe.command_env_var())
                            .ok()
                            .filter(|v| !v.trim().is_empty())
                            .unwrap_or_else(|| probe.default_program().to_string())
                    },
                    str::to_string,
                );
                let run = ralphus_runner::login_probe::run_status_command(
                    &program,
                    probe.status_args(),
                    STATUS_TIMEOUT,
                );
                checks.push(login_check(agent, probe, run));
            }
        }
    }
    if let Some(check) = long_paths_check() {
        checks.push(check);
    }
    checks.push(disk_check(remote_root));
    Report {
        checked_at_ms: ralphus_fleet::allocations::now_ms(),
        checks,
    }
}

fn git_check(probe: &VersionProbe) -> Check {
    match probe {
        VersionProbe::Ok { version, .. } => Check::new(CHECK_GIT, PASS, format!("git {version}")),
        VersionProbe::NotFound => {
            Check::new(CHECK_GIT, FAIL, "git is not on PATH").with_remedy(if cfg!(windows) {
                "install Git for Windows (https://git-scm.com/download/win) and restart \
                 ralphus-outpost so it sees the new PATH"
            } else {
                "install git with the system package manager (e.g. `sudo apt install git`)"
            })
        }
        other => Check::new(CHECK_GIT, FAIL, other.detail("git"))
            .with_remedy("repair the git installation so `git --version` runs"),
    }
}

fn agent_check(agent: &str, launcher: Result<(), String>) -> Check {
    let id = format!("{CHECK_AGENT}{agent}");
    match launcher {
        Ok(()) => Check::new(&id, PASS, format!("{agent} can start")),
        Err(detail) => Check::new(&id, FAIL, format!("{agent} cannot start: {detail}"))
            .with_remedy(format!(
                "install {agent} for the user running ralphus-outpost, or set its path in the \
                 fleet's [machine.agents]"
            )),
    }
}

/// The login probe for `agent`, when it has one.
fn login_probe_for(agent: &str) -> Option<&'static dyn LoginProbe> {
    let backend = match agent {
        "claude-code" | "claude-cli" => "claude-code",
        "codex" | "codex-cli" => "codex",
        _ => return None,
    };
    ralphus_runner::login_probe::login_probes()
        .into_iter()
        .find(|p| p.backend_name() == backend)
}

/// Turn a status command's run into a check that says logged in or not --
/// never the account.
fn login_check(agent: &str, probe: &dyn LoginProbe, run: StatusRun) -> Check {
    let id = format!("{CHECK_AGENT_LOGIN}{agent}");
    let remedy = format!(
        "run `{}` on the machine as the user running ralphus-outpost",
        probe.login_command()
    );
    let status = match run {
        StatusRun::Finished { exit_ok, output } => {
            let present = |name: &str| std::env::var_os(name).is_some_and(|v| !v.is_empty());
            probe.parse(&output, exit_ok, &present)
        }
        StatusRun::TimedOut => LoginStatus {
            state: LoginState::Unknown,
            summary: "the login status command timed out".to_string(),
        },
        StatusRun::SpawnFailed(_) => LoginStatus {
            state: LoginState::Unknown,
            summary: "the login status command could not run".to_string(),
        },
    };
    let name = probe.display_name();
    match status.state {
        LoginState::LoggedIn => Check::new(&id, PASS, format!("{name} is logged in")),
        LoginState::LoggedOut => {
            Check::new(&id, FAIL, format!("{name} is not logged in")).with_remedy(remedy)
        }
        LoginState::Unknown => Check::new(
            &id,
            WARN,
            format!("could not tell whether {name} is logged in"),
        )
        .with_remedy(remedy),
    }
}

#[cfg(windows)]
fn long_paths_check() -> Option<Check> {
    use winreg::RegKey;
    use winreg::enums::HKEY_LOCAL_MACHINE;
    let enabled = RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey("SYSTEM\\CurrentControlSet\\Control\\FileSystem")
        .and_then(|key| key.get_value::<u32, _>("LongPathsEnabled"))
        .unwrap_or(0);
    Some(if enabled == 1 {
        Check::new(
            ralphus_fleet::readiness::CHECK_LONG_PATHS,
            PASS,
            "long paths are enabled",
        )
    } else {
        Check::new(
            ralphus_fleet::readiness::CHECK_LONG_PATHS,
            WARN,
            "long paths are disabled (LongPathsEnabled is not 1); deep worktrees may fail",
        )
        .with_remedy(
            "in an elevated prompt: `reg add HKLM\\SYSTEM\\CurrentControlSet\\Control\\FileSystem \
             /v LongPathsEnabled /t REG_DWORD /d 1 /f`, then `git config --global core.longpaths \
             true`",
        )
    })
}

#[cfg(not(windows))]
fn long_paths_check() -> Option<Check> {
    None
}

fn disk_check(remote_root: &str) -> Check {
    let root = std::path::Path::new(remote_root);
    let disks = sysinfo::Disks::new_with_refreshed_list();
    let best = disks
        .list()
        .iter()
        .filter(|d| root.starts_with(d.mount_point()) || same_drive(root, d.mount_point()))
        .max_by_key(|d| d.mount_point().as_os_str().len());
    match best {
        None => Check::new(
            CHECK_DISK,
            WARN,
            format!("could not find the volume holding {remote_root}"),
        ),
        Some(disk) => disk_space(remote_root, disk.available_space()),
    }
}

/// Drive letters compare case-insensitively and `C:/x` lives on `C:\`.
fn same_drive(root: &std::path::Path, mount: &std::path::Path) -> bool {
    let (root, mount) = (root.to_string_lossy(), mount.to_string_lossy());
    cfg!(windows)
        && mount.len() <= 3
        && root.len() >= 2
        && mount
            .get(..2)
            .is_some_and(|m| root.get(..2).is_some_and(|r| r.eq_ignore_ascii_case(m)))
}

fn disk_space(remote_root: &str, available: u64) -> Check {
    let gib = available / (1024 * 1024 * 1024);
    if available < LOW_DISK_BYTES {
        Check::new(
            CHECK_DISK,
            WARN,
            format!("only {gib} GiB free on the volume holding {remote_root}"),
        )
        .with_remedy("free disk space, or move remote_root to a larger volume")
    } else {
        Check::new(CHECK_DISK, PASS, format!("{gib} GiB free"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_git_fails_with_how_to_install_it() {
        let check = git_check(&VersionProbe::NotFound);
        assert_eq!(check.status, FAIL);
        assert!(check.remedy.unwrap().contains("git"));
        let ok = git_check(&VersionProbe::Ok {
            path: "git".to_string(),
            version: "2.45.1".to_string(),
        });
        assert_eq!(ok.detail, "git 2.45.1");
    }

    #[test]
    fn a_login_check_says_logged_in_or_not_and_never_who() {
        let probe = login_probe_for("claude-code").unwrap();
        let logged_in = login_check(
            "claude-code",
            probe,
            StatusRun::Finished {
                exit_ok: true,
                output: r#"{"loggedIn": true, "authMethod": "claude.ai", "email": "someone@example.com", "orgName": "Secret Org"}"#.to_string(),
            },
        );
        assert_eq!(logged_in.status, PASS);
        assert!(
            !logged_in.detail.contains("example.com"),
            "{}",
            logged_in.detail
        );
        assert!(
            !logged_in.detail.contains("Secret Org"),
            "{}",
            logged_in.detail
        );
        let logged_out = login_check(
            "claude-code",
            probe,
            StatusRun::Finished {
                exit_ok: true,
                output: r#"{"loggedIn": false}"#.to_string(),
            },
        );
        assert_eq!(logged_out.status, FAIL);
        assert!(logged_out.remedy.unwrap().contains("claude auth login"));
        let timed_out = login_check("claude-code", probe, StatusRun::TimedOut);
        assert_eq!(timed_out.status, WARN);
        assert!(login_probe_for("ollama").is_none());
    }

    #[test]
    fn an_agent_that_cannot_start_fails_with_how_to_fix_it() {
        let check = agent_check("codex", Err("codex not found on PATH".to_string()));
        assert_eq!(check.id, "agent:codex");
        assert_eq!(check.status, FAIL);
        assert!(check.remedy.unwrap().contains("[machine.agents]"));
    }

    #[test]
    fn low_disk_space_warns() {
        assert_eq!(disk_space("/r", 1024).status, WARN);
        assert_eq!(disk_space("/r", LOW_DISK_BYTES * 2).status, PASS);
    }
}
