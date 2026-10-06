//! Opt-in end-to-end test of container mode against
//! `docker/ssh-docker-target-compose.yml`: a remote host that has its own
//! Docker engine, on which the provider creates and enters a work container.
//!
//! Every hop is real -- provider -> `ssh` -> remote host -> `docker exec` ->
//! work container -- only the network distance is faked. Start the fixture
//! with `scripts/ssh-docker-target.sh up`, then:
//!
//! ```bash
//! RALPHUS_SSH_DOCKER_CONTAINER_TEST=1 \
//! RALPHUS_SSH_CONFIG_FILE=$PWD/.docker-ssh-docker-target/ssh_config \
//!   cargo nextest run -p ralphus-ssh-provider --test docker_container_target --run-ignored all
//! ```
//!
//! Each test is its own process under nextest, so each configures container
//! mode itself. Under plain `cargo test` use `-- --ignored --test-threads=1`
//! is not enough (the setting is process-wide and shared); use nextest.

#![allow(clippy::print_stdout)]

use std::process::{Command, Stdio};

use ralphus_ssh_provider::container::{self, ContainerConfig};
use ralphus_ssh_provider::exec::EffectiveConfig;
use ralphus_ssh_provider::{exec, fileops, job, ping, provision, ssh};

const URI: &str = "ralphus-docker-docker";
const WORK_IMAGE: &str = "ralphus-remote-agent:test";
/// Where the work container's remote root lives, inside the container.
const REMOTE_ROOT: &str = "/home/ralphus/.ralphus/remote-work";
/// The same directory on the remote host, bind-mounted into the container.
const HOST_WORK_ROOT: &str = "/srv/ralphus-work";

fn work_container(name: Option<&str>) -> ContainerConfig {
    ContainerConfig {
        image: WORK_IMAGE.to_string(),
        name: name.map(str::to_string),
        mounts: vec![format!("{HOST_WORK_ROOT}:{REMOTE_ROOT}")],
        ..ContainerConfig::default()
    }
}

/// Enable container mode and return the effective config, or `None` (with a
/// SKIP note) when the fixture is not opted into.
fn fixture() -> Option<EffectiveConfig> {
    fixture_named(None)
}

/// [`fixture`] with an explicit container name. Tests that stop, restart or
/// remove the container use their own, so they cannot disturb tests that run
/// beside them against the shared default one.
fn fixture_named(name: Option<&str>) -> Option<EffectiveConfig> {
    if std::env::var("RALPHUS_SSH_DOCKER_CONTAINER_TEST")
        .ok()
        .as_deref()
        != Some("1")
    {
        println!(
            "SKIP: set RALPHUS_SSH_DOCKER_CONTAINER_TEST=1 after `scripts/ssh-docker-target.sh up`"
        );
        return None;
    }
    container::configure(work_container(name));
    Some(EffectiveConfig {
        remote_base: format!("{REMOTE_ROOT}/legacy-sync"),
        extra_excludes: Vec::new(),
        connect_timeout_secs: 10,
        remote_runner_cmd: "ralphus-runner".to_string(),
        ssh_config_file: std::env::var("RALPHUS_SSH_CONFIG_FILE").ok(),
        target_runner_config: None,
    })
}

/// Run `script` on the remote *host* (never in the container) and return
/// (exit code, stdout, stderr).
fn on_host(config: &EffectiveConfig, script: &str) -> (i32, String, String) {
    let args = ssh::host_command_args(
        URI,
        config.connect_timeout_secs,
        script,
        config.ssh_config_file.as_deref(),
    );
    let out = Command::new("ssh")
        .args(&args)
        .stdin(Stdio::null())
        .output()
        .expect("ssh must be installed");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).trim().to_string(),
        String::from_utf8_lossy(&out.stderr).trim().to_string(),
    )
}

fn container_name() -> String {
    container::container_name(
        &work_container(None),
        &container::ContainerScope::Machine,
        URI,
    )
}

fn runner_policy() -> serde_json::Value {
    serde_json::json!({
        "mode": "installed",
        "command": "ralphus-runner",
        "artifacts": {},
        "remote_root": REMOTE_ROOT,
    })
}

/// Seed a bare origin *inside the container* and return its clone URL.
fn seed_origin(config: &EffectiveConfig, name: &str) -> String {
    let mut with_target = config.clone();
    with_target.target_runner_config = Some(runner_policy().to_string());
    let run = |args: &[&str]| {
        let payload = serde_json::json!({"cwd": REMOTE_ROOT, "program": "git", "args": args});
        let (out, code) = fileops::run(URI, &payload.to_string(), &with_target)
            .unwrap_or_else(|e| panic!("git {args:?} failed to dispatch: {e}"));
        assert_eq!(code, 0, "git {args:?} exited {code}: {out}");
    };
    let origin = format!("{name}.git");
    let seed = format!("{name}-seed");
    let remove = serde_json::json!({"path": format!("{REMOTE_ROOT}/{seed}"), "recursive": true});
    fileops::remove_path(URI, &remove.to_string(), &with_target).expect("clean seed dir");
    let remove = serde_json::json!({"path": format!("{REMOTE_ROOT}/{origin}"), "recursive": true});
    fileops::remove_path(URI, &remove.to_string(), &with_target).expect("clean origin dir");
    run(&["init", "--bare", "-b", "main", &origin]);
    run(&["clone", &origin, &seed]);
    run(&[
        "-C",
        &seed,
        "-c",
        "user.name=fixture",
        "-c",
        "user.email=fixture@example.invalid",
        "commit",
        "--allow-empty",
        "-m",
        "seed",
    ]);
    run(&["-C", &seed, "push", "origin", "HEAD:main"]);
    format!("file://{REMOTE_ROOT}/{origin}")
}

fn provision_workspace(config: &EffectiveConfig, origin_url: &str, identity: &str) -> String {
    let request = serde_json::json!({
        "project": "container-test",
        "source": {
            "kind": "git",
            "url": origin_url,
            "branch": identity,
            "upstream": "origin/main",
        },
        "squad_id": identity,
        "cell_id": "workspace",
        "remote_root": REMOTE_ROOT,
        "runner": runner_policy(),
    });
    provision::run(URI, &request.to_string(), config).expect("provision inside the container")
}

fn wait_for_terminal(handle: &str, config: &EffectiveConfig) -> job::Status {
    for _ in 0..200 {
        let status = job::status(URI, handle, config).expect("job status must stay readable");
        if !matches!(status.state.as_str(), "starting" | "running") {
            return status;
        }
        std::thread::sleep(std::time::Duration::from_millis(150));
    }
    panic!("job {handle} did not finish in time");
}

fn target_config(config: &EffectiveConfig) -> EffectiveConfig {
    let mut c = config.clone();
    c.target_runner_config = Some(runner_policy().to_string());
    c
}

#[test]
#[ignore]
fn ping_creates_the_container_once_and_reuses_it() {
    let Some(config) = fixture() else { return };
    let detail = ping::run(URI, &config)
        .expect("ping must bring the container up")
        .expect("ping reports a detail");
    let name = container_name();
    assert!(detail.contains(&name), "{detail}");

    let first = container::ensure(URI, &config).expect("ensure");
    let second = container::ensure(URI, &config).expect("ensure is idempotent");
    assert_eq!(first, second);
    assert_eq!(first, name);

    // The machine's container is among the host's managed ones, and running.
    let (code, out, err) = on_host(
        &config,
        "docker ps --filter label=ralphus.managed=1 --format '{{.Names}} {{.Image}}'",
    );
    assert_eq!(code, 0, "{err}");
    let expected = format!("{name} {WORK_IMAGE}");
    assert_eq!(
        out.lines().filter(|line| *line == expected).count(),
        1,
        "exactly one container per machine: {out}"
    );
}

#[test]
#[ignore]
fn a_stopped_container_is_started_again_and_a_removed_one_recreated() {
    let Some(config) = fixture_named(Some("ralphus-test-recreate")) else {
        return;
    };
    let name = container::ensure(URI, &config).expect("create");
    let (code, _, err) = on_host(&config, &format!("docker stop -t 1 '{name}'"));
    assert_eq!(code, 0, "{err}");
    assert_eq!(container::ensure(URI, &config).expect("restart"), name);
    let (_, running, _) = on_host(
        &config,
        &format!("docker inspect -f '{{{{.State.Running}}}}' '{name}'"),
    );
    assert_eq!(running, "true");

    let (code, _, err) = on_host(&config, &format!("docker rm -f '{name}'"));
    assert_eq!(code, 0, "{err}");
    assert_eq!(container::ensure(URI, &config).expect("recreate"), name);
    let (_, running, _) = on_host(
        &config,
        &format!("docker inspect -f '{{{{.State.Running}}}}' '{name}'"),
    );
    assert_eq!(running, "true");
}

#[test]
#[ignore]
fn a_container_made_from_another_image_is_refused_not_adopted() {
    let Some(config) = fixture() else { return };
    let name = container::ensure(URI, &config).expect("create");
    let other = ContainerConfig {
        image: "alpine:latest".to_string(),
        ..work_container(None)
    };
    let script = container::ensure_script(&other, &name);
    let (code, _, err) = on_host(&config, &script);
    assert_eq!(code, 42, "must refuse, got exit {code}: {err}");
    assert!(err.contains("was created from image"), "{err}");
}

#[test]
#[ignore]
fn work_runs_in_the_container_not_in_the_host_account() {
    let Some(config) = fixture() else { return };
    container::ensure(URI, &config).expect("create");
    let config = target_config(&config);
    let origin = seed_origin(&config, "boundary-origin");
    let workspace = provision_workspace(&config, &origin, "boundary");
    assert!(workspace.starts_with(REMOTE_ROOT), "{workspace}");

    // The host is Alpine running as uid 10002; the container is Debian as
    // uid 10001. A cell that reports both can only have run in the container.
    let spec = serde_json::json!({
        "squad_id": "boundary", "task": "t", "cell_id": "c",
        "cwd": workspace,
        "command": "echo uid=$(id -u); grep ^ID= /etc/os-release",
        "agent": "claude-code", "model": null,
    });
    let handle = job::start(URI, &spec.to_string(), &config).expect("start");
    let status = wait_for_terminal(&handle, &config);
    assert_eq!(status.state, "done", "{status:?}");
    let out = job::stream(URI, &handle, 0, &config)
        .expect("stream")
        .output;
    assert!(out.contains("uid=10001"), "{out}");
    assert!(out.contains("ID=debian"), "{out}");
    job::cleanup(URI, &handle, &config).expect("cleanup");

    // The remote root is a bind mount: what the container wrote is visible on
    // the host, which is what lets provisioned state outlive the container.
    // The mount is owned by the container's uid, which the host account is not,
    // so list it from a throwaway root container sharing the host directory.
    let (code, listing, err) = on_host(
        &config,
        &format!(
            "docker run --rm --user 0 --entrypoint ls -v {HOST_WORK_ROOT}:/w {WORK_IMAGE} /w/projects"
        ),
    );
    assert_eq!(code, 0, "{err}");
    assert!(listing.contains("container-test"), "{listing}");
    // ...and the host account's own home was never touched.
    let (_, home, _) = on_host(&config, "ls -A ~ | grep -c remote-work || true");
    assert_eq!(home, "0", "the ssh account's home must be untouched");
}

#[test]
#[ignore]
fn mock_agent_cell_runs_through_the_legacy_synchronous_path() {
    let Some(config) = fixture() else { return };
    container::ensure(URI, &config).expect("create");
    let source =
        std::env::temp_dir().join(format!("ralphus-container-source-{}", std::process::id()));
    std::fs::create_dir_all(&source).expect("source dir");
    std::fs::write(source.join("marker.txt"), "host-side marker\n").expect("marker");
    let spec = serde_json::json!({
        "squad_id": "legacy", "task": "t", "cell_id": "c",
        "cwd": source.to_string_lossy(),
        "prompt": "Return the deterministic fixture response.",
        "agent": "claude-code", "model": "fixture-model",
    });
    let result = exec::run(URI, &spec.to_string(), &config).expect("runner in the container");
    assert_eq!(result["status"], serde_json::json!("done"), "{result:?}");
    assert_eq!(
        result["summary"],
        serde_json::json!("remote mock completed"),
        "{result:?}"
    );
    assert_eq!(result["tokens_in"], serde_json::json!(11), "{result:?}");
    let _ = std::fs::remove_dir_all(source);
}

#[test]
#[ignore]
fn async_job_lifecycle_works_in_one_pid_namespace() {
    let Some(config) = fixture() else { return };
    container::ensure(URI, &config).expect("create");
    let config = target_config(&config);
    let origin = seed_origin(&config, "lifecycle-origin");
    let workspace = provision_workspace(&config, &origin, "lifecycle");
    let spec = serde_json::json!({
        "squad_id": "lifecycle", "task": "t", "cell_id": "c",
        "cwd": workspace,
        "command": "test \"$CONTAINER_TEST_VALUE\" = in-container; printf 'first\\n'; sleep 1; printf 'second\\n'",
        "agent": "claude-code", "model": null,
        "execution_environment": {"CONTAINER_TEST_VALUE": "in-container"},
    });
    let handle = job::start(URI, &spec.to_string(), &config).expect("start");
    let duplicate = job::start(URI, &spec.to_string(), &config).expect("duplicate reconciles");
    assert_eq!(handle, duplicate, "duplicate dispatch spawned a new job");
    let running = job::status(URI, &handle, &config).expect("status while running");
    assert!(
        matches!(running.state.as_str(), "starting" | "running" | "done"),
        "a live job must not read as lost: {running:?}"
    );
    let terminal = wait_for_terminal(&handle, &config);
    assert_eq!(terminal.state, "done", "{terminal:?}");
    let out = job::stream(URI, &handle, 0, &config)
        .expect("stream")
        .output;
    assert!(out.contains("first") && out.contains("second"), "{out}");
    job::cleanup(URI, &handle, &config).expect("cleanup");
    assert!(job::status(URI, &handle, &config).is_err());
}

#[test]
#[ignore]
fn cancel_kills_the_whole_process_tree_inside_the_container() {
    let Some(config) = fixture() else { return };
    container::ensure(URI, &config).expect("create");
    let config = target_config(&config);
    let origin = seed_origin(&config, "cancel-origin");
    let workspace = provision_workspace(&config, &origin, "cancel");
    let spec = serde_json::json!({
        "squad_id": "cancel", "task": "t", "cell_id": "c",
        "cwd": workspace,
        "command": "sh -c 'sleep 4242 & wait'",
        "agent": "claude-code", "model": null,
    });
    let handle = job::start(URI, &spec.to_string(), &config).expect("start");
    // Let the child tree come up before cancelling.
    std::thread::sleep(std::time::Duration::from_secs(2));
    let name = container_name();
    let (_, before, _) = on_host(
        &config,
        &format!("docker exec '{name}' pgrep -fc '[s]leep 4242' || true"),
    );
    assert_ne!(
        before.trim(),
        "0",
        "the sleeper must be running before cancel"
    );

    job::cancel(URI, &handle, &config).expect("cancel");
    let status = job::status(URI, &handle, &config).expect("terminal result retained");
    assert_eq!(status.state, "failed", "{status:?}");
    std::thread::sleep(std::time::Duration::from_millis(500));
    let (_, after, _) = on_host(
        &config,
        &format!("docker exec '{name}' pgrep -fc '[s]leep 4242' || true"),
    );
    assert_eq!(after.trim(), "0", "descendants survived the cancel");
}

#[test]
#[ignore]
fn a_container_restart_reports_the_job_lost_not_running() {
    let Some(config) = fixture_named(Some("ralphus-test-restart")) else {
        return;
    };
    let name = container::ensure(URI, &config).expect("create");
    let config = target_config(&config);
    let origin = seed_origin(&config, "restart-origin");
    let workspace = provision_workspace(&config, &origin, "restart");
    let spec = serde_json::json!({
        "squad_id": "restart", "task": "t", "cell_id": "c",
        "cwd": workspace,
        "command": "sleep 300",
        "agent": "claude-code", "model": null,
    });
    let handle = job::start(URI, &spec.to_string(), &config).expect("start");
    std::thread::sleep(std::time::Duration::from_secs(1));
    let (code, _, err) = on_host(&config, &format!("docker restart -t 1 '{name}'"));
    assert_eq!(code, 0, "{err}");
    // The job dir survives on the mount, but its processes died with the
    // container: status must say so rather than keep claiming it runs.
    for _ in 0..40 {
        match job::status(URI, &handle, &config) {
            // A lost job is reported through the error channel; the daemon
            // classifies it from this text.
            Err(e) => {
                assert!(e.contains("is lost"), "{e}");
                return;
            }
            Ok(status) if matches!(status.state.as_str(), "running" | "starting") => {
                std::thread::sleep(std::time::Duration::from_millis(250));
            }
            Ok(status) => {
                assert!(
                    matches!(status.state.as_str(), "lost" | "failed"),
                    "{status:?}"
                );
                return;
            }
        }
    }
    panic!("job still reads as running after its container restarted");
}

#[test]
#[ignore]
fn file_operations_round_trip_binary_safe_content_in_the_container() {
    let Some(config) = fixture() else { return };
    container::ensure(URI, &config).expect("create");
    let config = target_config(&config);
    let path = format!("{REMOTE_ROOT}/fileops/nested/hello.txt");
    let write = serde_json::json!({"path": path, "content": "héllo 'quoted' $HOME `x`\nline2\n"});
    fileops::write_file(URI, &write.to_string(), &config).expect("write");
    let read = serde_json::json!({"path": path});
    let content = fileops::read_file(URI, &read.to_string(), &config).expect("read");
    assert_eq!(content, "héllo 'quoted' $HOME `x`\nline2\n");
    let rm = serde_json::json!({"path": format!("{REMOTE_ROOT}/fileops"), "recursive": true});
    fileops::remove_path(URI, &rm.to_string(), &config).expect("remove");
    assert!(fileops::read_file(URI, &read.to_string(), &config).is_err());
}

#[test]
#[ignore]
fn terminal_allocates_a_pty_inside_the_container_with_the_requested_size() {
    let Some(config) = fixture() else { return };
    container::ensure(URI, &config).expect("create");
    let mut args = vec![
        "--ssh-config".to_string(),
        config.ssh_config_file.clone().expect("ssh config"),
        "--container-image".to_string(),
        WORK_IMAGE.to_string(),
        "--container-mount".to_string(),
        format!("{HOST_WORK_ROOT}:{REMOTE_ROOT}"),
    ];
    args.extend(
        [
            "terminal",
            "--uri",
            URI,
            "--cols",
            "101",
            "--lines",
            "31",
            // The trailing sleep keeps the pty open long enough for the output
            // to drain before this test's closed stdin ends the session.
            "--command",
            "echo uid=$(id -u) cols=$COLUMNS lines=$LINES; tty; sleep 2",
        ]
        .map(str::to_string),
    );
    let out = Command::new(env!("CARGO_BIN_EXE_ralphus-ssh-provider"))
        .args(&args)
        .stdin(Stdio::null())
        .output()
        .expect("run the provider's terminal verb");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("uid=10001 cols=101 lines=31"),
        "the command must run in the container with the requested size: {stdout:?} / {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        stdout.contains("/dev/pts/"),
        "the command must have a real terminal: {stdout:?}"
    );
}

#[test]
#[ignore]
fn materialize_streams_binary_safe_files_and_directories_out_of_the_container() {
    use ralphus_ssh_provider::materialize;

    let Some(config) = fixture() else { return };
    container::ensure(URI, &config).expect("create");
    let config = target_config(&config);
    let dir = format!("{REMOTE_ROOT}/materialize-test-{}", std::process::id());
    // Bytes that text-mode transports corrupt: NUL, high bytes, CR/LF.
    let make = serde_json::json!({
        "cwd": REMOTE_ROOT,
        // An empty program with one argument is the provider's "authored
        // shell command" form.
        "program": "",
        "args": [format!(
            "mkdir -p {dir}/sub && printf '\\000\\001\\377\\r\\n\\000' > {dir}/blob.bin && printf 'nested' > {dir}/sub/n.txt"
        )],
    });
    let (out, code) = fileops::run(URI, &make.to_string(), &config).expect("seed files");
    assert_eq!(code, 0, "{out}");

    let local = std::env::temp_dir().join(format!("ralphus-materialize-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&local);
    let file_dest = local.join("out").join("blob.bin");
    let request = serde_json::json!({
        "source": format!("{dir}/blob.bin"),
        "destination": file_dest.to_string_lossy(),
    });
    materialize::run(URI, &request.to_string(), &config).expect("materialize a file");
    assert_eq!(
        std::fs::read(&file_dest).expect("materialized file"),
        [0_u8, 1, 255, b'\r', b'\n', 0],
        "bytes must cross the container boundary unchanged"
    );

    let dir_dest = local.join("out").join("tree");
    let request = serde_json::json!({
        "source": dir,
        "destination": dir_dest.to_string_lossy(),
    });
    materialize::run(URI, &request.to_string(), &config).expect("materialize a directory");
    assert_eq!(
        std::fs::read_to_string(dir_dest.join("sub").join("n.txt")).expect("nested file"),
        "nested"
    );

    let missing = serde_json::json!({
        "source": format!("{dir}/does-not-exist"),
        "destination": local.join("out").join("missing").to_string_lossy(),
    });
    assert!(
        materialize::run(URI, &missing.to_string(), &config).is_err(),
        "a missing source must fail rather than publish an empty destination"
    );
    let _ = std::fs::remove_dir_all(&local);
}
