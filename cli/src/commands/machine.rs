//! `ralphus machine <subcommand>`, ported from
//! `cli/src/ralphus/__main__.py`'s `machine` group (register/list/get/remove/
//! cleanup). Bare `ralphus machine` behaves like `machine list`
//! (`p_machine.set_defaults(func=_cmd_machine_list)` in the Python source,
//! mirroring bare `ralphus queue`/`ralphus agent`).
//!
//! Note: none of these Python handlers call `emit`/honor `--json` -- they
//! always print plain human-readable text and use their own literal exit
//! codes on a daemon error (1 for `register`/`remove`/`cleanup`, 2 for
//! `list`/`get`), rather than the `run_and_report`/`exit_code_for` convention
//! used elsewhere in this CLI. This is a faithful 1:1 port of that (mildly
//! inconsistent, but deliberate) Python behavior -- see
//! `_cmd_machine_register`/`_cmd_machine_list`/`_cmd_machine_get`/
//! `_cmd_machine_remove`/`_cmd_machine_cleanup`.

use serde_json::Value;

use crate::args::GlobalOpts;
use crate::commands::CommandError;
use crate::flags::{Scanner, UsageError};

#[derive(Debug, Clone)]
pub enum MachineCommand {
    Help,
    Register {
        scheme: String,
        program: String,
        description: String,
        args: Vec<String>,
        channel: bool,
    },
    List,
    Get {
        scheme: String,
    },
    Remove {
        scheme: String,
    },
    Cleanup {
        machine: String,
        project: String,
        branch: Option<String>,
    },
    UsageError(String),
}

#[must_use]
pub fn parse(args: &[String]) -> MachineCommand {
    let mut scanner = Scanner::new(&args[1.min(args.len())..]);
    match args.first().map(String::as_str) {
        None | Some("list") => MachineCommand::List,
        Some("help" | "--help" | "-h") => MachineCommand::Help,
        Some("register") => match parse_register(&mut scanner) {
            Ok(cmd) => cmd,
            Err(e) => MachineCommand::UsageError(e.0),
        },
        Some("get") => with_scheme(scanner, |scheme| MachineCommand::Get { scheme }, "get"),
        Some("remove") => with_scheme(
            scanner,
            |scheme| MachineCommand::Remove { scheme },
            "remove",
        ),
        Some("cleanup") => match parse_cleanup(scanner) {
            Ok(cmd) => cmd,
            Err(e) => MachineCommand::UsageError(e.0),
        },
        Some(other) => MachineCommand::UsageError(format!("unknown machine subcommand: {other}")),
    }
}

fn with_scheme(
    scanner: Scanner,
    make: impl FnOnce(String) -> MachineCommand,
    subcmd: &str,
) -> MachineCommand {
    match scanner.remaining().into_iter().next() {
        Some(scheme) => make(scheme),
        None => MachineCommand::UsageError(format!("{subcmd} requires a <scheme> argument")),
    }
}

fn parse_cleanup(mut scanner: Scanner) -> Result<MachineCommand, UsageError> {
    let project = scanner.take_value("--project")?;
    let branch = scanner.take_value("--branch")?;
    let Some(machine) = scanner.remaining().into_iter().next() else {
        return Err(UsageError(
            "cleanup requires a <machine> argument".to_string(),
        ));
    };
    let Some(project) = project else {
        return Err(UsageError("cleanup requires --project".to_string()));
    };
    Ok(MachineCommand::Cleanup {
        machine,
        project,
        branch,
    })
}

fn parse_register(scanner: &mut Scanner) -> Result<MachineCommand, UsageError> {
    let scheme = scanner.take_value("--scheme")?;
    let program = scanner.take_value("--program")?;
    let description = scanner.take_value("--description")?.unwrap_or_default();
    let args = scanner.take_repeated("--arg")?;
    let channel = scanner.take_bool("--channel");
    let Some(scheme) = scheme else {
        return Err(UsageError("machine register requires --scheme".to_string()));
    };
    let Some(program) = program else {
        return Err(UsageError(
            "machine register requires --program".to_string(),
        ));
    };
    Ok(MachineCommand::Register {
        scheme,
        program,
        description,
        args,
        channel,
    })
}

#[must_use]
pub fn dispatch(cmd: MachineCommand, opts: &GlobalOpts) -> i32 {
    let client = opts.client();
    match cmd {
        MachineCommand::Help => {
            println!(
                "{}",
                crate::help_map::command_help(&["machine"]).expect("machine help exists")
            );
            0
        }
        MachineCommand::UsageError(m) => {
            println!("usage error: {m}");
            2
        }
        MachineCommand::Register {
            scheme,
            program,
            description,
            args,
            channel,
        } => {
            match client.register_machine(
                &scheme,
                &program,
                &description,
                Some(&args),
                None,
                channel,
            ) {
                Ok(_) => {
                    println!("registered machine provider \"{scheme}\" -> {program}");
                    0
                }
                Err(e) => {
                    CommandError::Daemon(e).print(false, None);
                    1
                }
            }
        }
        MachineCommand::List => match client.list_machines() {
            Ok(payload) => {
                render_machine_list(&payload);
                0
            }
            Err(e) => {
                CommandError::Daemon(e).print(false, None);
                2
            }
        },
        MachineCommand::Get { scheme } => match client.get_machine(&scheme) {
            Ok(m) => {
                render_machine_detail(&m);
                0
            }
            Err(e) => {
                CommandError::Daemon(e).print(false, None);
                2
            }
        },
        MachineCommand::Remove { scheme } => match client.deregister_machine(&scheme) {
            Ok(_) => {
                println!("removed machine provider \"{scheme}\"");
                0
            }
            Err(e) => {
                CommandError::Daemon(e).print(false, None);
                1
            }
        },
        MachineCommand::Cleanup {
            machine,
            project,
            branch,
        } => match client.cleanup_machine(&machine, &project, branch.as_deref()) {
            Ok(payload) => {
                let removed = payload["removed"].as_str().unwrap_or("(unknown path)");
                println!("cleaned up workspace \"{machine}\" project={project}: {removed}");
                0
            }
            Err(e) => {
                CommandError::Daemon(e).print(false, None);
                1
            }
        },
    }
}

fn render_machine_list(payload: &Value) {
    let machines = payload["machines"].as_array().cloned().unwrap_or_default();
    for m in &machines {
        println!(
            "{:<20}  v{}  {}",
            m["scheme"].as_str().unwrap_or_default(),
            m["protocol_version"],
            m["program"].as_str().unwrap_or_default()
        );
        if let Some(description) = m["description"].as_str().filter(|d| !d.is_empty()) {
            println!("    {description}");
        }
    }
    let builtin: Vec<&str> = payload["builtin"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    if !builtin.is_empty() {
        println!("built-in (always available): {}", builtin.join(", "));
    } else if machines.is_empty() {
        println!("no registered machine providers");
    }
}

fn render_machine_detail(m: &Value) {
    println!("scheme:      {}", m["scheme"].as_str().unwrap_or_default());
    println!("program:     {}", m["program"].as_str().unwrap_or_default());
    println!("protocol:    v{}", m["protocol_version"]);
    if let Some(args) = m["args"].as_array().filter(|a| !a.is_empty()) {
        let joined = args
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join(" ");
        println!("args:        {joined}");
    }
    if let Some(description) = m["description"].as_str().filter(|d| !d.is_empty()) {
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
    fn bare_machine_behaves_like_list() {
        matches!(parse(&[]), MachineCommand::List);
    }

    #[test]
    fn parses_register_with_required_flags() {
        match parse(&v(&[
            "register",
            "--scheme",
            "ssh",
            "--program",
            "/bin/ssh-provider",
        ])) {
            MachineCommand::Register {
                scheme,
                program,
                description,
                args,
                channel,
            } => {
                assert_eq!(scheme, "ssh");
                assert_eq!(program, "/bin/ssh-provider");
                assert_eq!(description, "");
                assert!(args.is_empty());
                assert!(!channel);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parses_register_repeated_arg_and_channel() {
        match parse(&v(&[
            "register",
            "--scheme",
            "ssh",
            "--program",
            "/bin/x",
            "--arg",
            "a",
            "--arg",
            "b",
            "--channel",
        ])) {
            MachineCommand::Register { args, channel, .. } => {
                assert_eq!(args, v(&["a", "b"]));
                assert!(channel);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn register_requires_scheme_and_program() {
        matches!(
            parse(&v(&["register", "--scheme", "ssh"])),
            MachineCommand::UsageError(_)
        );
        matches!(
            parse(&v(&["register", "--program", "/bin/x"])),
            MachineCommand::UsageError(_)
        );
    }

    #[test]
    fn parses_get_remove_cleanup() {
        match parse(&v(&["get", "ssh"])) {
            MachineCommand::Get { scheme } => assert_eq!(scheme, "ssh"),
            other => panic!("unexpected: {other:?}"),
        }
        match parse(&v(&["remove", "ssh"])) {
            MachineCommand::Remove { scheme } => assert_eq!(scheme, "ssh"),
            other => panic!("unexpected: {other:?}"),
        }
        match parse(&v(&["cleanup", "ssh:host1", "--project", "proj"])) {
            MachineCommand::Cleanup {
                machine,
                project,
                branch,
            } => {
                assert_eq!(machine, "ssh:host1");
                assert_eq!(project, "proj");
                assert_eq!(branch, None);
            }
            other => panic!("unexpected: {other:?}"),
        }
        match parse(&v(&[
            "cleanup",
            "ssh:host1",
            "--project",
            "proj",
            "--branch",
            "feature/x",
        ])) {
            MachineCommand::Cleanup { branch, .. } => {
                assert_eq!(branch.as_deref(), Some("feature/x"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn get_remove_cleanup_require_argument() {
        matches!(parse(&v(&["get"])), MachineCommand::UsageError(_));
        matches!(parse(&v(&["remove"])), MachineCommand::UsageError(_));
        matches!(parse(&v(&["cleanup"])), MachineCommand::UsageError(_));
        matches!(
            parse(&v(&["cleanup", "ssh:host1"])),
            MachineCommand::UsageError(_)
        );
    }

    #[test]
    fn unknown_subcommand_is_usage_error() {
        matches!(parse(&v(&["bogus"])), MachineCommand::UsageError(_));
    }
}
