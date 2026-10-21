//! Per-cell worktree initialization (RAL-615): the `initialization_commands`
//! run once in a freshly created cell worktree before any cell uses it.
//!
//! This module owns the pure parts -- the spec a caller registers, the
//! command-list hash that decides whether a worktree needs re-initializing,
//! and the runner. Finding the spec for a worktree and persisting the outcome
//! is the caller's job (`scheduler.rs`, `worktrees.rs`), done through the
//! `recorder` the spec carries so the runner never touches the store.
//!
//! Trust level: the commands run through the platform shell with the daemon's
//! privileges, exactly like `[review] auto_build`; they come from the task
//! file or the git-tracked `.ralphus.toml`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

/// What happened during one initialization run, reported to the recorder.
#[derive(Debug, Clone)]
pub(crate) enum InitEvent {
    /// The commands are about to run.
    Started,
    /// The run finished (successfully or not).
    Finished(InitOutcome),
}

/// The result of one initialization run.
#[derive(Debug, Clone)]
pub(crate) struct InitOutcome {
    /// Whether every command exited zero within the timeout.
    pub(crate) ok: bool,
    /// `0`, `timeout`, or `non-zero`.
    pub(crate) exit_status: String,
    /// Wall time of the whole list.
    pub(crate) duration_ms: u64,
    /// Trimmed combined output of the commands that ran.
    pub(crate) output: String,
    /// The command that failed, when `ok` is false.
    pub(crate) failed_command: Option<String>,
    /// Hash of the command list this run executed ([`hash_commands`]).
    pub(crate) hash: String,
}

/// Receives [`InitEvent`]s; supplied by whoever knows how to persist them.
pub(crate) type InitRecorder = Arc<dyn Fn(&InitEvent) + Send + Sync>;

/// Everything needed to initialize one worktree.
#[derive(Clone)]
pub(crate) struct InitSpec {
    /// Commands to run in order; the first failure stops the list.
    pub(crate) commands: Vec<String>,
    /// Budget for the whole list.
    pub(crate) timeout: Duration,
    /// Environment for every command (cell overrides plus ralphus variables).
    pub(crate) env: BTreeMap<String, String>,
    /// Also initialize a worktree that already exists (a Retry after a failed
    /// or edited init). A newly created worktree is always initialized.
    pub(crate) rerun_existing: bool,
    /// Persistence/logging hook.
    pub(crate) recorder: InitRecorder,
}

/// Hash of a command list: the marker that says which commands a worktree was
/// last initialized with. The timeout is deliberately not part of it.
#[must_use]
pub(crate) fn hash_commands(commands: &[String]) -> String {
    let mut hasher = Sha256::new();
    for command in commands {
        hasher.update(command.as_bytes());
        hasher.update([0u8]);
    }
    hasher
        .finalize()
        .iter()
        .take(8)
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Specs registered by callers for the worktree they are about to create,
/// keyed by `(project root, lowercased branch)` -- the same key single-flight
/// creation uses -- so whichever thread ends up creating it runs the init.
static REGISTERED: LazyLock<
    parking_lot::Mutex<std::collections::HashMap<(PathBuf, String), InitSpec>>,
> = LazyLock::new(|| parking_lot::Mutex::new(std::collections::HashMap::new()));

fn key(root: &Path, branch: &str) -> (PathBuf, String) {
    (root.to_path_buf(), branch.to_ascii_lowercase())
}

/// Register `spec` for the next creation/reuse of `branch` under `root`.
pub(crate) fn register(root: &Path, branch: &str, spec: InitSpec) {
    REGISTERED.lock().insert(key(root, branch), spec);
}

/// Take the spec registered for `branch` under `root`, if any.
pub(crate) fn take(root: &Path, branch: &str) -> Option<InitSpec> {
    REGISTERED.lock().remove(&key(root, branch))
}

/// Drop a registration that was not consumed.
pub(crate) fn unregister(root: &Path, branch: &str) {
    REGISTERED.lock().remove(&key(root, branch));
}

/// Run `spec`'s commands in `wt` and report through the recorder. `Err` is the
/// human-readable failure (command, exit status, trimmed output).
pub(crate) fn run(wt: &Path, spec: &InitSpec) -> Result<InitOutcome, String> {
    (spec.recorder)(&InitEvent::Started);
    let started = Instant::now();
    let cwd = wt.to_string_lossy().into_owned();
    let cancel = crate::cancel::CancelToken::never();
    let mut output = String::new();
    let mut failure: Option<(String, String)> = None;
    for command in &spec.commands {
        let elapsed = started.elapsed();
        if elapsed >= spec.timeout {
            failure = Some((command.clone(), "timeout".to_string()));
            break;
        }
        let (ok, out) = crate::proof::run_command_proof_capture_with_timeout(
            &cwd,
            command,
            &opentelemetry::Context::new(),
            &spec.env,
            &cancel,
            Some(spec.timeout - elapsed),
        );
        if !out.trim().is_empty() {
            output.push_str(out.trim_end());
            output.push('\n');
        }
        if !ok {
            let status = if out.trim_end().ends_with("(timed out)") {
                "timeout"
            } else {
                "non-zero"
            };
            failure = Some((command.clone(), status.to_string()));
            break;
        }
    }
    let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let outcome = InitOutcome {
        ok: failure.is_none(),
        exit_status: failure
            .as_ref()
            .map_or_else(|| "0".to_string(), |(_, s)| s.clone()),
        duration_ms,
        output: output.trim().to_string(),
        failed_command: failure.as_ref().map(|(c, _)| c.clone()),
        hash: hash_commands(&spec.commands),
    };
    (spec.recorder)(&InitEvent::Finished(outcome.clone()));
    match failure {
        None => Ok(outcome),
        Some((command, status)) => Err(failure_message(&command, &status, &outcome.output)),
    }
}

/// Prefix every init failure message carries, so callers can tell it apart
/// from a git failure.
pub(crate) const FAILURE_PREFIX: &str = "worktree initialization failed";

fn failure_message(command: &str, status: &str, output: &str) -> String {
    let status_display = match status {
        "timeout" => "timed out".to_string(),
        other => format!("exit status {other}"),
    };
    if output.is_empty() {
        format!("{FAILURE_PREFIX}: `{command}` ({status_display})")
    } else {
        format!("{FAILURE_PREFIX}: `{command}` ({status_display}):\n{output}")
    }
}

/// Remediation text for a failed initialization.
pub(crate) const REMEDIATION: &str = "Fix the worktree initialization command with the cell edit form (or `ralphus cell edit --initialization-commands`), then Retry the squad: the worktree is left on disk and the commands run again in place.";

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn tmp() -> PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::SeqCst);
        let dir = ralphus_test_support::scratch(format!("ral615-init-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn spec(commands: &[&str], timeout: Duration) -> (InitSpec, Arc<Mutex<Vec<InitEvent>>>) {
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = events.clone();
        (
            InitSpec {
                commands: commands.iter().map(|c| (*c).to_string()).collect(),
                timeout,
                env: BTreeMap::new(),
                rerun_existing: false,
                recorder: Arc::new(move |e| sink.lock().unwrap().push(e.clone())),
            },
            events,
        )
    }

    #[test]
    fn hash_changes_with_the_commands_not_the_timeout() {
        let a = vec!["npm ci".to_string()];
        let b = vec!["npm ci".to_string(), "npm run build".to_string()];
        assert_eq!(hash_commands(&a), hash_commands(&a));
        assert_ne!(hash_commands(&a), hash_commands(&b));
    }

    #[test]
    fn runs_every_command_in_order_in_the_worktree() {
        let dir = tmp();
        let (spec, events) = spec(
            &["echo one > a.txt", "echo two > b.txt"],
            Duration::from_secs(60),
        );
        let outcome = run(dir.as_path(), &spec).unwrap();
        assert!(outcome.ok);
        assert_eq!(outcome.exit_status, "0");
        assert!(dir.as_path().join("a.txt").exists());
        assert!(dir.as_path().join("b.txt").exists());
        let events = events.lock().unwrap();
        assert!(matches!(events[0], InitEvent::Started));
        assert!(matches!(events[1], InitEvent::Finished(_)));
    }

    #[test]
    fn first_failure_stops_the_list_and_carries_command_and_output() {
        let dir = tmp();
        let (spec, events) = spec(
            &["echo before", "exit 3", "echo never > c.txt"],
            Duration::from_secs(60),
        );
        let err = run(dir.as_path(), &spec).unwrap_err();
        assert!(err.starts_with(FAILURE_PREFIX), "{err}");
        assert!(err.contains("exit 3"), "{err}");
        assert!(err.contains("before"), "{err}");
        assert!(!dir.as_path().join("c.txt").exists());
        let events = events.lock().unwrap();
        let InitEvent::Finished(outcome) = &events[1] else {
            panic!("expected a Finished event");
        };
        assert!(!outcome.ok);
        assert_eq!(outcome.failed_command.as_deref(), Some("exit 3"));
    }

    #[test]
    fn a_command_over_the_timeout_fails_with_timeout_status() {
        let dir = tmp();
        let slow = if cfg!(windows) {
            "ping -n 30 127.0.0.1 > nul"
        } else {
            "sleep 30"
        };
        let (spec, events) = spec(&[slow], Duration::from_millis(300));
        let err = run(dir.as_path(), &spec).unwrap_err();
        assert!(err.contains("timed out"), "{err}");
        let events = events.lock().unwrap();
        let InitEvent::Finished(outcome) = &events[1] else {
            panic!("expected a Finished event");
        };
        assert_eq!(outcome.exit_status, "timeout");
    }

    #[test]
    fn registration_is_taken_once_and_keyed_case_insensitively() {
        let root = PathBuf::from("/repo-for-registration-test");
        let (spec, _) = spec(&["true"], Duration::from_secs(1));
        register(&root, "Feat-X", spec);
        assert!(take(&root, "feat-x").is_some());
        assert!(take(&root, "feat-x").is_none());
    }
}
