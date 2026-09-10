//! RAL-397 Phase 2B: `ralphus-runner pipe-sink`'s actual read/write loop —
//! byte fidelity and the `--max-bytes` truncation cap.
//!
//! `pipe_sink` reads process-level stdin, which an in-process unit test
//! cannot redirect, so this spawns the real compiled binary with piped
//! stdin. This covers the binary's own byte-handling; the psmux/tmux wiring
//! around it (does `pipe-pane -o` actually deliver bytes to this binary's
//! stdin) is covered separately by `daemon/src/tmux.rs`'s
//! `live_tmux_pipe_pane_tees_raw_output_to_a_file`.

use std::io::Write as _;
use std::process::{Command, Stdio};

fn run_pipe_sink(input: &[u8], out: &std::path::Path, max_bytes: Option<u64>) {
    let exe = env!("CARGO_BIN_EXE_ralphus-runner");
    let mut cmd = Command::new(exe);
    cmd.arg("pipe-sink").arg("--out").arg(out);
    if let Some(cap) = max_bytes {
        cmd.arg("--max-bytes").arg(cap.to_string());
    }
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn ralphus-runner pipe-sink");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input)
        .expect("write stdin");
    let status = child.wait().expect("wait for pipe-sink");
    assert!(status.success(), "pipe-sink exited non-zero");
}

fn scratch_dir(label: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "ralphus-runner-pipe-sink-test-{}-{label}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn pipe_sink_appends_raw_stdin_bytes_to_the_file() {
    let dir = scratch_dir("appends");
    let out = dir.join("out.raw");
    run_pipe_sink(b"line one\nline two\n", &out, None);
    let content = std::fs::read_to_string(&out).unwrap();
    assert_eq!(content, "line one\nline two\n");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn pipe_sink_stops_persisting_past_max_bytes_but_still_drains_stdin() {
    let dir = scratch_dir("truncates");
    let out = dir.join("out.raw");
    // 10-byte cap, 30 bytes of input: only the first 10 bytes of real
    // content should be persisted, plus a single truncation marker. If the
    // process didn't keep draining stdin past the cap, the child would block
    // writing to a full pipe and `write_all`/`wait` above would hang.
    run_pipe_sink(b"0123456789ABCDEFGHIJabcdefghij", &out, Some(10));
    let content = std::fs::read_to_string(&out).unwrap();
    assert!(
        content.starts_with("0123456789"),
        "expected the first 10 bytes preserved, got: {content:?}"
    );
    assert!(
        content.contains("truncated at 10 bytes"),
        "expected a truncation marker, got: {content:?}"
    );
    assert!(
        !content.contains("ABCDEFGHIJ") && !content.contains("abcdefghij"),
        "expected content past the cap to be dropped, got: {content:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn pipe_sink_without_out_flag_fails() {
    let exe = env!("CARGO_BIN_EXE_ralphus-runner");
    let status = Command::new(exe)
        .arg("pipe-sink")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("spawn ralphus-runner pipe-sink");
    assert!(!status.success());
}
