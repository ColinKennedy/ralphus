//! RAL-550: push-based live change detection for a running cell's worktree.
//!
//! The runner lives on whichever host holds the cell's files (local, or an
//! SSH-remote machine), so it -- not the daemon -- notices that the worktree
//! changed and pushes a small numstat-only summary over the `RALPHUS_EVENT:`
//! stderr channel. The daemon never polls. Detection is a plain poll of local
//! `git` with exponential backoff that resets the moment the summary changes;
//! no inotify/`notify` dependency, since OS watch APIs are unreliable over
//! NFS/sshfs-backed worktrees.
//!
//! The summary is computed without touching the index (no `git add -N`), so it
//! cannot contend with the agent's own git operations or alter what it later
//! commits: tracked changes come from `git diff` against the commit `HEAD`
//! pointed at when the watcher started (so commits the agent makes still
//! count), and untracked files are counted by reading them.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::Duration;

use serde_json::json;

use crate::cartographer::{self, EventContext};

/// Event `source` for the push. Unique on purpose so it is easy to pick out in
/// processes and logs; `daemon/src/runner.rs::WORKTREE_DIFF_SOURCE` keys the
/// daemon's staleness tracking on it, so the two must stay in sync.
pub const WORKTREE_DIFF_SOURCE: &str = "worktree-diff";
/// Event `message` for the push; must match the daemon's
/// `WORKTREE_DIFF_MESSAGE`.
pub const WORKTREE_DIFF_MESSAGE: &str = "diff changed";

/// First poll interval, and the interval a change snaps back to. Each poll
/// is answered in-process by libgit2; when it cannot be (see
/// [`crate::git_inproc`]) it is two git processes, and an agent that is
/// editing keeps the interval at this floor, so it sets the watcher's process
/// rate while a cell is busy.
const MIN_INTERVAL: Duration = Duration::from_secs(2);
/// Backoff ceiling while the worktree is quiet.
const MAX_INTERVAL: Duration = Duration::from_secs(8);
/// Untracked files larger than this are counted as zero lines rather than read.
const MAX_UNTRACKED_READ_BYTES: u64 = 1024 * 1024;

/// Numstat-style counts of what a worktree has changed since the watcher began.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DiffSummary {
    pub files_changed: u64,
    pub files_added: u64,
    pub files_removed: u64,
    pub lines_added: u64,
    pub lines_removed: u64,
}

impl DiffSummary {
    fn payload(&self, baseline: &str) -> serde_json::Value {
        json!({
            "baseline": baseline,
            "files_changed": self.files_changed,
            "files_added": self.files_added,
            "files_removed": self.files_removed,
            "lines_added": self.lines_added,
            "lines_removed": self.lines_removed,
        })
    }
}

/// Exponential poll interval that resets to its floor on any change.
#[derive(Debug, Clone, Copy)]
pub struct Backoff {
    min: Duration,
    max: Duration,
    current: Duration,
}

impl Backoff {
    #[must_use]
    pub fn new(min: Duration, max: Duration) -> Self {
        Self {
            min,
            max,
            current: min,
        }
    }

    /// The interval to wait before the next poll.
    #[must_use]
    pub fn current(&self) -> Duration {
        self.current
    }

    /// Record a poll's outcome and advance: a change snaps back to the floor,
    /// a quiet poll doubles the interval up to the ceiling.
    pub fn observe(&mut self, changed: bool) {
        self.current = if changed {
            self.min
        } else {
            (self.current * 2).min(self.max)
        };
    }
}

/// As [`git`], but keeps why the command failed (spawn error, or the exit
/// status plus the head of its stderr) for the watcher's failure event.
fn git_detailed(root: &Path, args: &[&str]) -> Result<String, String> {
    let out = ralphus_core::git_spawn::command(args)
        .args(args)
        .current_dir(root)
        // A background poll must never wait on, or hold, the repo's index lock.
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()
        .map_err(|e| format!("could not run git {}: {e}", args.join(" ")))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let first = stderr.lines().next().unwrap_or("").trim();
        Err(format!(
            "git {} exited {}: {first}",
            args.join(" "),
            out.status
        ))
    }
}

/// The commit `HEAD` points at, or `None` when `root` is not a git worktree.
#[must_use]
pub fn head_commit(root: &Path) -> Option<String> {
    head_commit_detailed(root).ok()
}

/// As [`head_commit`], but keeps why git could not answer. Asks libgit2
/// first and only spawns git when libgit2 cannot answer (see
/// [`crate::git_inproc`]).
fn head_commit_detailed(root: &Path) -> Result<String, String> {
    if let Some(head) = crate::git_inproc::Repo::open(root)
        .ok()
        .and_then(|repo| repo.head_commit().ok())
    {
        return Ok(head);
    }
    git_detailed(root, &["rev-parse", "--verify", "HEAD"]).map(|s| s.trim().to_string())
}

fn count_lines(path: &Path) -> u64 {
    let Ok(meta) = std::fs::metadata(path) else {
        return 0;
    };
    if !meta.is_file() || meta.len() > MAX_UNTRACKED_READ_BYTES {
        return 0;
    }
    let Ok(bytes) = std::fs::read(path) else {
        return 0;
    };
    if bytes.contains(&0) {
        return 0;
    }
    let newlines = bytes.iter().filter(|&&b| b == b'\n').count() as u64;
    newlines + u64::from(bytes.last().is_some_and(|&b| b != b'\n'))
}

/// Summarise everything changed in `root` since `baseline`: committed,
/// uncommitted and untracked. `None` if git could not be queried.
#[must_use]
pub fn summarize(root: &Path, baseline: &str) -> Option<DiffSummary> {
    summarize_detailed(root, baseline).ok()
}

/// As [`summarize`], but keeps the failing git command's error.
fn summarize_detailed(root: &Path, baseline: &str) -> Result<DiffSummary, String> {
    let mut repo = crate::git_inproc::Repo::open(root).ok();
    summarize_polled(&mut repo, root, baseline)
}

/// One poll: libgit2 through the watcher's long-lived `repo` handle, or a
/// `git` subprocess when there is no handle or libgit2 cannot answer. A
/// handle that fails is dropped so a repository libgit2 cannot read costs one
/// failed attempt, not one per poll.
fn summarize_polled(
    repo: &mut Option<crate::git_inproc::Repo>,
    root: &Path,
    baseline: &str,
) -> Result<DiffSummary, String> {
    if let Some(open) = repo.as_ref() {
        match open.changes_since(baseline) {
            Ok(changes) => {
                let mut summary = DiffSummary {
                    files_changed: changes.files_changed,
                    files_added: changes.files_added,
                    files_removed: changes.files_removed,
                    lines_added: changes.lines_added,
                    lines_removed: changes.lines_removed,
                };
                for rel in &changes.untracked {
                    summary.files_changed += 1;
                    summary.files_added += 1;
                    summary.lines_added += count_lines(&root.join(rel));
                }
                return Ok(summary);
            }
            Err(_) => *repo = None,
        }
    }
    summarize_subprocess(root, baseline)
}

fn summarize_subprocess(root: &Path, baseline: &str) -> Result<DiffSummary, String> {
    // `--numstat --summary` gives per-file line counts plus a
    // ` create mode ...` / ` delete mode ...` line per added/removed file, so
    // one diff covers both. No external diff driver or textconv filter can
    // affect numstat output, and ruling them out keeps the call spawn-free.
    let diff = git_detailed(
        root,
        &[
            "diff",
            "--numstat",
            "--summary",
            "--no-renames",
            "--no-ext-diff",
            "--no-textconv",
            baseline,
        ],
    )?;
    let untracked = git_detailed(root, &["ls-files", "--others", "--exclude-standard", "-z"])?;

    let mut summary = DiffSummary::default();
    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix(' ') {
            if rest.starts_with("create mode ") {
                summary.files_added += 1;
            } else if rest.starts_with("delete mode ") {
                summary.files_removed += 1;
            }
            continue;
        }
        let mut parts = line.splitn(3, '\t');
        summary.files_changed += 1;
        // Binary files report `-`: counted as a changed file, zero lines.
        summary.lines_added += parts
            .next()
            .and_then(|n| n.parse::<u64>().ok())
            .unwrap_or(0);
        summary.lines_removed += parts
            .next()
            .and_then(|n| n.parse::<u64>().ok())
            .unwrap_or(0);
    }
    for rel in untracked.split('\0').filter(|p| !p.is_empty()) {
        summary.files_changed += 1;
        summary.files_added += 1;
        summary.lines_added += count_lines(&root.join(rel));
    }
    Ok(summary)
}

/// A watcher thread scoped to one backend run. Dropping it (success, error,
/// timeout or unwind) stops the thread, joins it, and reports a final summary
/// if the worktree changed after the last poll.
pub struct WorktreeWatcher {
    stop: Option<Sender<()>>,
    handle: Option<JoinHandle<()>>,
}

impl WorktreeWatcher {
    /// Starts watching `root`, or returns `None` when it is not a git worktree.
    /// Pushes the `RALPHUS_EVENT:` marker through [`cartographer::emit`].
    #[must_use]
    pub fn start(root: &Path, squad_id: &str, cell_id: &str, task: &str) -> Option<Self> {
        let (squad_id, cell_id, task) =
            (squad_id.to_string(), cell_id.to_string(), task.to_string());
        Self::start_with(
            root,
            MIN_INTERVAL,
            MAX_INTERVAL,
            move |baseline, summary| {
                cartographer::emit(
                    WORKTREE_DIFF_SOURCE,
                    WORKTREE_DIFF_MESSAGE,
                    "info",
                    EventContext {
                        squad_id: Some(&squad_id),
                        cell_id: Some(&cell_id),
                        task: Some(&task),
                    },
                    summary.payload(baseline),
                );
            },
        )
    }

    /// As [`Self::start`] with an explicit cadence and sink, for tests.
    pub fn start_with(
        root: &Path,
        min: Duration,
        max: Duration,
        emit: impl Fn(&str, &DiffSummary) + Send + 'static,
    ) -> Option<Self> {
        let baseline = match head_commit_detailed(root) {
            Ok(head) => head,
            Err(error) => {
                cartographer::emit(
                    WORKTREE_DIFF_SOURCE,
                    "live diff watcher not started: no HEAD commit",
                    "debug",
                    EventContext::default(),
                    json!({"root": root.display().to_string(), "error": error}),
                );
                return None;
            }
        };
        let root: PathBuf = root.to_path_buf();
        let (stop, rx) = mpsc::channel::<()>();
        let handle = std::thread::spawn(move || {
            let mut last = DiffSummary::default();
            let mut backoff = Backoff::new(min, max);
            // Polls repeat every few seconds, so a persistent failure is
            // reported once when it starts and once when it clears, not per
            // poll.
            let mut failing = false;
            // One libgit2 handle for the watcher's whole life; `None` when
            // libgit2 cannot read this repository (polls then spawn git).
            let mut repo = crate::git_inproc::Repo::open(&root).ok();
            let mut poll = |last: &mut DiffSummary| -> bool {
                match summarize_polled(&mut repo, &root, &baseline) {
                    Ok(now) => {
                        if failing {
                            failing = false;
                            cartographer::emit(
                                WORKTREE_DIFF_SOURCE,
                                "live diff summary recovered",
                                "info",
                                EventContext::default(),
                                json!({"root": root.display().to_string()}),
                            );
                        }
                        if now == *last {
                            return false;
                        }
                        *last = now;
                        emit(&baseline, &now);
                        true
                    }
                    Err(error) => {
                        if !failing {
                            failing = true;
                            cartographer::emit(
                                WORKTREE_DIFF_SOURCE,
                                "live diff summary failed",
                                "warning",
                                EventContext::default(),
                                json!({"root": root.display().to_string(), "error": error}),
                            );
                        }
                        false
                    }
                }
            };
            loop {
                match rx.recv_timeout(backoff.current()) {
                    Err(RecvTimeoutError::Timeout) => {
                        let changed = poll(&mut last);
                        backoff.observe(changed);
                    }
                    // Stop requested (or the owner vanished): one last look so
                    // the final state is reported, then exit.
                    Ok(()) | Err(RecvTimeoutError::Disconnected) => {
                        poll(&mut last);
                        return;
                    }
                }
            }
        });
        Some(Self {
            stop: Some(stop),
            handle: Some(handle),
        })
    }
}

impl Drop for WorktreeWatcher {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(handle) = self.handle.take() {
            if handle.join().is_err() {
                cartographer::emit(
                    WORKTREE_DIFF_SOURCE,
                    "live diff watcher thread panicked",
                    "warning",
                    EventContext::default(),
                    serde_json::Value::Null,
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use std::sync::{Arc, Mutex};

    fn run(root: &Path, args: &[&str]) {
        let ok = Command::new("git")
            .args(args)
            .current_dir(root)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .status()
            .unwrap()
            .success();
        assert!(ok, "git {args:?} failed");
    }

    fn temp_repo(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ralphus-wtdiff-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        run(&dir, &["init", "-q"]);
        std::fs::write(dir.join("a.txt"), "one\ntwo\n").unwrap();
        run(&dir, &["add", "."]);
        run(&dir, &["commit", "-q", "-m", "init"]);
        dir
    }

    #[test]
    fn backoff_doubles_to_the_cap_and_resets_on_change() {
        let mut b = Backoff::new(Duration::from_millis(500), Duration::from_secs(2));
        assert_eq!(b.current(), Duration::from_millis(500));
        b.observe(false);
        assert_eq!(b.current(), Duration::from_secs(1));
        b.observe(false);
        assert_eq!(b.current(), Duration::from_secs(2));
        b.observe(false);
        assert_eq!(b.current(), Duration::from_secs(2), "capped");
        b.observe(true);
        assert_eq!(b.current(), Duration::from_millis(500), "reset on change");
    }

    #[test]
    fn summarize_counts_committed_uncommitted_and_untracked() {
        let dir = temp_repo("summary");
        let base = head_commit(&dir).unwrap();
        assert_eq!(summarize(&dir, &base), Some(DiffSummary::default()));

        // Uncommitted edit: +1 line.
        std::fs::write(dir.join("a.txt"), "one\ntwo\nthree\n").unwrap();
        // Untracked file: counts as all-lines-added.
        std::fs::write(dir.join("new.txt"), "x\ny\n").unwrap();
        // A committed addition after the baseline still counts.
        std::fs::write(dir.join("b.txt"), "b\n").unwrap();
        run(&dir, &["add", "b.txt"]);
        run(&dir, &["commit", "-q", "-m", "b"]);

        let s = summarize(&dir, &base).unwrap();
        assert_eq!(
            s,
            DiffSummary {
                files_changed: 3,
                files_added: 2,
                files_removed: 0,
                lines_added: 4,
                lines_removed: 0,
            }
        );
        // The index must be untouched: the untracked file is still untracked.
        let status = git(&dir, &["status", "--porcelain"]).unwrap();
        assert!(status.contains("?? new.txt"), "{status}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_non_git_directory_has_no_watcher() {
        let dir = std::env::temp_dir().join(format!("ralphus-wtdiff-nogit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(WorktreeWatcher::start_with(&dir, MIN_INTERVAL, MAX_INTERVAL, |_, _| {}).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_watcher_reports_a_change_and_stops_when_dropped() {
        let dir = temp_repo("watch");
        let seen: Arc<Mutex<Vec<DiffSummary>>> = Arc::default();
        let sink = Arc::clone(&seen);
        let watcher = WorktreeWatcher::start_with(
            &dir,
            Duration::from_millis(20),
            Duration::from_millis(80),
            move |_, s| sink.lock().unwrap().push(*s),
        )
        .expect("git worktree");
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        // Drop joins the thread and runs the final poll, so the change is
        // reported even if no timed poll landed first.
        drop(watcher);
        let seen = seen.lock().unwrap();
        let last = seen.last().expect("one event");
        assert_eq!(last.files_changed, 1);
        assert_eq!(last.lines_removed, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn in_process_and_subprocess_summaries_agree() {
        let dir = temp_repo("ab");
        let base = head_commit(&dir).unwrap();
        // Edit, delete, binary, committed-after-baseline, untracked, ignored.
        std::fs::write(dir.join("a.txt"), "one\ntwo\nthree\nfour\n").unwrap();
        std::fs::write(dir.join("gone.txt"), "g\n").unwrap();
        run(&dir, &["add", "gone.txt"]);
        run(&dir, &["commit", "-q", "-m", "gone"]);
        std::fs::remove_file(dir.join("gone.txt")).unwrap();
        std::fs::write(dir.join("bin.dat"), [0u8, 1, 2, 0]).unwrap();
        run(&dir, &["add", "bin.dat"]);
        std::fs::create_dir_all(dir.join("x/y")).unwrap();
        std::fs::write(dir.join("x/y/new.txt"), "n1\nn2\nn3\n").unwrap();
        std::fs::write(dir.join(".gitignore"), "*.tmp\n").unwrap();
        std::fs::write(dir.join("skip.tmp"), "ignored\n").unwrap();

        let mut repo = crate::git_inproc::Repo::open(&dir).ok();
        assert!(repo.is_some(), "libgit2 should open a plain checkout");
        let in_process = summarize_polled(&mut repo, &dir, &base).unwrap();
        assert!(repo.is_some(), "a successful poll keeps the handle");
        let subprocess = summarize_subprocess(&dir, &base).unwrap();
        assert_eq!(in_process, subprocess);
        assert!(in_process.files_changed >= 4, "{in_process:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_handle_that_cannot_answer_is_dropped_and_git_is_used() {
        let dir = temp_repo("drop");
        let base = head_commit(&dir).unwrap();
        let mut repo = crate::git_inproc::Repo::open(&dir).ok();
        // Not a commit libgit2 can find: the handle is discarded and the
        // subprocess path produces the (failing) answer.
        let bogus = "0123456789012345678901234567890123456789";
        assert!(summarize_polled(&mut repo, &dir, bogus).is_err());
        assert!(repo.is_none());
        // With no handle the subprocess path still works.
        assert_eq!(
            summarize_polled(&mut repo, &dir, &base).unwrap(),
            DiffSummary::default()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dropping_during_unwind_still_joins_the_thread() {
        let dir = temp_repo("panic");
        let result = std::panic::catch_unwind(|| {
            let _watcher = WorktreeWatcher::start_with(
                &dir,
                Duration::from_millis(20),
                Duration::from_millis(80),
                |_, _| {},
            )
            .expect("git worktree");
            panic!("backend exploded");
        });
        assert!(result.is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
