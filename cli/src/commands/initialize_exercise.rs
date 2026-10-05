//! Shared private daemon bootstrap for guided live exercises.

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::client::DaemonClient;

const READY_TIMEOUT: Duration = Duration::from_secs(10);

/// The machine every `--remote` exercise targets: the loopback provider in
/// strict mode, with its own `remote_root` under the exercise's state root.
pub const REMOTE_MACHINE: &str = "loopback:exercise";

/// The options every guided exercise shares.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExerciseOptions {
    /// Where the exercise keeps its daemon, database, and fixtures.
    pub state_dir: Option<String>,
    /// Run every cell (and every review) on [`REMOTE_MACHINE`] instead of the
    /// daemon's own host.
    pub remote: bool,
    /// Stop the isolated daemon once the exercise has finished, instead of
    /// leaving it running for inspection.
    pub stop: bool,
}

/// A git project created for one exercise: a working clone with a bare
/// `origin` beside it, registered with the daemon by both path and clone URL
/// (a remote machine provisions from the URL).
pub struct Fixture {
    pub name: String,
    pub path: PathBuf,
}

pub struct Exercise {
    pub root: PathBuf,
    pub url: String,
    pub client: DaemonClient,
    remote: bool,
    stop: bool,
}

impl Exercise {
    pub fn start(kind: &str, options: &ExerciseOptions) -> Result<Self, String> {
        let root = options
            .state_dir
            .as_ref()
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                std::env::temp_dir().join(format!("ralphus-{kind}-exercise-{}", std::process::id()))
            });
        std::fs::create_dir_all(&root)
            .map_err(|e| format!("could not create {}: {e}", root.display()))?;
        let root = std::path::absolute(&root)
            .map_err(|e| format!("could not resolve {}: {e}", root.display()))?;
        if options.remote {
            write_remote_target(&root)?;
        }
        let port = reserve_port().map_err(|e| format!("could not reserve a local port: {e}"))?;
        let started = Instant::now();
        let pid = start_daemon(&root, kind, port, options.remote).map_err(|e| {
            // stderr only: stdout is this command's output.
            eprintln!(
                "ralphus [exercise] isolated daemon failed to spawn kind={kind} root={} error={e}",
                root.display()
            );
            format!("could not start isolated daemon: {e}")
        })?;
        let log_path = root.join("daemon.log");
        eprintln!(
            "ralphus [exercise] isolated daemon spawned kind={kind} pid={pid} port={port} remote={} root={} log={}",
            options.remote,
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
        let exercise = Self {
            root,
            url,
            client,
            remote: options.remote,
            stop: options.stop,
        };
        if options.remote {
            exercise
                .register_loopback()
                .map_err(|e| format!("could not register the loopback machine: {e}"))?;
        }
        Ok(exercise)
    }

    /// Whether this exercise runs its work on [`REMOTE_MACHINE`].
    pub fn remote(&self) -> bool {
        self.remote
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

    /// Create and register a fresh git fixture (`<label>-fixture` plus a bare
    /// `<label>-origin.git`, default branch `main`) under the state root.
    pub fn fixture_project(&self, label: &str) -> Result<Fixture, String> {
        let repo = self.root.join(format!("{label}-fixture"));
        let origin = self.root.join(format!("{label}-origin.git"));
        create_fixture_repo(&repo, &origin, label)
            .map_err(|e| format!("could not create the {label} fixture: {e}"))?;
        let name = format!("{label}-exercise-{}", std::process::id());
        let path = repo.to_string_lossy().into_owned();
        let url = origin.to_string_lossy().replace('\\', "/");
        self.client
            .register_project(
                &name,
                &path,
                "Disposable guided exercise fixture",
                "git",
                Some(&url),
                false,
                None,
            )
            .map_err(|e| format!("could not register the {label} fixture: {e}"))?;
        eprintln!("ralphus [exercise] fixture registered name={name} path={path} url={url}");
        Ok(Fixture { name, path: repo })
    }

    /// The `machine = ...` line a task needs to run on [`REMOTE_MACHINE`], or
    /// nothing for a local exercise.
    pub fn machine_line(&self) -> String {
        if self.remote {
            format!("machine = {REMOTE_MACHINE:?}\n")
        } else {
            String::new()
        }
    }

    /// A cell `cwd` for `fixture`: a fresh worktree branch provisioned on the
    /// machine when remote (a machine has no copy of the daemon's checkout),
    /// else the fixture checkout itself.
    pub fn cell_cwd(&self, fixture: &Fixture, branch: &str) -> String {
        if self.remote {
            format!("<<ralphus:new-worktree/{branch}?upstream=<<default>>>>")
        } else {
            fixture.path.to_string_lossy().into_owned()
        }
    }

    /// Register the loopback provider as this daemon's `loopback` scheme and
    /// confirm it answers.
    pub fn register_loopback(&self) -> Result<(), String> {
        let script = loopback_script()?;
        let args = vec![script.to_string_lossy().into_owned()];
        self.client
            .register_machine(
                "loopback",
                python_program(),
                "Disposable guided-exercise machine",
                Some(&args),
                None,
                false,
            )
            .map_err(|e| e.to_string())?;
        match self.client.check_machine("loopback") {
            Ok(value) if value["ok"] == true => Ok(()),
            Ok(value) => Err(format!("loopback ping failed: {value}")),
            Err(error) => Err(format!("loopback ping failed: {error}")),
        }
    }

    /// Poll `squad` until it is terminal, returning its final state.
    pub fn wait_terminal(&self, squad: &str, timeout: Duration) -> Result<String, String> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            let state = self.client.squad(squad).map_err(|e| e.to_string())?["state"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            if matches!(state.as_str(), "done" | "failed" | "cancelled") {
                return Ok(state);
            }
            thread::sleep(Duration::from_millis(250));
        }
        Err(format!("{squad} did not reach a terminal state"))
    }
}

impl Drop for Exercise {
    fn drop(&mut self) {
        if !self.stop {
            return;
        }
        match self.client.shutdown_daemon(true) {
            Ok(_) => eprintln!(
                "ralphus [exercise] isolated daemon stopped url={}",
                self.url
            ),
            Err(error) => eprintln!(
                "ralphus [exercise] isolated daemon did not stop url={} error={error}",
                self.url
            ),
        }
    }
}

/// Extract `squad_id` from a submission response.
pub fn squad_id(value: &Value) -> Result<String, String> {
    value["squad_id"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| "submission response omitted squad_id".to_string())
}

/// A shell line that creates `file` and commits + pushes it on the cell's own
/// branch. The exercise daemon pins `RALPHUS_SHELL` (`cmd` on Windows, `sh`
/// elsewhere), and both accept this exact syntax.
pub fn commit_and_push_command(file: &str, text: &str) -> String {
    format!(
        "echo {text}> {file} && git add {file} && git -c user.email=exercise@example.invalid -c user.name=Exercise commit -q -m {text} && git push -q -u origin HEAD"
    )
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

/// The interpreter the loopback provider runs under.
fn python_program() -> &'static str {
    if cfg!(windows) { "python" } else { "python3" }
}

/// Locate `examples/providers/loopback.py`: `RALPHUS_LOOPBACK_SCRIPT`, else
/// the current checkout, else a checkout above this executable (a
/// `target/<profile>/ralphus` build run from anywhere).
fn loopback_script() -> Result<PathBuf, String> {
    let relative = Path::new("examples").join("providers").join("loopback.py");
    if let Ok(path) = std::env::var("RALPHUS_LOOPBACK_SCRIPT") {
        let path = PathBuf::from(path);
        return path
            .is_file()
            .then_some(path.clone())
            .ok_or_else(|| format!("RALPHUS_LOOPBACK_SCRIPT={} is not a file", path.display()));
    }
    let mut candidates = Vec::new();
    if let Ok(cwd) = std::env::current_dir() {
        candidates.push(cwd.join(&relative));
    }
    if let Ok(exe) = std::env::current_exe() {
        candidates.extend(exe.ancestors().skip(1).map(|dir| dir.join(&relative)));
    }
    candidates.into_iter().find(|p| p.is_file()).ok_or_else(|| {
        "examples/providers/loopback.py was not found; run from a ralphus checkout or set \
         RALPHUS_LOOPBACK_SCRIPT"
            .to_string()
    })
}

/// Configure [`REMOTE_MACHINE`] as a machine target whose `remote_root` lives
/// under the exercise's own state root. The daemon reads targets from
/// `$RALPHUS_CONFIG_HOME/config.toml`, which [`start_daemon`] points here.
fn write_remote_target(root: &Path) -> Result<(), String> {
    let config = root.join("config");
    std::fs::create_dir_all(&config).map_err(|e| e.to_string())?;
    let remote_root = root
        .join("remote-root")
        .to_string_lossy()
        .replace('\\', "/");
    let body = format!(
        "[machine.targets.exercise]\nmachine = {REMOTE_MACHINE:?}\nremote_root = {remote_root:?}\n\
         # An exercise lives in a temporary directory by design.\nallow_ephemeral_remote_root = true\n"
    );
    std::fs::write(config.join("config.toml"), body).map_err(|e| e.to_string())
}

fn sibling_exe(name: &str) -> std::io::Result<PathBuf> {
    let file = if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    };
    Ok(std::env::current_exe()?
        .parent()
        .map(|dir| dir.join(&file))
        .filter(|path| path.is_file())
        .unwrap_or_else(|| PathBuf::from(file)))
}

/// Spawn the isolated daemon and return its pid.
fn start_daemon(root: &Path, kind: &str, port: u16, remote: bool) -> std::io::Result<u32> {
    let home = root.join("home");
    let config = root.join("config");
    let psmux_data = root.join("psmux");
    std::fs::create_dir_all(&home)?;
    std::fs::create_dir_all(&config)?;
    std::fs::create_dir_all(&psmux_data)?;
    // The private home hides the user's own git identity, and a review's
    // rebase commits as whoever git says the committer is.
    let gitconfig = home.join(".gitconfig");
    if !gitconfig.exists() {
        std::fs::write(
            &gitconfig,
            "[user]\n\tname = Ralphus Exercise\n\temail = exercise@example.invalid\n",
        )?;
    }
    let mut command = Command::new(sibling_exe("ralphus-daemon")?);
    command
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
        .env("RALPHUS_RUNNER_CMD", sibling_exe("ralphus-runner")?)
        // Raw fixture commands are written once for `cmd` and `sh`; never let
        // them land in whatever shell happened to launch this command.
        .env("RALPHUS_SHELL", if cfg!(windows) { "cmd" } else { "sh" })
        .env_remove("RALPHUS_CONFIGURATION_PATH")
        // The daemon outlives this command and logs to its own `--log-path`.
        // Inheriting this process's stdio would hand it the caller's pipe, so
        // `ralphus initialize ... | tee` or a CI step capturing output would
        // never see EOF and hang until the daemon is killed.
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if remote {
        // The provider inherits this; strict mode refuses any path that is not
        // on "the machine", the way a separate host would.
        command.env("RALPHUS_LOOPBACK_STRICT", "1");
    }
    command.spawn().map(|child| child.id())
}

fn create_fixture_repo(repo: &Path, origin: &Path, label: &str) -> Result<(), String> {
    let _ = std::fs::remove_dir_all(repo);
    let _ = std::fs::remove_dir_all(origin);
    std::fs::create_dir_all(repo).map_err(|e| e.to_string())?;
    let origin_text = origin.to_string_lossy().replace('\\', "/");
    git(
        repo.parent().unwrap_or(repo),
        &[
            "init",
            "--quiet",
            "--bare",
            "--initial-branch",
            "main",
            &origin_text,
        ],
    )?;
    git(repo, &["init", "--quiet", "--initial-branch", "main"])?;
    git(repo, &["config", "user.email", "exercise@example.invalid"])?;
    git(repo, &["config", "user.name", "Ralphus Exercise"])?;
    std::fs::write(repo.join("README.md"), format!("# {label} exercise\n"))
        .map_err(|e| e.to_string())?;
    git(repo, &["add", "README.md"])?;
    git(
        repo,
        &[
            "commit",
            "--quiet",
            "--message",
            &format!("seed {label} exercise"),
        ],
    )?;
    git(repo, &["remote", "add", "origin", &origin_text])?;
    git(
        repo,
        &["push", "--quiet", "--set-upstream", "origin", "main"],
    )?;
    git(repo, &["remote", "set-head", "origin", "--auto"])
}

fn git(cwd: &Path, args: &[&str]) -> Result<(), String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .map_err(|e| e.to_string())?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_target_points_the_exercise_machine_at_its_own_remote_root() {
        let root = std::env::temp_dir().join(format!("exercise-target-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        write_remote_target(&root).unwrap();
        let text = std::fs::read_to_string(root.join("config").join("config.toml")).unwrap();
        let parsed: toml::Value = toml::from_str(&text).unwrap();
        let target = &parsed["machine"]["targets"]["exercise"];
        assert_eq!(target["machine"].as_str(), Some(REMOTE_MACHINE));
        assert!(
            target["remote_root"]
                .as_str()
                .unwrap()
                .ends_with("/remote-root")
        );
        assert_eq!(target["allow_ephemeral_remote_root"].as_bool(), Some(true));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn commit_and_push_command_commits_on_the_cells_own_branch() {
        let line = commit_and_push_command("a.txt", "first");
        assert!(line.starts_with("echo first> a.txt && git add a.txt"));
        assert!(line.ends_with("git push -q -u origin HEAD"));
    }
}
