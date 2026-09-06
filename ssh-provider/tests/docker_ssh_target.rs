//! Opt-in end-to-end test against `docker/ssh-target-compose.yml`.
//!
//! Unlike `exec_live_ssh.rs`, this target has a genuinely separate filesystem
//! and its own installed Rust runner. Start it with
//! `scripts/ssh-target.ps1 up`, then run:
//!
//! ```powershell
//! $env:RALPHUS_SSH_DOCKER_TEST = '1'
//! $env:RALPHUS_SSH_CONFIG_FILE = (Resolve-Path .docker-ssh-target/ssh_config)
//! cargo test -p ralphus-ssh-provider --test docker_ssh_target -- --ignored --nocapture
//! ```

#![allow(clippy::print_stdout)]

use ralphus_ssh_provider::exec::EffectiveConfig;
use ralphus_ssh_provider::{exec, job, ping, provision};

fn fixture_config() -> Option<EffectiveConfig> {
    if std::env::var("RALPHUS_SSH_DOCKER_TEST").ok().as_deref() != Some("1") {
        println!("SKIP: set RALPHUS_SSH_DOCKER_TEST=1 after starting the SSH target fixture");
        return None;
    }
    Some(EffectiveConfig {
        remote_base: "/home/ralphus/.ralphus/remote-work/legacy-sync".to_string(),
        extra_excludes: Vec::new(),
        connect_timeout_secs: 10,
        remote_runner_cmd: "ralphus-runner".to_string(),
        ssh_config_file: std::env::var("RALPHUS_SSH_CONFIG_FILE").ok(),
        target_runner_config: None,
    })
}

#[test]
#[ignore]
fn installed_runner_and_mock_claude_cross_a_real_ssh_filesystem_boundary() {
    let Some(config) = fixture_config() else {
        return;
    };
    ping::run("ralphus-docker", &config).expect("fixture must be reachable");

    let source =
        std::env::temp_dir().join(format!("ralphus-docker-ssh-source-{}", std::process::id()));
    std::fs::create_dir_all(&source).expect("create source directory");
    std::fs::write(source.join("boundary-marker.txt"), "host-side marker\n")
        .expect("write source marker");

    let spec = serde_json::json!({
        "squad_id": "docker-ssh-squad",
        "task": "docker-ssh-task",
        "cell_id": "docker-ssh-cell",
        "cwd": source.to_string_lossy(),
        "prompt": "Return the deterministic fixture response.",
        "agent": "claude-code",
        "model": "fixture-model",
        "system_prompt": "This system prompt must cross the SSH boundary.",
        "system_prompt_position": "append",
    });

    let result = exec::run("ralphus-docker", &spec.to_string(), &config)
        .expect("remote Rust runner should complete through SSH");
    assert_eq!(result["status"], serde_json::json!("done"), "{result:?}");
    assert_eq!(
        result["summary"],
        serde_json::json!("remote mock completed"),
        "{result:?}"
    );
    assert_eq!(result["tokens_in"], serde_json::json!(11), "{result:?}");
    assert_eq!(result["tokens_out"], serde_json::json!(7), "{result:?}");
    assert_eq!(result["cost_usd"], serde_json::json!(0.0123), "{result:?}");

    let _ = std::fs::remove_dir_all(source);
}

#[test]
#[ignore]
fn upload_mode_installs_and_executes_a_content_addressed_runner() {
    let Some(mut config) = fixture_config() else {
        return;
    };
    let Ok(artifact) = std::env::var("RALPHUS_SSH_UPLOAD_TEST_ARTIFACT") else {
        println!("SKIP: set RALPHUS_SSH_UPLOAD_TEST_ARTIFACT to a Linux x86-64 runner");
        return;
    };
    let remote_root = "/home/ralphus/.ralphus/remote-work";
    let runner = serde_json::json!({
        "mode": "upload",
        "command": "ralphus-runner",
        "artifacts": {"x86_64-unknown-linux-musl": artifact},
        "remote_root": remote_root,
    });
    let branch = format!("phase5-upload-test-{}", std::process::id());
    let request = serde_json::json!({
        "project": "phase5-upload-test",
        "source": {
            "kind": "git",
            "url": "file:///srv/git/ralphus-test.git",
            "branch": branch,
            "upstream": "origin/main",
        },
        "squad_id": "phase5-upload-test",
        "cell_id": "upload",
        "remote_root": remote_root,
        "runner": runner,
    });
    let workspace = provision::run("ralphus-docker", &request.to_string(), &config)
        .expect("upload-mode provision should succeed");
    assert!(workspace.starts_with(remote_root), "{workspace}");

    config.target_runner_config = Some(runner.to_string());
    let source = std::env::temp_dir().join(format!(
        "ralphus-docker-upload-source-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&source).expect("create source directory");
    let spec = serde_json::json!({
        "squad_id": "docker-ssh-upload-squad",
        "task": "docker-ssh-upload-task",
        "cell_id": "docker-ssh-upload-cell",
        "cwd": source.to_string_lossy(),
        "prompt": "Return the deterministic fixture response.",
        "agent": "claude-code",
        "model": "fixture-model",
    });
    let result = exec::run("ralphus-docker", &spec.to_string(), &config)
        .expect("uploaded runner should execute through SSH");
    assert_eq!(result["status"], serde_json::json!("done"), "{result:?}");
    assert_eq!(
        result["summary"],
        serde_json::json!("remote mock completed"),
        "{result:?}"
    );
    let _ = std::fs::remove_dir_all(source);
}

fn async_workspace_and_config(name: &str) -> Option<(String, EffectiveConfig)> {
    let mut config = fixture_config()?;
    let remote_root = "/home/ralphus/.ralphus/remote-work";
    let runner = serde_json::json!({
        "mode": "installed",
        "command": "ralphus-runner",
        "artifacts": {},
        "remote_root": remote_root,
    });
    let identity = format!("{name}-{}", std::process::id());
    let request = serde_json::json!({
        "project": "phase6-async-test",
        "source": {
            "kind": "git",
            "url": "file:///srv/git/ralphus-test.git",
            "branch": identity,
            "upstream": "origin/main",
        },
        "squad_id": identity,
        "cell_id": "workspace",
        "remote_root": remote_root,
        "runner": runner,
    });
    let workspace = provision::run("ralphus-docker", &request.to_string(), &config)
        .expect("async test workspace should provision");
    config.target_runner_config = Some(runner.to_string());
    Some((workspace, config))
}

fn wait_for_terminal(handle: &str, config: &EffectiveConfig) -> job::Status {
    for _ in 0..100 {
        let status = job::status("ralphus-docker", handle, config)
            .expect("durable job status should remain readable");
        if !matches!(status.state.as_str(), "starting" | "running") {
            return status;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    panic!("remote job {handle} did not finish in time");
}

#[test]
#[ignore]
fn async_job_is_idempotent_streamable_restart_safe_and_cleanable() {
    let Some((workspace, config)) = async_workspace_and_config("phase6-complete") else {
        return;
    };
    let spec = serde_json::json!({
        "squad_id": "phase6-complete-squad",
        "task": "phase6-complete-task",
        "cell_id": "phase6-complete-cell",
        "cwd": workspace,
        "command": "test \"$PHASE7_REMOTE_VALUE\" = remote-only; printf 'first line\\n'; sleep 1; printf 'second line\\n'",
        "agent": "claude-code",
        "model": null,
        "execution_environment": {"PHASE7_REMOTE_VALUE": "remote-only"},
    });
    let handle =
        job::start("ralphus-docker", &spec.to_string(), &config).expect("async job should start");
    let duplicate = job::start("ralphus-docker", &spec.to_string(), &config)
        .expect("duplicate dispatch should reconcile");
    assert_eq!(handle, duplicate, "duplicate dispatch spawned a new job");

    let first = job::stream("ralphus-docker", &handle, 0, &config).expect("first stream read");
    let repeated =
        job::stream("ralphus-docker", &handle, 0, &config).expect("repeated stream read");
    assert_eq!(first.output, repeated.output);
    assert_eq!(first.next, repeated.next);

    // A fresh config value models a new provider process after restart: all
    // status is recovered from the target, not from local process memory.
    let restarted_config = config.clone();
    let terminal = wait_for_terminal(&handle, &restarted_config);
    assert_eq!(terminal.state, "done");
    assert_eq!(
        terminal
            .result
            .as_ref()
            .and_then(|value| value["status"].as_str()),
        Some("done")
    );
    let tail = job::stream("ralphus-docker", &handle, first.next, &config)
        .expect("stream tail should remain available");
    assert!(tail.output.contains("second line"), "{}", tail.output);

    job::cleanup("ralphus-docker", &handle, &config).expect("terminal job cleanup should work");
    assert!(job::status("ralphus-docker", &handle, &config).is_err());
}

#[test]
#[ignore]
fn cancellation_kills_descendants_and_preserves_reusable_git_metadata() {
    let Some((workspace, config)) = async_workspace_and_config("phase6-cancel") else {
        return;
    };
    let spec = serde_json::json!({
        "squad_id": "phase6-cancel-squad",
        "task": "phase6-cancel-task",
        "cell_id": "phase6-cancel-cell",
        "cwd": workspace,
        "command": "git status --short; sh -c 'sleep 60 & wait'",
        "agent": "claude-code",
        "model": null,
    });
    let handle = job::start("ralphus-docker", &spec.to_string(), &config)
        .expect("cancellable job should start");
    assert!(
        job::cleanup("ralphus-docker", &handle, &config).is_err(),
        "cleanup must refuse a live process tree"
    );
    job::cancel("ralphus-docker", &handle, &config).expect("process group cancellation");
    job::cancel("ralphus-docker", &handle, &config).expect("cancellation must be idempotent");
    let status = job::status("ralphus-docker", &handle, &config)
        .expect("cancelled job should retain a terminal result");
    assert_eq!(status.state, "failed");
    assert_eq!(
        status
            .result
            .as_ref()
            .and_then(|value| value["summary"].as_str()),
        Some("cancelled")
    );

    let verify = serde_json::json!({
        "squad_id": "phase6-git-verify-squad",
        "task": "phase6-git-verify-task",
        "cell_id": "phase6-git-verify-cell",
        "cwd": workspace,
        "command": "git fsck --no-dangling",
        "agent": "claude-code",
        "model": null,
    });
    let verify_handle = job::start("ralphus-docker", &verify.to_string(), &config)
        .expect("workspace should accept work after confirmed cancellation");
    let verify_status = wait_for_terminal(&verify_handle, &config);
    assert_eq!(
        verify_status
            .result
            .as_ref()
            .and_then(|value| value["status"].as_str()),
        Some("done"),
        "cancelled Git activity damaged reusable project metadata"
    );
}
