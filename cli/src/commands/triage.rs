//! `ralphus triage <subcommand>` (RAL-318): register and inspect Triage
//! types -- the categories the daemon's Arbiter subsystem classifies a
//! Triage-opted-in cell into (`[[task.cell]] triage = true`). Mirrors
//! `commands/machine.rs`'s register/list/get/deregister shape.

use serde_json::Value;

use crate::args::GlobalOpts;
use crate::commands::CommandError;
use crate::flags::Scanner;

#[derive(Debug, Clone)]
pub enum TriageCommand {
    Help,
    Type(TriageTypeCommand),
    UsageError(String),
}

#[derive(Debug, Clone)]
pub enum TriageTypeCommand {
    Help,
    Register {
        name: String,
        label: String,
        description: String,
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
pub fn parse(args: &[String]) -> TriageCommand {
    let scanner = Scanner::new(&args[1.min(args.len())..]);
    match args.first().map(String::as_str) {
        None | Some("help" | "--help" | "-h") => TriageCommand::Help,
        Some("type") => TriageCommand::Type(parse_type(&scanner.remaining())),
        Some(other) => TriageCommand::UsageError(format!("unknown triage subcommand: {other}")),
    }
}

fn parse_type(args: &[String]) -> TriageTypeCommand {
    let mut scanner = Scanner::new(&args[1.min(args.len())..]);
    match args.first().map(String::as_str) {
        None | Some("help" | "--help" | "-h") => TriageTypeCommand::Help,
        Some("register") => {
            let label = match scanner.take_value("--label") {
                Ok(v) => v.unwrap_or_default(),
                Err(e) => return TriageTypeCommand::UsageError(e.0),
            };
            let description = match scanner.take_value("--description") {
                Ok(v) => v.unwrap_or_default(),
                Err(e) => return TriageTypeCommand::UsageError(e.0),
            };
            match scanner.remaining().into_iter().next() {
                Some(name) => TriageTypeCommand::Register {
                    name,
                    label,
                    description,
                },
                None => {
                    TriageTypeCommand::UsageError("register requires a <name> argument".to_string())
                }
            }
        }
        Some("list") => TriageTypeCommand::List,
        Some("get") => with_name(scanner, |name| TriageTypeCommand::Get { name }, "get"),
        Some("deregister") => with_name(
            scanner,
            |name| TriageTypeCommand::Deregister { name },
            "deregister",
        ),
        Some(other) => {
            TriageTypeCommand::UsageError(format!("unknown triage type subcommand: {other}"))
        }
    }
}

fn with_name(
    scanner: Scanner,
    make: impl FnOnce(String) -> TriageTypeCommand,
    subcmd: &str,
) -> TriageTypeCommand {
    match scanner.remaining().into_iter().next() {
        Some(name) => make(name),
        None => TriageTypeCommand::UsageError(format!("{subcmd} requires a <name> argument")),
    }
}

#[must_use]
pub fn dispatch(cmd: TriageCommand, opts: &GlobalOpts) -> i32 {
    match cmd {
        TriageCommand::Help => {
            println!(
                "{}",
                crate::help_map::command_help(&["triage"]).expect("triage help exists")
            );
            0
        }
        TriageCommand::UsageError(m) => {
            println!("usage error: {m}");
            2
        }
        TriageCommand::Type(c) => dispatch_type(c, opts),
    }
}

fn dispatch_type(cmd: TriageTypeCommand, opts: &GlobalOpts) -> i32 {
    let client = opts.client();
    match cmd {
        TriageTypeCommand::Help => {
            println!(
                "{}",
                crate::help_map::command_help(&["triage", "type"])
                    .expect("triage type help exists")
            );
            0
        }
        TriageTypeCommand::UsageError(m) => {
            println!("usage error: {m}");
            2
        }
        TriageTypeCommand::Register {
            name,
            label,
            description,
        } => match client.register_triage_type(&name, &label, &description) {
            Ok(_) => {
                println!("registered triage type \"{name}\"");
                0
            }
            Err(e) => {
                CommandError::Daemon(e).print(false, None);
                1
            }
        },
        TriageTypeCommand::List => match client.list_triage_types() {
            Ok(payload) => {
                render_triage_type_list(&payload);
                0
            }
            Err(e) => {
                CommandError::Daemon(e).print(false, None);
                2
            }
        },
        TriageTypeCommand::Get { name } => match client.get_triage_type(&name) {
            Ok(t) => {
                render_triage_type_detail(&t);
                0
            }
            Err(e) => {
                CommandError::Daemon(e).print(false, None);
                2
            }
        },
        TriageTypeCommand::Deregister { name } => match client.deregister_triage_type(&name) {
            Ok(_) => {
                println!("removed triage type \"{name}\"");
                0
            }
            Err(e) => {
                CommandError::Daemon(e).print(false, None);
                1
            }
        },
    }
}

fn render_triage_type_list(payload: &Value) {
    let types = payload["types"].as_array().cloned().unwrap_or_default();
    if types.is_empty() {
        println!("no registered triage types");
        return;
    }
    for t in &types {
        println!(
            "{:<20}  {}",
            t["name"].as_str().unwrap_or_default(),
            t["label"].as_str().unwrap_or_default()
        );
        if let Some(description) = t["description"].as_str().filter(|d| !d.is_empty()) {
            println!("    {description}");
        }
    }
}

fn render_triage_type_detail(t: &Value) {
    println!("name:        {}", t["name"].as_str().unwrap_or_default());
    println!("label:       {}", t["label"].as_str().unwrap_or_default());
    if let Some(description) = t["description"].as_str().filter(|d| !d.is_empty()) {
        println!("description: {description}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn bare_triage_is_help() {
        matches!(parse(&[]), TriageCommand::Help);
    }

    #[test]
    fn parses_type_register_with_flags() {
        match parse(&v(&[
            "type",
            "register",
            "--label",
            "Security",
            "--description",
            "sensitive changes",
            "security",
        ])) {
            TriageCommand::Type(TriageTypeCommand::Register {
                name,
                label,
                description,
            }) => {
                assert_eq!(name, "security");
                assert_eq!(label, "Security");
                assert_eq!(description, "sensitive changes");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn register_requires_name() {
        matches!(
            parse(&v(&["type", "register", "--label", "Security"])),
            TriageCommand::Type(TriageTypeCommand::UsageError(_))
        );
    }

    #[test]
    fn parses_get_and_deregister() {
        match parse(&v(&["type", "get", "security"])) {
            TriageCommand::Type(TriageTypeCommand::Get { name }) => assert_eq!(name, "security"),
            other => panic!("unexpected: {other:?}"),
        }
        match parse(&v(&["type", "deregister", "security"])) {
            TriageCommand::Type(TriageTypeCommand::Deregister { name }) => {
                assert_eq!(name, "security");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn get_and_deregister_require_name() {
        matches!(
            parse(&v(&["type", "get"])),
            TriageCommand::Type(TriageTypeCommand::UsageError(_))
        );
        matches!(
            parse(&v(&["type", "deregister"])),
            TriageCommand::Type(TriageTypeCommand::UsageError(_))
        );
    }

    #[test]
    fn parses_list() {
        matches!(
            parse(&v(&["type", "list"])),
            TriageCommand::Type(TriageTypeCommand::List)
        );
    }

    #[test]
    fn unknown_subcommands_are_usage_errors() {
        matches!(parse(&v(&["bogus"])), TriageCommand::UsageError(_));
        matches!(
            parse(&v(&["type", "bogus"])),
            TriageCommand::Type(TriageTypeCommand::UsageError(_))
        );
    }
}
