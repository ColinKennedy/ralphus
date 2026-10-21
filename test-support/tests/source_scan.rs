//! RAL-690 guard: test code must not build fixtures from the system temp
//! folder. Any `std::env::temp_dir()` in `daemon/src` or `daemon/tests` fails
//! unless it is on the allow-list below (production code). Test code uses
//! `ralphus-test-support`.

use std::path::{Path, PathBuf};

/// (file suffix, substring of the allowed line).
const ALLOWED: &[(&str, &str)] = &[
    ("guardian_merge.rs", "let sid_path = std::env::temp_dir()"),
    ("guardian_merge.rs", "let build_root = std::env::temp_dir()"),
    (
        "guardian_merge.rs",
        "let daemon_stage = std::env::temp_dir()",
    ),
    ("runner/direct.rs", "let io_dir = std::env::temp_dir()"),
    ("timeline.rs", "let dir = std::env::temp_dir();"),
    (
        "ci_watch.rs",
        "let dir = std::env::temp_dir().join(format!(",
    ),
    ("ci_watch.rs", "std::fs::read_dir(std::env::temp_dir())"),
];

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap().flatten() {
        let path = entry.path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn daemon_sources_do_not_use_the_system_temp_dir_outside_the_allow_list() {
    let daemon = Path::new(env!("CARGO_MANIFEST_DIR")).join("../daemon");
    let mut files = Vec::new();
    rust_files(&daemon.join("src"), &mut files);
    rust_files(&daemon.join("tests"), &mut files);
    let mut offenders = Vec::new();
    for file in files {
        let text = std::fs::read_to_string(&file).unwrap();
        let name = file.to_string_lossy().replace('\\', "/");
        for (i, line) in text.lines().enumerate() {
            let code = line.trim_start();
            if code.starts_with("//") || !line.contains("env::temp_dir()") {
                continue;
            }
            let allowed = ALLOWED
                .iter()
                .any(|(suffix, needle)| name.ends_with(suffix) && line.contains(needle));
            if !allowed {
                offenders.push(format!("{name}:{}: {}", i + 1, code));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "use ralphus_test_support (TestDir / scratch) instead of env::temp_dir():\n{}",
        offenders.join("\n")
    );
}
