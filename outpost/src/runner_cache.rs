//! The **runner cache**: the `ralphus-runner` binaries the fleet sent this
//! outpost, one directory per sha256 (`<dir>/<sha256>/ralphus-runner[.exe]`)
//! plus a `current` file naming the one new jobs use.
//!
//! A runner is installed only after its bytes hash to the sha256 the fleet
//! named (and the sealed channel they came over authenticated them), into a
//! fresh directory renamed into place -- so a half-written runner is never
//! run. Swapping is changing `current`: a running job keeps the path it
//! started with (its `runner.json`), and an old runner's directory is
//! removed only when no unfinished job under `<remote_root>/jobs` names it.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use ralphus_fleet::runner_artifacts::sha256_hex;

/// Downloaded runners for one outpost.
#[derive(Debug, Clone)]
pub struct RunnerCache {
    dir: PathBuf,
    remote_root: PathBuf,
}

fn exe_name() -> &'static str {
    if cfg!(windows) {
        "ralphus-runner.exe"
    } else {
        "ralphus-runner"
    }
}

fn is_sha(text: &str) -> bool {
    text.len() == 64 && text.chars().all(|c| c.is_ascii_hexdigit())
}

impl RunnerCache {
    #[must_use]
    pub fn new(dir: PathBuf, remote_root: &str) -> Self {
        Self {
            dir,
            remote_root: PathBuf::from(remote_root),
        }
    }

    fn path_of(&self, sha256: &str) -> PathBuf {
        self.dir.join(sha256).join(exe_name())
    }

    /// The installed runner with `sha256`, if there is one.
    #[must_use]
    pub fn installed(&self, sha256: &str) -> Option<String> {
        let path = self.path_of(sha256);
        (is_sha(sha256) && path.is_file()).then(|| path.to_string_lossy().into_owned())
    }

    /// The runner `current` names, if it is installed and still hashes to
    /// its name.
    #[must_use]
    pub fn current(&self) -> Option<String> {
        let sha = std::fs::read_to_string(self.dir.join("current")).ok()?;
        let sha = sha.trim();
        let path = self.installed(sha)?;
        let bytes = std::fs::read(&path).ok()?;
        (sha256_hex(&bytes) == sha).then_some(path)
    }

    /// Install `bytes` as the runner `sha256`; its path.
    ///
    /// # Errors
    /// The bytes do not hash to `sha256`, or the files could not be written.
    pub fn install(&self, sha256: &str, bytes: &[u8]) -> Result<String, String> {
        if !is_sha(sha256) || sha256_hex(bytes) != sha256 {
            return Err(format!("the runner does not hash to {sha256}"));
        }
        if let Some(path) = self.installed(sha256) {
            return Ok(path);
        }
        let partial = self
            .dir
            .join(format!("{sha256}.partial-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&partial);
        std::fs::create_dir_all(&partial)
            .map_err(|e| format!("could not create {}: {e}", partial.display()))?;
        let file = partial.join(exe_name());
        std::fs::write(&file, bytes)
            .map_err(|e| format!("could not write {}: {e}", file.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755))
                .map_err(|e| format!("could not make {} executable: {e}", file.display()))?;
        }
        let target = self.dir.join(sha256);
        if let Err(e) = std::fs::rename(&partial, &target) {
            let _ = std::fs::remove_dir_all(&partial);
            if self.installed(sha256).is_none() {
                return Err(format!("could not install {}: {e}", target.display()));
            }
        }
        self.installed(sha256)
            .ok_or_else(|| format!("{} vanished", target.display()))
    }

    /// Make `sha256` the runner new jobs use.
    ///
    /// # Errors
    /// It is not installed, or `current` could not be written.
    pub fn set_current(&self, sha256: &str) -> Result<(), String> {
        if self.installed(sha256).is_none() {
            return Err(format!("runner {sha256} is not installed"));
        }
        ralphus_fleet::secret_store::write_private(&self.dir.join("current"), sha256.as_bytes())
    }

    /// Remove runners neither `current` nor any unfinished job uses, and
    /// leftovers of interrupted installs. A runner that cannot be removed
    /// (still running, on Windows) is left for next time.
    pub fn collect_garbage(&self) {
        let current = std::fs::read_to_string(self.dir.join("current"))
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        let in_use = self.in_use();
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let path = entry.path();
            if !path.is_dir() || name == current || in_use.contains(&name) {
                continue;
            }
            if is_sha(&name) || name.contains(".partial-") {
                let _ = std::fs::remove_dir_all(&path);
            }
        }
    }

    /// The cached runners (by sha256) that unfinished jobs were started
    /// with.
    fn in_use(&self) -> BTreeSet<String> {
        let mut used = BTreeSet::new();
        let mut runner_files = Vec::new();
        collect_runner_files(&self.remote_root.join("jobs"), 4, &mut runner_files);
        for file in runner_files {
            let dir = file.parent().unwrap_or(Path::new("."));
            let state = std::fs::read_to_string(dir.join("state")).unwrap_or_default();
            if !matches!(state.trim(), "starting" | "running") {
                continue;
            }
            let argv: Vec<String> = std::fs::read(&file)
                .ok()
                .and_then(|b| serde_json::from_slice(&b).ok())
                .unwrap_or_default();
            if let Some(program) = argv.first() {
                let program = PathBuf::from(program);
                if let Ok(relative) = program.strip_prefix(&self.dir) {
                    if let Some(first) = relative.components().next() {
                        used.insert(first.as_os_str().to_string_lossy().into_owned());
                    }
                }
            }
        }
        used
    }
}

fn collect_runner_files(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    let candidate = dir.join("runner.json");
    if candidate.is_file() {
        out.push(candidate);
        return;
    }
    if depth == 0 {
        return;
    }
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            if entry.path().is_dir() {
                collect_runner_files(&entry.path(), depth - 1, out);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_runner_installs_only_with_its_hash_and_swaps_without_touching_running_jobs() {
        let dir = ralphus_test_support::TestDir::new("runner-cache");
        let root = dir.join("root");
        let cache = RunnerCache::new(dir.join("runners"), &root.to_string_lossy());
        let one = b"runner one".to_vec();
        let two = b"runner two".to_vec();
        let (sha_one, sha_two) = (sha256_hex(&one), sha256_hex(&two));

        assert!(cache.install(&sha_one, &two).is_err(), "wrong bytes");
        assert!(cache.current().is_none());
        let path_one = cache.install(&sha_one, &one).unwrap();
        cache.set_current(&sha_one).unwrap();
        assert_eq!(cache.current(), Some(path_one.clone()));

        // A job still running on runner one.
        let job = root.join("jobs/aa/bb/job-1");
        std::fs::create_dir_all(&job).unwrap();
        std::fs::write(job.join("state"), "running").unwrap();
        std::fs::write(
            job.join("runner.json"),
            serde_json::to_vec(&vec![path_one.clone()]).unwrap(),
        )
        .unwrap();

        let path_two = cache.install(&sha_two, &two).unwrap();
        cache.set_current(&sha_two).unwrap();
        cache.collect_garbage();
        assert_eq!(cache.current(), Some(path_two));
        assert!(
            Path::new(&path_one).exists(),
            "a running job's runner is kept"
        );

        std::fs::write(job.join("state"), "done").unwrap();
        cache.collect_garbage();
        assert!(!Path::new(&path_one).exists(), "then removed");
    }

    #[test]
    fn a_corrupted_current_runner_is_not_used() {
        let dir = ralphus_test_support::TestDir::new("runner-cache-corrupt");
        let cache = RunnerCache::new(dir.join("runners"), &dir.to_string_lossy());
        let bytes = b"runner".to_vec();
        let sha = sha256_hex(&bytes);
        let path = cache.install(&sha, &bytes).unwrap();
        cache.set_current(&sha).unwrap();
        std::fs::write(&path, b"tampered").unwrap();
        assert!(cache.current().is_none());
    }
}
