//! The outpost's log: every line goes to stderr and, once [`open`] is
//! called (an installed outpost, `ralphus-outpost serve`), to a rotating
//! file -- `outpost.log`, moved to `outpost.log.1` (then `.2`, ...) when it
//! passes [`MAX_BYTES`], keeping [`KEEP`] old files.
//!
//! Callers never log a secret: request lines carry a verb and an id,
//! connection lines a URL and a machine name.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// A log file grows to this size before it is rotated.
pub const MAX_BYTES: u64 = 5 * 1024 * 1024;
/// Rotated files kept besides the current one.
pub const KEEP: usize = 3;

struct LogFile {
    path: PathBuf,
    max_bytes: u64,
}

static LOG: OnceLock<Mutex<LogFile>> = OnceLock::new();

/// Send log lines to `path` as well as stderr, from now on.
///
/// # Errors
/// The log directory could not be created.
pub fn open(path: &Path) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("could not create {}: {e}", parent.display()))?;
    }
    let _ = LOG.set(Mutex::new(LogFile {
        path: path.to_path_buf(),
        max_bytes: MAX_BYTES,
    }));
    Ok(())
}

/// Write one line to stderr and the log file.
pub fn line(text: &str) {
    eprintln!("{text}");
    if let Some(log) = LOG.get() {
        let log = log
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        append(&log.path, log.max_bytes, text);
    }
}

fn rotated(path: &Path, n: usize) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(format!(".{n}"));
    PathBuf::from(name)
}

/// Append `text` (timestamped) to `path`, rotating first when it is over
/// `max_bytes`.
fn append(path: &Path, max_bytes: u64, text: &str) {
    if std::fs::metadata(path).is_ok_and(|m| m.len() >= max_bytes) {
        let _ = std::fs::remove_file(rotated(path, KEEP));
        for n in (1..KEEP).rev() {
            let _ = std::fs::rename(rotated(path, n), rotated(path, n + 1));
        }
        let _ = std::fs::rename(path, rotated(path, 1));
    }
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(file, "{} {text}", ralphus_fleet::allocations::now_ms());
    }
}

/// The last `n` lines of the log at `path`.
#[must_use]
pub fn tail(path: &Path, n: usize) -> Vec<String> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(n)..]
        .iter()
        .map(|l| (*l).to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_log_rotates_and_keeps_a_bounded_number_of_files() {
        let dir = ralphus_test_support::TestDir::new("outpost-log");
        let path = dir.join("outpost.log");
        for i in 0..40 {
            append(
                &path,
                100,
                &format!("line {i} with some padding to grow the file"),
            );
        }
        assert!(std::fs::metadata(&path).unwrap().len() < 200);
        for n in 1..=KEEP {
            assert!(rotated(&path, n).exists(), "{n}");
        }
        assert!(!rotated(&path, KEEP + 1).exists());
        assert_eq!(tail(&path, 1).len(), 1);
        assert!(tail(&path, 1)[0].ends_with("line 39 with some padding to grow the file"));
    }
}
