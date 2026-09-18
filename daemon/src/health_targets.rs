//! Provider health for every configured `[machine.targets.*]` entry
//! (RAL-355 Phase 9): the daemon-side counterpart to
//! `ralphus check health --all-remotes`.
//!
//! Deliberately scoped to what can be checked without a live Docker fixture
//! and without expanding the SSH provider's `run` verb beyond git (a
//! security-relevant restriction `ssh-provider/src/fileops.rs` imposes
//! deliberately): SSH reachability, `capabilities`, the remote-root
//! readiness probe, git version, and git identity. Two checks are
//! *documented as unverified* rather than genuinely probed, and always
//! report `warn` with an explanation instead of a real pass/fail --
//! push-credential verification (no safe, non-mutating way to confirm push
//! authorization exists yet) and, as of RAL-416, remote runner-executable
//! resolution (`runner`, catalog id `remote-runner`): confirming the
//! configured `runner_command` resolves on the remote would need the SSH
//! provider's `run` verb to accept an arbitrary program, the same
//! remote-command-execution scope question `run`'s git-only restriction was
//! deliberately drawn to avoid. Not implemented at all here -- see each
//! check's own comment or `REMOTE_IMPROVEMENTS.local.md`'s Phase 9 notes for
//! why: remote clock/timezone skew (no non-git remote command verb exists to
//! ask for it) and project-specific clone-URL reachability (needs project
//! context this target-scoped sweep doesn't have).

use std::sync::Arc;

use crate::machine_targets::MachineTarget;
use crate::remote_runner::{Capabilities, RunRequest};
use crate::runner::RunnerSpec;
#[cfg(test)]
use crate::store::Store;
use ralphus_core::health_catalog::{
    ID_REMOTE_CAPABILITIES, ID_REMOTE_GIT_IDENTITY_EMAIL, ID_REMOTE_GIT_IDENTITY_NAME,
    ID_REMOTE_GIT_VERSION, ID_REMOTE_PUSH_CREDENTIALS, ID_REMOTE_RESOLVE, ID_REMOTE_ROOT,
    ID_REMOTE_RUNNER, ID_REMOTE_SSH_REACHABLE,
};

const PASS: &str = "pass";
const WARN: &str = "warn";
const FAIL: &str = "fail";

/// Bound on concurrent target checks. A config-file-sized target list is
/// never going to be huge, but an operator with many targets shouldn't have
/// this open unbounded simultaneous SSH connections.
const MAX_CONCURRENT: usize = 8;

/// One named check's outcome for one target.
#[derive(Debug, Clone, serde::Serialize)]
pub struct TargetCheck {
    pub name: String,
    pub status: &'static str,
    pub detail: String,
    /// RAL-416: the stable `ralphus_core::health_catalog` entry id this
    /// check belongs to (e.g. `"remote-ssh-reachable"`) -- see
    /// `cli::health::CheckResult::id`'s doc comment for the same
    /// name-vs-id distinction. Always one of this module's `remote-*`
    /// catalog entries; never empty, unlike the CLI's own defensive-fallback
    /// cases, since every check here is a normal, always-executed probe.
    pub id: &'static str,
}

/// Every check run against one configured target.
#[derive(Debug, Clone, serde::Serialize)]
pub struct TargetHealthReport {
    pub target: String,
    pub machine: String,
    pub checks: Vec<TargetCheck>,
}

impl TargetHealthReport {
    #[must_use]
    pub fn any_fail(&self) -> bool {
        self.checks.iter().any(|c| c.status == FAIL)
    }
}

fn synthetic_spec(target_name: &str, cwd: &str) -> RunnerSpec {
    RunnerSpec::for_command_proof(
        "health-check",
        "health-check",
        target_name,
        cwd,
        "",
        "claude",
        Some(30),
    )
}

fn capability_check(caps: &Capabilities) -> TargetCheck {
    let mut parts = Vec::new();
    if let Some(os) = &caps.os {
        parts.push(format!("os={os}"));
    }
    if let Some(arch) = &caps.arch {
        parts.push(format!("arch={arch}"));
    }
    parts.push(format!("async_exec={}", caps.async_exec));
    parts.push(format!("terminal={}", caps.terminal));
    if let Some(v) = &caps.runner_version {
        parts.push(format!("runner_version={v}"));
    }
    if !caps.supported_ops.is_empty() {
        parts.push(format!("supported_ops=[{}]", caps.supported_ops.join(",")));
    }
    TargetCheck {
        name: "capabilities".to_string(),
        status: PASS,
        detail: parts.join(", "),
        id: ID_REMOTE_CAPABILITIES,
    }
}

fn git_identity_check(
    provider: &crate::remote_runner::ProviderRunner,
    spec: &RunnerSpec,
    remote_root: &str,
    key: &str,
) -> TargetCheck {
    let req = RunRequest {
        cwd: remote_root.to_string(),
        program: "git".to_string(),
        args: vec![
            "config".to_string(),
            "--global".to_string(),
            "--get".to_string(),
            key.to_string(),
        ],
    };
    let id = if key == "user.name" {
        ID_REMOTE_GIT_IDENTITY_NAME
    } else {
        ID_REMOTE_GIT_IDENTITY_EMAIL
    };
    match provider.run_vcs(&req, spec) {
        Ok(out) if !out.trim().is_empty() => TargetCheck {
            name: format!("git_{key}"),
            status: PASS,
            detail: out.trim().to_string(),
            id,
        },
        Ok(_) | Err(_) => TargetCheck {
            name: format!("git_{key}"),
            status: WARN,
            detail: format!(
                "no global git {key} is set for the remote account -- commits will fail until \
                 it is, unless every repository sets it per-repo instead"
            ),
            id,
        },
    }
}

/// Run every check for one target. Short-circuits after SSH reachability
/// fails -- every later check would fail the identical way, and reporting
/// the same connectivity problem five times would bury the one fact that
/// actually matters.
fn check_one_target(
    store: &crate::store_lock::StoreHandle,
    target: &MachineTarget,
) -> TargetHealthReport {
    let mut checks = Vec::new();
    let report = |checks| TargetHealthReport {
        target: target.name.clone(),
        machine: target.machine.clone(),
        checks,
    };

    let provider = {
        let guard = store.lock();
        crate::remote_runner::provider_from_store(&guard, &target.machine)
    };
    let provider = match provider {
        Ok(Some(p)) => p,
        Ok(None) => {
            checks.push(TargetCheck {
                name: "resolve".to_string(),
                status: FAIL,
                detail: format!(
                    "{:?} resolves to the local machine, not a remote provider",
                    target.machine
                ),
                id: ID_REMOTE_RESOLVE,
            });
            return report(checks);
        }
        Err(e) => {
            let kind = crate::remote_failure::classify(&e);
            checks.push(TargetCheck {
                name: "resolve".to_string(),
                status: FAIL,
                detail: format!("{e} ({kind})"),
                id: ID_REMOTE_RESOLVE,
            });
            return report(checks);
        }
    };
    let spec = synthetic_spec(&target.name, &target.remote_root);

    match provider.ping(&spec) {
        Ok(detail) => checks.push(TargetCheck {
            name: "ssh_reachable".to_string(),
            status: PASS,
            detail: detail.unwrap_or_default(),
            id: ID_REMOTE_SSH_REACHABLE,
        }),
        Err(e) => {
            checks.push(TargetCheck {
                name: "ssh_reachable".to_string(),
                status: FAIL,
                detail: e,
                id: ID_REMOTE_SSH_REACHABLE,
            });
            return report(checks);
        }
    }

    match provider.capabilities(&spec) {
        Ok(Some(caps)) => checks.push(capability_check(&caps)),
        Ok(None) => checks.push(TargetCheck {
            name: "capabilities".to_string(),
            status: WARN,
            detail: "provider does not implement the optional capabilities verb".to_string(),
            id: ID_REMOTE_CAPABILITIES,
        }),
        Err(e) => checks.push(TargetCheck {
            name: "capabilities".to_string(),
            status: WARN,
            detail: e,
            id: ID_REMOTE_CAPABILITIES,
        }),
    }

    match provider.probe_remote_root(&target.remote_root, &spec) {
        Ok(()) => checks.push(TargetCheck {
            name: "remote_root".to_string(),
            status: PASS,
            detail: format!(
                "create/read/rename/delete all succeeded under {:?}",
                target.remote_root
            ),
            id: ID_REMOTE_ROOT,
        }),
        Err(e) => checks.push(TargetCheck {
            name: "remote_root".to_string(),
            status: FAIL,
            detail: e,
            id: ID_REMOTE_ROOT,
        }),
    }

    let version_req = RunRequest {
        cwd: target.remote_root.clone(),
        program: "git".to_string(),
        args: vec!["--version".to_string()],
    };
    match provider.run_vcs(&version_req, &spec) {
        Ok(out) => checks.push(TargetCheck {
            name: "git_version".to_string(),
            status: PASS,
            detail: out.trim().to_string(),
            id: ID_REMOTE_GIT_VERSION,
        }),
        Err(e) => checks.push(TargetCheck {
            name: "git_version".to_string(),
            status: FAIL,
            detail: e,
            id: ID_REMOTE_GIT_VERSION,
        }),
    }

    checks.push(git_identity_check(
        &provider,
        &spec,
        &target.remote_root,
        "user.name",
    ));
    checks.push(git_identity_check(
        &provider,
        &spec,
        &target.remote_root,
        "user.email",
    ));

    checks.push(TargetCheck {
        name: "push_credentials".to_string(),
        status: WARN,
        detail: "not verified -- no safe, non-mutating way to confirm push authorization exists \
                 yet"
        .to_string(),
        id: ID_REMOTE_PUSH_CREDENTIALS,
    });

    checks.push(TargetCheck {
        name: "runner".to_string(),
        status: WARN,
        detail: format!(
            "not verified -- confirming '{}' resolves on the remote would require the SSH \
             provider's run verb to accept an arbitrary program, which is deliberately scoped \
             to git only",
            target.runner_command
        ),
        id: ID_REMOTE_RUNNER,
    });

    report(checks)
}

/// Check every configured `[machine.targets.*]` entry, bounded to
/// [`MAX_CONCURRENT`] concurrent probes.
///
/// # Errors
/// A config-loading failure (malformed `.ralphus.toml`). Individual target
/// failures are reported *inside* their own [`TargetHealthReport`], never as
/// this function's own `Err` -- one unreachable machine must not hide every
/// other target's results.
pub fn check_all_targets(
    store: &crate::store_lock::StoreHandle,
) -> Result<Vec<TargetHealthReport>, String> {
    let targets = crate::machine_targets::load_machine_targets()?;
    Ok(check_targets(store, targets.into_values().collect()))
}

fn check_targets(
    store: &crate::store_lock::StoreHandle,
    targets: Vec<MachineTarget>,
) -> Vec<TargetHealthReport> {
    let mut reports = Vec::with_capacity(targets.len());
    for chunk in targets.chunks(MAX_CONCURRENT) {
        std::thread::scope(|scope| {
            let handles: Vec<_> = chunk
                .iter()
                .map(|target| {
                    let store = Arc::clone(store);
                    scope.spawn(move || check_one_target(&store, target))
                })
                .collect();
            for (handle, target) in handles.into_iter().zip(chunk) {
                reports.push(handle.join().unwrap_or_else(|_| TargetHealthReport {
                    target: target.name.clone(),
                    machine: target.machine.clone(),
                    checks: vec![TargetCheck {
                        name: "panic".to_string(),
                        status: FAIL,
                        detail: "the health check itself panicked".to_string(),
                        id: ID_REMOTE_SSH_REACHABLE,
                    }],
                }));
            }
        });
    }
    reports
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_provider(dir: &std::path::Path, responses: &[(&str, &str)]) -> std::path::PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let py = dir.join("provider.py");
        let branches: String = responses
            .iter()
            .enumerate()
            .map(|(i, (verb, json))| {
                let keyword = if i == 0 { "if" } else { "elif" };
                format!("{keyword} verb == {verb:?}:\n    print({json:?})\n")
            })
            .collect();
        std::fs::write(
            &py,
            format!(
                "import sys\nverb = sys.argv[1]\n{branches}else:\n    print('{{\"ok\": false, \"protocol_version\": 1, \"error\": \"unexpected verb \" + verb}}')\n"
            ),
        )
        .unwrap();
        py
    }

    fn target(name: &str, machine: &str, remote_root: &str) -> MachineTarget {
        MachineTarget {
            name: name.to_string(),
            machine: machine.to_string(),
            remote_root: remote_root.to_string(),
            runner_mode: crate::machine_targets::RunnerMode::Installed,
            runner_command: "ralphus-runner".to_string(),
            runner_artifacts: std::collections::BTreeMap::new(),
            agent_executables: std::collections::BTreeMap::new(),
            retirement_opt_out: false,
        }
    }

    #[test]
    fn an_unresolvable_machine_reports_one_failing_check() {
        let store = Arc::new(crate::store_lock::StoreMutex::new(
            Store::open_in_memory().unwrap(),
        ));
        let report = check_one_target(&store, &target("t", "ssh:nope", "/srv/ralphus"));
        assert_eq!(report.checks.len(), 1);
        assert_eq!(report.checks[0].name, "resolve");
        assert!(report.any_fail());
    }

    #[test]
    fn ssh_unreachable_short_circuits_the_rest() {
        let dir =
            std::env::temp_dir().join(format!("ral355-health-unreachable-{}", std::process::id()));
        let py = fake_provider(
            &dir,
            &[(
                "ping",
                "{\"ok\": false, \"protocol_version\": 1, \"error\": \"connection refused\"}",
            )],
        );
        let store = Arc::new(crate::store_lock::StoreMutex::new(
            Store::open_in_memory().unwrap(),
        ));
        store
            .lock()
            .register_machine_provider(
                "healthtest",
                "",
                "python",
                &[py.to_string_lossy().into_owned()],
                crate::machines::PROTOCOL_VERSION,
                false,
            )
            .unwrap();
        let report = check_one_target(&store, &target("t", "healthtest:A", "/srv/ralphus"));
        assert_eq!(report.checks.len(), 1, "{:?}", report.checks);
        assert_eq!(report.checks[0].name, "ssh_reachable");
        assert!(report.any_fail());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_fully_healthy_target_reports_no_failures() {
        let dir = std::env::temp_dir().join(format!("ral355-health-ok-{}", std::process::id()));
        let py = fake_provider(
            &dir,
            &[
                (
                    "ping",
                    "{\"ok\": true, \"protocol_version\": 1, \"detail\": \"reachable\"}",
                ),
                (
                    "capabilities",
                    "{\"ok\": true, \"protocol_version\": 1, \"capabilities\": {\"os\": \"linux\", \"arch\": \"x86_64\", \"supported_ops\": [\"run\"], \"async_exec\": true, \"terminal\": false, \"runner_version\": null}}",
                ),
                (
                    "read-file",
                    "{\"ok\": true, \"protocol_version\": 1, \"stdout\": \"ralphus-probe\"}",
                ),
                ("write-file", "{\"ok\": true, \"protocol_version\": 1}"),
                ("remove-path", "{\"ok\": true, \"protocol_version\": 1}"),
                (
                    "run",
                    "{\"ok\": true, \"protocol_version\": 1, \"exit_code\": 0, \"stdout\": \"git version 2.43.0\"}",
                ),
            ],
        );
        let store = Arc::new(crate::store_lock::StoreMutex::new(
            Store::open_in_memory().unwrap(),
        ));
        store
            .lock()
            .register_machine_provider(
                "healthtest2",
                "",
                "python",
                &[py.to_string_lossy().into_owned()],
                crate::machines::PROTOCOL_VERSION,
                false,
            )
            .unwrap();
        let report = check_one_target(&store, &target("t", "healthtest2:A", "/srv/ralphus"));
        assert!(!report.any_fail(), "{:?}", report.checks);
        let names: Vec<&str> = report.checks.iter().map(|c| c.name.as_str()).collect();
        assert!(names.contains(&"ssh_reachable"));
        assert!(names.contains(&"capabilities"));
        assert!(names.contains(&"remote_root"));
        assert!(names.contains(&"git_version"));
        assert!(names.contains(&"push_credentials"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn check_all_targets_runs_every_configured_target_and_is_bounded_by_thread_scope() {
        let dir = std::env::temp_dir().join(format!("ral355-health-many-{}", std::process::id()));
        let py = fake_provider(
            &dir,
            &[(
                "ping",
                "{\"ok\": false, \"protocol_version\": 1, \"error\": \"connection refused\"}",
            )],
        );
        let store = Arc::new(crate::store_lock::StoreMutex::new(
            Store::open_in_memory().unwrap(),
        ));
        {
            let guard = store.lock();
            for i in 0..3 {
                guard
                    .register_machine_provider(
                        &format!("healthmany{i}"),
                        "",
                        "python",
                        &[py.to_string_lossy().into_owned()],
                        crate::machines::PROTOCOL_VERSION,
                        false,
                    )
                    .unwrap();
            }
        }
        let targets: Vec<MachineTarget> = (0..3)
            .map(|i| {
                target(
                    &format!("t{i}"),
                    &format!("healthmany{i}:A"),
                    "/srv/ralphus",
                )
            })
            .collect();
        let reports = check_targets(&store, targets);
        assert_eq!(reports.len(), 3);
        assert!(reports.iter().all(TargetHealthReport::any_fail));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
