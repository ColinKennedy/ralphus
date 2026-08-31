//! Executes an already-parsed `ralphus_cli::commands::Command` against a
//! [`DaemonClient`], building a `serde_json::Value` response instead of
//! printing (RAL-301). Mirrors `ralphus_cli::commands::dispatch`'s match
//! arms one-for-one -- same client calls, same fields -- but returns the
//! same `Value` `dispatch` would have handed to `emit()` in `--json` mode,
//! rather than printing it, since an MCP tool call returns a value, not
//! stdout text (this crate's stdout is the MCP JSON-RPC wire, not a place
//! any of this may print to -- see `protocol.rs`).
//!
//! Argv parsing/validation is fully reused from `ralphus_cli::commands`
//! (`parse_args`, `flags::Scanner`, `selector::resolve_*`) -- nothing here
//! re-derives which flags a command accepts or how a selector resolves.
//! Only the final "call the client, shape a `Value`" step -- the one place
//! `dispatch` prints instead of returning -- is duplicated, which is exactly
//! the ticket's own "wraps DaemonClient ... directly" architecture.

mod agent;
mod cell;
mod machine;
mod mailbox;
mod project;
mod proof;
mod queue;
mod review;
mod show;
mod squad;
mod task;

use ralphus_cli::client::DaemonClient;
use ralphus_cli::commands::{Command, CommandError, misc};
use ralphus_cli::selector;
use serde_json::{Value, json};

pub type ExecResult = Result<Value, CommandError>;

fn usage(message: impl Into<String>) -> CommandError {
    CommandError::Usage(message.into())
}

/// Renders a [`CommandError`] as plain text for an MCP tool error result --
/// mirrors `CommandError::print`'s message selection without the JSON-vs
/// human branching that only matters for a terminal.
#[must_use]
pub fn message(e: &CommandError) -> String {
    match e {
        CommandError::Daemon(err) => err.to_string(),
        CommandError::Selector(err) => err.to_string(),
        CommandError::Usage(m) => m.clone(),
    }
}

pub fn execute(cmd: Command, client: &DaemonClient) -> ExecResult {
    match cmd {
        Command::Help => Err(usage("no such tool")),
        Command::License => Ok(json!({"license": ralphus_core::license::embedded_license()})),
        Command::Validate { files } => exec_validate(client, &files),
        Command::Submit(args) => exec_submit(client, args),
        Command::Status {
            squad_id,
            concurrency,
        } => exec_status(client, squad_id, concurrency),
        Command::Resources => Ok(client.resources()?),
        Command::Graph { squad_id, dot, all } => exec_graph(client, squad_id, dot, all),
        Command::Get { uri, field } => exec_get(client, &uri, field.as_deref()),
        Command::Cartographer(args) => exec_cartographer(client, args),
        Command::History { selector } => exec_history(client, &selector),
        Command::Listen {
            selector,
            until,
            timeout,
        } => exec_listen(client, &selector, &until, timeout),
        Command::RetryRun { squad_id } => Ok(client.retry_squad(&squad_id)?),
        Command::Clear(args) => exec_clear(client, args),
        Command::Check(args) => Ok(exec_check(client, args)),
        Command::Completion => Ok(json!({
            "message": format!(
                "# {} shell completion is not yet ported in this Rust build.",
                ralphus_cli::program_name::resolve_program_name()
            ),
        })),
        Command::Configuration => Ok(exec_configuration()),
        Command::Task(c) => task::execute(c, client),
        Command::TutorShow => Ok(json!({"tutor": ralphus_cli::tutor::task_tutor()})),
        Command::Cell(c) => cell::execute(c, client),
        Command::Proof(c) => proof::execute(c, client),
        Command::Review(c) => review::execute(c, client),
        Command::Queue(c) => queue::execute(c, client),
        Command::InitializeGit { path } => exec_initialize_git(path),
        Command::Project(c) => project::execute(c, client),
        Command::Machine(c) => machine::execute(c, client),
        Command::Agent(c) => agent::execute(c),
        Command::Show(c) => show::execute(c),
        Command::Squad(c) => squad::execute(c, client),
        Command::Mailbox(c) => mailbox::execute(c, client),
        Command::QuickStart(_) => Err(usage("quick-start is excluded from the MCP tool surface")),
        Command::UsageError(m) => Err(usage(m)),
    }
}

fn exec_validate(client: &DaemonClient, files: &[String]) -> ExecResult {
    let mut texts = Vec::new();
    for path in files {
        let text = std::fs::read_to_string(path)
            .map_err(|e| usage(format!("could not read {path}: {e}")))?;
        texts.push(text);
    }
    let combined = texts.join("\n\n");
    Ok(client.validate(&combined)?)
}

fn exec_submit(client: &DaemonClient, args: misc::SubmitArgs) -> ExecResult {
    let (sources, is_batch) = misc::resolve_submit_sources(&args.files).map_err(usage)?;
    let hold = args.hold || args.activate;

    let submit_one = |text: &str| -> ExecResult {
        if !args.no_validate {
            let outcome = client.validate(text)?;
            if !outcome["valid"].as_bool().unwrap_or(false) {
                return Ok(json!({"valid": false, "errors": outcome["errors"]}));
            }
        }
        let result = client.submit(text, hold, args.label.as_deref())?;
        let squad_id = result["squad_id"].as_str().map(str::to_string);
        if args.activate {
            if let Some(id) = &squad_id {
                client.activate_squad(id)?;
            }
        }
        if args.wait {
            if let Some(id) = &squad_id {
                return Ok(wait_for_terminal(client, id)?);
            }
        }
        Ok(result)
    };

    if is_batch {
        let mut results = Vec::new();
        for source in &sources {
            let text = misc::read_submit_source(source).map_err(usage)?;
            results.push(submit_one(&text)?);
        }
        return Ok(json!({"batch": true, "results": results}));
    }

    let mut texts = Vec::new();
    for source in &sources {
        texts.push(misc::read_submit_source(source).map_err(usage)?);
    }
    submit_one(&texts.join("\n\n"))
}

/// A silent (no progress printing) version of `misc::wait_for_terminal` --
/// polls until the squad reaches a terminal state and returns it, without
/// `println!`ing each transition (MCP has no per-tool-call progress
/// channel to print to; a caller that wants incremental status can call
/// `status`/`listen` itself).
fn wait_for_terminal(
    client: &DaemonClient,
    squad_id: &str,
) -> Result<Value, ralphus_cli::client::DaemonError> {
    loop {
        let squad = client.squad(squad_id)?;
        if matches!(
            squad["state"].as_str(),
            Some("done" | "failed" | "cancelled")
        ) {
            return Ok(squad);
        }
        std::thread::sleep(std::time::Duration::from_secs(2));
    }
}

fn exec_status(client: &DaemonClient, squad_id: Option<String>, concurrency: bool) -> ExecResult {
    if concurrency {
        return Ok(client.tasks(None, None, None)?);
    }
    if let Some(id) = &squad_id {
        return Ok(client.squad(id)?);
    }
    Ok(client.tasks(None, None, None)?)
}

fn exec_graph(
    client: &DaemonClient,
    squad_id: Option<String>,
    dot: bool,
    global: bool,
) -> ExecResult {
    if !global && squad_id.is_none() {
        return Err(usage("a squad id is required unless --all is given"));
    }
    let data = if global {
        client.global_graph(false)?
    } else {
        client.squad_graph(squad_id.as_deref().unwrap_or_default())?
    };
    let nodes = data["nodes"].as_array().cloned().unwrap_or_default();
    let edges = data["edges"].as_array().cloned().unwrap_or_default();
    let labelled_nodes: Vec<Value> = nodes
        .into_iter()
        .map(|mut n| {
            let label = if global {
                format!(
                    "{} [{}]",
                    n["label"]
                        .as_str()
                        .unwrap_or_else(|| n["id"].as_str().unwrap_or_default()),
                    n["state"]
                )
            } else {
                format!("{}/{}", n["task_name"], n["cell_id"])
            };
            n["label"] = Value::String(label);
            n
        })
        .collect();
    let rendered = if dot {
        ralphus_cli::graphview::render_dot(&labelled_nodes, &edges)
    } else {
        ralphus_cli::graphview::render_ascii(&labelled_nodes, &edges)
    };
    Ok(json!({"nodes": labelled_nodes, "edges": edges, "rendered": rendered}))
}

fn exec_get(client: &DaemonClient, sel: &str, field: Option<&str>) -> ExecResult {
    let data: Value = if misc::looks_like_guardian_selector(sel) {
        let resolved =
            selector::resolve_guardian_selector(client, sel, selector::DEFAULT_REVIEW_LIST_HINT)?;
        let guardian = client.guardian_get(&resolved.guardian_id)?;
        if let Some(branch_id) = &resolved.branch_id {
            guardian["branches"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|b| b["id"].as_str() == Some(branch_id.as_str()))
                .cloned()
                .ok_or_else(|| {
                    CommandError::Selector(ralphus_cli::selector::SelectorError(format!(
                        "no branch '{branch_id}'"
                    )))
                })?
        } else {
            guardian
        }
    } else {
        let resolved = selector::resolve_squad_selector(client, sel)?;
        let squad = client.squad(&resolved.squad_id)?;
        match resolved.kind.as_str() {
            "squad" => squad,
            "task" => squad["tasks"][resolved.task_idx as usize].clone(),
            "cell" => squad["tasks"][resolved.task_idx as usize]["cells"]
                [resolved.cell_idx as usize]
                .clone(),
            _ => misc::proof_step_for(&squad, &resolved).clone(),
        }
    };
    match field {
        Some(f) => Ok(misc::walk_field(&data, f)?),
        None => Ok(data),
    }
}

fn exec_cartographer(client: &DaemonClient, args: misc::CartographerArgs) -> ExecResult {
    let mut entity = args.entity.clone();
    if let Some(sel) = &args.for_selector {
        let resolved = selector::resolve_squad_selector(client, sel)?;
        entity = Some(ralphus_cli::entity_uri::from_resolved_selector(&resolved).to_string());
    }
    let filters = ralphus_cli::client::CartographerFilters {
        source: args.source.as_deref(),
        scope: args.scope.as_deref(),
        level: args.level.as_deref(),
        squad_id: args.squad_id.as_deref(),
        guardian_id: args.guardian_id.as_deref(),
        cell_id: args.cell_id.as_deref(),
        task: args.task.as_deref(),
        entity: entity.as_deref(),
        q: args.q.as_deref(),
        since_ms: None,
        until_ms: None,
        limit: args.limit,
        offset: args.offset,
        ascending: args.ascending,
    };
    Ok(client.cartographer(filters)?)
}

fn exec_history(client: &DaemonClient, sel: &str) -> ExecResult {
    let resolved = selector::resolve_squad_selector(client, sel)?;
    if resolved.kind != "cell" && resolved.kind != "proof" {
        return Err(CommandError::Selector(
            ralphus_cli::selector::SelectorError(format!(
                "'{sel}' is a {} selector -- history targets a cell or proof step",
                resolved.kind
            )),
        ));
    }
    let pane = misc::history_pane(client, &resolved)?;
    if pane["active"].as_bool().unwrap_or(false) {
        let content = pane["content"].as_str().unwrap_or_default();
        return Ok(
            json!({"active": true, "content": ralphus_core::redact::redact_secrets(content)}),
        );
    }
    let (content, found) = misc::history_debug_events(client, &resolved)?;
    Ok(json!({
        "active": false,
        "found": found,
        "content": ralphus_core::redact::redact_secrets(&content),
    }))
}

fn exec_listen(client: &DaemonClient, sel: &str, until: &str, timeout: Option<f64>) -> ExecResult {
    let target = until.to_lowercase();
    let start = std::time::Instant::now();
    loop {
        let (kind, status) = misc::listen_status(client, sel)?;
        if status.to_lowercase() == target {
            return Ok(json!({"selector": sel, "kind": kind, "status": status}));
        }
        if let Some(t) = timeout {
            if start.elapsed().as_secs_f64() >= t {
                return Err(usage(format!("timed out after {t}s waiting for '{until}'")));
            }
        }
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
}

fn exec_clear(client: &DaemonClient, args: misc::ClearArgs) -> ExecResult {
    let mut states: Vec<String> = Vec::new();
    if let Some(status) = &args.status {
        states = status
            .split(',')
            .map(|s| s.trim().to_lowercase())
            .filter(|s| !s.is_empty())
            .collect();
        let invalid: Vec<&String> = states
            .iter()
            .filter(|s| !misc::SQUAD_STATES.contains(&s.as_str()))
            .collect();
        if !invalid.is_empty() {
            return Err(usage(format!(
                "unknown status {} (valid: {})",
                invalid
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
                misc::SQUAD_STATES.join(", ")
            )));
        }
    }
    if !args.all && states.is_empty() {
        return Err(usage(
            "pass all=true to clear everything, or status to filter by squad state",
        ));
    }
    // `clear`'s CLI form prompts on stdin for confirmation unless `--yes` is
    // given; an MCP server's stdin is the JSON-RPC transport itself (see
    // `protocol.rs`), so reading a confirmation line from it here would
    // corrupt the wire -- `yes=true` is required, not merely honored.
    if !args.yes {
        return Err(usage(
            "clear is destructive and requires yes=true (there is no interactive confirmation \
             prompt over MCP)",
        ));
    }
    let states_opt = if states.is_empty() {
        None
    } else {
        Some(states.as_slice())
    };
    Ok(client.clear(states_opt, args.keep_temporary)?)
}

fn exec_check(client: &DaemonClient, args: misc::CheckArgs) -> Value {
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let results =
        ralphus_cli::health::run_checks(client.base_url(), &cwd, args.enable_developer_checks);
    let checks: Vec<Value> = results
        .iter()
        .map(|r| {
            json!({
                "section": r.section,
                "status": r.status,
                "name": r.name,
                "detail": r.detail,
            })
        })
        .collect();
    let file_issues = ralphus_cli::config::validate_config_files(&cwd, true);
    let issues: Vec<Value> = file_issues
        .iter()
        .map(|fi| {
            json!({
                "path": fi.path.display().to_string(),
                "label": fi.label,
                "syntax_error": fi.syntax_error,
                "issues": fi.issues,
            })
        })
        .collect();
    let failed = results.iter().filter(|r| r.is_fail()).count() + file_issues.len();
    json!({"checks": checks, "config_file_issues": issues, "failed": failed})
}

fn exec_configuration() -> Value {
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let config = ralphus_cli::config::load_config(&cwd, true);
    let sources: Vec<Value> = config
        .sources
        .iter()
        .map(|src| {
            let label = config
                .source_labels
                .iter()
                .find(|(p, _)| p == src)
                .map(|(_, l)| l.as_str())
                .unwrap_or("unknown");
            json!({"path": src.display().to_string(), "label": label})
        })
        .collect();
    json!({
        "sources": sources,
        "task_maximum_timeout_seconds": config.task.maximum_timeout_seconds,
        "daemon_log_path": config.daemon.log_path,
        "daemon_log_level": config.daemon.log_level,
    })
}

fn exec_initialize_git(path: Option<String>) -> ExecResult {
    let target = path.map_or_else(
        || std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
        |p| ralphus_core::expand_home(&p),
    );
    let target = ralphus_core::strip_verbatim_prefix(target.canonicalize().unwrap_or(target));

    let probe = std::process::Command::new("git")
        .args([
            "-C",
            &target.to_string_lossy(),
            "rev-parse",
            "--is-inside-work-tree",
        ])
        .output()
        .map_err(|e| usage(format!("could not run git: {e}")))?;
    if !probe.status.success() || String::from_utf8_lossy(&probe.stdout).trim() != "true" {
        return Err(usage(format!(
            "{} is not inside a git working tree (run this from a repository, or pass path)",
            target.display()
        )));
    }
    for (key, value) in [("rerere.enabled", "true"), ("rerere.autoupdate", "true")] {
        let result = std::process::Command::new("git")
            .args(["-C", &target.to_string_lossy(), "config", key, value])
            .output()
            .map_err(|e| usage(format!("git config {key} failed: {e}")))?;
        if !result.status.success() {
            return Err(usage(format!(
                "git config {key} failed: {}",
                String::from_utf8_lossy(&result.stderr).trim()
            )));
        }
    }
    Ok(json!({
        "path": target.display().to_string(),
        "message": "git rerere enabled: conflict resolutions during review rebases will now be \
                     recorded and replayed automatically.",
    }))
}
