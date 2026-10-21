//! RAII scratch directories for tests (RAL-690).
//!
//! Every test directory lives under one per-process run directory,
//! `<location>/<pid>-<start-secs>/`, where `<location>` is `RALPHUS_TEST_TMP`
//! if set, else `%LOCALAPPDATA%\ralphus\test-tmp` on Windows (outside the
//! `%TEMP%` tree the profile service walks at logon), else
//! `<system temp>/ralphus-tests`.
//!
//! Directories are removed when their guard drops, panic included, with a
//! retry for Windows sharing violations and read-only git objects. The first
//! use in a process sweeps the run directories of dead processes (throttled by
//! a marker file) and old legacy flat `ral<N>-*`/`ralphus-*` directories in the
//! system temp folder. `RALPHUS_KEEP_TEST_TMP=1` keeps everything.

use std::cell::RefCell;
use std::ffi::OsStr;
use std::io;
use std::ops::Deref;
use std::path::{Component, Path, PathBuf};
use std::process::{Child, Command};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const LOCATION_ENV: &str = "RALPHUS_TEST_TMP";
const KEEP_ENV: &str = "RALPHUS_KEEP_TEST_TMP";
const MARKER: &str = ".last-sweep";
const SWEEP_INTERVAL: Duration = Duration::from_secs(10 * 60);
const STALE_AGE: Duration = Duration::from_secs(24 * 60 * 60);
const REMOVE_ATTEMPTS: u32 = 10;

/// Where run directories are created.
pub fn location() -> PathBuf {
    if let Some(p) = std::env::var_os(LOCATION_ENV).filter(|p| !p.is_empty()) {
        return PathBuf::from(p);
    }
    #[cfg(windows)]
    if let Some(p) = std::env::var_os("LOCALAPPDATA").filter(|p| !p.is_empty()) {
        return PathBuf::from(p).join("ralphus").join("test-tmp");
    }
    std::env::temp_dir().join("ralphus-tests")
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn keep() -> bool {
    std::env::var_os(KEEP_ENV).is_some_and(|v| !v.is_empty() && v != "0")
}

/// This process's run directory, created on demand. The first call also runs
/// the throttled stale sweep.
pub fn scratch_root() -> PathBuf {
    static RUN: OnceLock<PathBuf> = OnceLock::new();
    let run = RUN.get_or_init(|| {
        let loc = location();
        let _ = std::fs::create_dir_all(&loc);
        let run = loc.join(format!("{}-{}", std::process::id(), now_secs()));
        sweep_if_due(&loc, &run);
        run
    });
    let _ = std::fs::create_dir_all(run);
    run.clone()
}

/// A directory (or file) deleted when dropped, after stopping any registered
/// child processes. Children must go first: on Windows a process whose cwd is
/// inside the tree blocks the delete.
#[must_use = "the directory is deleted as soon as the guard is dropped"]
pub struct TestDir {
    path: PathBuf,
    children: Vec<Child>,
}

impl TestDir {
    /// A fresh, empty directory named `<tag>-<seq>` in the run directory.
    pub fn new(tag: &str) -> TestDir {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let leaf = format!("{tag}-{}", SEQ.fetch_add(1, Ordering::Relaxed));
        let path = scratch_root().join(leaf);
        let _ = remove_path(&path);
        std::fs::create_dir_all(&path).expect("create test directory");
        TestDir {
            path,
            children: Vec::new(),
        }
    }

    /// Another independent directory beside this one, for fixtures with
    /// several roots (a remote, a clone, a fork). Each guard cleans its own.
    pub fn sibling(&self, tag: &str) -> TestDir {
        TestDir::new(tag)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Hand a child process to the guard; it is killed and reaped before the
    /// directory is deleted.
    pub fn register_child(&mut self, child: Child) {
        self.children.push(child);
    }

    /// Point a child's `TEMP`/`TMP`/`TMPDIR` at this directory so it does not
    /// write into the system temp folder.
    pub fn apply_env(&self, cmd: &mut Command) {
        cmd.env("TEMP", &self.path)
            .env("TMP", &self.path)
            .env("TMPDIR", &self.path);
    }
}

impl Deref for TestDir {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.path
    }
}

impl AsRef<Path> for TestDir {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        for child in &mut self.children {
            let _ = child.kill();
            let _ = child.wait();
        }
        if keep() {
            return;
        }
        if let Err(e) = remove_path(&self.path) {
            eprintln!(
                "ralphus-test-support: could not remove {}: {e}",
                self.path.display()
            );
        }
        // Tidy the run directory once nothing is left in it.
        if let Some(run) = self.path.parent() {
            let _ = std::fs::remove_dir(run);
        }
    }
}

thread_local! {
    static OWNED: RefCell<Vec<TestDir>> = const { RefCell::new(Vec::new()) };
}

/// Drop-in for `std::env::temp_dir().join(leaf)` in tests: the path is inside
/// this process's run directory, and its top-level entry is deleted when the
/// calling thread (the test) ends, including by panic. The path is not
/// created. An absolute `leaf` is returned unchanged and not owned.
pub fn scratch(leaf: impl AsRef<Path>) -> PathBuf {
    let leaf = leaf.as_ref();
    let root = scratch_root();
    let path = root.join(leaf);
    if leaf.is_relative()
        && let Some(first) = leaf.components().find_map(|c| match c {
            Component::Normal(n) => Some(n.to_owned()),
            _ => None,
        })
    {
        let owned = TestDir {
            path: root.join(first),
            children: Vec::new(),
        };
        // `try_with`: registering from a thread-local destructor must not panic.
        let _ = OWNED.try_with(|o| o.borrow_mut().push(owned));
    }
    path
}

/// Remove a file or tree, retrying while Windows reports a sharing violation
/// or access denied. A path that is already gone is success.
pub fn remove_path(path: &Path) -> io::Result<()> {
    let mut delay = Duration::from_millis(50);
    for attempt in 1..=REMOVE_ATTEMPTS {
        clear_readonly(path);
        let result = match std::fs::symlink_metadata(path) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => Err(e),
            Ok(md) if md.is_dir() => std::fs::remove_dir_all(path),
            Ok(_) => std::fs::remove_file(path),
        };
        match result {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) if attempt == REMOVE_ATTEMPTS => return Err(e),
            Err(_) => {
                std::thread::sleep(delay);
                delay = (delay * 2).min(Duration::from_secs(1));
            }
        }
    }
    Ok(())
}

/// Git writes pack files read-only; Windows refuses to delete those.
#[cfg(windows)]
fn clear_readonly(path: &Path) {
    let Ok(md) = std::fs::symlink_metadata(path) else {
        return;
    };
    if md.permissions().readonly() {
        let mut perms = md.permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        perms.set_readonly(false);
        let _ = std::fs::set_permissions(path, perms);
    }
    if md.is_dir()
        && let Ok(entries) = std::fs::read_dir(path)
    {
        for entry in entries.flatten() {
            clear_readonly(&entry.path());
        }
    }
}

#[cfg(not(windows))]
fn clear_readonly(_path: &Path) {}

fn modified_age(path: &Path) -> Option<Duration> {
    let modified = std::fs::symlink_metadata(path).ok()?.modified().ok()?;
    SystemTime::now().duration_since(modified).ok()
}

/// `<pid>-<start-secs>` -> (pid, start-secs).
fn parse_run_name(name: &OsStr) -> Option<(u32, u64)> {
    let (pid, ts) = name.to_str()?.split_once('-')?;
    Some((pid.parse().ok()?, ts.parse().ok()?))
}

/// Remove run directories under `parent` whose owning process is gone (pid not
/// alive, or alive but started after the directory was made, i.e. a reused
/// pid) or that are older than 24 h. `keep_run` is never touched. Returns how
/// many were removed.
pub fn sweep_stale_runs(parent: &Path, keep_run: Option<&Path>) -> usize {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

    let Ok(entries) = std::fs::read_dir(parent) else {
        return 0;
    };
    let mut sys = System::new();
    sys.refresh_processes_specifics(ProcessesToUpdate::All, true, ProcessRefreshKind::nothing());
    let mut removed = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        if keep_run == Some(path.as_path()) || !entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let too_old = modified_age(&path).is_some_and(|a| a > STALE_AGE);
        let owner_gone = match parse_run_name(&entry.file_name()) {
            Some((pid, ts)) => match sys.process(Pid::from_u32(pid)) {
                None => true,
                Some(p) => p.start_time() > ts.saturating_add(2),
            },
            None => false,
        };
        if (owner_gone || too_old) && remove_path(&path).is_ok() {
            removed += 1;
        }
    }
    removed
}

/// A legacy flat directory name from before the run-directory layout.
fn is_legacy_name(name: &str) -> bool {
    if matches!(name, "ralphus-tests" | "ralphus-runner-io") {
        return false;
    }
    if name.starts_with("ralphus-")
        || name.starts_with("ral-agent-profiles-")
        || name.starts_with("wt-rec-")
    {
        return true;
    }
    name.strip_prefix("ral")
        .and_then(|rest| rest.split_once('-'))
        .is_some_and(|(digits, _)| !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
}

/// Remove legacy flat test directories in `dir` older than 24 h, stopping
/// after `budget` so a large backlog is worked down over several runs instead
/// of stalling one test.
pub fn sweep_legacy(dir: &Path, budget: Duration) -> usize {
    let deadline = std::time::Instant::now() + budget;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        if std::time::Instant::now() > deadline {
            break;
        }
        let path = entry.path();
        let named = entry.file_name();
        if !entry.file_type().is_ok_and(|t| t.is_dir())
            || !named.to_str().is_some_and(is_legacy_name)
            || modified_age(&path).is_none_or(|a| a <= STALE_AGE)
        {
            continue;
        }
        if remove_path(&path).is_ok() {
            removed += 1;
        }
    }
    removed
}

fn sweep_if_due(loc: &Path, run: &Path) {
    if keep() {
        return;
    }
    let marker = loc.join(MARKER);
    if modified_age(&marker).is_some_and(|a| a < SWEEP_INTERVAL) {
        return;
    }
    let _ = std::fs::write(&marker, now_secs().to_string());
    sweep_stale_runs(loc, Some(run));
    sweep_legacy(&std::env::temp_dir(), Duration::from_secs(10));
}
