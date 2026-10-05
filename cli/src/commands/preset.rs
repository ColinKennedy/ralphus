//! `ralphus preset <subcommand>` (RAL-…): register and inspect presets --
//! named bundles of field defaults an `extends = ["<<ralphus:presets/<name>>>"]`
//! entry stamps into a task's, cell's, or proof step's own unset fields at
//! submit time. Mirrors `commands/triage.rs`'s `type` subcommand's
//! register/list/get/deregister shape.

use serde_json::Value;

use crate::args::GlobalOpts;
use crate::commands::CommandError;
use crate::flags::Scanner;

#[derive(Debug, Clone, PartialEq)]
pub enum PresetCommand {
    Help,
    Register {
        name: String,
        system_prompt: Option<String>,
        system_prompt_position: Option<String>,
        maximum_context: Option<u64>,
        auto_compact_threshold: Option<u64>,
        maximum_tool_output_tokens: Option<u64>,
    },
    List,
    Get {
        name: String,
    },
    Deregister {
        name: String,
    },
    UsageError(String),
}

#[must_use]
pub fn parse(args: &[String]) -> PresetCommand {
    let mut scanner = Scanner::new(&args[1.min(args.len())..]);
    match args.first().map(String::as_str) {
        None | Some("help" | "--help" | "-h") => PresetCommand::Help,
        Some("register") => {
            let system_prompt = match scanner.take_value("--system-prompt") {
                Ok(v) => v,
                Err(e) => return PresetCommand::UsageError(e.0),
            };
            let system_prompt_position = match scanner.take_value("--system-prompt-position") {
                Ok(v) => v,
                Err(e) => return PresetCommand::UsageError(e.0),
            };
            let maximum_context = match scanner.take_parsed::<u64>("--maximum-context") {
                Ok(v) => v,
                Err(e) => return PresetCommand::UsageError(e.0),
            };
            let auto_compact_threshold =
                match scanner.take_parsed::<u64>("--auto-compact-threshold") {
                    Ok(v) => v,
                    Err(e) => return PresetCommand::UsageError(e.0),
                };
            let maximum_tool_output_tokens =
                match scanner.take_parsed::<u64>("--maximum-tool-output-tokens") {
                    Ok(v) => v,
                    Err(e) => return PresetCommand::UsageError(e.0),
                };
            match scanner.remaining().into_iter().next() {
                Some(name) => PresetCommand::Register {
                    name,
                    system_prompt,
                    system_prompt_position,
                    maximum_context,
                    auto_compact_threshold,
                    maximum_tool_output_tokens,
                },
                None => {
                    PresetCommand::UsageError("register requires a <name> argument".to_string())
                }
            }
        }
        Some("list") => PresetCommand::List,
        Some("get") => with_name(scanner, |name| PresetCommand::Get { name }, "get"),
        Some("deregister") => with_name(
            scanner,
            |name| PresetCommand::Deregister { name },
            "deregister",
        ),
        Some(other) => PresetCommand::UsageError(format!("unknown preset subcommand: {other}")),
    }
}

fn with_name(
    scanner: Scanner,
    make: impl FnOnce(String) -> PresetCommand,
    subcmd: &str,
) -> PresetCommand {
    match scanner.remaining().into_iter().next() {
        Some(name) => make(name),
        None => PresetCommand::UsageError(format!("{subcmd} requires a <name> argument")),
    }
}

#[must_use]
pub fn dispatch(cmd: PresetCommand, opts: &GlobalOpts) -> i32 {
    let client = opts.client();
    match cmd {
        PresetCommand::Help => {
            println!(
                "{}",
                crate::help_map::command_help(&["preset"]).expect("preset help exists")
            );
            0
        }
        PresetCommand::UsageError(m) => {
            println!("usage error: {m}");
            2
        }
        PresetCommand::Register {
            name,
            system_prompt,
            system_prompt_position,
            maximum_context,
            auto_compact_threshold,
            maximum_tool_output_tokens,
        } => match client.register_preset(
            &name,
            system_prompt.as_deref(),
            system_prompt_position.as_deref(),
            maximum_context,
            auto_compact_threshold,
            maximum_tool_output_tokens,
        ) {
            Ok(_) => {
                println!("registered preset \"{name}\"");
                0
            }
            Err(e) => {
                CommandError::Daemon(e).print(false, None);
                1
            }
        },
        PresetCommand::List => match client.list_presets() {
            Ok(payload) => {
                render_preset_list(&payload);
                0
            }
            Err(e) => {
                CommandError::Daemon(e).print(false, None);
                2
            }
        },
        PresetCommand::Get { name } => match client.get_preset(&name) {
            Ok(p) => {
                render_preset_detail(&p);
                0
            }
            Err(e) => {
                CommandError::Daemon(e).print(false, None);
                2
            }
        },
        PresetCommand::Deregister { name } => match client.deregister_preset(&name) {
            Ok(_) => {
                println!("removed preset \"{name}\"");
                0
            }
            Err(e) => {
                CommandError::Daemon(e).print(false, None);
                1
            }
        },
    }
}

fn render_preset_list(payload: &Value) {
    let presets = payload["presets"].as_array().cloned().unwrap_or_default();
    if presets.is_empty() {
        println!("no registered presets");
        return;
    }
    for p in &presets {
        println!("{}", p["name"].as_str().unwrap_or_default());
    }
}

fn render_preset_detail(p: &Value) {
    println!("name: {}", p["name"].as_str().unwrap_or_default());
    if let Some(v) = p["system_prompt"].as_str() {
        println!("system_prompt: {v}");
    }
    if let Some(v) = p["system_prompt_position"].as_str() {
        println!("system_prompt_position: {v}");
    }
    if let Some(v) = p["maximum_context"].as_u64() {
        println!("maximum_context: {v}");
    }
    if let Some(v) = p["auto_compact_threshold"].as_u64() {
        println!("auto_compact_threshold: {v}");
    }
    if let Some(v) = p["maximum_tool_output_tokens"].as_u64() {
        println!("maximum_tool_output_tokens: {v}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn bare_preset_is_help() {
        assert_eq!(parse(&[]), PresetCommand::Help);
    }

    #[test]
    fn parses_register_with_flags() {
        match parse(&v(&[
            "register",
            "--system-prompt",
            "do the thing",
            "--system-prompt-position",
            "append",
            "--maximum-context",
            "75000",
            "--auto-compact-threshold",
            "51000",
            "--maximum-tool-output-tokens",
            "8000",
            "easy_task",
        ])) {
            PresetCommand::Register {
                name,
                system_prompt,
                system_prompt_position,
                maximum_context,
                auto_compact_threshold,
                maximum_tool_output_tokens,
            } => {
                assert_eq!(name, "easy_task");
                assert_eq!(system_prompt.as_deref(), Some("do the thing"));
                assert_eq!(system_prompt_position.as_deref(), Some("append"));
                assert_eq!(maximum_context, Some(75_000));
                assert_eq!(auto_compact_threshold, Some(51_000));
                assert_eq!(maximum_tool_output_tokens, Some(8_000));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn register_requires_name() {
        assert!(matches!(
            parse(&v(&["register", "--maximum-context", "1000"])),
            PresetCommand::UsageError(_)
        ));
    }

    #[test]
    fn register_with_no_flags_is_a_name_only_preset() {
        match parse(&v(&["register", "no_git_commit"])) {
            PresetCommand::Register { name, .. } => assert_eq!(name, "no_git_commit"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parses_get_and_deregister() {
        match parse(&v(&["get", "easy_task"])) {
            PresetCommand::Get { name } => assert_eq!(name, "easy_task"),
            other => panic!("unexpected: {other:?}"),
        }
        match parse(&v(&["deregister", "easy_task"])) {
            PresetCommand::Deregister { name } => assert_eq!(name, "easy_task"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn get_and_deregister_require_name() {
        assert!(matches!(parse(&v(&["get"])), PresetCommand::UsageError(_)));
        assert!(matches!(
            parse(&v(&["deregister"])),
            PresetCommand::UsageError(_)
        ));
    }

    #[test]
    fn parses_list() {
        assert_eq!(parse(&v(&["list"])), PresetCommand::List);
    }

    #[test]
    fn unknown_subcommand_is_usage_error() {
        assert!(matches!(
            parse(&v(&["bogus"])),
            PresetCommand::UsageError(_)
        ));
    }
}
