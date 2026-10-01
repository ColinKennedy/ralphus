//! `ralphus waypoint ...` -- RAL-400 cross-squad waypoints: create/list/get/
//! close/reopen a waypoint, manage its affected and bearing feed. A thin HTTP
//! client over `DaemonClient`'s `waypoint_*` methods, following the same
//! parse/dispatch/render shape as `review.rs`'s nested subcommands
//! (`upstream`/`pr`/`branch`).

use serde_json::{Value, json};

use crate::args::GlobalOpts;
use crate::flags::Scanner;

use super::{CommandError, emit, run_and_report};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WaypointCommand {
    Help,
    Create {
        prompt: String,
        label: Option<String>,
        agent: Option<String>,
        model: Option<String>,
        allow_advisory: bool,
        affected: Vec<String>,
    },
    List {
        project: Option<String>,
        state: Option<String>,
    },
    Get {
        waypoint_id: String,
    },
    Close {
        waypoint_id: String,
    },
    Reopen {
        waypoint_id: String,
    },
    Redo {
        waypoint_id: String,
        entry_id: String,
    },
    Edit {
        waypoint_id: String,
        label: Option<String>,
        prompt: Option<String>,
        agent: Option<String>,
        model: Option<String>,
        allow_advisory: Option<bool>,
        resurvey: bool,
    },
    ResurveyPreview {
        waypoint_id: String,
    },
    RosterAdd {
        waypoint_id: String,
        entry_id: String,
        note: Option<String>,
    },
    RosterRemove {
        waypoint_id: String,
        entry_id: String,
    },
    Affected(WaypointAffectedCommand),
    Bearing(WaypointBearingCommand),
    Bearings {
        waypoint_id: String,
    },
    Deliveries {
        waypoint_id: String,
    },
    UsageError(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WaypointAffectedCommand {
    Help,
    Add {
        waypoint_id: String,
        kind: String,
        entry_id: String,
        mode: Option<String>,
    },
    Remove {
        waypoint_id: String,
        entry_id: String,
    },
    Mode {
        waypoint_id: String,
        entry_id: String,
        mode: String,
    },
    UsageError(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WaypointBearingCommand {
    Help,
    Add {
        waypoint_id: String,
        producer_kind: String,
        producer_id: String,
        summary: String,
        entity_uri: Option<String>,
        commit_id: Option<String>,
        commit_summary: Option<String>,
    },
    UsageError(String),
}

#[must_use]
pub fn parse(args: &[String]) -> WaypointCommand {
    let mut scanner = Scanner::new(&args[1.min(args.len())..]);
    match args.first().map(String::as_str) {
        None | Some("help" | "--help" | "-h") => WaypointCommand::Help,
        Some("create") => {
            let label = scanner.take_value("--label").ok().flatten();
            let prompt = scanner.take_value("--prompt").ok().flatten();
            let agent = scanner.take_value("--agent").ok().flatten();
            let model = scanner.take_value("--model").ok().flatten();
            let allow_advisory = scanner.take_bool("--allow-advisory");
            let affected = scanner.take_repeated("--affected").unwrap_or_default();
            match prompt {
                Some(prompt) => WaypointCommand::Create {
                    prompt,
                    label,
                    agent,
                    model,
                    allow_advisory,
                    affected,
                },
                None => WaypointCommand::UsageError(
                    "create requires --prompt <text> and at least one --affected kind:entry_id[:mode]"
                        .to_string(),
                ),
            }
        }
        Some("list") => {
            let project = scanner.take_value("--project").ok().flatten();
            let state = scanner.take_value("--state").ok().flatten();
            WaypointCommand::List { project, state }
        }
        Some("get") => {
            with_waypoint_id(scanner, |waypoint_id| WaypointCommand::Get { waypoint_id })
        }
        Some("close") => with_waypoint_id(scanner, |waypoint_id| WaypointCommand::Close {
            waypoint_id,
        }),
        Some("reopen") => with_waypoint_id(scanner, |waypoint_id| WaypointCommand::Reopen {
            waypoint_id,
        }),
        Some("resurvey-preview") => match scanner.remaining().first() {
            Some(waypoint_id) => WaypointCommand::ResurveyPreview {
                waypoint_id: waypoint_id.clone(),
            },
            None => {
                WaypointCommand::UsageError("resurvey-preview requires <waypoint_id>".to_string())
            }
        },
        Some("edit") => {
            let label = scanner.take_value("--label").ok().flatten();
            let prompt = scanner.take_value("--prompt").ok().flatten();
            let agent = scanner.take_value("--agent").ok().flatten();
            let model = scanner.take_value("--model").ok().flatten();
            let resurvey = scanner.take_bool("--resurvey");
            let allow_advisory = if scanner.take_bool("--allow-advisory") {
                Some(true)
            } else if scanner.take_bool("--no-allow-advisory") {
                Some(false)
            } else {
                None
            };
            match scanner.remaining().first() {
                Some(waypoint_id) => WaypointCommand::Edit {
                    waypoint_id: waypoint_id.clone(),
                    label,
                    prompt,
                    agent,
                    model,
                    allow_advisory,
                    resurvey,
                },
                None => WaypointCommand::UsageError("edit requires <waypoint_id>".to_string()),
            }
        }
        Some("redo") => {
            let rest = scanner.remaining();
            match (rest.first(), rest.get(1)) {
                (Some(waypoint_id), Some(entry_id)) => WaypointCommand::Redo {
                    waypoint_id: waypoint_id.clone(),
                    entry_id: entry_id.clone(),
                },
                _ => WaypointCommand::UsageError(
                    "redo requires <waypoint_id> <entry_id>".to_string(),
                ),
            }
        }
        Some("roster") => {
            let rest = scanner.remaining();
            let sub = rest.first().map(String::as_str);
            let mut inner = Scanner::new(&rest[1.min(rest.len())..]);
            match sub {
                Some("add") => {
                    let note = inner.take_value("--note").ok().flatten();
                    let args = inner.remaining();
                    match (args.first(), args.get(1)) {
                        (Some(waypoint_id), Some(entry_id)) => WaypointCommand::RosterAdd {
                            waypoint_id: waypoint_id.clone(),
                            entry_id: entry_id.clone(),
                            note,
                        },
                        _ => WaypointCommand::UsageError(
                            "roster add requires <waypoint_id> <entry_id>".to_string(),
                        ),
                    }
                }
                Some("remove") => {
                    let args = inner.remaining();
                    match (args.first(), args.get(1)) {
                        (Some(waypoint_id), Some(entry_id)) => WaypointCommand::RosterRemove {
                            waypoint_id: waypoint_id.clone(),
                            entry_id: entry_id.clone(),
                        },
                        _ => WaypointCommand::UsageError(
                            "roster remove requires <waypoint_id> <entry_id>".to_string(),
                        ),
                    }
                }
                other => WaypointCommand::UsageError(format!(
                    "unknown roster subcommand: {}",
                    other.unwrap_or("(none)")
                )),
            }
        }
        Some("affected") => WaypointCommand::Affected(parse_affected(&scanner.remaining())),
        Some("bearing") => WaypointCommand::Bearing(parse_bearing(&scanner.remaining())),
        Some("bearings") => with_waypoint_id(scanner, |waypoint_id| WaypointCommand::Bearings {
            waypoint_id,
        }),
        Some("deliveries") => with_waypoint_id(scanner, |waypoint_id| {
            WaypointCommand::Deliveries { waypoint_id }
        }),
        Some(other) => WaypointCommand::UsageError(format!("unknown waypoint subcommand: {other}")),
    }
}

fn with_waypoint_id(
    scanner: Scanner,
    make: impl FnOnce(String) -> WaypointCommand,
) -> WaypointCommand {
    match scanner.remaining().into_iter().next() {
        Some(waypoint_id) => make(waypoint_id),
        None => WaypointCommand::UsageError("missing required <waypoint_id> argument".to_string()),
    }
}

fn parse_affected(args: &[String]) -> WaypointAffectedCommand {
    let mut scanner = Scanner::new(&args[1.min(args.len())..]);
    match args.first().map(String::as_str) {
        None | Some("help" | "--help" | "-h") => WaypointAffectedCommand::Help,
        Some("add") => {
            let mode = scanner.take_value("--mode").ok().flatten();
            let rest = scanner.remaining();
            match (rest.first(), rest.get(1), rest.get(2)) {
                (Some(waypoint_id), Some(kind), Some(entry_id)) => WaypointAffectedCommand::Add {
                    waypoint_id: waypoint_id.clone(),
                    kind: kind.clone(),
                    entry_id: entry_id.clone(),
                    mode,
                },
                _ => WaypointAffectedCommand::UsageError(
                    "affected add requires <waypoint_id> <kind> <entry_id>".to_string(),
                ),
            }
        }
        Some("remove") => {
            let rest = scanner.remaining();
            match (rest.first(), rest.get(1)) {
                (Some(waypoint_id), Some(entry_id)) => WaypointAffectedCommand::Remove {
                    waypoint_id: waypoint_id.clone(),
                    entry_id: entry_id.clone(),
                },
                _ => WaypointAffectedCommand::UsageError(
                    "affected remove requires <waypoint_id> <entry_id>".to_string(),
                ),
            }
        }
        Some("mode") => {
            let rest = scanner.remaining();
            match (rest.first(), rest.get(1), rest.get(2)) {
                (Some(waypoint_id), Some(entry_id), Some(mode)) => WaypointAffectedCommand::Mode {
                    waypoint_id: waypoint_id.clone(),
                    entry_id: entry_id.clone(),
                    mode: mode.clone(),
                },
                _ => WaypointAffectedCommand::UsageError(
                    "affected mode requires <waypoint_id> <entry_id> <mode>".to_string(),
                ),
            }
        }
        Some(other) => WaypointAffectedCommand::UsageError(format!(
            "unknown waypoint affected subcommand: {other}"
        )),
    }
}

fn parse_bearing(args: &[String]) -> WaypointBearingCommand {
    let mut scanner = Scanner::new(&args[1.min(args.len())..]);
    match args.first().map(String::as_str) {
        None | Some("help" | "--help" | "-h") => WaypointBearingCommand::Help,
        Some("add") => {
            let summary = scanner.take_value("--summary").ok().flatten();
            let entity_uri = scanner.take_value("--entity-uri").ok().flatten();
            let commit_id = scanner.take_value("--commit-id").ok().flatten();
            let commit_summary = scanner.take_value("--commit-summary").ok().flatten();
            let rest = scanner.remaining();
            match (rest.first(), rest.get(1), rest.get(2), summary) {
                (Some(waypoint_id), Some(producer_kind), Some(producer_id), Some(summary)) => {
                    WaypointBearingCommand::Add {
                        waypoint_id: waypoint_id.clone(),
                        producer_kind: producer_kind.clone(),
                        producer_id: producer_id.clone(),
                        summary,
                        entity_uri,
                        commit_id,
                        commit_summary,
                    }
                }
                _ => WaypointBearingCommand::UsageError(
                    "bearing add requires <waypoint_id> <producer_kind> <producer_id> --summary <text>"
                        .to_string(),
                ),
            }
        }
        Some(other) => WaypointBearingCommand::UsageError(format!(
            "unknown waypoint bearing subcommand: {other}"
        )),
    }
}

/// Parse one `--affected kind:entry_id[:mode]` flag value into the JSON body
/// shape `client.waypoint_create` expects.
pub fn parse_affected_spec(spec: &str) -> Result<Value, CommandError> {
    let mut parts = spec.splitn(3, ':');
    let kind = parts.next().filter(|s| !s.is_empty());
    let entry_id = parts.next().filter(|s| !s.is_empty());
    let mode = parts.next();
    match (kind, entry_id) {
        (Some(kind), Some(entry_id)) => {
            let mut entry = json!({"kind": kind, "entry_id": entry_id});
            if let Some(mode) = mode {
                entry["mode"] = Value::String(mode.to_string());
            }
            Ok(entry)
        }
        _ => Err(CommandError::Usage(format!(
            "invalid --affected spec {spec:?}: expected kind:entry_id[:mode]"
        ))),
    }
}

pub fn dispatch(cmd: WaypointCommand, opts: &GlobalOpts) -> i32 {
    let client = opts.client();
    match cmd {
        WaypointCommand::Help => {
            println!(
                "{}",
                crate::help_map::command_help(&["waypoint"]).expect("waypoint help exists")
            );
            0
        }
        WaypointCommand::UsageError(m) => {
            println!("usage error: {m}");
            2
        }
        WaypointCommand::Create {
            prompt,
            label,
            agent,
            model,
            allow_advisory,
            affected,
        } => run_and_report(opts, None, || {
            let mut entries = Vec::with_capacity(affected.len());
            for spec in &affected {
                entries.push(parse_affected_spec(spec)?);
            }
            let created = client.waypoint_create(
                &prompt,
                label.as_deref(),
                agent.as_deref(),
                model.as_deref(),
                allow_advisory,
                &entries,
            )?;
            // `POST /api/waypoints` replies `201 {"id": ...}`, not a waypoint
            // detail, so rendering the reply directly printed a detail view
            // with every field blank and "no affected entries". Read the
            // just-created waypoint back so both the human and `--json`
            // output match every other `waypoint` subcommand. If that read
            // fails the create still succeeded, so fall back to the id reply
            // rather than reporting an error.
            let result = created
                .get("id")
                .and_then(Value::as_str)
                .and_then(|id| client.waypoint_get(id).ok())
                .unwrap_or(created);
            emit(opts, &result, render_waypoint_detail);
            Ok(())
        }),
        WaypointCommand::List { project, state } => run_and_report(opts, None, || {
            let result = client.waypoint_list(project.as_deref(), state.as_deref())?;
            emit(opts, &result, render_waypoint_list);
            Ok(())
        }),
        WaypointCommand::Get { waypoint_id } => run_and_report(opts, None, || {
            let result = client.waypoint_get(&waypoint_id)?;
            emit(opts, &result, render_waypoint_detail);
            Ok(())
        }),
        WaypointCommand::Close { waypoint_id } => run_and_report(opts, None, || {
            let result = client.waypoint_close(&waypoint_id)?;
            emit(opts, &result, render_waypoint_detail);
            Ok(())
        }),
        WaypointCommand::RosterAdd {
            waypoint_id,
            entry_id,
            note,
        } => run_and_report(opts, None, || {
            // Kind is inferred from the id's prefix, the same way the affected
            // remove path does it -- an id already says which it is.
            let kind = if entry_id.starts_with("guardian-") {
                "review"
            } else {
                "squad"
            };
            let result =
                client.waypoint_add_roster_entry(&waypoint_id, kind, &entry_id, note.as_deref())?;
            emit(opts, &result, render_waypoint_detail);
            Ok(())
        }),
        WaypointCommand::RosterRemove {
            waypoint_id,
            entry_id,
        } => run_and_report(opts, None, || {
            let result = client.waypoint_remove_roster_entry(&waypoint_id, &entry_id)?;
            emit(opts, &result, render_waypoint_detail);
            Ok(())
        }),
        WaypointCommand::ResurveyPreview { waypoint_id } => run_and_report(opts, None, || {
            let result = client.waypoint_resurvey_preview(&waypoint_id)?;
            emit(opts, &result, render_resurvey_preview);
            Ok(())
        }),
        WaypointCommand::Edit {
            waypoint_id,
            label,
            prompt,
            agent,
            model,
            allow_advisory,
            resurvey,
        } => run_and_report(opts, None, || {
            // Settings are a whole-object PUT-shaped update server-side, so an
            // omitted flag has to mean "keep what is there" rather than
            // "clear it" -- read the current values and overlay the flags that
            // were actually passed.
            let current = client.waypoint_get(&waypoint_id)?;
            let field = |name: &str| -> Option<String> {
                current
                    .get(name)
                    .and_then(Value::as_str)
                    .map(str::to_string)
            };
            let merged_prompt = prompt
                .clone()
                .or_else(|| field("prompt"))
                .unwrap_or_default();
            let merged_allow = allow_advisory.unwrap_or_else(|| {
                current
                    .get("allow_advisory")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
            });
            let result = client.waypoint_update(
                &waypoint_id,
                label.clone().or_else(|| field("label")).as_deref(),
                &merged_prompt,
                agent.clone().or_else(|| field("agent")).as_deref(),
                model.clone().or_else(|| field("model")).as_deref(),
                merged_allow,
                resurvey,
            )?;
            emit(opts, &result, render_waypoint_detail);
            Ok(())
        }),
        WaypointCommand::Redo {
            waypoint_id,
            entry_id,
        } => run_and_report(opts, None, || {
            let result = client.waypoint_redo_affected_entry(&waypoint_id, &entry_id)?;
            emit(opts, &result, render_waypoint_detail);
            Ok(())
        }),
        WaypointCommand::Reopen { waypoint_id } => run_and_report(opts, None, || {
            let result = client.waypoint_reopen(&waypoint_id)?;
            emit(opts, &result, render_waypoint_detail);
            Ok(())
        }),
        WaypointCommand::Affected(c) => dispatch_affected(c, opts, &client),
        WaypointCommand::Bearing(c) => dispatch_bearing(c, opts, &client),
        WaypointCommand::Bearings { waypoint_id } => run_and_report(opts, None, || {
            let result = client.waypoint_list_bearings(&waypoint_id)?;
            emit(opts, &result, render_bearings);
            Ok(())
        }),
        WaypointCommand::Deliveries { waypoint_id } => run_and_report(opts, None, || {
            let result = client.waypoint_deliveries(&waypoint_id)?;
            emit(opts, &result, render_deliveries);
            Ok(())
        }),
    }
}

fn dispatch_affected(
    cmd: WaypointAffectedCommand,
    opts: &GlobalOpts,
    client: &crate::client::DaemonClient,
) -> i32 {
    match cmd {
        WaypointAffectedCommand::Help => {
            println!(
                "{}",
                crate::help_map::command_help(&["waypoint", "affected"])
                    .expect("waypoint affected help exists")
            );
            0
        }
        WaypointAffectedCommand::UsageError(m) => {
            println!("usage error: {m}");
            2
        }
        WaypointAffectedCommand::Add {
            waypoint_id,
            kind,
            entry_id,
            mode,
        } => run_and_report(opts, None, || {
            let result = client.waypoint_add_affected_entry(
                &waypoint_id,
                &kind,
                &entry_id,
                mode.as_deref(),
            )?;
            emit(opts, &result, render_waypoint_detail);
            Ok(())
        }),
        WaypointAffectedCommand::Remove {
            waypoint_id,
            entry_id,
        } => run_and_report(opts, None, || {
            let result = client.waypoint_remove_affected_entry(&waypoint_id, &entry_id)?;
            emit(opts, &result, render_waypoint_detail);
            Ok(())
        }),
        WaypointAffectedCommand::Mode {
            waypoint_id,
            entry_id,
            mode,
        } => run_and_report(opts, None, || {
            let result = client.waypoint_patch_affected_entry(&waypoint_id, &entry_id, &mode)?;
            emit(opts, &result, render_waypoint_detail);
            Ok(())
        }),
    }
}

fn dispatch_bearing(
    cmd: WaypointBearingCommand,
    opts: &GlobalOpts,
    client: &crate::client::DaemonClient,
) -> i32 {
    match cmd {
        WaypointBearingCommand::Help => {
            println!(
                "{}",
                crate::help_map::command_help(&["waypoint", "bearing"])
                    .expect("waypoint bearing help exists")
            );
            0
        }
        WaypointBearingCommand::UsageError(m) => {
            println!("usage error: {m}");
            2
        }
        WaypointBearingCommand::Add {
            waypoint_id,
            producer_kind,
            producer_id,
            summary,
            entity_uri,
            commit_id,
            commit_summary,
        } => run_and_report(opts, None, || {
            let result = client.waypoint_append_bearing(
                &waypoint_id,
                &producer_kind,
                &producer_id,
                &summary,
                entity_uri.as_deref(),
                commit_id.as_deref(),
                commit_summary.as_deref(),
            )?;
            emit(opts, &result, |b| {
                crate::output::print_kv(&[
                    ("id", b["id"].to_string()),
                    (
                        "producer",
                        format!(
                            "{} {}",
                            b["producer_kind"].as_str().unwrap_or_default(),
                            b["producer_id"].as_str().unwrap_or_default()
                        ),
                    ),
                    (
                        "summary",
                        b["summary"].as_str().unwrap_or_default().to_string(),
                    ),
                ]);
            });
            Ok(())
        }),
    }
}

fn render_waypoint_list(waypoints: &Value) {
    let list = waypoints.as_array().cloned().unwrap_or_default();
    if list.is_empty() {
        println!("no waypoints");
        return;
    }
    let rows: Vec<Vec<String>> = list
        .iter()
        .map(|w| {
            vec![
                w["id"].as_str().unwrap_or_default().to_string(),
                w["label"].as_str().unwrap_or_default().to_string(),
                w["state"].as_str().unwrap_or_default().to_string(),
                w["affected_count"].to_string(),
                w["projects"]
                    .as_array()
                    .map(|ps| {
                        ps.iter()
                            .filter_map(|p| p.as_str())
                            .collect::<Vec<_>>()
                            .join(",")
                    })
                    .unwrap_or_default(),
            ]
        })
        .collect();
    crate::output::print_table(&["ID", "LABEL", "STATE", "ROSTER", "PROJECTS"], &rows);
}

/// Renders a `GET /api/waypoints/{id}/resurvey-preview` body: what a
/// `--resurvey` edit would re-judge, and what it would leave alone.
fn render_resurvey_preview(v: &Value) {
    let rows = |key: &str| -> Vec<&Value> {
        v.get(key)
            .and_then(Value::as_array)
            .map(|a| a.iter().collect())
            .unwrap_or_default()
    };
    let line = |e: &Value| {
        let kind = e.get("kind").and_then(Value::as_str).unwrap_or("?");
        let id = e.get("entry_id").and_then(Value::as_str).unwrap_or("?");
        let label = e
            .get("label")
            .and_then(Value::as_str)
            .map(|l| format!(" \"{l}\""))
            .unwrap_or_default();
        let verdict = e
            .get("current_verdict")
            .and_then(Value::as_str)
            .unwrap_or("not yet judged");
        let held = e
            .get("will_be_held_until_judged")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        println!(
            "  {kind} {id}{label} -- now {verdict}{}",
            if held { "; held until re-judged" } else { "" }
        );
    };
    let targets = rows("targets");
    if targets.is_empty() {
        println!("Nothing would be re-judged: this waypoint has no daemon-enrolled entries.");
    } else {
        println!(
            "Would re-judge {} entr{}:",
            targets.len(),
            if targets.len() == 1 { "y" } else { "ies" }
        );
        for e in &targets {
            line(e);
        }
    }
    let explicit = rows("held_explicit");
    if !explicit.is_empty() {
        println!(
            "Left alone ({} human-declared, never second-guessed by the survey):",
            explicit.len()
        );
        for e in &explicit {
            line(e);
        }
    }
}

fn render_waypoint_detail(w: &Value) {
    let mut rows: Vec<(&str, String)> = Vec::new();
    rows.push(("id", w["id"].as_str().unwrap_or_default().to_string()));
    if let Some(label) = w["label"].as_str() {
        rows.push(("label", label.to_string()));
    }
    rows.push(("state", w["state"].as_str().unwrap_or_default().to_string()));
    rows.push((
        "prompt",
        w["prompt"].as_str().unwrap_or_default().to_string(),
    ));
    if let Some(agent) = w["agent"].as_str() {
        rows.push(("agent", agent.to_string()));
    }
    if let Some(model) = w["model"].as_str() {
        rows.push(("model", model.to_string()));
    }
    rows.push((
        "allow_advisory",
        w["allow_advisory"].as_bool().unwrap_or(false).to_string(),
    ));
    rows.push((
        "projects",
        w["projects"]
            .as_array()
            .map(|ps| {
                ps.iter()
                    .filter_map(|p| p.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default(),
    ));
    crate::output::print_kv(&rows);

    let affected = w["affected"].as_array().cloned().unwrap_or_default();
    if affected.is_empty() {
        println!("\nno affected entries");
    } else {
        println!("\naffected:");
        let affected_rows: Vec<Vec<String>> = affected
            .iter()
            .map(|r| {
                vec![
                    r["kind"].as_str().unwrap_or_default().to_string(),
                    r["entry_id"].as_str().unwrap_or_default().to_string(),
                    r["mode"].as_str().unwrap_or_default().to_string(),
                    r["delivery_status"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string(),
                ]
            })
            .collect();
        crate::output::print_table(
            &["KIND", "ENTRY_ID", "MODE", "DELIVERY_STATUS"],
            &affected_rows,
        );
    }

    let summary = &w["delivery_summary"];
    if summary.is_object() {
        println!(
            "\ndeliveries: {} undelivered, {} delivered, {} via_restack, {} failed",
            summary["undelivered"], summary["delivered"], summary["via_restack"], summary["failed"]
        );
    }
}

fn render_bearings(bearings: &Value) {
    let list = bearings.as_array().cloned().unwrap_or_default();
    if list.is_empty() {
        println!("no bearings");
        return;
    }
    for b in &list {
        println!(
            "[{}] {} {}: {}",
            b["created_at_ms"],
            b["producer_kind"].as_str().unwrap_or_default(),
            b["producer_id"].as_str().unwrap_or_default(),
            b["summary"].as_str().unwrap_or_default()
        );
    }
}

fn render_deliveries(events: &Value) {
    let list = events.as_array().cloned().unwrap_or_default();
    if list.is_empty() {
        println!("no delivery events");
        return;
    }
    for e in &list {
        println!("[{}] {}: {}", e["at_ms"], e["level"], e["message"]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parse_no_args_is_help() {
        match parse(&[]) {
            WaypointCommand::Help => {}
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parse_create_requires_prompt() {
        match parse(&v(&["create", "--affected", "squad:squad-1"])) {
            WaypointCommand::UsageError(_) => {}
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parse_create_with_prompt_and_affected() {
        match parse(&v(&[
            "create",
            "--prompt",
            "coordinate the release",
            "--affected",
            "squad:squad-1",
            "--affected",
            "review:review-2:advisory",
            "--allow-advisory",
        ])) {
            WaypointCommand::Create {
                prompt,
                affected,
                allow_advisory,
                ..
            } => {
                assert_eq!(prompt, "coordinate the release");
                assert_eq!(affected, vec!["squad:squad-1", "review:review-2:advisory"]);
                assert!(allow_advisory);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parse_list_with_filters() {
        match parse(&v(&["list", "--project", "proj", "--state", "open"])) {
            WaypointCommand::List { project, state } => {
                assert_eq!(project.as_deref(), Some("proj"));
                assert_eq!(state.as_deref(), Some("open"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parse_get_requires_waypoint_id() {
        match parse(&v(&["get"])) {
            WaypointCommand::UsageError(_) => {}
            other => panic!("unexpected: {other:?}"),
        }
        match parse(&v(&["get", "waypoint-1"])) {
            WaypointCommand::Get { waypoint_id } => assert_eq!(waypoint_id, "waypoint-1"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parse_close_and_reopen() {
        match parse(&v(&["close", "waypoint-1"])) {
            WaypointCommand::Close { waypoint_id } => assert_eq!(waypoint_id, "waypoint-1"),
            other => panic!("unexpected: {other:?}"),
        }
        match parse(&v(&["reopen", "waypoint-1"])) {
            WaypointCommand::Reopen { waypoint_id } => assert_eq!(waypoint_id, "waypoint-1"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parse_bearings_and_deliveries() {
        match parse(&v(&["bearings", "waypoint-1"])) {
            WaypointCommand::Bearings { waypoint_id } => assert_eq!(waypoint_id, "waypoint-1"),
            other => panic!("unexpected: {other:?}"),
        }
        match parse(&v(&["deliveries", "waypoint-1"])) {
            WaypointCommand::Deliveries { waypoint_id } => assert_eq!(waypoint_id, "waypoint-1"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parse_affected_add_remove_mode() {
        match parse(&v(&["affected", "add", "waypoint-1", "squad", "squad-2"])) {
            WaypointCommand::Affected(WaypointAffectedCommand::Add {
                waypoint_id,
                kind,
                entry_id,
                mode,
            }) => {
                assert_eq!(waypoint_id, "waypoint-1");
                assert_eq!(kind, "squad");
                assert_eq!(entry_id, "squad-2");
                assert_eq!(mode, None);
            }
            other => panic!("unexpected: {other:?}"),
        }
        match parse(&v(&[
            "affected",
            "add",
            "--mode",
            "advisory",
            "waypoint-1",
            "review",
            "review-2",
        ])) {
            WaypointCommand::Affected(WaypointAffectedCommand::Add { mode, .. }) => {
                assert_eq!(mode.as_deref(), Some("advisory"));
            }
            other => panic!("unexpected: {other:?}"),
        }
        match parse(&v(&["affected", "remove", "waypoint-1", "squad-2"])) {
            WaypointCommand::Affected(WaypointAffectedCommand::Remove {
                waypoint_id,
                entry_id,
            }) => {
                assert_eq!(waypoint_id, "waypoint-1");
                assert_eq!(entry_id, "squad-2");
            }
            other => panic!("unexpected: {other:?}"),
        }
        match parse(&v(&["affected", "mode", "waypoint-1", "squad-2", "block"])) {
            WaypointCommand::Affected(WaypointAffectedCommand::Mode {
                waypoint_id,
                entry_id,
                mode,
            }) => {
                assert_eq!(waypoint_id, "waypoint-1");
                assert_eq!(entry_id, "squad-2");
                assert_eq!(mode, "block");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parse_affected_usage_errors() {
        match parse(&v(&["affected", "add", "waypoint-1", "squad"])) {
            WaypointCommand::Affected(WaypointAffectedCommand::UsageError(_)) => {}
            other => panic!("unexpected: {other:?}"),
        }
        match parse(&v(&["affected", "bogus"])) {
            WaypointCommand::Affected(WaypointAffectedCommand::UsageError(_)) => {}
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parse_bearing_add() {
        match parse(&v(&[
            "bearing",
            "add",
            "waypoint-1",
            "squad",
            "squad-2",
            "--summary",
            "shipped the migration",
            "--commit-id",
            "abc123",
        ])) {
            WaypointCommand::Bearing(WaypointBearingCommand::Add {
                waypoint_id,
                producer_kind,
                producer_id,
                summary,
                commit_id,
                ..
            }) => {
                assert_eq!(waypoint_id, "waypoint-1");
                assert_eq!(producer_kind, "squad");
                assert_eq!(producer_id, "squad-2");
                assert_eq!(summary, "shipped the migration");
                assert_eq!(commit_id.as_deref(), Some("abc123"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parse_bearing_add_requires_summary() {
        match parse(&v(&["bearing", "add", "waypoint-1", "squad", "squad-2"])) {
            WaypointCommand::Bearing(WaypointBearingCommand::UsageError(_)) => {}
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn affected_spec_parses_kind_entry_and_optional_mode() {
        let entry = parse_affected_spec("squad:squad-1").unwrap();
        assert_eq!(entry["kind"], "squad");
        assert_eq!(entry["entry_id"], "squad-1");
        assert!(entry.get("mode").is_none());

        let entry = parse_affected_spec("review:review-2:advisory").unwrap();
        assert_eq!(entry["kind"], "review");
        assert_eq!(entry["entry_id"], "review-2");
        assert_eq!(entry["mode"], "advisory");

        assert!(parse_affected_spec("bogus").is_err());
        assert!(parse_affected_spec("").is_err());
    }
}
