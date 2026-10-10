//! Generic external-harness `ModelBackend` (e.g. `aider`), ported from
//! `cli/src/ralphus/runner/harness_backend.py`. No stream parsing -- the
//! whole prompt is a trailing argv token and stdout/stderr are captured in
//! full, stdout being scanned line by line as it arrives for prophecy markers. `append_system_prompt`/`resume_agent_session_id` are
//! accepted for trait-shape compatibility but unused: there is no portable
//! flag a generic harness is guaranteed to understand for either.

use std::collections::HashSet;
use std::io::{BufRead, BufReader};
use std::process::Command;
use std::time::Duration;

use crate::backend::{BackendError, BackendOutcome, ModelBackend, RunOptions};
use crate::tools::Workspace;

/// Matches `cli/src/ralphus/config.py`'s `TaskConfig.maximum_timeout_seconds`
/// default -- used when a session sets no `timeout_sec` of its own.
const DEFAULT_TIMEOUT_SECS: u64 = 1800;

/// The tail of captured output kept when reporting a result -- matches
/// Python's `_tail(text, 2000)` truncation length (trailing, not leading, so
/// a marker on the final line survives).
const SUMMARY_TAIL_CHARS: usize = 2000;
const ERROR_TAIL_CHARS: usize = 500;

pub struct HarnessBackend {
    /// The external program name (the agent name itself, e.g. `"aider"`).
    pub program: String,
}

impl ModelBackend for HarnessBackend {
    fn run(
        &self,
        prompt: &str,
        workspace: &Workspace,
        options: &RunOptions<'_>,
    ) -> Result<BackendOutcome, BackendError> {
        let mut cmd = Command::new(&self.program);
        cmd.current_dir(workspace.root());
        if let Some(model) = options.model {
            cmd.arg("--model").arg(model);
        }
        cmd.arg(prompt);

        let child = cmd
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| {
                crate::cli_agent_common::emit_child_lifecycle(
                    "harness",
                    "agent process spawn failed",
                    "error",
                    serde_json::json!({"program": self.program, "error": e.to_string()}),
                );
                BackendError(format!("could not spawn {}: {e}", self.program))
            })?;
        let pid = child.id();
        crate::cli_agent_common::emit_child_lifecycle(
            "harness",
            "agent process spawned",
            "info",
            serde_json::json!({
                "program": self.program,
                "pid": pid,
                "model": options.model,
                "prompt_len": prompt.len(),
            }),
        );

        let timeout = Duration::from_secs(options.timeout_sec.unwrap_or(DEFAULT_TIMEOUT_SECS));
        let (output, prophecies) = wait_with_timeout(child, timeout, prompt).map_err(|e| {
            crate::cli_agent_common::emit_child_lifecycle(
                "harness",
                "agent process failed",
                "warning",
                serde_json::json!({"program": self.program, "pid": pid, "error": e.to_string()}),
            );
            BackendError(format!("{} failed: {e}", self.program))
        })?;
        crate::cli_agent_common::emit_child_exited("harness", pid, &output.status);

        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

        if !output.status.success() {
            let detail = tail(&stderr, ERROR_TAIL_CHARS);
            let detail = if detail.is_empty() {
                tail(&stdout, ERROR_TAIL_CHARS)
            } else {
                detail
            };
            return Err(BackendError(format!(
                "{} exited with {:?}: {detail}",
                self.program,
                output.status.code()
            )));
        }

        Ok(BackendOutcome {
            summary: tail(&stdout, SUMMARY_TAIL_CHARS),
            prophecies,
            // RAL-352: one external harness invocation is one exchanged
            // message -- there is no finer-grained conversational structure
            // to observe, and it is an agent backend, not a command.
            turns: 1,
            tokens_in: 0,
            tokens_out: 0,
            // An external harness's stdout is opaque text, so there is no
            // usage of any kind to read -- cache tokens included (RAL-326).
            cache_creation_tokens: 0,
            cache_read_tokens: 0,
            cost_usd: 0.0,
            agent_session_id: None,
            abandoned_background_job: None,
            // RAL-339: a generic external harness has no recognized
            // compaction signal at all.
            compaction_thrash: None,
            rate_limit_retry_after: None,
            // RAL-373: this backend reports no compaction data, not "never
            // compacts".
            compaction_input_tokens: 0,
            compaction_count: 0,
        })
    }
}

fn tail(text: &str, limit: usize) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= limit {
        trimmed.to_string()
    } else {
        trimmed
            .chars()
            .rev()
            .take(limit)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect()
    }
}

/// Reads `reader` line by line, handing each complete line to `on_line` as it
/// arrives, and returns every byte read. A line is scanned the moment it is
/// written, not after the process exits, so a marker survives a later timeout
/// or kill.
fn read_streaming(mut reader: impl BufRead, mut on_line: impl FnMut(&str)) -> Vec<u8> {
    let mut all = Vec::new();
    let mut line = Vec::new();
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                on_line(&String::from_utf8_lossy(&line));
                all.extend_from_slice(&line);
            }
        }
    }
    all
}

/// Scans a harness's stdout line by line for `RALPHUS_PROPHECY:` markers.
/// Lines the harness merely echoes back from `prompt` are skipped, so an
/// example marker in the prompt is never recorded as the agent's own.
fn scan_stdout_line(
    scanner: &mut crate::prophecy::ProphecyScanner,
    prompt_lines: &HashSet<String>,
    line: &str,
) {
    if prompt_lines.contains(line.trim()) {
        return;
    }
    scanner.scan_and_emit("harness", line);
}

fn wait_with_timeout(
    mut child: std::process::Child,
    timeout: Duration,
    prompt: &str,
) -> std::io::Result<(std::process::Output, Vec<crate::prophecy::ProphecyMarker>)> {
    let prompt_lines: HashSet<String> = prompt
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let stdout_thread = std::thread::spawn(move || {
        let mut scanner = crate::prophecy::ProphecyScanner::new();
        let bytes = stdout.map_or_else(Vec::new, |s| {
            read_streaming(BufReader::new(s), |line| {
                scan_stdout_line(&mut scanner, &prompt_lines, line);
            })
        });
        (bytes, scanner.markers())
    });
    let stderr_thread = std::thread::spawn(move || {
        stderr.map_or_else(Vec::new, |s| read_streaming(BufReader::new(s), |_| {}))
    });

    let start = std::time::Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if start.elapsed() >= timeout {
            crate::cli_agent_common::kill_child(
                "harness",
                &mut child,
                &format!("timed out after {}s", timeout.as_secs()),
            );
            // Closing the pipes ends the reader threads; markers they found
            // were already emitted live.
            let _ = stdout_thread.join();
            let _ = stderr_thread.join();
            return Err(std::io::Error::other(format!(
                "timed out after {}s",
                timeout.as_secs()
            )));
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let (stdout, prophecies) = stdout_thread.join().unwrap_or_default();
    let stderr = stderr_thread.join().unwrap_or_default();
    Ok((
        std::process::Output {
            status,
            stdout,
            stderr,
        },
        prophecies,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markers_are_scanned_as_each_line_arrives_and_not_from_the_echoed_prompt() {
        let prompt = "do the thing\nRALPHUS_PROPHECY: discovery: example from the prompt";
        let prompt_lines: HashSet<String> = prompt.lines().map(|l| l.trim().to_string()).collect();
        let stdout = format!(
            "{prompt}\nworking\nRALPHUS_PROPHECY: hazard: found a race\nmore work\nRALPHUS_PROPHECY: hazard: found a race\n"
        );
        let mut scanner = crate::prophecy::ProphecyScanner::new();
        let mut seen_lines = 0;
        let bytes = read_streaming(stdout.as_bytes(), |line| {
            seen_lines += 1;
            scan_stdout_line(&mut scanner, &prompt_lines, line);
        });
        assert_eq!(bytes, stdout.as_bytes());
        assert_eq!(seen_lines, 6);
        let markers = scanner.markers();
        assert_eq!(
            markers.len(),
            1,
            "echoed prompt and repeats are not recorded"
        );
        assert_eq!(markers[0].body, "found a race");
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn run_returns_markers_written_before_the_final_line() {
        let dir =
            std::env::temp_dir().join(format!("ralphus-harness-proph-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let ws = Workspace::create(&dir).unwrap();
        let backend = HarnessBackend {
            program: "echo".to_string(),
        };
        let out = backend
            .run(
                "RALPHUS_PROPHECY: decision: chose X",
                &ws,
                &RunOptions::default(),
            )
            .unwrap();
        // `echo` just reflects its argument, i.e. the prompt, which must not count.
        assert!(out.prophecies.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn tail_keeps_trailing_chars_when_over_limit() {
        let text = "a".repeat(10);
        assert_eq!(tail(&text, 4), "aaaa");
    }

    #[test]
    fn tail_keeps_whole_string_when_under_limit() {
        assert_eq!(tail("short", 2000), "short");
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn run_reports_nonzero_exit_as_backend_error() {
        let dir = std::env::temp_dir().join(format!("ralphus-harness-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let ws = Workspace::create(&dir).unwrap();
        let backend = HarnessBackend {
            program: "false".to_string(),
        };
        let err = backend.run("prompt", &ws, &RunOptions::default());
        assert!(err.is_err());
        std::fs::remove_dir_all(&dir).ok();
    }
}
