//! Shared private daemon bootstrap for guided live exercises.

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use crate::client::DaemonClient;

const READY_TIMEOUT: Duration = Duration::from_secs(10);

pub struct Exercise {
    pub root: PathBuf,
    pub url: String,
    pub client: DaemonClient,
}

impl Exercise {
    pub fn start(kind: &str, state_dir: Option<String>) -> Result<Self, String> {
        let root = state_dir.map(PathBuf::from).unwrap_or_else(|| {
            std::env::temp_dir().join(format!("ralphus-{kind}-exercise-{}", std::process::id()))
        });
        std::fs::create_dir_all(&root)
            .map_err(|e| format!("could not create {}: {e}", root.display()))?;
        let port = reserve_port().map_err(|e| format!("could not reserve a local port: {e}"))?;
        let started = Instant::now();
        let pid = start_daemon(&root, kind, port).map_err(|e| {
            // stderr only: stdout is this command's output.
            eprintln!(
                "ralphus [exercise] isolated daemon failed to spawn kind={kind} root={} error={e}",
                root.display()
            );
            format!("could not start isolated daemon: {e}")
        })?;
        let log_path = root.join("daemon.log");
        eprintln!(
            "ralphus [exercise] isolated daemon spawned kind={kind} pid={pid} port={port} root={} log={}",
            root.display(),
            log_path.display()
        );
        let token = wait_for_token(&root.join("home").join(".ralphus").join("daemon.token"))
            .inspect_err(|e| log_not_ready(kind, pid, &log_path, e))?;
        let url = format!("http://127.0.0.1:{port}");
        let client = DaemonClient::with_token(&url, token);
        wait_for_daemon(&client).inspect_err(|e| log_not_ready(kind, pid, &log_path, e))?;
        eprintln!(
            "ralphus [exercise] isolated daemon ready kind={kind} pid={pid} url={url} elapsed_ms={}",
            started.elapsed().as_millis()
        );
        Ok(Self { root, url, client })
    }

    pub fn examples_dir(&self) -> Result<PathBuf, String> {
        let path = self.root.join("examples");
        std::fs::create_dir_all(&path).map_err(|e| e.to_string())?;
        Ok(path)
    }

    pub fn register_current_project(&self, label: &str) -> Result<(String, String), String> {
        let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
        let path = cwd.to_string_lossy().into_owned();
        let name = format!("{label}-{}", std::process::id());
        self.client
            .register_project(
                &name,
                &path,
                "Disposable guided exercise",
                "git",
                None,
                false,
                None,
            )
            .map_err(|e| {
                eprintln!(
                    "ralphus [exercise] project registration failed name={name} path={path} error={e}"
                );
                e.to_string()
            })?;
        eprintln!("ralphus [exercise] project registered name={name} path={path}");
        Ok((name, path))
    }
}

/// Run one guided exercise and log its outcome on stderr: which exercise,
/// its exit code, and how long it took. Every exercise reports its own
/// failure reason on stdout; this line is the stderr record that it ran.
pub fn run_logged(kind: &str, run: impl FnOnce() -> i32) -> i32 {
    let started = Instant::now();
    let code = run();
    let outcome = if code == 0 { "completed" } else { "failed" };
    eprintln!(
        "ralphus [exercise] {outcome} kind={kind} exit_code={code} elapsed_ms={}",
        started.elapsed().as_millis()
    );
    code
}

/// The daemon outlives this process by design (the exercise leaves it running
/// for inspection), so a startup timeout names its pid and log file -- the
/// only places left to find out why it never came up, or to stop it.
fn log_not_ready(kind: &str, pid: u32, log_path: &Path, error: &str) {
    eprintln!(
        "ralphus [exercise] isolated daemon not ready kind={kind} pid={pid} log={} error={error}",
        log_path.display()
    );
}

fn reserve_port() -> std::io::Result<u16> {
    TcpListener::bind("127.0.0.1:0")?
        .local_addr()
        .map(|a| a.port())
}

/// Spawn the isolated daemon and return its pid.
fn start_daemon(root: &Path, kind: &str, port: u16) -> std::io::Result<u32> {
    let home = root.join("home");
    let config = root.join("config");
    let psmux_data = root.join("psmux");
    std::fs::create_dir_all(&home)?;
    std::fs::create_dir_all(&config)?;
    std::fs::create_dir_all(&psmux_data)?;
    let daemon = std::env::current_exe()?
        .parent()
        .map(|dir| {
            dir.join(if cfg!(windows) {
                "ralphus-daemon.exe"
            } else {
                "ralphus-daemon"
            })
        })
        .filter(|path| path.is_file())
        .unwrap_or_else(|| PathBuf::from("ralphus-daemon"));
    let runner = std::env::current_exe()?
        .parent()
        .map(|dir| {
            dir.join(if cfg!(windows) {
                "ralphus-runner.exe"
            } else {
                "ralphus-runner"
            })
        })
        .filter(|path| path.is_file())
        .unwrap_or_else(|| PathBuf::from("ralphus-runner"));
    Command::new(daemon)
        .args(["serve", "--port", &port.to_string(), "--db"])
        .arg(root.join(format!("{kind}-exercise.db")))
        .arg("--log-path")
        .arg(root.join("daemon.log"))
        .env("USERPROFILE", &home)
        .env("HOME", &home)
        .env("RALPHUS_CONFIG_HOME", &config)
        .env("RALPHUS_LOOPBACK_STATE_ROOT", root.join("providers"))
        // psmux otherwise shares machine-wide session state with a developer's
        // normal daemon. Its data directory is part of an exercise's private
        // state just like the database and token.
        .env("PSMUX_DATA_DIR", psmux_data)
        .env("RALPHUS_RUNNER_CMD", runner)
        .env_remove("RALPHUS_CONFIGURATION_PATH")
        .spawn()
        .map(|child| child.id())
}

fn wait_for_token(path: &Path) -> Result<String, String> {
    let deadline = Instant::now() + READY_TIMEOUT;
    while Instant::now() < deadline {
        if let Ok(token) = std::fs::read_to_string(path) {
            let token = token.trim();
            if !token.is_empty() {
                return Ok(token.to_string());
            }
        }
        thread::sleep(Duration::from_millis(100));
    }
    Err(format!("isolated daemon did not write {}", path.display()))
}

fn wait_for_daemon(client: &DaemonClient) -> Result<(), String> {
    let deadline = Instant::now() + READY_TIMEOUT;
    while Instant::now() < deadline {
        if client.health().is_ok() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(100));
    }
    Err("isolated daemon health endpoint timed out".to_string())
}
