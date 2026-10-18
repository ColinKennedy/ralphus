//! Live-test results in PR/MR descriptions.
//!
//! Every PR/MR of a review carries a managed `## Live-Test Results` section,
//! delimited by [`SECTION_START`] / [`SECTION_END`], that this module renders
//! from the review's live runs. This file is the pure part: judging a run (the
//! verdict, whether a fix was confirmed, which reproduction commands were
//! really executed), rendering the section, and splicing it into a body. The
//! durable publish queue lives in [`crate::live_publish`].
//!
//! A run's verdict comes from the `RALPHUS_LIVE_RESULT:` line the live agent
//! ends its reply with. The agent's reproduction recipe is only believed where
//! it matches a shell call recorded as a `RALPHUS_TOOL:` transcript record.

use crate::live_actions::LiveAction;
use crate::live_runs::LiveRunRow;

/// The marker the live agent writes its structured result under.
pub const RESULT_MARKER: &str = "RALPHUS_LIVE_RESULT:";

/// Appended to a live run's system prompt: how to report the structured result.
pub const RESULT_INSTRUCTION: &str = "After your report, as the very last line of your reply, write one line `RALPHUS_LIVE_RESULT:` followed by a single-line JSON object with these keys: \"verdict\" (\"passed\" when the live test showed the change working, \"failed\" when it showed a problem, \"inconclusive\" when you could not tell), \"summary\" (one sentence), \"fix_attempted\" (true if you changed code to fix a problem), \"fix_confirmed\" (true only if you re-ran the failing steps after the fix and they then worked), and \"recipe\" (the shell commands, in order, that reproduce the finding or confirm the fix, as [{\"command\": \"...\", \"expect\": \"what correct looks like\"}]). List only commands you really ran in this session, exactly as you ran them; never put secrets in them.";

/// Opens the managed section of a description.
pub const SECTION_START: &str = "<!-- ralphus:live-test-results:start -->";

/// Closes the managed section of a description.
pub const SECTION_END: &str = "<!-- ralphus:live-test-results:end -->";

/// Most runs shown per action.
pub const MAX_RUNS_PER_ACTION: usize = 3;

/// Longest prompt copied into a description before it is cut.
const PROMPT_MAX_CHARS: usize = 6000;

/// Longest single reproduction command shown.
const COMMAND_MAX_CHARS: usize = 600;

/// Marker the transcript writer puts before each structured tool record.
const TOOL_MARKER: &str = "RALPHUS_TOOL: ";

/// Tool names that run a shell command, across the agent backends.
const SHELL_TOOLS: [&str; 6] = [
    "bash",
    "powershell",
    "exec",
    "run_bash",
    "shell",
    "exec_command",
];

/// What a live run found, independent of how its agent session ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Passed,
    Failed,
    Inconclusive,
}

impl Verdict {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Passed => "passed",
            Self::Failed => "failed",
            Self::Inconclusive => "inconclusive",
        }
    }

    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "passed" | "pass" => Some(Self::Passed),
            "failed" | "fail" => Some(Self::Failed),
            "inconclusive" => Some(Self::Inconclusive),
            _ => None,
        }
    }
}

/// One step of the reproduction recipe the agent proposed.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, PartialEq, Eq)]
pub struct RecipeStep {
    pub command: String,
    #[serde(default)]
    pub expect: Option<String>,
}

/// The agent's `RALPHUS_LIVE_RESULT:` line, parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedResult {
    pub verdict: Verdict,
    pub summary: String,
    pub fix_attempted: bool,
    pub fix_confirmed: bool,
    pub recipe: Vec<RecipeStep>,
}

#[derive(serde::Deserialize)]
struct RawResult {
    verdict: String,
    #[serde(default)]
    summary: String,
    #[serde(default)]
    fix_attempted: bool,
    #[serde(default)]
    fix_confirmed: bool,
    #[serde(default)]
    recipe: Vec<RecipeStep>,
}

/// The last well-formed `RALPHUS_LIVE_RESULT: {json}` line of a reply. `None`
/// when there is none, or its JSON or verdict is unusable.
#[must_use]
pub fn parse_result(reply: &str) -> Option<ParsedResult> {
    let line = reply
        .lines()
        .rev()
        .find_map(|l| l.trim_start().strip_prefix(RESULT_MARKER))?;
    let raw: RawResult = serde_json::from_str(line.trim()).ok()?;
    Some(ParsedResult {
        verdict: Verdict::parse(&raw.verdict)?,
        summary: raw.summary.trim().to_string(),
        fix_attempted: raw.fix_attempted,
        fix_confirmed: raw.fix_confirmed,
        recipe: raw.recipe,
    })
}

/// One shell command the agent really ran, from the transcript.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellCall {
    pub command: String,
    /// How the call ended, in words ("exit 0", "exit status not recorded", ...).
    pub result: String,
}

#[derive(Default)]
struct ToolHalves {
    tool: Option<String>,
    input: Option<String>,
    output: Option<String>,
    is_error: Option<bool>,
}

fn command_of(input: &str) -> Option<String> {
    let command = match serde_json::from_str::<serde_json::Value>(input) {
        Ok(serde_json::Value::Object(map)) => {
            let v = map.get("command").or_else(|| map.get("cmd"))?;
            match v {
                serde_json::Value::String(s) => s.clone(),
                serde_json::Value::Array(parts) => parts
                    .iter()
                    .filter_map(|p| p.as_str())
                    .collect::<Vec<_>>()
                    .join(" "),
                _ => return None,
            }
        }
        Ok(serde_json::Value::String(s)) => s,
        _ => input.to_string(),
    };
    let command = command.trim().to_string();
    (!command.is_empty()).then_some(command)
}

fn result_words(output: Option<&str>, is_error: Option<bool>) -> String {
    if let Some(code) = output.and_then(|o| {
        let rest = o.split("exit_code=").nth(1)?;
        let digits: String = rest
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == '-')
            .collect();
        digits.parse::<i64>().ok()
    }) {
        return format!("exit {code}");
    }
    match is_error {
        Some(true) => "the tool reported an error".to_string(),
        Some(false) => "no error reported; exit status not recorded".to_string(),
        None => "result not recorded".to_string(),
    }
}

/// The shell commands recorded in a raw transcript, in the order they were
/// issued. Call and result halves of one record are merged by `id`.
#[must_use]
pub fn shell_calls(transcript: &str) -> Vec<ShellCall> {
    let mut order: Vec<String> = Vec::new();
    let mut by_id: std::collections::HashMap<String, ToolHalves> = std::collections::HashMap::new();
    for line in transcript.lines() {
        let Some(at) = line.find(TOOL_MARKER) else {
            continue;
        };
        let Ok(rec) = serde_json::from_str::<serde_json::Value>(&line[at + TOOL_MARKER.len()..])
        else {
            continue;
        };
        let Some(id) = rec.get("id").and_then(|v| v.as_str()) else {
            continue;
        };
        if !by_id.contains_key(id) {
            order.push(id.to_string());
        }
        let halves = by_id.entry(id.to_string()).or_default();
        if let Some(tool) = rec.get("tool").and_then(|v| v.as_str()) {
            halves.tool = Some(tool.to_string());
        }
        if let Some(input) = rec.get("input").and_then(|v| v.as_str()) {
            halves.input = Some(input.to_string());
        }
        if let Some(output) = rec.get("output").and_then(|v| v.as_str()) {
            halves.output = Some(output.to_string());
        }
        if let Some(e) = rec.get("is_error").and_then(serde_json::Value::as_bool) {
            halves.is_error = Some(e);
        }
    }
    order
        .iter()
        .filter_map(|id| {
            let h = by_id.get(id)?;
            let tool = h.tool.as_deref()?.to_ascii_lowercase();
            if !SHELL_TOOLS.contains(&tool.as_str()) {
                return None;
            }
            Some(ShellCall {
                command: command_of(h.input.as_deref()?)?,
                result: result_words(h.output.as_deref(), h.is_error),
            })
        })
        .collect()
}

/// A reproduction command that matched a recorded shell call.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, PartialEq, Eq)]
pub struct VerifiedCommand {
    pub command: String,
    /// The directory it ran in, `None` when it cannot be known.
    pub cwd: Option<String>,
    pub result: String,
}

fn normalize(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Whether a command changes directory before doing anything else, which
/// makes the run's cwd the wrong answer for where it executed.
fn changes_directory(command: &str) -> bool {
    command.split(['\n', ';', '&', '|']).any(|part| {
        let first = part.split_whitespace().next().unwrap_or_default();
        matches!(
            first.to_ascii_lowercase().as_str(),
            "cd" | "pushd" | "set-location" | "sl" | "chdir"
        )
    })
}

/// Keep the recipe steps that match a recorded shell call, in the order the
/// calls were made. A step matches a call whose command equals it, or contains
/// it. Returns the verified commands and how many steps matched nothing.
#[must_use]
pub fn verify_recipe(
    recipe: &[RecipeStep],
    calls: &[ShellCall],
    run_cwd: Option<&str>,
) -> (Vec<VerifiedCommand>, usize) {
    let mut cursor = 0;
    let mut verified = Vec::new();
    let mut dropped = 0;
    for step in recipe {
        let wanted = normalize(&step.command);
        let found = (!wanted.is_empty())
            .then(|| {
                calls
                    .iter()
                    .enumerate()
                    .skip(cursor)
                    .find(|(_, c)| normalize(&c.command).contains(&wanted))
            })
            .flatten();
        match found {
            Some((i, call)) => {
                cursor = i + 1;
                verified.push(VerifiedCommand {
                    command: redact(&clip(&wanted, COMMAND_MAX_CHARS)),
                    cwd: if changes_directory(&call.command) {
                        None
                    } else {
                        run_cwd.map(str::to_string)
                    },
                    result: call.result.clone(),
                });
            }
            None => dropped += 1,
        }
    }
    (verified, dropped)
}

fn redact(text: &str) -> String {
    ralphus_core::redact::redact_secrets(text).into_owned()
}

fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let kept: String = text.chars().take(max).collect();
    format!("{kept}… [truncated]")
}

/// What ralphus concluded about one run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Judgement {
    pub verdict: Verdict,
    pub detail: String,
    pub fix_attempted: bool,
    pub fix_confirmed: bool,
    pub commands: Vec<VerifiedCommand>,
    /// The agent's recipe as proposed, as JSON, for the record.
    pub recipe_json: Option<String>,
}

/// Judge a run. `session_finished` is whether the agent session itself ended
/// normally; a run that did not finish is inconclusive whatever it printed.
/// A fix counts as confirmed only when the agent claims it, the verdict is
/// `passed`, and at least one reproduction command is backed by the transcript.
#[must_use]
pub fn judge(
    session_finished: bool,
    reply: &str,
    calls: &[ShellCall],
    run_cwd: Option<&str>,
    has_commits: bool,
) -> Judgement {
    let parsed = parse_result(reply);
    let recipe_json = parsed
        .as_ref()
        .and_then(|p| serde_json::to_string(&p.recipe).ok());
    let (commands, dropped) = parsed
        .as_ref()
        .map(|p| verify_recipe(&p.recipe, calls, run_cwd))
        .unwrap_or_default();
    let claimed_attempt = parsed.as_ref().is_some_and(|p| p.fix_attempted);
    let fix_attempted = claimed_attempt || has_commits;
    let (verdict, mut detail) = match (&parsed, session_finished) {
        (_, false) => (
            Verdict::Inconclusive,
            "the live agent session did not finish, so nothing can be concluded".to_string(),
        ),
        (None, true) => (
            Verdict::Inconclusive,
            format!("the agent gave no valid {RESULT_MARKER} line"),
        ),
        (Some(p), true) => (p.verdict, p.summary.clone()),
    };
    if dropped > 0 {
        let note = format!(
            "{dropped} proposed reproduction step(s) did not match any recorded shell call and were left out"
        );
        detail = if detail.is_empty() {
            note
        } else {
            format!("{detail} ({note})")
        };
    }
    let fix_confirmed = session_finished
        && verdict == Verdict::Passed
        && parsed.as_ref().is_some_and(|p| p.fix_confirmed)
        && !commands.is_empty();
    Judgement {
        verdict,
        detail: redact(&detail),
        fix_attempted,
        fix_confirmed,
        commands,
        recipe_json,
    }
}

// ---- rendering ----------------------------------------------------------------

/// One action's entry in a description.
#[derive(Debug, Clone)]
pub struct ActionEntry {
    pub action: LiveAction,
    /// Newest first.
    pub runs: Vec<LiveRunRow>,
    /// The action has no resolved branch, so it is shown only on the top PR.
    pub review_wide: bool,
}

/// Days since 1970-01-01 to a civil date (proleptic Gregorian).
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// `2026-10-16 12:30 UTC` for a Unix-millisecond time.
#[must_use]
pub fn format_time(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
    let rem = secs.rem_euclid(86_400);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02} UTC",
        rem / 3600,
        rem % 3600 / 60
    )
}

fn fence(body: &str) -> String {
    let mut longest = 0;
    let mut run = 0;
    for c in body.chars() {
        if c == '`' {
            run += 1;
            longest = longest.max(run);
        } else {
            run = 0;
        }
    }
    let ticks = "`".repeat((longest + 1).max(3));
    format!("{ticks}text\n{body}\n{ticks}")
}

fn yes_no(v: Option<bool>) -> &'static str {
    match v {
        Some(true) => "yes",
        Some(false) => "no",
        None => "not recorded",
    }
}

fn integration_text(run: &LiveRunRow) -> String {
    let mut text = match run.integration.as_str() {
        "none" if run.commits.is_empty() => "no commits to carry".to_string(),
        other => other.to_string(),
    };
    if let Some(detail) = run.integration_detail.as_deref().filter(|d| !d.is_empty()) {
        text.push_str(&format!(" — {}", redact(detail)));
    }
    if run.integration == "integrated" {
        match run.integration_branch_id.as_deref() {
            Some(branch) => text.push_str(&format!(
                "; destination branch `{branch}`, push: {}",
                match run.integration_pushed {
                    Some(true) => "pushed",
                    Some(false) => "not pushed",
                    None => "result not recorded",
                }
            )),
            None => text.push_str("; destination branch not recorded"),
        }
        text.push_str(". Other PRs in the stack were not checked for this change.");
    } else if !run.commits.is_empty() {
        text.push_str(". The fix has not been confirmed on any branch.");
    }
    text
}

fn render_run(out: &mut String, run: &LiveRunRow) {
    let verdict = run.verdict.as_deref().unwrap_or("inconclusive");
    out.push_str(&format!(
        "- **Run `{}`** ({}, {}) — verdict **{verdict}**; agent session: {}\n",
        run.run_id,
        run.trigger,
        format_time(run.finished_at_ms.unwrap_or(run.queued_at_ms)),
        run.state,
    ));
    if let Some(detail) = run.verdict_detail.as_deref().filter(|d| !d.is_empty()) {
        out.push_str(&format!("  - Finding: {}\n", detail.replace('\n', " ")));
    }
    out.push_str(&format!(
        "  - Fix attempted: {}; fix confirmed: {}; commits: {}\n",
        yes_no(run.fix_attempted),
        yes_no(run.fix_confirmed),
        run.commits.len()
    ));
    out.push_str(&format!("  - Integration: {}\n", integration_text(run)));
    if run.commands.is_empty() {
        out.push_str("  - Reproduction commands: none verified against the recorded session\n");
    } else {
        out.push_str("  - Reproduction commands (checked against the recorded session):\n");
        for c in &run.commands {
            out.push_str(&format!(
                "    - `{}` — cwd: {}; result: {}\n",
                c.command.replace('`', "'"),
                c.cwd.as_deref().unwrap_or("unknown"),
                c.result
            ));
        }
    }
    if let Some(prompt) = run.prompt.as_deref() {
        let body = redact(&clip(prompt, PROMPT_MAX_CHARS));
        out.push_str("  <details><summary>Prompt sent to the runner</summary>\n\n");
        for line in fence(&body).lines() {
            out.push_str(&format!("  {line}\n"));
        }
        out.push_str("\n  </details>\n");
    }
}

/// The managed section for a set of action entries, or `None` when there is
/// nothing to show (the description is then left alone).
#[must_use]
pub fn render_section(entries: &[ActionEntry]) -> Option<String> {
    let shown: Vec<&ActionEntry> = entries.iter().filter(|e| !e.runs.is_empty()).collect();
    if shown.is_empty() {
        return None;
    }
    let mut out = format!("{SECTION_START}\n## Live-Test Results\n\n");
    for entry in shown {
        let label = entry.action.label.replace('\n', " ");
        out.push_str(&format!("### {label} (action {})\n", entry.action.idx));
        if entry.review_wide {
            out.push_str(
                "_Review-wide entry: this action could not be tied to a branch of the review._\n",
            );
        } else if entry.action.branch_ids.len() > 1 {
            out.push_str(
                "_This action covers several branches of the review and ran once against the review checkout._\n",
            );
        }
        out.push('\n');
        for run in entry.runs.iter().take(MAX_RUNS_PER_ACTION) {
            render_run(&mut out, run);
        }
        out.push('\n');
    }
    out.push_str(SECTION_END);
    Some(out)
}

/// Splice `section` into `body`: replace what lies between the markers when
/// both are present, otherwise append. Idempotent.
#[must_use]
pub fn merge_section(body: &str, section: &str) -> String {
    if let (Some(start), Some(end)) = (body.find(SECTION_START), body.find(SECTION_END)) {
        if start < end {
            let tail = end + SECTION_END.len();
            return format!("{}{section}{}", &body[..start], &body[tail..]);
        }
    }
    let trimmed = body.trim_end();
    if trimmed.is_empty() {
        section.to_string()
    } else {
        format!("{trimmed}\n\n{section}\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(v: serde_json::Value) -> String {
        format!("{TOOL_MARKER}{v}")
    }

    fn transcript() -> String {
        [
            "noise line",
            &record(serde_json::json!({"id":"a","tool":"Bash","input":"{\"command\":\"cargo build\"}"})),
            &record(serde_json::json!({"id":"a","output":"exit_code=0\nstdout:","is_error":false})),
            &record(serde_json::json!({"id":"b","tool":"Read","input":"{\"path\":\"x\"}"})),
            &record(serde_json::json!({"id":"c","tool":"Bash","input":"{\"command\":\"cd sub && ./run.sh --go\"}","output":"exit_code=3","is_error":true})),
        ]
        .join("\n")
    }

    #[test]
    fn parses_last_valid_result_line() {
        let reply = "report\nRALPHUS_LIVE_RESULT: {\"verdict\":\"failed\"}\nRALPHUS_LIVE_RESULT: {\"verdict\":\"Passed\",\"summary\":\"ok\",\"fix_attempted\":true}";
        let p = parse_result(reply).unwrap();
        assert_eq!(p.verdict, Verdict::Passed);
        assert!(p.fix_attempted && !p.fix_confirmed);
        assert!(parse_result("RALPHUS_LIVE_RESULT: {\"verdict\":\"maybe\"}").is_none());
        assert!(parse_result("RALPHUS_LIVE_RESULT: not json").is_none());
        assert!(parse_result("nothing").is_none());
    }

    #[test]
    fn extracts_only_shell_calls_with_results() {
        let calls = shell_calls(&transcript());
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].command, "cargo build");
        assert_eq!(calls[0].result, "exit 0");
        assert_eq!(calls[1].result, "exit 3");
    }

    #[test]
    fn unrecorded_status_is_stated_plainly() {
        let t = record(
            serde_json::json!({"id":"x","tool":"exec","input":"echo hi","output":"hi","is_error":false}),
        );
        let calls = shell_calls(&t);
        assert_eq!(calls[0].command, "echo hi");
        assert!(calls[0].result.contains("not recorded"));
    }

    #[test]
    fn recipe_keeps_only_recorded_steps_in_order() {
        let calls = shell_calls(&transcript());
        let recipe = vec![
            RecipeStep {
                command: "./run.sh --go".into(),
                expect: None,
            },
            RecipeStep {
                command: "cargo build".into(),
                expect: None,
            },
            RecipeStep {
                command: "never ran".into(),
                expect: None,
            },
        ];
        let (verified, dropped) = verify_recipe(&recipe, &calls, Some("/repo"));
        // `cargo build` came before `run.sh`, so it cannot follow it.
        assert_eq!(verified.len(), 1);
        assert_eq!(dropped, 2);
        assert_eq!(verified[0].cwd, None, "a cd makes the cwd unknown");
        assert_eq!(verified[0].result, "exit 3");
    }

    #[test]
    fn recipe_commands_are_redacted() {
        let t = record(
            serde_json::json!({"id":"s","tool":"Bash","input":"{\"command\":\"export API_TOKEN=supersecretvalue123 && run\"}","output":"exit_code=0"}),
        );
        let recipe = vec![RecipeStep {
            command: "export API_TOKEN=supersecretvalue123 && run".into(),
            expect: None,
        }];
        let (verified, _) = verify_recipe(&recipe, &shell_calls(&t), Some("/r"));
        assert!(!verified[0].command.contains("supersecretvalue123"));
    }

    #[test]
    fn judge_separates_verdict_from_session_and_commits() {
        let calls = shell_calls(&transcript());
        let reply = "RALPHUS_LIVE_RESULT: {\"verdict\":\"passed\",\"fix_attempted\":true,\"fix_confirmed\":true,\"recipe\":[{\"command\":\"cargo build\"}]}";
        let j = judge(true, reply, &calls, Some("/r"), true);
        assert_eq!(j.verdict, Verdict::Passed);
        assert!(j.fix_attempted && j.fix_confirmed);

        // No verified command: the claim is not believed.
        let reply = "RALPHUS_LIVE_RESULT: {\"verdict\":\"passed\",\"fix_confirmed\":true,\"recipe\":[{\"command\":\"nope\"}]}";
        let j = judge(true, reply, &calls, None, true);
        assert!(j.fix_attempted && !j.fix_confirmed);

        // Commits alone are an attempt, never a confirmation.
        let j = judge(true, "no result line", &calls, None, true);
        assert_eq!(j.verdict, Verdict::Inconclusive);
        assert!(j.fix_attempted && !j.fix_confirmed);

        // An unfinished session is inconclusive whatever it printed.
        let reply = "RALPHUS_LIVE_RESULT: {\"verdict\":\"passed\"}";
        assert_eq!(
            judge(false, reply, &calls, None, false).verdict,
            Verdict::Inconclusive
        );
    }

    #[test]
    fn merge_replaces_between_markers_and_is_idempotent() {
        let section = format!("{SECTION_START}\nnew\n{SECTION_END}");
        let appended = merge_section("Body text", &section);
        assert!(appended.starts_with("Body text\n\n"));
        let old = format!("Intro\n{SECTION_START}\nold\n{SECTION_END}\nOutro");
        let replaced = merge_section(&old, &section);
        assert_eq!(replaced, format!("Intro\n{section}\nOutro"));
        assert_eq!(merge_section(&replaced, &section), replaced);
        assert_eq!(merge_section(&appended, &section), appended);
    }

    #[test]
    fn formats_time() {
        assert_eq!(format_time(0), "1970-01-01 00:00 UTC");
        assert_eq!(format_time(1_760_617_800_000), "2025-10-16 12:30 UTC");
    }

    #[test]
    fn fence_outgrows_backticks_in_the_body() {
        assert!(fence("a ``` b").starts_with("````text"));
    }
}
