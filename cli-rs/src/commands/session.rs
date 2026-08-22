//! `ralphus session <subcommand>`, ported from `cli/src/ralphus/__main__.py`'s
//! `session` group (show/worktree/reviews/set-status/restart/restart-verify/
//! edit/terminal).

use serde_json::Value;

use crate::args::GlobalOpts;
use crate::client::DaemonClient;
use crate::commands::{CommandError, emit, run_and_report};
use crate::flags::Scanner;
use crate::selector::{ResolvedSelector, SelectorError, resolve_run_selector, run_view_uri};

#[derive(Debug, Clone)]
pub enum SessionCommand {
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
    RestartVerify {
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
    UsageError(String),
}

#[must_use]
pub fn parse(args: &[String]) -> SessionCommand {
    let mut scanner = Scanner::new(&args[1.min(args.len())..]);
    match args.first().map(String::as_str) {
        None => SessionCommand::Help,
        Some("show") => with_selector(scanner, |selector| SessionCommand::Show { selector }),
        Some("worktree") => {
            with_selector(scanner, |selector| SessionCommand::Worktree { selector })
        }
        Some("reviews") => with_selector(scanner, |selector| SessionCommand::Reviews { selector }),
        Some("set-status") => {
            let rest = scanner.remaining();
            match (rest.first(), rest.get(1)) {
                (Some(selector), Some(state)) => SessionCommand::SetStatus {
                    selector: selector.clone(),
                    state: state.clone(),
                },
                _ => {
                    SessionCommand::UsageError("set-status requires <selector> <state>".to_string())
                }
            }
        }
        Some("restart") => with_selector(scanner, |selector| SessionCommand::Restart { selector }),
        Some("restart-verify") => match scanner.take_parsed::<i64>("--from") {
            Ok(Some(from)) => with_selector(scanner, |selector| SessionCommand::RestartVerify {
                selector,
                from,
            }),
            Ok(None) => {
                SessionCommand::UsageError("restart-verify requires --from <index>".to_string())
            }
            Err(e) => SessionCommand::UsageError(e.0),
        },
        Some("edit") => {
            let cwd = scanner.take_value("--cwd").ok().flatten();
            let agent = scanner.take_value("--agent").ok().flatten();
            let model = scanner.take_value("--model").ok().flatten();
            let prompt = scanner.take_value("--prompt").ok().flatten();
            let command = scanner.take_value("--command").ok().flatten();
            with_selector(scanner, |selector| SessionCommand::Edit {
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
                    return SessionCommand::UsageError(format!(
                        "--mode: invalid choice '{other}' (choose from 'open', 'readonly')"
                    ));
                }
                None => "open".to_string(),
            };
            with_selector(scanner, |selector| SessionCommand::Terminal {
                selector,
                mode,
            })
        }
        Some(other) => SessionCommand::UsageError(format!("unknown session subcommand: {other}")),
    }
}

fn with_selector(scanner: Scanner, make: impl FnOnce(String) -> SessionCommand) -> SessionCommand {
    match scanner.remaining().into_iter().next() {
        Some(selector) => make(selector),
        None => SessionCommand::UsageError("missing required <selector> argument".to_string()),
    }
}

/// Mirrors `_resolve_selector_or_none`'s kind check; see `task.rs`'s twin for
/// why this returns a `Result` instead of printing inline.
fn resolve_scoped(
    client: &DaemonClient,
    selector: &str,
    want_kind: &str,
) -> Result<ResolvedSelector, CommandError> {
    let resolved = resolve_run_selector(client, selector)?;
    if resolved.kind != want_kind {
        return Err(CommandError::Selector(SelectorError(format!(
            "'{selector}' is a {} selector, not a {want_kind}",
            resolved.kind
        ))));
    }
    Ok(resolved)
}

fn with_uri(mut payload: Value, uri: String) -> Value {
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
fn agent_resume_command(agent: Option<&str>, agent_session_id: &str, mode: &str) -> Vec<String> {
    let mut cmd: Vec<String>;
    if matches!(agent, Some("codex") | Some("codex-cli")) {
        cmd = vec!["codex".to_string()];
        if mode == "readonly" {
            cmd.push("-c".to_string());
            cmd.push(format!(
                "developer_instructions={READONLY_RESUME_INSTRUCTIONS}"
            ));
        }
        cmd.push("exec".to_string());
        cmd.push("resume".to_string());
        cmd.push(agent_session_id.to_string());
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
pub fn dispatch(cmd: SessionCommand, opts: &GlobalOpts) -> i32 {
    let client = opts.client();
    match cmd {
        SessionCommand::Help => {
            println!(
                "ralphus session <show|worktree|reviews|set-status|restart|restart-verify|edit|terminal>"
            );
            0
        }
        SessionCommand::UsageError(m) => {
            println!("usage error: {m}");
            2
        }
        SessionCommand::Show { selector } => run_and_report(opts, None, || {
            let resolved = resolve_scoped(&client, &selector, "session")?;
            let run = client.run(&resolved.run_id)?;
            let uri = run_view_uri(&run, &resolved);
            let session = with_uri(
                run["tasks"][resolved.task_idx as usize]["sessions"][resolved.session_idx as usize]
                    .clone(),
                uri,
            );
            emit(opts, &session, render_session_detail);
            Ok(())
        }),
        SessionCommand::Worktree { selector } => run_and_report(opts, None, || {
            let resolved = resolve_scoped(&client, &selector, "session")?;
            let paths = client.run_worktrees(&resolved.run_id)?;
            let paths = paths.as_array().cloned().unwrap_or_default();
            let found = paths.iter().find(|p| {
                p["task_idx"].as_i64() == Some(resolved.task_idx)
                    && p["session_idx"].as_i64() == Some(resolved.session_idx)
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
        SessionCommand::Reviews { selector } => run_and_report(opts, None, || {
            let resolved = resolve_scoped(&client, &selector, "session")?;
            let run = client.run(&resolved.run_id)?;
            let session = &run["tasks"][resolved.task_idx as usize]["sessions"]
                [resolved.session_idx as usize];
            let reviews = session["reviews"].as_array().cloned().unwrap_or_default();
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
        SessionCommand::SetStatus { selector, state } => run_and_report(opts, None, || {
            let resolved = resolve_scoped(&client, &selector, "session")?;
            let result = client.set_status(
                &resolved.run_id,
                &state,
                "session",
                resolved.task_idx,
                resolved.session_idx,
                -1,
                "",
            )?;
            emit(opts, &result, |_| println!("{selector} -> {state}"));
            Ok(())
        }),
        SessionCommand::Restart { selector } => run_and_report(opts, None, || {
            let resolved = resolve_scoped(&client, &selector, "session")?;
            let result = client.restart_session(
                &resolved.run_id,
                resolved.task_idx,
                resolved.session_idx,
            )?;
            emit(opts, &result, render_dirtied);
            Ok(())
        }),
        SessionCommand::RestartVerify { selector, from } => run_and_report(opts, None, || {
            let resolved = resolve_scoped(&client, &selector, "session")?;
            let result = client.restart_session_verify(
                &resolved.run_id,
                resolved.task_idx,
                resolved.session_idx,
                from,
            )?;
            emit(opts, &result, render_dirtied);
            Ok(())
        }),
        SessionCommand::Edit {
            selector,
            cwd,
            agent,
            model,
            prompt,
            command,
        } => run_and_report(opts, None, || {
            let resolved = resolve_scoped(&client, &selector, "session")?;
            let result = client.edit_session(
                &resolved.run_id,
                resolved.task_idx,
                resolved.session_idx,
                cwd.as_deref(),
                agent.as_deref(),
                model.as_deref(),
                prompt.as_deref(),
                command.as_deref(),
            )?;
            emit(opts, &result, |_| println!("{selector} updated"));
            Ok(())
        }),
        SessionCommand::Terminal { selector, mode } => run_and_report(opts, None, || {
            let resolved = resolve_scoped(&client, &selector, "session")?;
            let run = client.run(&resolved.run_id)?;
            let session = run["tasks"][resolved.task_idx as usize]["sessions"]
                [resolved.session_idx as usize]
                .clone();
            let agent_session_id = session["agent_session_id"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            if agent_session_id.is_empty() {
                return Err(CommandError::Selector(SelectorError(format!(
                    "no agent_session_id available for '{selector}' -- the session may not have completed yet"
                ))));
            }
            emit(opts, &session, |s| {
                let cmd = agent_resume_command(s["agent"].as_str(), &agent_session_id, &mode);
                crate::output::print_kv(&[
                    ("cwd", s["cwd"].as_str().unwrap_or("-").to_string()),
                    ("command", cmd.join(" ")),
                ]);
            });
            Ok(())
        }),
    }
}

fn render_session_detail(s: &Value) {
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
    for (vi, v) in s["verify"].as_array().into_iter().flatten().enumerate() {
        println!("  verify/{vi}  {}  {}", v["kind"], v["state"]);
    }
}

fn render_dirtied(result: &Value) {
    println!("state: {}", result["state"]);
    let dirtied = result["dirtied"].as_array().cloned().unwrap_or_default();
    if !dirtied.is_empty() {
        println!("dirtied downstream runs:");
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
        match parse(&v(&["show", "run-1/build/0"])) {
            SessionCommand::Show { selector } => assert_eq!(selector, "run-1/build/0"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parses_worktree() {
        match parse(&v(&["worktree", "run-1/build/0"])) {
            SessionCommand::Worktree { selector } => assert_eq!(selector, "run-1/build/0"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parses_set_status_positional_pair() {
        match parse(&v(&["set-status", "run-1/build/0", "done"])) {
            SessionCommand::SetStatus { selector, state } => {
                assert_eq!(selector, "run-1/build/0");
                assert_eq!(state, "done");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parses_restart_verify_with_from_flag() {
        match parse(&v(&["restart-verify", "run-1/build/0", "--from", "1"])) {
            SessionCommand::RestartVerify { selector, from } => {
                assert_eq!(selector, "run-1/build/0");
                assert_eq!(from, 1);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parses_edit_with_optional_flags() {
        match parse(&v(&[
            "edit",
            "run-1/build/0",
            "--agent",
            "claude",
            "--command",
            "echo hi",
        ])) {
            SessionCommand::Edit {
                selector,
                agent,
                command,
                ..
            } => {
                assert_eq!(selector, "run-1/build/0");
                assert_eq!(agent.as_deref(), Some("claude"));
                assert_eq!(command.as_deref(), Some("echo hi"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parses_terminal_default_mode() {
        match parse(&v(&["terminal", "run-1/build/0"])) {
            SessionCommand::Terminal { selector, mode } => {
                assert_eq!(selector, "run-1/build/0");
                assert_eq!(mode, "open");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parses_terminal_readonly_mode() {
        match parse(&v(&["terminal", "run-1/build/0", "--mode", "readonly"])) {
            SessionCommand::Terminal { mode, .. } => assert_eq!(mode, "readonly"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn terminal_rejects_invalid_mode() {
        matches!(
            parse(&v(&["terminal", "run-1/build/0", "--mode", "bogus"])),
            SessionCommand::UsageError(_)
        );
    }

    #[test]
    fn agent_resume_command_uses_codex_for_codex_agents() {
        let cmd = agent_resume_command(Some("codex"), "sess-1", "open");
        assert_eq!(cmd, vec!["codex", "exec", "resume", "sess-1"]);
    }

    #[test]
    fn agent_resume_command_uses_claude_by_default() {
        let cmd = agent_resume_command(Some("claude"), "sess-1", "open");
        assert_eq!(cmd, vec!["claude", "--resume", "sess-1"]);
    }

    #[test]
    fn agent_resume_command_readonly_appends_instructions() {
        let cmd = agent_resume_command(Some("claude"), "sess-1", "readonly");
        assert!(cmd.contains(&"--append-system-prompt".to_string()));
    }

    #[test]
    fn missing_selector_is_usage_error() {
        matches!(parse(&v(&["show"])), SessionCommand::UsageError(_));
    }

    #[test]
    fn bare_session_is_help() {
        matches!(parse(&v(&[])), SessionCommand::Help);
    }
}
