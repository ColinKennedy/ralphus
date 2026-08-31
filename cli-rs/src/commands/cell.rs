//! `ralphus cell <subcommand>`, ported from `cli/src/ralphus/__main__.py`'s
//! `session` group (show/worktree/reviews/set-status/restart/restart-proof/
//! edit/terminal).

use serde_json::Value;

use crate::args::GlobalOpts;
use crate::client::DaemonClient;
use crate::commands::{CommandError, emit, run_and_report};
use crate::flags::Scanner;
use crate::selector::{ResolvedSelector, SelectorError, resolve_squad_selector, squad_view_uri};

#[derive(Debug, Clone)]
pub enum CellCommand {
    Help,
    Show {
        selector: String,
    },
    Worktree {
        selector: String,
    },
    Reviews {
        selector: String,
    },
    SetStatus {
        selector: String,
        state: String,
    },
    Restart {
        selector: String,
    },
    RestartProof {
        selector: String,
        from: i64,
    },
    Edit {
        selector: String,
        cwd: Option<String>,
        agent: Option<String>,
        model: Option<String>,
        prompt: Option<String>,
        command: Option<String>,
    },
    Terminal {
        selector: String,
        mode: String,
    },
    OpenAgent {
        selector: String,
    },
    ResumeAutomation {
        selector: String,
    },
    UsageError(String),
}

#[must_use]
pub fn parse(args: &[String]) -> CellCommand {
    let mut scanner = Scanner::new(&args[1.min(args.len())..]);
    match args.first().map(String::as_str) {
        None | Some("help" | "--help" | "-h") => CellCommand::Help,
        Some("show") => with_selector(scanner, |selector| CellCommand::Show { selector }),
        Some("worktree") => with_selector(scanner, |selector| CellCommand::Worktree { selector }),
        Some("reviews") => with_selector(scanner, |selector| CellCommand::Reviews { selector }),
        Some("set-status") => {
            let rest = scanner.remaining();
            match (rest.first(), rest.get(1)) {
                (Some(selector), Some(state)) => CellCommand::SetStatus {
                    selector: selector.clone(),
                    state: state.clone(),
                },
                _ => CellCommand::UsageError("set-status requires <selector> <state>".to_string()),
            }
        }
        Some("restart") => with_selector(scanner, |selector| CellCommand::Restart { selector }),
        Some("restart-proof") => match scanner.take_parsed::<i64>("--from") {
            Ok(Some(from)) => with_selector(scanner, |selector| CellCommand::RestartProof {
                selector,
                from,
            }),
            Ok(None) => {
                CellCommand::UsageError("restart-proof requires --from <index>".to_string())
            }
            Err(e) => CellCommand::UsageError(e.0),
        },
        Some("edit") => {
            let cwd = scanner.take_value("--cwd").ok().flatten();
            let agent = scanner.take_value("--agent").ok().flatten();
            let model = scanner.take_value("--model").ok().flatten();
            let prompt = scanner.take_value("--prompt").ok().flatten();
            let command = scanner.take_value("--command").ok().flatten();
            with_selector(scanner, |selector| CellCommand::Edit {
                selector,
                cwd,
                agent,
                model,
                prompt,
                command,
            })
        }
        Some("terminal") => {
            let mode = scanner.take_value("--mode").ok().flatten();
            let mode = match mode {
                Some(m) if m == "open" || m == "readonly" => m,
                Some(other) => {
                    return CellCommand::UsageError(format!(
                        "--mode: invalid choice '{other}' (choose from 'open', 'readonly')"
                    ));
                }
                None => "open".to_string(),
            };
            with_selector(scanner, |selector| CellCommand::Terminal { selector, mode })
        }
        Some("open-agent") => {
            with_selector(scanner, |selector| CellCommand::OpenAgent { selector })
        }
        Some("resume-automation") => with_selector(scanner, |selector| {
            CellCommand::ResumeAutomation { selector }
        }),
        Some(other) => CellCommand::UsageError(format!("unknown cell subcommand: {other}")),
    }
}

fn with_selector(scanner: Scanner, make: impl FnOnce(String) -> CellCommand) -> CellCommand {
    match scanner.remaining().into_iter().next() {
        Some(selector) => make(selector),
        None => CellCommand::UsageError("missing required <selector> argument".to_string()),
    }
}

/// Mirrors `_resolve_selector_or_none`'s kind check; see `task.rs`'s twin for
/// why this returns a `Result` instead of printing inline.
pub fn resolve_scoped(
    client: &DaemonClient,
    selector: &str,
    want_kind: &str,
) -> Result<ResolvedSelector, CommandError> {
    let resolved = resolve_squad_selector(client, selector)?;
    if resolved.kind != want_kind {
        return Err(CommandError::Selector(SelectorError(format!(
            "'{selector}' is a {} selector, not a {want_kind}",
            resolved.kind
        ))));
    }
    Ok(resolved)
}

pub fn with_uri(mut payload: Value, uri: String) -> Value {
    if let Value::Object(map) = &mut payload {
        let mut ordered = serde_json::Map::new();
        ordered.insert("uri".to_string(), Value::String(uri));
        ordered.extend(map.clone());
        *map = ordered;
    }
    payload
}

/// Appended to a resumed agent's context in `--mode readonly` (mirrors
/// Python's `_READONLY_RESUME_INSTRUCTIONS`).
const READONLY_RESUME_INSTRUCTIONS: &str = "You are in read-only mode. You may only read files. Do NOT write, \
edit, delete, commit, or push anything.";

/// Builds the local CLI command that resumes `agent_session_id`, ported from
/// Python's `_agent_resume_command` (itself a hand-mirrored duplicate of
/// `daemon/src/server.rs::open_agent_terminal`'s dispatch -- see that
/// function's docstring for why there is no single shared implementation).
pub fn agent_resume_command(
    agent: Option<&str>,
    agent_session_id: &str,
    mode: &str,
) -> Vec<String> {
    let mut cmd: Vec<String>;
    if matches!(agent, Some("codex") | Some("codex-cli")) {
        // The top-level interactive `codex resume`, not `codex exec resume`
        // (Codex's non-interactive headless mode, which requires a prompt
        // argument or piped stdin and fails immediately with "No prompt
        // provided" otherwise -- exactly the reported symptom of resuming
        // this way into an interactive terminal with nothing to pipe in).
        cmd = vec!["codex".to_string()];
        if mode == "readonly" {
            cmd.push("-c".to_string());
            cmd.push(format!(
                "developer_instructions={READONLY_RESUME_INSTRUCTIONS}"
            ));
        }
        cmd.push("resume".to_string());
        cmd.push(agent_session_id.to_string());
    } else if matches!(agent, Some("pi")) {
        cmd = vec![
            "pi".to_string(),
            "--session".to_string(),
            agent_session_id.to_string(),
            "--approve".to_string(),
        ];
        if mode == "readonly" {
            cmd.push("--append-system-prompt".to_string());
            cmd.push(READONLY_RESUME_INSTRUCTIONS.to_string());
        }
    } else {
        cmd = vec![
            "claude".to_string(),
            "--resume".to_string(),
            agent_session_id.to_string(),
        ];
        if mode == "readonly" {
            cmd.push("--dangerously-skip-permissions".to_string());
            cmd.push("--append-system-prompt".to_string());
            cmd.push(READONLY_RESUME_INSTRUCTIONS.to_string());
        }
    }
    cmd
}

#[must_use]
pub fn dispatch(cmd: CellCommand, opts: &GlobalOpts) -> i32 {
    let client = opts.client();
    match cmd {
        CellCommand::Help => {
            println!(
                "{}",
                crate::help_map::command_help(&["cell"]).expect("cell help exists")
            );
            0
        }
        CellCommand::UsageError(m) => {
            println!("usage error: {m}");
            2
        }
        CellCommand::Show { selector } => run_and_report(opts, None, || {
            let resolved = resolve_scoped(&client, &selector, "cell")?;
            let squad = client.squad(&resolved.squad_id)?;
            let uri = squad_view_uri(&squad, &resolved);
            let cell = with_uri(
                squad["tasks"][resolved.task_idx as usize]["cells"][resolved.cell_idx as usize]
                    .clone(),
                uri,
            );
            emit(opts, &cell, render_cell_detail);
            Ok(())
        }),
        CellCommand::Worktree { selector } => run_and_report(opts, None, || {
            let resolved = resolve_scoped(&client, &selector, "cell")?;
            let paths = client.squad_worktrees(&resolved.squad_id)?;
            let paths = paths.as_array().cloned().unwrap_or_default();
            let found = paths.iter().find(|p| {
                p["task_idx"].as_i64() == Some(resolved.task_idx)
                    && p["cell_idx"].as_i64() == Some(resolved.cell_idx)
            });
            let Some(found) = found else {
                return Err(CommandError::Selector(SelectorError(format!(
                    "no worktree recorded for '{selector}'"
                ))));
            };
            let found = found.clone();
            emit(opts, &found, |m| {
                crate::output::print_kv(&[
                    (
                        "worktree",
                        m["worktree"].as_str().unwrap_or("-").to_string(),
                    ),
                    ("project", m["project"].as_str().unwrap_or("-").to_string()),
                ]);
            });
            Ok(())
        }),
        CellCommand::Reviews { selector } => run_and_report(opts, None, || {
            let resolved = resolve_scoped(&client, &selector, "cell")?;
            let squad = client.squad(&resolved.squad_id)?;
            let cell =
                &squad["tasks"][resolved.task_idx as usize]["cells"][resolved.cell_idx as usize];
            let reviews = cell["reviews"].as_array().cloned().unwrap_or_default();
            let reviews_value = Value::Array(reviews);
            emit(opts, &reviews_value, |rs| {
                let rs = rs.as_array().cloned().unwrap_or_default();
                if rs.is_empty() {
                    println!("no reviews");
                    return;
                }
                for r in &rs {
                    let branch = r["branch"]
                        .as_str()
                        .map(|b| format!(" branch={b}"))
                        .unwrap_or_default();
                    println!("{}  {}  {}{branch}", r["id"], r["name"], r["status"]);
                }
            });
            Ok(())
        }),
        CellCommand::SetStatus { selector, state } => run_and_report(opts, None, || {
            let resolved = resolve_scoped(&client, &selector, "cell")?;
            let result = client.set_status(
                &resolved.squad_id,
                &state,
                "cell",
                resolved.task_idx,
                resolved.cell_idx,
                -1,
                "",
            )?;
            emit(opts, &result, |_| println!("{selector} -> {state}"));
            Ok(())
        }),
        CellCommand::Restart { selector } => run_and_report(opts, None, || {
            let resolved = resolve_scoped(&client, &selector, "cell")?;
            let result =
                client.restart_cell(&resolved.squad_id, resolved.task_idx, resolved.cell_idx)?;
            emit(opts, &result, render_dirtied);
            Ok(())
        }),
        CellCommand::RestartProof { selector, from } => run_and_report(opts, None, || {
            let resolved = resolve_scoped(&client, &selector, "cell")?;
            let result = client.restart_cell_proof(
                &resolved.squad_id,
                resolved.task_idx,
                resolved.cell_idx,
                from,
            )?;
            emit(opts, &result, render_dirtied);
            Ok(())
        }),
        CellCommand::Edit {
            selector,
            cwd,
            agent,
            model,
            prompt,
            command,
        } => run_and_report(opts, None, || {
            let resolved = resolve_scoped(&client, &selector, "cell")?;
            let result = client.edit_cell(
                &resolved.squad_id,
                resolved.task_idx,
                resolved.cell_idx,
                cwd.as_deref(),
                agent.as_deref(),
                model.as_deref(),
                prompt.as_deref(),
                command.as_deref(),
            )?;
            emit(opts, &result, |_| println!("{selector} updated"));
            Ok(())
        }),
        CellCommand::Terminal { selector, mode } => run_and_report(opts, None, || {
            let resolved = resolve_scoped(&client, &selector, "cell")?;
            let squad = client.squad(&resolved.squad_id)?;
            let cell = squad["tasks"][resolved.task_idx as usize]["cells"]
                [resolved.cell_idx as usize]
                .clone();
            let agent_session_id = cell["agent_session_id"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            if agent_session_id.is_empty() {
                return Err(CommandError::Selector(SelectorError(format!(
                    "no agent_session_id available for '{selector}' -- the cell may not have completed yet"
                ))));
            }
            emit(opts, &cell, |s| {
                let cmd = agent_resume_command(s["agent"].as_str(), &agent_session_id, &mode);
                crate::output::print_kv(&[
                    ("cwd", s["cwd"].as_str().unwrap_or("-").to_string()),
                    ("command", cmd.join(" ")),
                ]);
            });
            Ok(())
        }),
        CellCommand::OpenAgent { selector } => run_and_report(opts, None, || {
            let resolved = resolve_scoped(&client, &selector, "cell")?;
            let result = client.open_agent_terminal(
                &resolved.squad_id,
                resolved.task_idx,
                resolved.cell_idx,
            )?;
            emit(opts, &result, |_| {
                println!("{selector}: agent terminal opened")
            });
            Ok(())
        }),
        CellCommand::ResumeAutomation { selector } => run_and_report(opts, None, || {
            let resolved = resolve_scoped(&client, &selector, "cell")?;
            let result = client.resume_automation(
                &resolved.squad_id,
                resolved.task_idx,
                resolved.cell_idx,
            )?;
            emit(opts, &result, |_| {
                println!("{selector}: automation resumed")
            });
            Ok(())
        }),
    }
}

fn render_cell_detail(s: &Value) {
    let prompt = s["prompt"].as_str().filter(|p| !p.is_empty());
    let (label, value) = match prompt {
        Some(p) => ("prompt", p.to_string()),
        None => (
            "command",
            s["command"].as_str().unwrap_or_default().to_string(),
        ),
    };
    crate::output::print_kv(&[
        ("uri", s["uri"].as_str().unwrap_or_default().to_string()),
        ("id", s["id"].as_str().unwrap_or_default().to_string()),
        ("name", s["name"].as_str().unwrap_or_default().to_string()),
        ("state", s["state"].as_str().unwrap_or_default().to_string()),
        ("cwd", s["cwd"].as_str().unwrap_or_default().to_string()),
        ("agent", s["agent"].as_str().unwrap_or_default().to_string()),
        ("model", s["model"].as_str().unwrap_or_default().to_string()),
        ("tokens_in", s["tokens_in"].to_string()),
        ("tokens_out", s["tokens_out"].to_string()),
        (label, value),
    ]);
    for (vi, v) in s["proof"].as_array().into_iter().flatten().enumerate() {
        println!("  proof/{vi}  {}  {}", v["kind"], v["state"]);
    }
}

fn render_dirtied(result: &Value) {
    println!("state: {}", result["state"]);
    let dirtied = result["dirtied"].as_array().cloned().unwrap_or_default();
    if !dirtied.is_empty() {
        println!("dirtied downstream squads:");
        for d in &dirtied {
            println!("  {d}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn parses_show_with_selector() {
        match parse(&v(&["show", "squad-1/build/0"])) {
            CellCommand::Show { selector } => assert_eq!(selector, "squad-1/build/0"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parses_worktree() {
        match parse(&v(&["worktree", "squad-1/build/0"])) {
            CellCommand::Worktree { selector } => assert_eq!(selector, "squad-1/build/0"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parses_open_agent_with_selector() {
        match parse(&v(&["open-agent", "squad-1/build/0"])) {
            CellCommand::OpenAgent { selector } => assert_eq!(selector, "squad-1/build/0"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn open_agent_without_selector_is_usage_error() {
        match parse(&v(&["open-agent"])) {
            CellCommand::UsageError(_) => {}
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parses_resume_automation_with_selector() {
        match parse(&v(&["resume-automation", "squad-1/build/0"])) {
            CellCommand::ResumeAutomation { selector } => assert_eq!(selector, "squad-1/build/0"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parses_set_status_positional_pair() {
        match parse(&v(&["set-status", "squad-1/build/0", "done"])) {
            CellCommand::SetStatus { selector, state } => {
                assert_eq!(selector, "squad-1/build/0");
                assert_eq!(state, "done");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parses_restart_proof_with_from_flag() {
        match parse(&v(&["restart-proof", "squad-1/build/0", "--from", "1"])) {
            CellCommand::RestartProof { selector, from } => {
                assert_eq!(selector, "squad-1/build/0");
                assert_eq!(from, 1);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parses_edit_with_optional_flags() {
        match parse(&v(&[
            "edit",
            "squad-1/build/0",
            "--agent",
            "claude",
            "--command",
            "echo hi",
        ])) {
            CellCommand::Edit {
                selector,
                agent,
                command,
                ..
            } => {
                assert_eq!(selector, "squad-1/build/0");
                assert_eq!(agent.as_deref(), Some("claude"));
                assert_eq!(command.as_deref(), Some("echo hi"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parses_terminal_default_mode() {
        match parse(&v(&["terminal", "squad-1/build/0"])) {
            CellCommand::Terminal { selector, mode } => {
                assert_eq!(selector, "squad-1/build/0");
                assert_eq!(mode, "open");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parses_terminal_readonly_mode() {
        match parse(&v(&["terminal", "squad-1/build/0", "--mode", "readonly"])) {
            CellCommand::Terminal { mode, .. } => assert_eq!(mode, "readonly"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn terminal_rejects_invalid_mode() {
        matches!(
            parse(&v(&["terminal", "squad-1/build/0", "--mode", "bogus"])),
            CellCommand::UsageError(_)
        );
    }

    #[test]
    fn agent_resume_command_uses_codex_for_codex_agents() {
        let cmd = agent_resume_command(Some("codex"), "sess-1", "open");
        assert_eq!(cmd, vec!["codex", "resume", "sess-1"]);
    }

    #[test]
    fn agent_resume_command_uses_claude_by_default() {
        let cmd = agent_resume_command(Some("claude"), "sess-1", "open");
        assert_eq!(cmd, vec!["claude", "--resume", "sess-1"]);
    }

    #[test]
    fn agent_resume_command_uses_pi_for_pi_agents() {
        let cmd = agent_resume_command(Some("pi"), "sess-1", "open");
        assert_eq!(cmd, vec!["pi", "--session", "sess-1", "--approve"]);
    }

    #[test]
    fn agent_resume_command_readonly_appends_instructions() {
        let cmd = agent_resume_command(Some("claude"), "sess-1", "readonly");
        assert!(cmd.contains(&"--append-system-prompt".to_string()));
    }

    #[test]
    fn missing_selector_is_usage_error() {
        matches!(parse(&v(&["show"])), CellCommand::UsageError(_));
    }

    #[test]
    fn bare_cell_is_help() {
        matches!(parse(&v(&[])), CellCommand::Help);
    }
}
