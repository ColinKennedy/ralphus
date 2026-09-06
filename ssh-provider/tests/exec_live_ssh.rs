//! Opt-in, live-SSH integration test (RAL-200).
//!
//! Everything else in this crate is unit-tested with pure argv/string
//! builders and no live network (see the `#[cfg(test)]` modules under
//! `src/`). This is the one test that actually shells out over `ssh` end to
//! end, asserting the thing those unit tests structurally cannot: that
//! `RALPHUS_EVENT:` marker lines -- **including `llm-invoke` usage events**,
//! the ones the live cost-cap kill reads -- survive a real round trip through
//! `ssh` without being dropped, mangled, or interleaved with regular output.
//!
//! `#[ignore]`d by default (never runs under a plain `cargo test`), gated on
//! `RALPHUS_SSH_LIVE_TEST_TARGET`, and skips gracefully (prints `SKIP:` and
//! returns) whenever that variable is unset or the target isn't reachable --
//! mirroring the Ollama-backed test idiom used elsewhere in this repo
//! (`AGENTS.md`'s "Live-Ollama tests are `#[ignore]`d by default" section).
//!
//! Run it against your own machine, which every dev box with OpenSSH server
//! enabled and key-based auth to itself already satisfies:
//! ```bash
//! RALPHUS_SSH_LIVE_TEST_TARGET=127.0.0.1 cargo test -p ralphus-ssh-provider \
//!     --test exec_live_ssh -- --ignored --nocapture
//! ```
//!
//! The fake "remote runner" this test points `RALPHUS_SSH_REMOTE_RUNNER_CMD`
//! at is written to the *local* filesystem and invoked by absolute path --
//! this only works when the target's filesystem is the same as the one this
//! test runs on, i.e. `127.0.0.1`/`localhost`/an SSH alias for the local
//! machine. Pointing this at a genuinely separate host will fail with "no
//! such file" rather than skip -- that's expected, not a bug in the guard.
#![allow(clippy::print_stdout)] // SKIP notices, matching the repo's existing test idiom.

use std::io::Write;
use std::process::{Command, Stdio};

use ralphus_ssh_provider::exec::EffectiveConfig;
use ralphus_ssh_provider::{exec, ping};

/// A POSIX shell "remote runner" standing in for the real `ralphus-runner`:
/// consumes the spec on stdin (ignored), emits one `RALPHUS_EVENT:` line
/// (an `llm-invoke` usage event, the one the live cost-cap kill depends on)
/// to stderr, then a minimal `CellResult` JSON to stdout.
const FAKE_RUNNER_SCRIPT: &str = r#"#!/bin/sh
cat >/dev/null
echo 'RALPHUS_EVENT: {"source":"llm-invoke","message":"live ssh test","payload":{"tokens_in":1,"tokens_out":2,"cost_usd":0.01}}' 1>&2
echo '{"status":"done","summary":"live ssh round trip ok","tokens_in":1,"tokens_out":2,"cost_usd":0.01}'
"#;

#[test]
#[ignore] // opt-in: shells out over real ssh, see the module docs.
fn ralphus_event_and_llm_invoke_markers_survive_the_ssh_round_trip() {
    let Ok(target) = std::env::var("RALPHUS_SSH_LIVE_TEST_TARGET") else {
        println!(
            "SKIP: RALPHUS_SSH_LIVE_TEST_TARGET not set -- see this test's module docs \
             for how to opt in"
        );
        return;
    };

    let config = EffectiveConfig {
        remote_base: std::env::temp_dir()
            .join("ralphus-ssh-provider-live-test-workspaces")
            .to_string_lossy()
            .into_owned(),
        extra_excludes: vec![],
        connect_timeout_secs: 10,
        remote_runner_cmd: String::new(), // filled in below once the target is known reachable
        ssh_config_file: std::env::var("RALPHUS_SSH_CONFIG_FILE").ok(),
        target_runner_config: None,
    };

    if let Err(e) = ping::run(&target, &config) {
        println!("SKIP: {target:?} is not reachable over ssh right now: {e}");
        return;
    }

    let script_dir = std::env::temp_dir().join(format!(
        "ralphus-ssh-provider-live-test-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&script_dir).expect("create script dir");
    let script_path = script_dir.join("fake_runner.sh");
    std::fs::write(&script_path, FAKE_RUNNER_SCRIPT).expect("write fake runner script");

    let local_source_dir = script_dir.join("source");
    std::fs::create_dir_all(&local_source_dir).expect("create source dir");
    std::fs::write(local_source_dir.join("hello.txt"), b"hi").expect("write source file");

    let config = EffectiveConfig {
        remote_runner_cmd: format!("sh {}", script_path.to_string_lossy()),
        ..config
    };

    let spec = serde_json::json!({
        "squad_id": "live-ssh-test-run",
        "task": "live-ssh-test-task",
        "cell_id": "s0",
        "cwd": local_source_dir.to_string_lossy(),
        "prompt": "unused -- the fake runner ignores its stdin payload",
        "agent": "claude",
    });

    let result =
        exec::run(&target, &spec.to_string(), &config).expect("exec::run against a live target");
    assert_eq!(result["status"], serde_json::json!("done"), "{result:?}");
    assert_eq!(
        result["summary"],
        serde_json::json!("live ssh round trip ok"),
        "{result:?}"
    );

    let _ = std::fs::remove_dir_all(&script_dir);
}

/// The full round trip through the *built binary* (not just the library
/// function), so the `RALPHUS_EVENT:` line's presence on this **process's
/// own stderr** -- what the daemon actually scrapes -- is exercised for
/// real, not just the in-process forwarding logic `exec::run` performs.
#[test]
#[ignore] // opt-in: shells out over real ssh, see the module docs.
fn the_built_binary_forwards_ralphus_event_lines_onto_its_own_stderr() {
    let Ok(target) = std::env::var("RALPHUS_SSH_LIVE_TEST_TARGET") else {
        println!(
            "SKIP: RALPHUS_SSH_LIVE_TEST_TARGET not set -- see this test's module docs \
             for how to opt in"
        );
        return;
    };
    let probe_config = EffectiveConfig {
        remote_base: String::new(),
        extra_excludes: vec![],
        connect_timeout_secs: 10,
        remote_runner_cmd: String::new(),
        ssh_config_file: std::env::var("RALPHUS_SSH_CONFIG_FILE").ok(),
        target_runner_config: None,
    };
    if let Err(e) = ping::run(&target, &probe_config) {
        println!("SKIP: {target:?} is not reachable over ssh right now: {e}");
        return;
    }

    let script_dir = std::env::temp_dir().join(format!(
        "ralphus-ssh-provider-live-bin-test-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&script_dir).expect("create script dir");
    let script_path = script_dir.join("fake_runner.sh");
    std::fs::write(&script_path, FAKE_RUNNER_SCRIPT).expect("write fake runner script");
    let local_source_dir = script_dir.join("source");
    std::fs::create_dir_all(&local_source_dir).expect("create source dir");
    std::fs::write(local_source_dir.join("hello.txt"), b"hi").expect("write source file");

    let spec = serde_json::json!({
        "squad_id": "live-ssh-bin-test-run",
        "task": "live-ssh-bin-test-task",
        "cell_id": "s0",
        "cwd": local_source_dir.to_string_lossy(),
        "prompt": "unused",
        "agent": "claude",
    });

    let bin = env!("CARGO_BIN_EXE_ralphus-ssh-provider");
    let mut child = Command::new(bin)
        .arg("exec")
        .arg("--uri")
        .arg(&target)
        .env(
            "RALPHUS_SSH_REMOTE_BASE",
            std::env::temp_dir()
                .join("ralphus-ssh-provider-live-bin-test-workspaces")
                .to_string_lossy()
                .into_owned(),
        )
        .env(
            "RALPHUS_SSH_REMOTE_RUNNER_CMD",
            format!("sh {}", script_path.to_string_lossy()),
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn ralphus-ssh-provider");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(spec.to_string().as_bytes())
        .expect("write spec to stdin");
    let out = child.wait_with_output().expect("wait on child");

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("RALPHUS_EVENT: ") && stderr.contains("llm-invoke"),
        "the forwarded llm-invoke event must appear on the provider's own \
         stderr, which is what the daemon scrapes; got stderr: {stderr:?}"
    );

    let stdout = String::from_utf8_lossy(&out.stdout);
    let reply: serde_json::Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("provider stdout was not valid JSON ({e}): {stdout:?}"));
    assert_eq!(reply["ok"], serde_json::json!(true), "{reply:?}");
    assert_eq!(
        reply["result"]["summary"],
        serde_json::json!("live ssh round trip ok"),
        "{reply:?}"
    );

    let _ = std::fs::remove_dir_all(&script_dir);
}
