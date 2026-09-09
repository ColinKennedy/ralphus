//! RAL-385: end-to-end argument delivery through a Windows `.cmd` launcher.
//!
//! Windows resolves `pi`/`claude`/`codex` to npm's `.cmd` shim, and a batch
//! launcher cannot carry a newline in any argument: cmd.exe ends its command
//! at the newline, so the rest of the system prompt *and* every argument after
//! it are dropped without an error. That is what made a live squad's agent
//! receive its system prompt as ~50 separate one-word turns instead of a task.
//!
//! These tests drive the real [`PiBackend`] against a stand-in `.cmd` shim
//! that records the argv and stdin it actually received, so they fail if the
//! prompt ever moves back into argv or the system prompt stops being a file.

#![cfg(windows)]

use std::path::{Path, PathBuf};

use ralphus_runner::backend::{ModelBackend, RunOptions};
use ralphus_runner::pi_backend::PiBackend;
use ralphus_runner::tools::Workspace;

/// A multiline system prompt, shaped like the real one: the guardrail line the
/// live failure fed to the agent one word at a time, then a blank-line break.
const SYSTEM_PROMPT: &str = "Do NOT commit and do NOT push under any circumstances.\n\n\
     You are running unattended in a non-interactive cell.";

/// A multiline task prompt, shaped like the ticket markdown a cell really gets.
const PROMPT: &str = "# RAL-385: Expose Worktree Retirement Status\n\n\
     ## Summary\n\nAdd a queryable view of worktrees.\n\n- [ ] first\n- [ ] second";

fn node_available() -> bool {
    std::process::Command::new("node")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

/// Writes the stand-in shim plus the node script it forwards to, and returns
/// the `.cmd` path and the report path it will write.
fn write_fake_shim(dir: &Path) -> (PathBuf, PathBuf) {
    let script = dir.join("report.js");
    let report = dir.join("report.txt");
    // Reports argv and stdin to a file (not stdout) so the assertions hold
    // regardless of what the backend makes of the process's output, then emits
    // one well-formed Pi JSON event so the backend has something to parse.
    std::fs::write(
        &script,
        format!(
            r#"
const fs = require("fs");
let stdin = "";
process.stdin.on("data", (c) => (stdin += c));
process.stdin.on("end", () => {{
  fs.writeFileSync({report:?}, JSON.stringify({{argv: process.argv.slice(2), stdin}}, null, 2));
  process.stdout.write(JSON.stringify({{type: "session", id: "pi-test"}}) + "\n");
}});
"#,
            report = report.display().to_string(),
        ),
    )
    .expect("write report.js");

    let shim = dir.join("fake-pi.cmd");
    std::fs::write(
        &shim,
        format!("@echo off\r\nnode \"{}\" %*\r\n", script.display()),
    )
    .expect("write fake-pi.cmd");
    (shim, report)
}

/// Runs `PiBackend` against the stand-in shim and returns `(argv, stdin)`.
///
/// `label` keeps concurrent tests off each other's report file. The directory
/// name deliberately contains a space: a launcher path like `C:\Program
/// Files\...` is the everyday form of the quoting bug this guards, and it is
/// only visible when the path really has one.
fn run_against_shim(
    label: &str,
    system_prompt: Option<&str>,
    model: Option<&str>,
) -> (Vec<String>, String) {
    let dir = std::env::temp_dir().join(format!("ralphus ral385 {label}"));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    let (shim, report) = write_fake_shim(&dir);
    let _ = std::fs::remove_file(&report);

    let workspace = Workspace::create(&dir).expect("workspace");
    let backend = PiBackend {
        keep_temporary_files: false,
        program_override: Some(shim.display().to_string()),
    };
    // The outcome itself is not under test -- the stand-in is not Pi. What
    // matters is what reached the process, which it recorded on the way.
    let _ = backend.run(
        PROMPT,
        &workspace,
        &RunOptions {
            append_system_prompt: system_prompt,
            model,
            ..Default::default()
        },
    );

    let raw = std::fs::read_to_string(&report)
        .unwrap_or_else(|e| panic!("shim wrote no report at {}: {e}", report.display()));
    let parsed: serde_json::Value = serde_json::from_str(&raw).expect("report is json");
    let argv = parsed["argv"]
        .as_array()
        .expect("argv array")
        .iter()
        .map(|v| v.as_str().unwrap_or_default().to_string())
        .collect();
    let stdin = parsed["stdin"].as_str().unwrap_or_default().to_string();
    (argv, stdin)
}

/// The regression itself: before the fix the multiline prompt was an argv
/// token, cmd split it at every space, and Pi read each fragment as its own
/// message.
#[test]
fn multiline_prompt_reaches_a_batch_launcher_intact_over_stdin() {
    if !node_available() {
        eprintln!("SKIP: node is not on PATH");
        return;
    }
    let (argv, stdin) = run_against_shim("prompt", Some(SYSTEM_PROMPT), None);

    assert_eq!(stdin, PROMPT, "the whole prompt must arrive over stdin");
    assert!(
        !argv.iter().any(|a| a.contains('\n')),
        "no argument may contain a newline: {argv:?}"
    );
    assert!(
        !argv.iter().any(|a| a.contains("RAL-385")),
        "the prompt must not be an argument at all: {argv:?}"
    );
}

/// The system prompt travels as a readable file whose path is one clean token,
/// and the file's contents are the prompt exactly.
#[test]
fn multiline_system_prompt_reaches_a_batch_launcher_as_a_file() {
    if !node_available() {
        eprintln!("SKIP: node is not on PATH");
        return;
    }
    let (argv, _) = run_against_shim("sysprompt", Some(SYSTEM_PROMPT), None);

    let index = argv
        .iter()
        .position(|a| a == "--append-system-prompt")
        .unwrap_or_else(|| panic!("--append-system-prompt missing from {argv:?}"));
    let value = argv
        .get(index + 1)
        .unwrap_or_else(|| panic!("--append-system-prompt has no value in {argv:?}"));

    assert!(
        !value.contains("Do NOT commit"),
        "the system prompt text must not be inlined as an argument: {value:?}"
    );
    assert_eq!(
        std::fs::read_to_string(value).expect("system prompt file is readable"),
        SYSTEM_PROMPT,
    );
}

/// A space must not split an argument in half. This is the half of the bug
/// that has nothing to do with prompts: `cmd.exe` does not read MSVCRT
/// `\"` escaping, so before the fix every quoted token was torn apart at its
/// spaces. It bites any user whose repo or home directory is
/// `C:\Users\John Smith`, with no newline involved anywhere.
#[test]
fn a_space_bearing_argument_survives_a_batch_launcher() {
    if !node_available() {
        eprintln!("SKIP: node is not on PATH");
        return;
    }
    let (argv, _) = run_against_shim("space", None, Some("openrouter/glm 5 3 flash"));
    assert!(
        argv.windows(2)
            .any(|w| w[0] == "--model" && w[1] == "openrouter/glm 5 3 flash"),
        "a space-bearing argument must arrive as one token: {argv:?}"
    );
}
