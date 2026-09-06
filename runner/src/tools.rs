//! Workspace-confined file/shell tools, ported from `cli/src/ralphus/runner/tools.py`.
//! These are the three tools handed to the tool-calling agent backends
//! ([`crate::agent_backend`]) and used directly by `command`-kind sessions.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::Duration;

/// A tool call failed (escaped the workspace, I/O error, etc). The
/// tool-calling loop reports this back to the model as a tool-result error
/// string rather than aborting the session, mirroring Python's `except
/// ToolError: return f"error: {exc}"` pattern at each call site.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolError(pub String);

impl std::fmt::Display for ToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for ToolError {}

/// The result of a `run_bash` call.
#[derive(Debug, Clone)]
pub struct CommandOutput {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl CommandOutput {
    #[must_use]
    pub fn ok(&self) -> bool {
        self.exit_code == 0
    }
}

/// A directory a session/verify/tool call is confined to. Every relative path
/// a tool receives is resolved against `root` and rejected if it would escape
/// it (`../../etc/passwd`-style traversal).
#[derive(Debug, Clone)]
pub struct Workspace {
    root: PathBuf,
}

impl Workspace {
    /// `path` must already exist as a directory.
    pub fn create(path: impl AsRef<Path>) -> Result<Self, ToolError> {
        let root = path.as_ref();
        let canonical = root
            .canonicalize()
            .map_err(|e| ToolError(format!("workspace root {}: {e}", root.display())))?;
        if !canonical.is_dir() {
            return Err(ToolError(format!(
                "workspace root is not a directory: {}",
                canonical.display()
            )));
        }
        Ok(Self { root: canonical })
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Resolve `relpath` against the workspace root, rejecting any path that
    /// would escape it.
    fn resolve(&self, relpath: &str) -> Result<PathBuf, ToolError> {
        let candidate = self.root.join(relpath);
        // The target need not exist yet (e.g. `write_file` creating a new
        // file), so canonicalize the parent and rejoin the leaf rather than
        // requiring the full path to already resolve.
        let (parent, leaf) = match (candidate.parent(), candidate.file_name()) {
            (Some(p), Some(l)) => (p, l),
            _ => return Err(ToolError(format!("invalid path: {relpath}"))),
        };
        std::fs::create_dir_all(parent)
            .map_err(|e| ToolError(format!("could not create {}: {e}", parent.display())))?;
        let canonical_parent = parent
            .canonicalize()
            .map_err(|e| ToolError(format!("path escapes the workspace: {relpath} ({e})")))?;
        if !canonical_parent.starts_with(&self.root) {
            return Err(ToolError(format!("path escapes the workspace: {relpath}")));
        }
        Ok(canonical_parent.join(leaf))
    }

    pub fn read_file(&self, relpath: &str) -> Result<String, ToolError> {
        let path = self.resolve(relpath)?;
        std::fs::read_to_string(&path)
            .map_err(|e| ToolError(format!("could not read {relpath}: {e}")))
    }

    pub fn write_file(&self, relpath: &str, content: &str) -> Result<(), ToolError> {
        let path = self.resolve(relpath)?;
        std::fs::write(&path, content)
            .map_err(|e| ToolError(format!("could not write {relpath}: {e}")))
    }

    /// Runs `command` through a real shell (`cmd /C` on Windows, `sh -c`
    /// elsewhere) rooted at the workspace, capturing combined output up to
    /// `timeout_sec` (unbounded when `None`). A timeout is reported as exit
    /// code 124 with whatever output had been captured -- there is no
    /// portable way to read partial output from a hard-killed child's pipes
    /// after the fact, so this reports an empty capture on timeout rather
    /// than the Python version's "whatever was captured so far" (which relied
    /// on `subprocess.TimeoutExpired.stdout` still being populated).
    ///
    /// Unlike `read_file`/`write_file`, this does not attempt path-prefix
    /// confinement (e.g. rejecting `../`) -- a shell command's text can reach
    /// outside the workspace in too many ways (absolute paths, symlinks,
    /// `cd`, command substitution) for string-level checks to meaningfully
    /// stop it, and a regex trying to catch such patterns is bypassable
    /// while giving false confidence. Real confinement comes from RAL-225's
    /// opt-in container execution mode, which restricts what the OS lets the
    /// subprocess reach regardless of what the command text says.
    pub fn run_bash(
        &self,
        command: &str,
        timeout_sec: Option<u64>,
    ) -> Result<CommandOutput, ToolError> {
        let mut child = shell_command(command)
            .current_dir(&self.root)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| ToolError(format!("could not spawn command: {e}")))?;
        let mut tree = ProcessTree::confine(&child);

        let Some(secs) = timeout_sec else {
            let output = child
                .wait_with_output()
                .map_err(|e| ToolError(format!("command failed: {e}")))?;
            return Ok(command_output(&output));
        };

        let deadline = Duration::from_secs(secs);
        let start = std::time::Instant::now();
        loop {
            match child.try_wait() {
                Ok(Some(_)) => {
                    let output = child
                        .wait_with_output()
                        .map_err(|e| ToolError(format!("command failed: {e}")))?;
                    return Ok(command_output(&output));
                }
                Ok(None) => {
                    if start.elapsed() >= deadline {
                        tree.kill(&mut child);
                        let _ = child.wait();
                        return Ok(CommandOutput {
                            exit_code: 124,
                            stdout: String::new(),
                            stderr: format!("command timed out after {secs}s"),
                        });
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(e) => return Err(ToolError(format!("command failed: {e}"))),
            }
        }
    }
}

#[cfg(target_os = "windows")]
fn shell_command(command: &str) -> Command {
    let mut c = Command::new("cmd");
    c.arg("/C").arg(command);
    c
}

#[cfg(not(target_os = "windows"))]
fn shell_command(command: &str) -> Command {
    use std::os::unix::process::CommandExt as _;
    let mut c = Command::new("sh");
    c.arg("-c").arg(command);
    // RAL-321: a fresh process group (pgid == the child's own pid) so
    // `ProcessTree::kill`'s `killpg` reaches this command's own descendants
    // on timeout, not just the `sh` wrapping them.
    c.process_group(0);
    c
}

/// Confine a spawned child to its own process tree so a timeout can kill it
/// *and every process it spawned* -- `run_bash`'s command is a shell wrapping
/// something (`npm run build`, an `a && b` chain) that can spawn children of
/// its own. Killing only the direct child (the shell) leaves those
/// grandchildren running past the deadline and keeps their output pipes open.
/// Mirrors `daemon/src/proof.rs`'s `ProcessTree`, reimplemented here since
/// that type is private to the `daemon` crate.
#[cfg(windows)]
struct ProcessTree(Option<win32job::Job>);

#[cfg(windows)]
impl ProcessTree {
    /// Must be called right after `spawn()` -- before the child has had a
    /// chance to spawn anything of its own.
    fn confine(child: &Child) -> Self {
        use std::os::windows::io::AsRawHandle;
        let job = (|| -> Result<win32job::Job, win32job::JobError> {
            let job = win32job::Job::create()?;
            let mut info = win32job::ExtendedLimitInfo::new();
            info.limit_kill_on_job_close();
            job.set_extended_limit_info(&info)?;
            job.assign_process(child.as_raw_handle() as isize)?;
            Ok(job)
        })();
        match job {
            Ok(job) => Self(Some(job)),
            Err(e) => {
                eprintln!(
                    "[run_bash] could not confine command to a job object, \
                     timeout kill may not reach its children: {e}"
                );
                Self(None)
            }
        }
    }

    /// Kill every process in the tree. `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`
    /// means closing the job's only handle (via `Drop`) terminates every
    /// process assigned to it.
    fn kill(&mut self, child: &mut Child) {
        self.0 = None;
        // Belt-and-suspenders: if job confinement failed above, still kill
        // the direct child so at least the shell itself stops.
        let _ = child.kill();
    }
}

#[cfg(unix)]
struct ProcessTree;

#[cfg(unix)]
impl ProcessTree {
    /// On Unix, tree confinement happens on the `Command` builder before
    /// spawn (see `shell_command`'s `process_group(0)`), not on the spawned
    /// `Child` -- so this is a no-op constructor kept only to give both
    /// platforms the same call shape at the use site.
    fn confine(_child: &Child) -> Self {
        Self
    }

    /// Signal the whole process group the child was placed into at spawn,
    /// not just the child itself, so a shell's already-spawned children die
    /// with it instead of being orphaned past the timeout.
    fn kill(&mut self, child: &mut Child) {
        let _ = nix::sys::signal::killpg(
            nix::unistd::Pid::from_raw(child.id() as i32),
            nix::sys::signal::Signal::SIGKILL,
        );
        let _ = child.kill();
    }
}

fn command_output(output: &std::process::Output) -> CommandOutput {
    CommandOutput {
        exit_code: output.status.code().unwrap_or(-1),
        stdout: decode_lossy(&output.stdout),
        stderr: decode_lossy(&output.stderr),
    }
}

/// Decodes subprocess output as UTF-8, replacing invalid sequences --
/// mirrors Python's explicit `encoding="utf-8", errors="replace"` (needed
/// because plain OS-locale decoding mangles UTF-8 punctuation on Windows).
fn decode_lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Writes `content` to a fresh temp file and returns its path -- used by the
/// prompt-file-injection path shared with `cli_agent_common`.
pub fn write_temp_file(dir: &Path, name: &str, content: &str) -> Result<PathBuf, ToolError> {
    std::fs::create_dir_all(dir).map_err(|e| ToolError(format!("{}: {e}", dir.display())))?;
    let path = dir.join(name);
    let mut f =
        std::fs::File::create(&path).map_err(|e| ToolError(format!("{}: {e}", path.display())))?;
    f.write_all(content.as_bytes())
        .map_err(|e| ToolError(format!("{}: {e}", path.display())))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_write_roundtrip() {
        let dir = std::env::temp_dir().join(format!("ralphus-runner-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let ws = Workspace::create(&dir).unwrap();
        ws.write_file("a/b.txt", "hello").unwrap();
        assert_eq!(ws.read_file("a/b.txt").unwrap(), "hello");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rejects_path_escape() {
        let dir =
            std::env::temp_dir().join(format!("ralphus-runner-test-esc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let ws = Workspace::create(&dir).unwrap();
        let err = ws.read_file("../../../etc/passwd");
        assert!(err.is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn run_bash_captures_exit_code_and_output() {
        let dir =
            std::env::temp_dir().join(format!("ralphus-runner-test-bash-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let ws = Workspace::create(&dir).unwrap();
        let out = ws.run_bash("exit 3", None).unwrap();
        assert_eq!(out.exit_code, 3);
        assert!(!out.ok());
        std::fs::remove_dir_all(&dir).ok();
    }
}
