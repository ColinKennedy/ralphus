//! The `capabilities` verb (RAL-355 Phase 0 remainder): report what this
//! provider actually supports against `uri`, best-effort.
//!
//! Deliberately non-mutating and cheap: `os`/`arch` come from one `uname`
//! call already used elsewhere for artifact selection
//! (`runner_install::probe_target_triple`'s sibling probe, duplicated here
//! in raw form since `capabilities` wants the raw OS/arch strings, not a
//! Rust target triple); `runner_version` only probes an already-installed
//! runner's `--version` (never triggers an upload, unlike
//! `runner_install::ensure`, which this verb must not call as a side
//! effect of merely being asked what the machine supports).

use serde_json::{Value, json};

use crate::exec::EffectiveConfig;
use crate::job::ssh_command;
use crate::uri;

/// Every verb this provider implements today, kept as one literal list so a
/// documentation/report call and the crate's actual dispatch table
/// (`src/main.rs`) can be compared by a human without cross-referencing two
/// files -- see `mcp`'s parity-test precedent for why that matters.
const SUPPORTED_OPS: &[&str] = &[
    "provision",
    "exec",
    "status",
    "stream",
    "cancel",
    "job-cleanup",
    "ping",
    "run",
    "read-file",
    "write-file",
    "remove-path",
    "cleanup",
    "capabilities",
    "terminal",
];

/// Run `capabilities`: best-effort OS/arch probe plus static/derived facts
/// about this provider build. Never fails on an unreachable machine --
/// unlike every other verb, a health/capability probe that can't reach the
/// host still has something true to say (protocol version, supported ops),
/// so a probe failure degrades to `os`/`arch`/`runner_version` all `None`
/// rather than the whole call erroring.
#[must_use]
pub fn run(uri: &str, config: &EffectiveConfig) -> Value {
    let (os, arch) = uri::parse(uri)
        .ok()
        .and_then(|target| ssh_command(&target, "uname -s; uname -m", config, None).ok())
        .map(|output| {
            let mut lines = output
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty());
            (
                lines.next().map(str::to_lowercase),
                lines.next().map(str::to_lowercase),
            )
        })
        .unwrap_or((None, None));

    let runner_version = uri::parse(uri).ok().and_then(|target| {
        let policy = crate::runner_install::policy_from_env(config.target_runner_config.as_deref())
            .ok()
            .flatten()?;
        if policy.mode != "installed" {
            // Anything other than "installed" mode may need to upload a
            // runner before one exists remotely -- probing its version
            // would mean silently triggering that upload as a side effect
            // of a capabilities check, which this verb must never do.
            return None;
        }
        let probe = format!("{} --version", policy.command);
        ssh_command(&target, &probe, config, None)
            .ok()
            .and_then(|out| out.lines().next().map(str::trim).map(str::to_string))
            .filter(|v| v.starts_with("ralphus-runner "))
    });

    let async_exec = crate::runner_install::policy_from_env(config.target_runner_config.as_deref())
        .ok()
        .flatten()
        .is_some();

    json!({
        "os": os,
        "arch": arch,
        "supported_ops": SUPPORTED_OPS,
        "async_exec": async_exec,
        // True: this provider implements the `terminal` verb (RAL-355 Phase
        // 10). The daemon still brokers the actual WebSocket listener a
        // browser/CLI client connects to -- this field is this provider's
        // half of the capability (can it open a remote pty at all).
        "terminal": true,
        "runner_version": runner_version,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> EffectiveConfig {
        EffectiveConfig {
            remote_base: crate::config::DEFAULT_REMOTE_BASE.to_string(),
            extra_excludes: vec![],
            connect_timeout_secs: crate::config::DEFAULT_CONNECT_TIMEOUT_SECS,
            remote_runner_cmd: crate::config::DEFAULT_REMOTE_RUNNER_CMD.to_string(),
            ssh_config_file: None,
            target_runner_config: None,
        }
    }

    #[test]
    fn an_unreachable_machine_still_reports_static_facts_instead_of_failing() {
        let result = run("nosuchhost.invalid.example", &config());
        assert_eq!(result["os"], Value::Null);
        assert_eq!(result["arch"], Value::Null);
        assert_eq!(result["runner_version"], Value::Null);
        assert_eq!(result["async_exec"], json!(false));
        assert_eq!(result["terminal"], json!(true));
        let ops = result["supported_ops"].as_array().expect("array");
        assert!(ops.iter().any(|v| v == "provision"));
        assert!(ops.iter().any(|v| v == "run"));
        assert!(ops.iter().any(|v| v == "terminal"));
        assert!(
            !ops.iter().any(|v| v == "channel"),
            "channel is not implemented"
        );
    }

    #[test]
    fn async_exec_reflects_whether_a_target_is_configured() {
        let mut cfg = config();
        assert_eq!(
            run("alice@host", &cfg)["async_exec"],
            json!(false),
            "no [machine.targets.*] entry configured"
        );
        cfg.target_runner_config = Some(
            serde_json::json!({
                "mode": "installed",
                "command": "ralphus-runner",
                "remote_root": "/srv/ralphus",
            })
            .to_string(),
        );
        assert_eq!(run("alice@host", &cfg)["async_exec"], json!(true));
    }
}
