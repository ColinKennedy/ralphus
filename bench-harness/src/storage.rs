//! Per-test, per-commit JSON storage for benchmark records (RAL-94). Mirrors
//! `cli/src/ralphus/bench/storage.py`; layout: `<repo_root>/bench_data/rust/
//! <relative source file>/<test_name>.json`, kept entirely separate from the
//! Python side's `bench_data/python/` tree.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::gitinfo::{GitError, GitState};
use crate::stats::StatsBundle;

const DATA_ROOT_NAME: &str = "bench_data";

#[derive(Debug, Error)]
pub enum StorageError {
    #[error("no git repository found above {0}")]
    NoRepoRoot(PathBuf),
    #[error("failed to read {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to write {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error(transparent)]
    Git(#[from] GitError),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchRecord {
    pub commit: String,
    pub dirty: bool,
    pub durable_min: f64,
    pub max: f64,
    pub mean: f64,
    pub median: f64,
    pub stddev: f64,
    pub iqr: f64,
    #[serde(default)]
    pub outliers: Vec<f64>,
    #[serde(default)]
    pub samples: Vec<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestRecordFile {
    pub test_id: String,
    #[serde(default)]
    pub records: Vec<BenchRecord>,
}

/// Builds a `BenchRecord` from a git state + stats bundle pair.
#[must_use]
pub fn record_from_stats(git_state: &GitState, stats: &StatsBundle) -> BenchRecord {
    BenchRecord {
        commit: git_state.commit.clone(),
        dirty: git_state.dirty,
        durable_min: stats.durable_min,
        max: stats.max,
        mean: stats.mean,
        median: stats.median,
        stddev: stats.stddev,
        iqr: stats.iqr,
        outliers: stats.outliers.clone(),
        samples: stats.samples.clone(),
    }
}

/// Walks upward from `start` looking for a `.git` directory.
pub fn find_repo_root(start: &Path) -> Result<PathBuf, StorageError> {
    let mut current = start.canonicalize().map_err(|source| StorageError::Read {
        path: start.to_path_buf(),
        source,
    })?;
    loop {
        if current.join(".git").exists() {
            return Ok(current);
        }
        match current.parent() {
            Some(parent) => current = parent.to_path_buf(),
            None => return Err(StorageError::NoRepoRoot(start.to_path_buf())),
        }
    }
}

/// Path to the JSON file accumulating one test's records across commits.
/// `source_file` is typically a `file!()` value, already repo-root-relative.
#[must_use]
pub fn test_data_path(
    language: &str,
    source_file: &Path,
    test_name: &str,
    repo_root: &Path,
) -> PathBuf {
    repo_root
        .join(DATA_ROOT_NAME)
        .join(language)
        .join(source_file)
        .join(format!("{}.json", sanitize_filename(test_name)))
}

/// Windows MAX_PATH is 260 chars; a `module::path::fn_name`-derived filename
/// stem can run long on its own once nested a few modules deep. Keep
/// individual filename stems well under that ceiling by hashing long
/// identifiers instead of embedding them verbatim (RAL-94 Q3).
const MAX_FILENAME_STEM_LEN: usize = 80;

/// Replaces characters that are unsafe in a filename (notably `:` from
/// `BenchMeta::name`'s `module::path::fn_name` form, which Windows rejects)
/// with `_`, then bounds the result's length, hashing it if it's long. Short
/// names pass through unchanged (keeps filenames human-readable for the
/// common case); names over the length budget are truncated and suffixed
/// with a content hash so two long-but-differently-tailed names never collide.
fn sanitize_filename(name: &str) -> String {
    let safe: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();

    if safe.chars().count() <= MAX_FILENAME_STEM_LEN {
        return safe;
    }

    let digest = format!("{:016x}", fnv1a_hash(name));
    let keep = MAX_FILENAME_STEM_LEN - digest.len() - 1;
    let truncated: String = safe.chars().take(keep).collect();
    format!("{truncated}_{digest}")
}

/// FNV-1a 64-bit: a small, dependency-free, algorithm-stable hash (unlike
/// `std::collections::hash_map::DefaultHasher`, whose algorithm is explicitly
/// *not* guaranteed stable across Rust versions — which would silently
/// rename a committed test's data file on a toolchain bump).
fn fnv1a_hash(input: &str) -> u64 {
    const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0100_0000_01b3;
    input.bytes().fold(FNV_OFFSET_BASIS, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(FNV_PRIME)
    })
}

/// Loads an existing per-test record file, or `None` if it does not exist yet.
pub fn load_records(path: &Path) -> Result<Option<TestRecordFile>, StorageError> {
    if !path.exists() {
        return Ok(None);
    }
    let raw = fs::read_to_string(path).map_err(|source| StorageError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    let parsed = serde_json::from_str(&raw).map_err(|source| StorageError::Parse {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(Some(parsed))
}

/// Appends one commit's record to a test's data file, creating it if needed.
pub fn append_record(
    path: &Path,
    test_id: &str,
    record: BenchRecord,
) -> Result<TestRecordFile, StorageError> {
    let mut existing = load_records(path)?.unwrap_or_else(|| TestRecordFile {
        test_id: test_id.to_string(),
        records: Vec::new(),
    });
    existing.records.push(record);

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|source| StorageError::Write {
            path: path.to_path_buf(),
            source,
        })?;
    }
    let json = serde_json::to_string_pretty(&existing).map_err(|source| StorageError::Parse {
        path: path.to_path_buf(),
        source,
    })?;
    fs::write(path, format!("{json}\n")).map_err(|source| StorageError::Write {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(existing)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_record(commit: &str) -> BenchRecord {
        BenchRecord {
            commit: commit.to_string(),
            dirty: false,
            durable_min: 0.001,
            max: 0.002,
            mean: 0.0015,
            median: 0.0015,
            stddev: 0.0001,
            iqr: 0.0002,
            outliers: vec![],
            samples: vec![0.001, 0.002],
        }
    }

    #[test]
    fn append_creates_then_accumulates() {
        let dir = std::env::temp_dir().join(format!(
            "ralphus-bench-storage-test-{}-{}",
            std::process::id(),
            "append_creates_then_accumulates"
        ));
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("my_test.json");

        assert!(load_records(&path).unwrap().is_none());

        append_record(&path, "pkg::my_test", sample_record("aaa")).unwrap();
        let after_one = load_records(&path).unwrap().unwrap();
        assert_eq!(after_one.records.len(), 1);
        assert_eq!(after_one.test_id, "pkg::my_test");

        append_record(&path, "pkg::my_test", sample_record("bbb")).unwrap();
        let after_two = load_records(&path).unwrap().unwrap();
        assert_eq!(after_two.records.len(), 2);
        assert_eq!(after_two.records[0].commit, "aaa");
        assert_eq!(after_two.records[1].commit, "bbb");

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_data_path_layout() {
        let root = Path::new("/repo");
        let path = test_data_path("rust", Path::new("core/src/schema.rs"), "my_test", root);
        assert_eq!(
            path,
            Path::new("/repo/bench_data/rust/core/src/schema.rs/my_test.json")
        );
    }

    #[test]
    fn test_data_path_sanitizes_module_path_separators() {
        let root = Path::new("/repo");
        let path = test_data_path(
            "rust",
            Path::new("core/src/bench_demo.rs"),
            "ralphus_core::bench_demo::my_test",
            root,
        );
        assert_eq!(
            path,
            Path::new(
                "/repo/bench_data/rust/core/src/bench_demo.rs/ralphus_core__bench_demo__my_test.json"
            )
        );
    }

    #[test]
    fn test_data_path_hashes_long_identifiers() {
        let root = Path::new("/repo");
        let long_name = format!(
            "ralphus_core::bench_demo::{}",
            "a_very_long_parametrized_test_name_segment".repeat(4)
        );
        let path = test_data_path(
            "rust",
            Path::new("core/src/bench_demo.rs"),
            &long_name,
            root,
        );
        let filename = path.file_name().unwrap().to_str().unwrap();

        assert!(
            filename.len() <= MAX_FILENAME_STEM_LEN + ".json".len(),
            "filename should stay bounded: {filename}"
        );
        // Deterministic: hashing the same long name twice must land on the
        // same file, so records keep accumulating in one place.
        let path2 = test_data_path(
            "rust",
            Path::new("core/src/bench_demo.rs"),
            &long_name,
            root,
        );
        assert_eq!(path, path2);
    }

    #[test]
    fn sanitize_filename_leaves_short_names_untouched() {
        assert_eq!(sanitize_filename("my_test"), "my_test");
    }
}
