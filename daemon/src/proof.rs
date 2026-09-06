//! Proof-step execution.
//!
//! `command` proof steps (fmt/lint/test) run as shell commands — exit code
//! is the verdict. `agent` provers need a model; `brain` and `approval`
//! provers are deferred (left pending) until their respective backends
//! land.
//!
//! This module's direct, un-wrapped subprocess execution is used by Guardian
//! review check gates (`daemon/src/guardian_merge.rs`), which have no live
//! view of their own to preserve. A task/cell `command`-kind proof step
//! instead runs through [`crate::runner::Runner`] (RAL-151), tmux-wrapped
//! exactly like a `prompt`-kind proof step, so it can be watched live from
//! the board — see `daemon/src/scheduler.rs::run_proofs`'s `"command"`
//! branch.

use std::collections::BTreeMap;
use std::io::Read as _;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use opentelemetry::Context;
use opentelemetry::trace::{SpanKind, Status};

use crate::cancel::CancelToken;

/// How often the wait loop polls the child's exit status and the cancel
/// token — small enough that a cancelled check gate dies promptly, large
/// enough not to busy-loop.
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Confine a spawned child to its own process tree so a cancellation can kill
/// it *and every process it spawned* — a check-gate command is a shell
/// wrapping something (`cargo test`, `npm run build`, an `a && b` chain) that
/// almost always spawns children of its own. Killing only the direct child
/// (the shell) leaves those grandchildren running against the worktree, and
/// keeps the piped stdout/stderr open until they eventually exit on their
/// own — the exact hang this exists to prevent.
///
/// Must be called after `spawn()` (Windows needs the child's handle) but
/// before the child has had a chance to spawn anything of its own, which in
/// practice just means "as soon as possible after spawn".
#[cfg(windows)]
struct ProcessTree(Option<win32job::Job>);

#[cfg(windows)]
impl ProcessTree {
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
                // ralphus[ignore-rlog-pair]: this low-level helper has no Store; its Store-owning caller records the structured workflow outcome
                crate::rlog!(
                    WARNING,
                    "ralphus [proof] could not confine check-gate process to a job object, \
                     cancellation may not reach its children: {e}"
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
    /// spawn (see `prepare_command`), not on the spawned `Child` — so this is
    /// a no-op constructor kept only to give both platforms the same call
    /// shape at the use site.
    fn confine(_child: &Child) -> Self {
        Self
    }

    /// Signal the whole process group `prepare_command` placed the child
    /// into, not just the child itself, so a shell's already-spawned
    /// children die with it instead of being orphaned.
    fn kill(&mut self, child: &mut Child) {
        // `child.id()` is the pid of a process we spawned with its own
        // process group (`process_group(0)`), so its pgid equals its pid;
        // `killpg` targets that whole group per `kill(2)`. `unsafe_code =
        // "forbid"` bans a raw `libc::kill` call in this crate, so `nix`'s
        // safe wrapper is used instead (the `unsafe` syscall stays confined
        // to that dependency).
        let _ = nix::sys::signal::killpg(
            nix::unistd::Pid::from_raw(child.id() as i32),
            nix::sys::signal::Signal::SIGKILL,
        );
        let _ = child.kill();
    }
}

#[cfg(unix)]
fn prepare_command(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;
    // A fresh process group (pgid == the child's own pid) isolates it from
    // this daemon process's group, so `ProcessTree::kill`'s `kill(-pgid, ...)`
    // reaches only the check-gate command's own descendants.
    cmd.process_group(0);
}

#[cfg(windows)]
fn prepare_command(_cmd: &mut Command) {}

/// Run a `command` proof step in `cwd`. Returns true when the command exits 0.
///
/// Used by callers (e.g. Guardian review checks) that have no trace to
/// continue and no squad-scoped env overrides to apply; the span it creates
/// starts a fresh trace of its own.
#[must_use]
pub fn run_command_proof(cwd: &str, command: &str) -> bool {
    run_command_proof_capture(
        cwd,
        command,
        &Context::new(),
        &BTreeMap::new(),
        &CancelToken::never(),
    )
    .0
}

/// Like [`run_command_proof`] but also captures the combined stdout+stderr
/// (truncated) so it can be shown in the log viewer (CCTL-99). Wraps the
/// subprocess in an OpenTelemetry span (RAL-96), as a child of `parent`, and
/// applies `env` — the owning squad's persistent environment-variable overrides
/// (RAL-150), if any — on top of the daemon's own inherited environment.
///
/// Polls `cancel` while the command runs (RAL-239 review-cancel gap) and
/// kills it the moment it trips, instead of blocking to completion — a
/// long-running check gate (`cargo test`, `npm run build`) must not keep
/// running against a review worktree after the review was cancelled.
#[must_use]
pub fn run_command_proof_capture(
    cwd: &str,
    command: &str,
    parent: &Context,
    env: &BTreeMap<String, String>,
    cancel: &CancelToken,
) -> (bool, String) {
    let span = crate::otel::start_span("proof.command", parent, SpanKind::Internal);
    span.set_attribute("proof.cwd", cwd.to_string());
    span.set_attribute("proof.command", command.to_string());
    // ralphus[ignore-rlog-pair]: this low-level helper has no Store; its Store-owning caller records the structured workflow outcome
    crate::rlog!(
        DEBUG,
        "ralphus [proof] command starting cwd={cwd:?} command={command:?}"
    );
    let (shell, flag) = if cfg!(windows) {
        ("cmd", "/C")
    } else {
        ("sh", "-c")
    };
    let mut builder = Command::new(shell);
    builder
        .arg(flag)
        .arg(command)
        .current_dir(cwd)
        .envs(env)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    prepare_command(&mut builder);
    let result = match builder.spawn() {
        Ok(mut child) => {
            let mut tree = ProcessTree::confine(&child);
            let mut stdout = child.stdout.take();
            let mut stderr = child.stderr.take();
            let stdout_reader = std::thread::spawn(move || {
                let mut buf = Vec::new();
                if let Some(s) = stdout.as_mut() {
                    let _ = s.read_to_end(&mut buf);
                }
                buf
            });
            let stderr_reader = std::thread::spawn(move || {
                let mut buf = Vec::new();
                if let Some(s) = stderr.as_mut() {
                    let _ = s.read_to_end(&mut buf);
                }
                buf
            });
            let cancelled = loop {
                match child.try_wait() {
                    Ok(Some(_)) => break false,
                    Ok(None) => {
                        if cancel.is_cancelled() {
                            tree.kill(&mut child);
                            break true;
                        }
                        std::thread::sleep(POLL_INTERVAL);
                    }
                    Err(e) => {
                        // ralphus[ignore-rlog-pair]: this low-level helper has no Store; its Store-owning caller records the structured workflow outcome
                        crate::rlog!(WARNING, "ralphus [proof] command wait failed: {e}");
                        break false;
                    }
                }
            };
            let status = child.wait();
            let out = stdout_reader.join().unwrap_or_default();
            let err = stderr_reader.join().unwrap_or_default();
            let mut buf = String::new();
            buf.push_str(&String::from_utf8_lossy(&out));
            let err = String::from_utf8_lossy(&err);
            if !err.trim().is_empty() {
                buf.push_str(&err);
            }
            if cancelled {
                buf.push_str("\n(cancelled)");
                (false, truncate_output(&buf))
            } else {
                let ok = status.map(|s| s.success()).unwrap_or(false);
                (ok, truncate_output(&buf))
            }
        }
        Err(e) => (false, format!("could not run command: {e}")),
    };
    span.set_status(if result.0 {
        Status::Ok
    } else {
        Status::error("proof command failed")
    });
    // ralphus[ignore-rlog-pair]: this low-level helper has no Store; its Store-owning caller records the structured workflow outcome
    crate::rlog!(
        DEBUG,
        "ralphus [proof] command completed passed={} cwd={cwd:?}",
        result.0
    );
    result
}

/// Cap captured output so a runaway verifier can't bloat the DB / UI.
fn truncate_output(s: &str) -> String {
    const MAX: usize = 16 * 1024;
    if s.len() <= MAX {
        return s.to_string();
    }
    let mut cut = MAX;
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}\n…(truncated)", &s[..cut])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passing_command_is_true() {
        assert!(run_command_proof(".", "exit 0"));
    }

    #[test]
    fn failing_command_is_false() {
        assert!(!run_command_proof(".", "exit 1"));
    }

    #[test]
    fn bad_cwd_is_false() {
        assert!(!run_command_proof("/no/such/dir/ralphus-xyz", "exit 0"));
    }

    #[test]
    fn capture_returns_output() {
        let (ok, out) = run_command_proof_capture(
            ".",
            "echo hello-proof",
            &Context::new(),
            &BTreeMap::new(),
            &CancelToken::never(),
        );
        assert!(ok);
        assert!(out.contains("hello-proof"), "captured: {out:?}");
    }

    #[test]
    fn capture_applies_env_overrides() {
        let mut env = BTreeMap::new();
        env.insert(
            "RALPHUS_TEST_VAR".to_string(),
            "hello-env-override".to_string(),
        );
        let cmd = if cfg!(windows) {
            "echo %RALPHUS_TEST_VAR%"
        } else {
            "echo $RALPHUS_TEST_VAR"
        };
        let (ok, out) =
            run_command_proof_capture(".", cmd, &Context::new(), &env, &CancelToken::never());
        assert!(ok);
        assert!(out.contains("hello-env-override"), "captured: {out:?}");
    }

    #[test]
    fn capture_kills_command_and_reports_failure_when_cancelled() {
        let cancel = CancelToken::new();
        cancel.cancel();
        let cmd = if cfg!(windows) {
            "ping -n 30 127.0.0.1 >NUL"
        } else {
            "sleep 30"
        };
        let (ok, out) =
            run_command_proof_capture(".", cmd, &Context::new(), &BTreeMap::new(), &cancel);
        assert!(!ok);
        assert!(out.contains("cancelled"), "captured: {out:?}");
    }

    #[test]
    fn truncate_caps_huge_output() {
        let big = "x".repeat(40_000);
        let out = truncate_output(&big);
        assert!(out.len() <= 16 * 1024 + 32, "not truncated: {}", out.len());
        assert!(out.ends_with("…(truncated)"));
        // Small output is returned verbatim.
        assert_eq!(truncate_output("short"), "short");
    }
}
