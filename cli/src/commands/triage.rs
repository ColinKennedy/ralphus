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
    Pool(TriagePoolCommand),
    Type(TriageTypeCommand),
    UsageError(String),
}

#[derive(Debug, Clone)]
pub enum TriagePoolCommand {
    Help,
    List,
    Threshold {
        project: String,
        triage_type: String,
        /// `None` clears a previously configured threshold.
        threshold: Option<i64>,
        /// `--preview`: compute and print what confirming would drain,
        /// without persisting anything (RAL-421).
        preview: bool,
    },
    UsageError(String),
}

#[derive(Debug, Clone)]
pub enum TriageTypeCommand {
    Help,
    Register {
        name: String,
        label: String,
        description: String,
        /// Project the review upstream applies to; paired with `upstream`.
        project: Option<String>,
        upstream: Option<String>,
    },
    /// Set (or `--clear`) the review upstream for a `(project, type)` pair.
    Upstream {
        project: String,
        triage_type: String,
        upstream: Option<String>,
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
        Some("pool") => TriageCommand::Pool(parse_pool(&scanner.remaining())),
        Some("type") => TriageCommand::Type(parse_type(&scanner.remaining())),
        Some(other) => TriageCommand::UsageError(format!("unknown triage subcommand: {other}")),
    }
}

fn parse_pool(args: &[String]) -> TriagePoolCommand {
    let mut scanner = Scanner::new(&args[1.min(args.len())..]);
    match args.first().map(String::as_str) {
        None | Some("help" | "--help" | "-h") => TriagePoolCommand::Help,
        Some("list") => TriagePoolCommand::List,
        Some("threshold") => {
            let threshold = match scanner.take_parsed::<i64>("--threshold") {
                Ok(v) => v,
                Err(e) => return TriagePoolCommand::UsageError(e.0),
            };
            let clear = scanner.take_bool("--clear");
            if threshold.is_some() && clear {
                return TriagePoolCommand::UsageError(
                    "--threshold and --clear are mutually exclusive".to_string(),
                );
            }
            if threshold.is_none() && !clear {
                return TriagePoolCommand::UsageError(
                    "threshold requires either --threshold <n> or --clear".to_string(),
                );
            }
            // RAL-421: `--preview` turns the command into a non-mutating
            // estimate -- print what confirming would drain and exit 0
            // without touching the store. The CLI itself never prompts;
            // invoking the command WITHOUT `--preview` is the confirmation.
            let preview = scanner.take_bool("--preview");
            let rest = scanner.remaining();
            let mut rest = rest.into_iter();
            let (Some(project), Some(triage_type)) = (rest.next(), rest.next()) else {
                return TriagePoolCommand::UsageError(
                    "threshold requires <project> and <triage_type> arguments".to_string(),
                );
            };
            TriagePoolCommand::Threshold {
                project,
                triage_type,
                threshold,
                preview,
            }
        }
        Some(other) => {
            TriagePoolCommand::UsageError(format!("unknown triage pool subcommand: {other}"))
        }
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
            let project = match scanner.take_value("--project") {
                Ok(v) => v,
                Err(e) => return TriageTypeCommand::UsageError(e.0),
            };
            let upstream = match scanner.take_value("--upstream") {
                Ok(v) => v,
                Err(e) => return TriageTypeCommand::UsageError(e.0),
            };
            if project.is_some() != upstream.is_some() {
                return TriageTypeCommand::UsageError(
                    "--project and --upstream must be given together".to_string(),
                );
            }
            match scanner.remaining().into_iter().next() {
                Some(name) => TriageTypeCommand::Register {
                    name,
                    label,
                    description,
                    project,
                    upstream,
                },
                None => {
                    TriageTypeCommand::UsageError("register requires a <name> argument".to_string())
                }
            }
        }
        Some("upstream") => {
            let upstream = match scanner.take_value("--upstream") {
                Ok(v) => v,
                Err(e) => return TriageTypeCommand::UsageError(e.0),
            };
            let clear = scanner.take_bool("--clear");
            if upstream.is_some() == clear {
                return TriageTypeCommand::UsageError(
                    "upstream requires exactly one of --upstream <branch> or --clear".to_string(),
                );
            }
            let mut rest = scanner.remaining().into_iter();
            let (Some(project), Some(triage_type)) = (rest.next(), rest.next()) else {
                return TriageTypeCommand::UsageError(
                    "upstream requires <project> and <triage_type> arguments".to_string(),
                );
            };
            TriageTypeCommand::Upstream {
                project,
                triage_type,
                upstream,
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
        TriageCommand::Pool(c) => dispatch_pool(c, opts),
        TriageCommand::Type(c) => dispatch_type(c, opts),
    }
}

fn dispatch_pool(cmd: TriagePoolCommand, opts: &GlobalOpts) -> i32 {
    let client = opts.client();
    match cmd {
        TriagePoolCommand::Help => {
            println!(
                "{}",
                crate::help_map::command_help(&["triage", "pool"])
                    .expect("triage pool help exists")
            );
            0
        }
        TriagePoolCommand::UsageError(m) => {
            println!("usage error: {m}");
            2
        }
        TriagePoolCommand::List => match client.list_triage_pools() {
            Ok(payload) => {
                render_triage_pool_list(&payload);
                0
            }
            Err(e) => {
                CommandError::Daemon(e).print(false, None);
                2
            }
        },
        TriagePoolCommand::Threshold {
            project,
            triage_type,
            threshold,
            preview,
        } => {
            if preview {
                return match client.preview_triage_pool_threshold(&project, &triage_type, threshold)
                {
                    Ok(payload) => {
                        print!("{}", render_triage_threshold_preview(&payload));
                        0
                    }
                    Err(e) => {
                        CommandError::Daemon(e).print(false, None);
                        2
                    }
                };
            }
            match client.set_triage_pool_threshold(&project, &triage_type, threshold) {
                Ok(payload) => {
                    match threshold {
                        Some(t) => {
                            let reviews = payload["reviews_created"].as_i64().unwrap_or_default();
                            let drained = payload["cells_drained"].as_i64().unwrap_or_default();
                            let left = payload["cells_left"].as_i64().unwrap_or_default();
                            if reviews > 0 {
                                println!(
                                    "set threshold for ({project}, {triage_type}) to {t}: drained {drained} cell(s) as {reviews} review(s), {left} cell(s) still pooled"
                                );
                            } else {
                                println!(
                                    "set threshold for ({project}, {triage_type}) to {t} ({} pooled cell(s), none eligible yet)",
                                    left
                                );
                            }
                        }
                        None => println!("cleared threshold for ({project}, {triage_type})"),
                    }
                    0
                }
                Err(e) => {
                    CommandError::Daemon(e).print(false, None);
                    1
                }
            }
        }
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
            project,
            upstream,
        } => match client.register_triage_type(
            &name,
            &label,
            &description,
            project.as_deref(),
            upstream.as_deref(),
        ) {
            Ok(_) => {
                println!("registered triage type \"{name}\"");
                0
            }
            Err(e) => {
                CommandError::Daemon(e).print(false, None);
                1
            }
        },
        TriageTypeCommand::Upstream {
            project,
            triage_type,
            upstream,
        } => match client.set_triage_type_upstream(&project, &triage_type, upstream.as_deref()) {
            Ok(_) => {
                match upstream {
                    Some(u) => {
                        println!(
                            "triage type \"{triage_type}\" in {project} now reviews against {u}"
                        );
                    }
                    None => {
                        println!("cleared the review upstream for \"{triage_type}\" in {project}")
                    }
                }
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

fn render_triage_pool_list(payload: &Value) {
    let pools = payload["pools"].as_array().cloned().unwrap_or_default();
    if pools.is_empty() {
        println!("no triage pools");
        return;
    }
    for p in &pools {
        let threshold = p["threshold"]
            .as_i64()
            .map_or_else(|| "none".to_string(), |t| t.to_string());
        println!(
            "{:<24}  {:<16}  count={:<4}  threshold={}",
            p["project"].as_str().unwrap_or_default(),
            p["triage_type"].as_str().unwrap_or_default(),
            p["count"].as_i64().unwrap_or_default(),
            threshold
        );
    }
}

/// Prints the non-mutating preview of a proposed threshold (RAL-421): what
/// confirming would drain right now, in rough terms. The daemon computed the
/// estimate from the pool's current viable count under its store lock;
/// between this call and a later confirm the pool can change, so this is an
/// estimate, never a reservation.
fn render_triage_threshold_preview(payload: &Value) -> String {
    let project = payload["project"].as_str().unwrap_or_default();
    let triage_type = payload["triage_type"].as_str().unwrap_or_default();
    let pooled = payload["pooled"].as_i64().unwrap_or_default();
    if payload["clearing"].as_bool().unwrap_or_default() {
        return format!(
            "clearing the threshold for ({project}, {triage_type}) would not drain anything; {pooled} cell(s) currently pooled"
        );
    }
    let proposed = payload["proposed_threshold"].as_i64().unwrap_or_default();
    let batches = payload["full_batches"].as_i64().unwrap_or_default();
    let drained = payload["cells_drained"].as_i64().unwrap_or_default();
    let left = payload["cells_left"].as_i64().unwrap_or_default();
    if batches == 0 {
        return format!(
            "({project}, {triage_type}): {pooled} cell(s) pooled -- below the proposed threshold {proposed}, so confirming would just record it; no review would be created now"
        );
    }
    format!(
        "({project}, {triage_type}): proposed threshold {proposed} with {pooled} cell(s) currently pooled -- confirming would drain {drained} cell(s) as {batches} review(s), leaving {left} cell(s) pooled. Nothing changed yet."
    )
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
        print_upstreams(t, "    ");
    }
}

fn print_upstreams(t: &Value, indent: &str) {
    for u in t["upstreams"].as_array().into_iter().flatten() {
        println!(
            "{indent}upstream[{}]: {}",
            u["project"].as_str().unwrap_or_default(),
            u["upstream"].as_str().unwrap_or_default()
        );
    }
}

fn render_triage_type_detail(t: &Value) {
    println!("name:        {}", t["name"].as_str().unwrap_or_default());
    println!("label:       {}", t["label"].as_str().unwrap_or_default());
    if let Some(description) = t["description"].as_str().filter(|d| !d.is_empty()) {
        println!("description: {description}");
    }
    print_upstreams(t, "");
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
                project,
                upstream,
            }) => {
                assert_eq!(name, "security");
                assert_eq!(label, "Security");
                assert_eq!(description, "sensitive changes");
                assert!(project.is_none() && upstream.is_none());
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parses_type_register_with_upstream() {
        match parse(&v(&[
            "type",
            "register",
            "--project",
            "web",
            "--upstream",
            "beta",
            "security",
        ])) {
            TriageCommand::Type(TriageTypeCommand::Register {
                project, upstream, ..
            }) => {
                assert_eq!(project.as_deref(), Some("web"));
                assert_eq!(upstream.as_deref(), Some("beta"));
            }
            other => panic!("unexpected: {other:?}"),
        }
        assert!(matches!(
            parse(&v(&["type", "register", "--upstream", "beta", "security"])),
            TriageCommand::Type(TriageTypeCommand::UsageError(_))
        ));
    }

    #[test]
    fn parses_type_upstream_set_and_clear() {
        match parse(&v(&[
            "type",
            "upstream",
            "web",
            "sec",
            "--upstream",
            "beta",
        ])) {
            TriageCommand::Type(TriageTypeCommand::Upstream {
                project,
                triage_type,
                upstream,
            }) => {
                assert_eq!((project.as_str(), triage_type.as_str()), ("web", "sec"));
                assert_eq!(upstream.as_deref(), Some("beta"));
            }
            other => panic!("unexpected: {other:?}"),
        }
        assert!(matches!(
            parse(&v(&["type", "upstream", "web", "sec", "--clear"])),
            TriageCommand::Type(TriageTypeCommand::Upstream { upstream: None, .. })
        ));
        assert!(matches!(
            parse(&v(&["type", "upstream", "web", "sec"])),
            TriageCommand::Type(TriageTypeCommand::UsageError(_))
        ));
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

    #[test]
    fn parses_pool_list() {
        matches!(
            parse(&v(&["pool", "list"])),
            TriageCommand::Pool(TriagePoolCommand::List)
        );
    }

    #[test]
    fn parses_pool_threshold_set() {
        match parse(&v(&[
            "pool",
            "threshold",
            "proj",
            "bug",
            "--threshold",
            "4",
        ])) {
            TriageCommand::Pool(TriagePoolCommand::Threshold {
                project,
                triage_type,
                threshold,
                preview,
            }) => {
                assert_eq!(project, "proj");
                assert_eq!(triage_type, "bug");
                assert_eq!(threshold, Some(4));
                assert!(!preview, "without --preview, the command confirms");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parses_pool_threshold_clear() {
        match parse(&v(&["pool", "threshold", "proj", "bug", "--clear"])) {
            TriageCommand::Pool(TriagePoolCommand::Threshold {
                project,
                triage_type,
                threshold,
                preview,
            }) => {
                assert_eq!(project, "proj");
                assert_eq!(triage_type, "bug");
                assert_eq!(threshold, None);
                assert!(!preview);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    /// RAL-421: `--preview` turns the threshold command into a non-mutating
    /// estimate -- parsed as a flag on the same subcommand, never a new one.
    #[test]
    fn parses_pool_threshold_preview_flag() {
        match parse(&v(&[
            "pool",
            "threshold",
            "proj",
            "bug",
            "--threshold",
            "4",
            "--preview",
        ])) {
            TriageCommand::Pool(TriagePoolCommand::Threshold {
                project,
                triage_type,
                threshold,
                preview,
            }) => {
                assert_eq!(project, "proj");
                assert_eq!(triage_type, "bug");
                assert_eq!(threshold, Some(4));
                assert!(preview);
            }
            other => panic!("unexpected: {other:?}"),
        }
        // `--preview` composes with `--clear` too: "what would clearing do?"
        match parse(&v(&[
            "pool",
            "threshold",
            "proj",
            "bug",
            "--clear",
            "--preview",
        ])) {
            TriageCommand::Pool(TriagePoolCommand::Threshold {
                threshold, preview, ..
            }) => {
                assert_eq!(threshold, None);
                assert!(preview);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn pool_threshold_requires_either_threshold_or_clear() {
        matches!(
            parse(&v(&["pool", "threshold", "proj", "bug"])),
            TriageCommand::Pool(TriagePoolCommand::UsageError(_))
        );
    }

    #[test]
    fn pool_threshold_rejects_both_threshold_and_clear() {
        matches!(
            parse(&v(&[
                "pool",
                "threshold",
                "proj",
                "bug",
                "--threshold",
                "4",
                "--clear"
            ])),
            TriageCommand::Pool(TriagePoolCommand::UsageError(_))
        );
    }

    #[test]
    fn pool_threshold_requires_project_and_type() {
        matches!(
            parse(&v(&["pool", "threshold", "--threshold", "4"])),
            TriageCommand::Pool(TriagePoolCommand::UsageError(_))
        );
    }
}
