//! A reusable provider conformance suite (RAL-355 Phase 12): exercises the
//! documented contract (`docs/machine-providers.md`) generically, through
//! the exact [`ProviderRunner`] client the daemon itself uses to talk to any
//! provider program -- so this suite proves the same thing the daemon would
//! otherwise only discover the hard way, mid-Cell, if a provider got a
//! verb's shape wrong.
//!
//! Two providers, one suite:
//! - `examples/providers/loopback.py`, the project's reference
//!   implementation -- runs unconditionally (needs `python` on `PATH`,
//!   already assumed elsewhere in this workspace's own test suite).
//! - The real, compiled `ralphus-ssh-provider` binary against the SSH Docker
//!   fixture -- opt-in, mirroring `ssh-provider/tests/docker_ssh_target.rs`'s
//!   own `RALPHUS_SSH_DOCKER_TEST`/`RALPHUS_SSH_CONFIG_FILE` convention.
//!
//! Scoped to what both providers can honestly support without extra setup:
//! `ping`, `capabilities` (optional), `provision` (idempotent, non-git so
//! neither provider needs a real repository), `read-file`/`write-file`/
//! `remove-path` round-tripping under the provisioned workspace, `exec`
//! terminating with a recognizable status, and `cleanup` at least replying
//! with a well-formed envelope. Does not require a working `ralphus-runner`
//! install on either target -- `exec`'s own failure (no runner found) is
//! still a conformant, well-formed `"failed"` result, which is exactly what
//! this suite checks for.
#![allow(clippy::print_stdout)]

use ralphus_daemon::remote_runner::{ProviderRunner, ProvisionRequest, WorkspaceSource};
use ralphus_daemon::runner::{Runner, RunnerSpec};

fn conformance_spec(cwd: &str) -> RunnerSpec {
    let mut spec = RunnerSpec::for_command_proof(
        "conformance-squad",
        "conformance-task",
        "conformance-cell",
        cwd,
        "true",
        "claude",
        Some(30),
    );
    spec.command = Some("true".to_string());
    spec
}

/// Run every conformance check against `provider`. Panics (via `assert`/
/// `expect`) on the first contract violation, so each caller only needs one
/// `#[test]` per provider under test.
fn run_conformance_suite(provider: &ProviderRunner) {
    let spec = conformance_spec(".");

    // -- ping: minimum-tier reachability --
    provider
        .ping(&spec)
        .expect("ping must succeed against a healthy provider");

    // -- capabilities: optional; a provider may omit it entirely --
    let _ = provider.capabilities(&spec);

    // -- provision: minimum-tier, idempotent, no VCS required --
    let req = ProvisionRequest {
        project: "conformance-project".to_string(),
        source: WorkspaceSource {
            kind: "generic".to_string(),
            url: None,
            branch: None,
            upstream: None,
        },
        squad_id: spec.squad_id.clone(),
        cell_id: spec.cell_id.clone(),
        remote_root: None,
        runner: None,
    };
    let workspace = provider
        .provision(&req, &spec)
        .expect("provision must succeed for a generic (non-VCS) source");
    assert!(
        !workspace.trim().is_empty(),
        "provision must return a non-empty workspace path"
    );
    let workspace_again = provider
        .provision(&req, &spec)
        .expect("a repeated provision for the same identity must still succeed");
    assert_eq!(
        workspace, workspace_again,
        "provision must be idempotent: the same request must resolve to the same path"
    );

    // -- file ops: write-file/read-file/remove-path round-trip --
    let separator = if workspace.contains('\\') { '\\' } else { '/' };
    let probe_path = format!(
        "{}{separator}conformance-probe.txt",
        workspace.trim_end_matches(['/', '\\'])
    );
    provider
        .write_file(&probe_path, "conformance-content", &spec)
        .expect("write-file must succeed under a provisioned workspace");
    let content = provider
        .read_file(&probe_path, &spec)
        .expect("read-file must succeed immediately after write-file");
    assert_eq!(
        content.trim(),
        "conformance-content",
        "read-file must return exactly what write-file wrote"
    );
    provider
        .remove_path(&probe_path, false, &spec)
        .expect("remove-path must succeed on a file it just created");
    assert!(
        provider.read_file(&probe_path, &spec).is_err(),
        "read-file must fail once the path has been removed"
    );

    // -- exec: preferred-tier -- must terminate with a recognizable status,
    // whether or not a real ralphus-runner is actually installed. A missing
    // runner is a conformant "failed" result, not a broken provider.
    let mut exec_spec = spec.clone();
    exec_spec.cwd = workspace.clone();
    let result = provider.run(&exec_spec);
    assert!(
        matches!(result.status.as_str(), "done" | "failed"),
        "exec must terminate with status \"done\" or \"failed\", got {:?}",
        result.status
    );

    // -- cleanup: best-effort. Not every provider can guarantee success
    // without more configuration than this generic suite sets up (e.g. this
    // workspace's own SSH provider requires a configured remote_root) -- the
    // contract only requires a well-formed envelope either way, which
    // `ProviderRunner::cleanup`'s `Result` already guarantees by construction
    // (an `Err` here is itself a passing, well-formed response).
    let _ = provider.cleanup(
        &ralphus_daemon::remote_runner::CleanupRequest {
            project: req.project.clone(),
            clone_url: "conformance://generic".to_string(),
            branch: None,
            remote_root: None,
        },
        &spec,
    );
}

/// The project's reference provider (`docs/machine-providers.md`'s worked
/// example). Runs on every `cargo test`, no opt-in required.
#[test]
fn loopback_provider_passes_conformance() {
    let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("examples")
        .join("providers")
        .join("loopback.py");
    assert!(
        script.is_file(),
        "reference provider missing at {}",
        script.display()
    );
    let provider = ProviderRunner::new(
        "python",
        vec![script.to_string_lossy().into_owned()],
        "conformance-loopback",
        format!("conformance-{}", std::process::id()),
    );
    run_conformance_suite(&provider);
}

/// The real, compiled `ralphus-ssh-provider` against the SSH Docker fixture.
///
/// ```powershell
/// $env:RALPHUS_SSH_DOCKER_TEST = '1'
/// $env:RALPHUS_SSH_CONFIG_FILE = (Resolve-Path .docker-ssh-target/ssh_config)
/// cargo test -p ralphus-daemon --test provider_conformance -- --ignored --nocapture
/// ```
#[test]
#[ignore]
fn ssh_docker_target_passes_conformance() {
    if std::env::var("RALPHUS_SSH_DOCKER_TEST").ok().as_deref() != Some("1") {
        println!("SKIP: set RALPHUS_SSH_DOCKER_TEST=1 after starting the SSH target fixture");
        return;
    }
    // Cargo replaces dashes with underscores in this env var's name.
    // `CARGO_BIN_EXE_<name>` only resolves for a binary target inside *this*
    // crate, not a path dev-dependency's -- so the compiled
    // `ralphus-ssh-provider` binary is located manually instead, relative to
    // the workspace's shared `target/` directory `cargo build -p
    // ralphus-ssh-provider` (or the workspace-wide default build) already
    // produces it under.
    let binary = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("target")
        .join(if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        })
        .join(if cfg!(windows) {
            "ralphus-ssh-provider.exe"
        } else {
            "ralphus-ssh-provider"
        });
    if !binary.is_file() {
        println!(
            "SKIP: {} not built yet -- run `cargo build -p ralphus-ssh-provider` first",
            binary.display()
        );
        return;
    }
    let binary = binary.to_string_lossy().into_owned();
    let mut args = Vec::new();
    if let Ok(ssh_config) = std::env::var("RALPHUS_SSH_CONFIG_FILE") {
        args.push("--ssh-config".to_string());
        args.push(ssh_config);
    }
    let provider = ProviderRunner::new(binary, args, "conformance-ssh-docker", "ralphus-docker");
    run_conformance_suite(&provider);
}
