//! The checks a review branch's agent actually ran, for the PR writer.
//!
//! A review branch's commits come from one cell, and that cell's terminal-log
//! attempts record every shell call it made (`RALPHUS_TOOL:` lines) with its
//! result and output. The PR writer is shown the ones that were not plain file
//! inspection or editing -- builds, linters, type checkers, tests -- so a
//! description's testing section reports what was run and what it showed
//! instead of guessing from the diff.

use crate::guardian::GuardianView;
use crate::store::Store;

/// Most checks shown to the PR writer; the latest ones are kept.
const MAX_CHECKS: usize = 20;
/// Longest command text shown per check.
const MAX_COMMAND_CHARS: usize = 600;
/// Last lines of output shown per check.
const MAX_OUTPUT_LINES: usize = 15;
/// Longest output tail shown per check.
const MAX_OUTPUT_CHARS: usize = 1_500;

/// Tool names whose calls are shell commands.
const SHELL_TOOLS: &[&str] = &[
    "bash",
    "powershell",
    "shell",
    "exec_command",
    "local_shell",
    "run_shell_command",
    "command_execution",
    "execute",
];

/// Commands that only read, search, navigate, print or move files around. A
/// call made of nothing else checked nothing.
const NOT_A_CHECK: &[&str] = &[
    "git",
    "ls",
    "dir",
    "cat",
    "type",
    "head",
    "tail",
    "less",
    "more",
    "rg",
    "grep",
    "egrep",
    "find",
    "fd",
    "tree",
    "pwd",
    "echo",
    "printf",
    "wc",
    "stat",
    "file",
    "which",
    "where",
    "sed",
    "awk",
    "sort",
    "diff",
    "jq",
    "cd",
    "pushd",
    "popd",
    "sleep",
    "true",
    "false",
    "exit",
    "mkdir",
    "rm",
    "rmdir",
    "cp",
    "mv",
    "touch",
    "chmod",
    "get-content",
    "gc",
    "get-childitem",
    "gci",
    "select-string",
    "sls",
    "get-location",
    "set-location",
    "push-location",
    "pop-location",
    "test-path",
    "get-item",
    "resolve-path",
    "write-output",
    "write-host",
    "measure-object",
    "select-object",
    "format-list",
    "format-table",
    "out-string",
    "start-sleep",
    "new-item",
    "remove-item",
    "copy-item",
    "move-item",
    "set-content",
    "add-content",
    "out-file",
];

/// One shell call the agent made, with how it ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckRun {
    pub command: String,
    /// `None` when no result was recorded.
    pub succeeded: Option<bool>,
    /// The last lines of its output; empty when none was recorded.
    pub output_tail: String,
}

fn command_from_input(input: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(input.trim()).ok()?;
    let object = value.as_object()?;
    match object.get("command").or_else(|| object.get("cmd"))? {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Array(parts) => parts.last()?.as_str().map(str::to_string),
        _ => None,
    }
}

fn clip_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let kept: String = text.chars().take(max).collect();
    format!("{kept} [... cut]")
}

fn tail(output: &str) -> String {
    let lines: Vec<&str> = output.trim_end().lines().collect();
    let start = lines.len().saturating_sub(MAX_OUTPUT_LINES);
    let text = lines[start..].join("\n");
    let count = text.chars().count();
    if count <= MAX_OUTPUT_CHARS {
        return text;
    }
    text.chars().skip(count - MAX_OUTPUT_CHARS).collect()
}

/// Whether every command in `command` is one of [`NOT_A_CHECK`]. A command
/// line that cannot be split into commands (a here-document, say) is not
/// known to only inspect, so it is kept.
fn only_inspects(command: &str) -> bool {
    let units = crate::live_command_units::command_units(command);
    !units.is_empty()
        && units.iter().all(|unit| {
            let first = unit
                .key()
                .split_whitespace()
                .next()
                .unwrap_or_default()
                .trim_matches(|c| c == '"' || c == '\'')
                .to_ascii_lowercase();
            let name = first.rsplit(['/', '\\']).next().unwrap_or_default();
            let name = name.strip_suffix(".exe").unwrap_or(name);
            NOT_A_CHECK.contains(&name)
        })
}

/// The checks recorded in one transcript, in the order they last ran. A
/// command run more than once keeps only its last result, so a check that
/// failed and was fixed reads as passing.
#[must_use]
pub fn check_runs(transcript: &str) -> Vec<CheckRun> {
    let mut order: Vec<String> = Vec::new();
    let mut commands: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut results: std::collections::HashMap<String, (bool, String)> =
        std::collections::HashMap::new();
    for line in transcript.lines() {
        let Some(json) = line.trim_start().strip_prefix("RALPHUS_TOOL: ") else {
            continue;
        };
        let Ok(record) = serde_json::from_str::<serde_json::Value>(json) else {
            continue;
        };
        let Some(id) = record.get("id").and_then(|v| v.as_str()) else {
            continue;
        };
        if let Some(is_error) = record.get("is_error").and_then(serde_json::Value::as_bool) {
            let output = record
                .get("output")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            results.insert(id.to_string(), (!is_error, output));
        }
        let tool = record
            .get("tool")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        if !SHELL_TOOLS.contains(&tool.as_str()) || commands.contains_key(id) {
            continue;
        }
        if let Some(command) = record
            .get("input")
            .and_then(|v| v.as_str())
            .and_then(command_from_input)
        {
            order.push(id.to_string());
            commands.insert(id.to_string(), command);
        }
    }
    let mut runs: Vec<CheckRun> = Vec::new();
    for id in order {
        let Some(command) = commands.remove(&id) else {
            continue;
        };
        let command = command.trim().to_string();
        if command.is_empty() || only_inspects(&command) {
            continue;
        }
        let (succeeded, output) = match results.remove(&id) {
            Some((ok, output)) => (Some(ok), output),
            None => (None, String::new()),
        };
        runs.retain(|r| r.command != command);
        runs.push(CheckRun {
            command,
            succeeded,
            output_tail: tail(&output),
        });
    }
    let skip = runs.len().saturating_sub(MAX_CHECKS);
    runs.drain(..skip);
    runs
}

/// The transcript session name of the cell a review branch's commits came
/// from, or `None` when the branch has no recorded source cell.
#[must_use]
pub fn branch_session_name(
    store: &Store,
    guardian: &GuardianView,
    position: i64,
) -> Option<String> {
    let branch = guardian.branches.iter().find(|b| b.position == position)?;
    let squad = branch.source_squad_id.as_deref()?;
    let task_idx = branch.source_task_idx?;
    let cell_idx = branch.source_cell_idx?;
    let task = store.get_task_name(squad, task_idx).ok()?;
    let cell = store.get_cell_id(squad, task_idx, cell_idx).ok()?;
    Some(crate::cell_session::session_name(squad, &task, &cell))
}

/// The checks recorded across every attempt of one cell session.
#[must_use]
pub fn session_check_runs(session_name: &str) -> Vec<CheckRun> {
    let transcript: String = crate::terminal_log::list_attempts(session_name)
        .into_iter()
        .filter_map(|a| crate::terminal_log::read_attempt(session_name, a.attempt))
        .collect::<Vec<_>>()
        .join("\n");
    check_runs(&transcript)
}

/// The block the PR writer is given, or an empty string when there are no
/// checks. Commands and output are redacted and clipped.
#[must_use]
pub fn prompt_section(checks: &[CheckRun]) -> String {
    if checks.is_empty() {
        return String::new();
    }
    let mut out = String::from(
        "\n\nCHECKS RUN WHILE BUILDING THIS BRANCH (the shell commands the agent that \
         wrote these commits ran, other than ones that only read or moved files, \
         in the order they last ran, each with the whole call's exit status and the \
         end of its output. A call ending in a pipe such as `| tail` reports that \
         command's status, so when the output shows a failure, trust the output):\n",
    );
    for (n, check) in checks.iter().enumerate() {
        let result = match check.succeeded {
            Some(true) => "exit status: success",
            Some(false) => "exit status: FAILURE",
            None => "no result recorded",
        };
        let command = ralphus_core::redact::redact_secrets(&check.command);
        let _ = std::fmt::Write::write_fmt(
            &mut out,
            format_args!(
                "\n--- check {} ({result}) ---\ncommand:\n{}\n",
                n + 1,
                clip_chars(&command, MAX_COMMAND_CHARS)
            ),
        );
        if !check.output_tail.is_empty() {
            let output = ralphus_core::redact::redact_secrets(&check.output_tail);
            let _ =
                std::fmt::Write::write_fmt(&mut out, format_args!("end of output:\n{output}\n"));
        }
    }
    out.push_str(
        "\nWrite the testing section from this evidence only. Name the checks that \
         verify this change (build, lint, type check, tests -- skip any that only \
         edited files) as commands a reviewer can copy, in a code block, and say what \
         each showed, such as how many tests passed. Say plainly when a check failed \
         and was not re-run successfully. Do not claim anything this evidence does not \
         show, and do not say checks were not run when they appear above.",
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(id: &str, tool: &str, command: &str) -> String {
        let input = serde_json::json!({ "command": command }).to_string();
        format!(
            "RALPHUS_TOOL: {}",
            serde_json::json!({ "id": id, "tool": tool, "input": input })
        )
    }

    fn result(id: &str, ok: bool, output: &str) -> String {
        format!(
            "RALPHUS_TOOL: {}",
            serde_json::json!({ "id": id, "tool": "Bash", "is_error": !ok, "output": output })
        )
    }

    #[test]
    fn checks_keep_their_result_and_output_and_skip_inspection() {
        let transcript = [
            call("1", "Bash", "git status --short && ls apps"),
            result("1", true, "M a.py"),
            call(
                "2",
                "PowerShell",
                "Get-Content a.py | Select-Object -First 5",
            ),
            result("2", true, "x"),
            "plain text".to_string(),
            call(
                "3",
                "Bash",
                "moon run acme_cli:lint acme_cli:test 2>&1 | tail -30",
            ),
            result(
                "3",
                true,
                "lots\nof\noutput\n4 passed in 0.29s\nTasks: 8 completed",
            ),
            call("4", "Read", "{\"file_path\":\"x\"}"),
        ]
        .join("\n");
        let runs = check_runs(&transcript);
        assert_eq!(runs.len(), 1);
        assert_eq!(
            runs[0].command,
            "moon run acme_cli:lint acme_cli:test 2>&1 | tail -30"
        );
        assert_eq!(runs[0].succeeded, Some(true));
        assert!(
            runs[0]
                .output_tail
                .ends_with("4 passed in 0.29s\nTasks: 8 completed")
        );
    }

    #[test]
    fn a_call_that_cannot_be_split_into_commands_is_kept() {
        // The acme-cli cell of the monorepo test appended its code with a
        // here-document and ran its checks in the same call.
        let command =
            "cat >> app.py <<'EOF'\ndef hello():\n    pass\nEOF\nmoon run app:test 2>&1 | tail -30";
        let transcript = [
            call("1", "Bash", command),
            result("1", true, "4 passed\nTasks: 8 completed"),
        ]
        .join("\n");
        let runs = check_runs(&transcript);
        assert_eq!(runs.len(), 1);
        assert!(runs[0].output_tail.ends_with("Tasks: 8 completed"));
    }

    #[test]
    fn a_rerun_check_keeps_only_its_last_result() {
        let transcript = [
            call("1", "Bash", "uv run pytest packages/utils/tests"),
            result("1", false, "No module named 'acme_core'"),
            call("2", "Bash", "moon run acme_utils:test"),
            result("2", true, "7 passed"),
            call("3", "Bash", "uv run pytest packages/utils/tests"),
            result("3", true, "7 passed"),
        ]
        .join("\n");
        let runs = check_runs(&transcript);
        let summary: Vec<_> = runs
            .iter()
            .map(|r| (r.command.as_str(), r.succeeded))
            .collect();
        assert_eq!(
            summary,
            [
                ("moon run acme_utils:test", Some(true)),
                ("uv run pytest packages/utils/tests", Some(true)),
            ]
        );
    }

    #[test]
    fn a_long_output_keeps_only_its_last_lines() {
        let output: String = (0..100).map(|i| format!("line {i}\n")).collect();
        let transcript = [call("1", "Bash", "cargo test"), result("1", true, &output)].join("\n");
        let runs = check_runs(&transcript);
        assert_eq!(runs[0].output_tail.lines().count(), MAX_OUTPUT_LINES);
        assert!(runs[0].output_tail.ends_with("line 99"));
    }

    #[test]
    fn at_most_the_latest_checks_are_kept() {
        let transcript: Vec<String> = (0..MAX_CHECKS + 5)
            .flat_map(|i| {
                let id = i.to_string();
                [
                    call(&id, "Bash", &format!("make check-{i}")),
                    result(&id, true, ""),
                ]
            })
            .collect();
        let runs = check_runs(&transcript.join("\n"));
        assert_eq!(runs.len(), MAX_CHECKS);
        assert_eq!(runs[0].command, "make check-5");
    }

    #[test]
    fn the_prompt_section_lists_each_check_and_is_empty_without_any() {
        assert_eq!(prompt_section(&[]), "");
        let section = prompt_section(&[
            CheckRun {
                command: "moon run acme_cli:test".into(),
                succeeded: Some(true),
                output_tail: "4 passed".into(),
            },
            CheckRun {
                command: "ANTHROPIC_API_KEY=NOTAREALKEY make".into(),
                succeeded: Some(false),
                output_tail: String::new(),
            },
        ]);
        assert!(section.contains(
            "--- check 1 (exit status: success) ---\ncommand:\nmoon run acme_cli:test\nend of output:\n4 passed\n"
        ));
        assert!(section.contains("--- check 2 (exit status: FAILURE) ---"));
        assert!(section.contains("do not say checks were not run"));
        assert!(!section.contains("NOTAREALKEY"));
    }

    #[test]
    fn a_branch_without_a_source_cell_has_no_session() {
        let store = Store::open_in_memory().unwrap();
        let gid = store.create_guardian("g", "main", "/tmp/x").unwrap();
        store.add_guardian_branch(&gid, "b").unwrap();
        let guardian = store.get_guardian(&gid).unwrap();
        assert!(branch_session_name(&store, &guardian, 0).is_none());
    }
}
