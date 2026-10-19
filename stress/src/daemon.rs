//! A `ralphus-daemon serve` child process in a throwaway sandbox.
//!
//! Isolation matters: the daemon resolves `~/.ralphus` (token, terminal logs,
//! checkout holds) from `USERPROFILE`/`HOME`, its global config from
//! `RALPHUS_CONFIG_HOME`, and a project `.ralphus.toml` by walking up from its
//! working directory. Pointing all three into the sandbox keeps a stress run
//! from reading or deleting anything that belongs to a developer's own daemon.
//! The sandbox config also declares an all-day scheduler downtime window so no
//! submitted squad is ever claimed and run (submits use `hold` as well).

use std::fs::{self, File, OpenOptions};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

pub struct Sandbox {
    pub root: PathBuf,
    pub home: PathBuf,
    pub cfg: PathBuf,
    pub cwd: PathBuf,
    pub db: PathBuf,
    pub log: PathBuf,
    pub stdout: PathBuf,
    pub stderr: PathBuf,
    pub repos: PathBuf,
}

const SANDBOX_CONFIG: &str = r#"# Written by ralphus-stress: never let the scheduler claim a squad.
[daemon]
[[daemon.downtime]]
start = "00:00"
end = "23:59"
[[daemon.downtime]]
start = "23:59"
end = "00:00"
"#;

impl Sandbox {
    pub fn create(root: &Path) -> std::io::Result<Self> {
        if root.exists() {
            fs::remove_dir_all(root)?;
        }
        let sb = Sandbox {
            root: root.to_path_buf(),
            home: root.join("home"),
            cfg: root.join("cfg"),
            cwd: root.join("cwd"),
            db: root.join("tasks.db"),
            log: root.join("daemon.log"),
            stdout: root.join("daemon.stdout.log"),
            stderr: root.join("daemon.stderr.log"),
            repos: root.join("repos"),
        };
        for d in [&sb.home, &sb.cfg, &sb.cwd, &sb.repos] {
            fs::create_dir_all(d)?;
        }
        fs::write(sb.cfg.join("config.toml"), SANDBOX_CONFIG)?;
        Ok(sb)
    }

    pub fn log_files(&self) -> Vec<PathBuf> {
        vec![self.stderr.clone(), self.stdout.clone(), self.log.clone()]
    }

    /// The cell working directory submits use, as a forward-slash path so it
    /// can sit inside a TOML basic string unescaped.
    pub fn cwd_toml(&self) -> String {
        slash(&self.cwd)
    }
}

pub fn slash(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

pub fn free_port() -> std::io::Result<u16> {
    let l = TcpListener::bind("127.0.0.1:0")?;
    Ok(l.local_addr()?.port())
}

pub struct Daemon {
    pub exe: PathBuf,
    pub sandbox: Sandbox,
    pub port: u16,
    pub pid: u32,
    pub pass_log_path: bool,
    child: Option<Child>,
}

impl Daemon {
    pub fn base(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    fn spawn(&mut self) -> Result<(), String> {
        let out = append(&self.sandbox.stdout)?;
        let err = append(&self.sandbox.stderr)?;
        let mut cmd = Command::new(&self.exe);
        cmd.arg("serve")
            .arg("--port")
            .arg(self.port.to_string())
            .arg("--db")
            .arg(&self.sandbox.db);
        if self.pass_log_path {
            cmd.arg("--log-path").arg(&self.sandbox.log);
        }
        cmd.current_dir(&self.sandbox.cwd)
            .env("USERPROFILE", &self.sandbox.home)
            .env("HOME", &self.sandbox.home)
            .env("RALPHUS_CONFIG_HOME", &self.sandbox.cfg)
            .env("RALPHUS_BIND_ADDR", "127.0.0.1")
            .env_remove("RALPHUS_CONFIGURATION_PATH")
            .env_remove("OTEL_EXPORTER_OTLP_ENDPOINT")
            .stdin(Stdio::null())
            .stdout(out)
            .stderr(err);
        let child = cmd
            .spawn()
            .map_err(|e| format!("spawning {}: {e}", self.exe.display()))?;
        self.pid = child.id();
        self.child = Some(child);
        Ok(())
    }

    /// Start a daemon and wait until it answers HTTP. Daemons older than the
    /// `--log-path` flag exit on it, so a quick exit retries without it.
    pub fn start(exe: &Path, sandbox: Sandbox) -> Result<(Self, Duration), String> {
        let port = free_port().map_err(|e| format!("picking a port: {e}"))?;
        let mut d = Daemon {
            exe: exe.to_path_buf(),
            sandbox,
            port,
            pid: 0,
            pass_log_path: true,
            child: None,
        };
        d.spawn()?;
        match d.wait_ready(Duration::from_secs(90)) {
            Ok(t) => Ok((d, t)),
            Err(_) if !d.alive() => {
                d.pass_log_path = false;
                d.spawn()?;
                let t = d.wait_ready(Duration::from_secs(90))?;
                Ok((d, t))
            }
            Err(e) => Err(e),
        }
    }

    pub fn wait_ready(&mut self, timeout: Duration) -> Result<Duration, String> {
        let t0 = Instant::now();
        let ag = ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(2))
            .build();
        while t0.elapsed() < timeout {
            if !self.alive() {
                return Err(format!(
                    "daemon exited during startup: {}",
                    self.exit_status().unwrap_or_default()
                ));
            }
            match ag.get(&format!("{}/api/daemon", self.base())).call() {
                Ok(_) | Err(ureq::Error::Status(_, _)) => return Ok(t0.elapsed()),
                Err(_) => std::thread::sleep(Duration::from_millis(100)),
            }
        }
        Err(format!(
            "daemon not answering on port {} after {timeout:?}",
            self.port
        ))
    }

    pub fn token(&self) -> Option<String> {
        let t = fs::read_to_string(self.sandbox.home.join(".ralphus").join("daemon.token")).ok()?;
        let t = t.trim().to_string();
        if t.is_empty() { None } else { Some(t) }
    }

    /// The last `n` non-empty lines of the daemon's stderr, joined with ` | `.
    pub fn stderr_tail(&self, n: usize) -> String {
        let text = fs::read(&self.sandbox.stderr).unwrap_or_default();
        let text = String::from_utf8_lossy(&text);
        let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
        lines[lines.len().saturating_sub(n)..].join(" | ")
    }

    pub fn alive(&mut self) -> bool {
        match self.child.as_mut() {
            Some(c) => matches!(c.try_wait(), Ok(None)),
            None => false,
        }
    }

    pub fn exit_status(&mut self) -> Option<String> {
        let c = self.child.as_mut()?;
        match c.try_wait() {
            Ok(Some(s)) => Some(format!("{s}")),
            _ => None,
        }
    }

    /// Hard kill (TerminateProcess on Windows, SIGKILL elsewhere) -- no
    /// graceful shutdown, which is the point for the crash scenario.
    pub fn kill(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }

    /// Kill and start again on the same port and database.
    pub fn restart(&mut self) -> Result<Duration, String> {
        self.kill();
        let t0 = Instant::now();
        self.spawn()?;
        self.wait_ready(Duration::from_secs(90))?;
        Ok(t0.elapsed())
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        self.kill();
    }
}

fn append(p: &Path) -> Result<File, String> {
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(p)
        .map_err(|e| format!("opening {}: {e}", p.display()))
}
