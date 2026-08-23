//! Top-level commands with no subcommand tree of their own, ported from
//! `cli/src/ralphus/__main__.py`: `validate`, `submit`, `status`,
//! `resources`, `graph`, `get`, `cartographer`, `history`, `listen`,
//! `retry`, `clear`, `check health`, `completion`, `configuration show`,
//! `initialize git`. `author` (the agentic TOML-authoring loop) is
//! deliberately not ported and has no command here at all.

use std::time::Duration;

use serde_json::Value;

use crate::args::GlobalOpts;
use crate::client::{CartographerFilters, DaemonClient, DaemonError};
use crate::commands::{CommandError, emit, run_and_report};
use crate::entity_uri::from_resolved_selector;
use crate::flags::Scanner;
use crate::selector::{self, ResolvedSelector, SelectorError};

const SQUAD_STATES: [&str; 6] = [
    "queued",
    "pending",
    "running",
    "done",
    "failed",
    "cancelled",
];
const STDIN_SOURCE: &str = "-";

// ---- validate -----------------------------------------------------------

pub fn cmd_validate(opts: &GlobalOpts, files: &[String]) -> i32 {
    let client = opts.client();
    // More than one file: validate them as ONE combined submission, the same
    // way `ralphus submit a.toml b.toml ...` joins multiple files' raw text
    // client-side (`texts.join("\n\n")`) before either validating or
    // submitting. Without this, a cross-file `upstream = "<<task:...>>"`
    // reference would validate clean under `submit` (which joins first) but
    // falsely report as unknown under `validate` (which used to check each
    // file in isolation) -- the two commands must agree on what "this
    // submission" contains.
    if files.len() > 1 {
        let mut texts = Vec::new();
        for path in files {
            match std::fs::read_to_string(path) {
                Ok(t) => texts.push(t),
                Err(e) => {
                    println!("error: could not read {path}: {e}");
                    return 1;
                }
            }
        }
        let combined = texts.join("\n\n");
        return match client.validate(&combined) {
            Ok(outcome) => {
                let valid = outcome["valid"].as_bool().unwrap_or(false);
                emit(opts, &outcome, |o| {
                    if o["valid"].as_bool().unwrap_or(false) {
                        println!("{}: valid", files.join(" "));
                    } else {
                        print_validation_errors(o["errors"].as_array());
                    }
                });
                i32::from(!valid)
            }
            Err(e) => {
                CommandError::Daemon(e).print(opts.json, None);
                1
            }
        };
    }
    let mut had_error = false;
    for path in files {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) => {
                println!("error: could not read {path}: {e}");
                had_error = true;
                continue;
            }
        };
        match client.validate(&text) {
            Ok(outcome) => {
                let valid = outcome["valid"].as_bool().unwrap_or(false);
                emit(opts, &outcome, |o| {
                    if o["valid"].as_bool().unwrap_or(false) {
                        println!("{path}: valid");
                    } else {
                        print_validation_errors(o["errors"].as_array());
                    }
                });
                if !valid {
                    had_error = true;
                }
            }
            Err(e) => {
                CommandError::Daemon(e).print(opts.json, None);
                had_error = true;
            }
        }
    }
    i32::from(had_error)
}

fn print_validation_errors(errors: Option<&Vec<Value>>) {
    for err in errors.into_iter().flatten() {
        let line = err["line"]
            .as_i64()
            .map_or_else(|| "?".to_string(), |l| l.to_string());
        println!(
            "error [line {line}]: {}",
            err["message"].as_str().unwrap_or("")
        );
    }
}

// ---- submit ---------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct SubmitArgs {
    pub files: Vec<String>,
    pub hold: bool,
    pub activate: bool,
    pub wait: bool,
    pub no_validate: bool,
    pub label: Option<String>,
}

pub fn parse_submit(scanner: &mut Scanner) -> super::Command {
    let hold = scanner.take_bool("--hold");
    let activate = scanner.take_bool("--activate");
    let wait = scanner.take_bool("--wait");
    let no_validate = scanner.take_bool("--no-validate");
    let label = scanner.take_value("--label").ok().flatten();
    super::Command::Submit(SubmitArgs {
        files: scanner.clone().remaining(),
        hold,
        activate,
        wait,
        no_validate,
        label,
    })
}

/// Expands each `submit` file argument into concrete sources. `is_batch` is
/// true when any argument was a directory or glob -- each resolved file then
/// becomes its own separate squad, rather than combining into one (the
/// existing behavior for explicit file paths).
fn resolve_submit_sources(raw_args: &[String]) -> Result<(Vec<String>, bool), String> {
    let mut sources = Vec::new();
    let mut is_batch = false;
    for raw in raw_args {
        if raw == STDIN_SOURCE {
            sources.push(STDIN_SOURCE.to_string());
            continue;
        }
        if raw.contains(['*', '?', '[']) {
            let mut matches: Vec<String> = glob_match(raw);
            if matches.is_empty() {
                return Err(format!("no files matched glob '{raw}'"));
            }
            matches.sort();
            sources.extend(matches);
            is_batch = true;
            continue;
        }
        let path = std::path::Path::new(raw);
        if path.is_dir() {
            let mut matches: Vec<String> = std::fs::read_dir(path)
                .into_iter()
                .flatten()
                .filter_map(Result::ok)
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|e| e == "toml"))
                .map(|p| p.to_string_lossy().into_owned())
                .collect();
            if matches.is_empty() {
                return Err(format!("no .toml files found in directory '{raw}'"));
            }
            matches.sort();
            sources.extend(matches);
            is_batch = true;
            continue;
        }
        sources.push(raw.clone());
    }
    Ok((sources, is_batch))
}

/// A minimal glob expander covering `*`/`?`/`[...]` against the pattern's
/// parent directory -- ordinary shell-glob semantics, not a full glob crate
/// (the workspace has none; this is the only place one is needed).
fn glob_match(pattern: &str) -> Vec<String> {
    let path = std::path::Path::new(pattern);
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| std::path::Path::new("."));
    let file_pattern = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let regex = glob_to_simple_matcher(&file_pattern);
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            regex(&name).then(|| entry.path().to_string_lossy().into_owned())
        })
        .collect()
}

fn glob_to_simple_matcher(pattern: &str) -> impl Fn(&str) -> bool {
    let pattern = pattern.to_string();
    move |name: &str| glob_matches(&pattern, name)
}

fn glob_matches(pattern: &str, name: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let n: Vec<char> = name.chars().collect();
    fn rec(p: &[char], n: &[char]) -> bool {
        match p.first() {
            None => n.is_empty(),
            Some('*') => rec(&p[1..], n) || (!n.is_empty() && rec(p, &n[1..])),
            Some('?') => !n.is_empty() && rec(&p[1..], &n[1..]),
            Some('[') => {
                let Some(close) = p.iter().position(|c| *c == ']') else {
                    return !n.is_empty() && p[0] == n[0] && rec(&p[1..], &n[1..]);
                };
                if n.is_empty() {
                    return false;
                }
                let set: &[char] = &p[1..close];
                if set.contains(&n[0]) {
                    rec(&p[close + 1..], &n[1..])
                } else {
                    false
                }
            }
            Some(c) => !n.is_empty() && *c == n[0] && rec(&p[1..], &n[1..]),
        }
    }
    rec(&p, &n)
}

fn read_submit_source(source: &str) -> Result<String, String> {
    if source == STDIN_SOURCE {
        use std::io::Read as _;
        let mut text = String::new();
        std::io::stdin()
            .read_to_string(&mut text)
            .map_err(|e| e.to_string())?;
        if text.trim().is_empty() {
            return Err("stdin is empty".to_string());
        }
        return Ok(text);
    }
    std::fs::read_to_string(source).map_err(|e| format!("could not read {source}: {e}"))
}

fn wait_for_terminal(client: &DaemonClient, squad_id: &str) -> Result<Value, DaemonError> {
    let mut last_state: Option<String> = None;
    loop {
        let squad = client.squad(squad_id)?;
        let state = squad["state"].as_str().map(str::to_string);
        if state != last_state {
            println!("{squad_id}: {}", state.as_deref().unwrap_or("?"));
            last_state = state.clone();
        }
        if matches!(state.as_deref(), Some("done" | "failed" | "cancelled")) {
            return Ok(squad);
        }
        std::thread::sleep(Duration::from_secs(2));
    }
}

fn finish_submission(
    opts: &GlobalOpts,
    client: &DaemonClient,
    result: &Value,
    args: &SubmitArgs,
) -> i32 {
    let squad_id = result["squad_id"].as_str().map(str::to_string);
    if args.activate {
        if let Some(id) = &squad_id {
            if let Err(e) = client.activate_squad(id) {
                let code = crate::output::exit_code_for(&e);
                CommandError::Daemon(e).print(opts.json, None);
                return code;
            }
        }
    }
    if args.wait {
        if let Some(id) = &squad_id {
            return match wait_for_terminal(client, id) {
                Ok(squad) => i32::from(squad["state"].as_str() != Some("done")),
                Err(e) => {
                    CommandError::Daemon(e).print(opts.json, None);
                    1
                }
            };
        }
    }
    0
}

fn validate_before_submit(
    opts: &GlobalOpts,
    client: &DaemonClient,
    text: &str,
    no_validate: bool,
) -> Option<i32> {
    if no_validate {
        return None;
    }
    match client.validate(text) {
        Err(e) => {
            let code = crate::output::exit_code_for(&e);
            CommandError::Daemon(e).print(opts.json, None);
            Some(code)
        }
        Ok(outcome) => {
            if outcome["valid"].as_bool().unwrap_or(false) {
                None
            } else {
                print_validation_errors(outcome["errors"].as_array());
                Some(1)
            }
        }
    }
}

pub fn cmd_submit(opts: &GlobalOpts, args: SubmitArgs) -> i32 {
    let (sources, is_batch) = match resolve_submit_sources(&args.files) {
        Ok(v) => v,
        Err(e) => {
            println!("error: {e}");
            return 2;
        }
    };
    let hold = args.hold || args.activate;
    let client = opts.client();

    if is_batch {
        let mut exit_code = 0;
        for source in &sources {
            let text = match read_submit_source(source) {
                Ok(t) => t,
                Err(e) => {
                    println!("error: {e}");
                    return 2;
                }
            };
            if let Some(code) = validate_before_submit(opts, &client, &text, args.no_validate) {
                return code;
            }
            match client.submit(&text, hold, args.label.as_deref()) {
                Ok(result) => {
                    emit(opts, &result, |r| {
                        println!("{} ({})", r["squad_id"], r["state"])
                    });
                    exit_code = exit_code.max(finish_submission(opts, &client, &result, &args));
                }
                Err(e) => {
                    let code = crate::output::exit_code_for(&e);
                    CommandError::Daemon(e).print(opts.json, None);
                    return code;
                }
            }
        }
        return exit_code;
    }

    let mut texts = Vec::new();
    for source in &sources {
        match read_submit_source(source) {
            Ok(t) => texts.push(t),
            Err(e) => {
                println!("error: {e}");
                return 2;
            }
        }
    }
    let text = texts.join("\n\n");
    if let Some(code) = validate_before_submit(opts, &client, &text, args.no_validate) {
        return code;
    }
    match client.submit(&text, hold, args.label.as_deref()) {
        Ok(result) => {
            emit(opts, &result, |r| {
                println!("{} ({})", r["squad_id"], r["state"])
            });
            finish_submission(opts, &client, &result, &args)
        }
        Err(e) => {
            let code = crate::output::exit_code_for(&e);
            CommandError::Daemon(e).print(opts.json, None);
            code
        }
    }
}

// ---- status / resources / graph -----------------------------------------

pub fn cmd_status(opts: &GlobalOpts, squad_id: Option<String>, concurrency: bool) -> i32 {
    let client = opts.client();
    run_and_report(opts, None, || {
        if concurrency {
            let board = client.tasks(None, None, None)?;
            emit(opts, &board, render_concurrency);
        } else if let Some(id) = &squad_id {
            let squad = client.squad(id)?;
            emit(opts, &squad, print_squad);
        } else {
            let board = client.tasks(None, None, None)?;
            emit(opts, &board, render_squad_list);
        }
        Ok(())
    })
}

fn render_concurrency(board: &Value) {
    let d = &board["daemon"];
    crate::output::print_kv(&[
        ("running", d["running"].to_string()),
        ("max_concurrent", d["max_concurrent"].to_string()),
    ]);
    if let Some(reviews) = d["running_reviews"].as_array() {
        if !reviews.is_empty() {
            println!("\nrunning reviews:");
            for r in reviews {
                println!("  {}  {}", r["id"], r["name"]);
            }
        }
    }
}

pub fn render_squad_list(board: &Value) {
    let squads = board["squads"].as_array().cloned().unwrap_or_default();
    if squads.is_empty() {
        println!("no squads");
        return;
    }
    let rows: Vec<Vec<String>> = squads
        .iter()
        .map(|s| {
            vec![
                s["id"].as_str().unwrap_or_default().to_string(),
                s["state"].as_str().unwrap_or_default().to_string(),
                s["label"].as_str().unwrap_or_default().to_string(),
            ]
        })
        .collect();
    crate::output::print_table(&["ID", "STATE", "LABEL"], &rows);
}

fn print_squad(squad: &Value) {
    println!("{}  {}", squad["id"], squad["state"]);
    if let Some(tasks) = squad["tasks"].as_array() {
        for task in tasks {
            println!("  task {}: {}", task["name"], task["state"]);
        }
    }
}

pub fn cmd_resources(opts: &GlobalOpts) -> i32 {
    let client = opts.client();
    run_and_report(opts, None, || {
        let result = client.resources()?;
        emit(opts, &result, |res| {
            let rows = res["resources"].as_array().cloned().unwrap_or_default();
            if rows.is_empty() {
                println!("no running cells");
                return;
            }
            let table_rows: Vec<Vec<String>> = rows
                .iter()
                .map(|row| {
                    let mb = row["mem_bytes"]
                        .as_f64()
                        .map(|m| format!("{:.0}", m / (1024.0 * 1024.0)))
                        .unwrap_or_else(|| "-".to_string());
                    let cpu = row["cpu_percent"]
                        .as_f64()
                        .map(|c| format!("{c:.1}"))
                        .unwrap_or_else(|| "-".to_string());
                    vec![
                        row["squad_id"].as_str().unwrap_or_default().to_string(),
                        row["task_name"].as_str().unwrap_or_default().to_string(),
                        row["cell_id"].as_str().unwrap_or_default().to_string(),
                        row["pid"].to_string(),
                        cpu,
                        mb,
                    ]
                })
                .collect();
            crate::output::print_table(
                &["SQUAD", "TASK", "CELL", "PID", "CPU%", "MEM_MB"],
                &table_rows,
            );
        });
        Ok(())
    })
}

pub fn cmd_graph(opts: &GlobalOpts, squad_id: Option<String>, dot: bool, global: bool) -> i32 {
    if !global && squad_id.is_none() {
        println!("error: a squad id is required unless --global is given");
        return 2;
    }
    let client = opts.client();
    run_and_report(opts, None, || {
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
        if dot {
            println!("{}", crate::graphview::render_dot(&labelled_nodes, &edges));
        } else {
            println!(
                "{}",
                crate::graphview::render_ascii(&labelled_nodes, &edges)
            );
        }
        Ok(())
    })
}

// ---- get ------------------------------------------------------------------

fn looks_like_guardian_selector(selector: &str) -> bool {
    if ralphus_core::uri::looks_like_uri(selector) {
        return ralphus_core::uri::parse_uri(selector)
            .map(|u| u.kinds().first() == Some(&"REVIEW"))
            .unwrap_or(false);
    }
    selector.starts_with('@')
        || selector.contains('~')
        || selector.contains('#')
        || selector.starts_with("guardian")
}

fn proof_step_for<'a>(squad: &'a Value, resolved: &ResolvedSelector) -> &'a Value {
    let task = &squad["tasks"][resolved.task_idx as usize];
    if resolved.proof_scope == "cell" {
        &task["cells"][resolved.cell_idx as usize]["proof"][resolved.proof_idx as usize]
    } else {
        &task["proof"][resolved.proof_idx as usize]
    }
}

pub fn cmd_get(opts: &GlobalOpts, selector: &str, field: Option<&str>) -> i32 {
    let client = opts.client();
    run_and_report(opts, None, || {
        let data: Value = if looks_like_guardian_selector(selector) {
            let resolved = selector::resolve_guardian_selector(
                &client,
                selector,
                selector::DEFAULT_REVIEW_LIST_HINT,
            )?;
            let guardian = client.guardian_get(&resolved.guardian_id)?;
            if let Some(branch_id) = &resolved.branch_id {
                guardian["branches"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .find(|b| b["id"].as_str() == Some(branch_id.as_str()))
                    .cloned()
                    .ok_or_else(|| SelectorError(format!("no branch '{branch_id}'")))?
            } else {
                guardian
            }
        } else {
            let resolved = selector::resolve_squad_selector(&client, selector)?;
            let squad = client.squad(&resolved.squad_id)?;
            match resolved.kind.as_str() {
                "squad" => squad,
                "task" => squad["tasks"][resolved.task_idx as usize].clone(),
                "cell" => squad["tasks"][resolved.task_idx as usize]["cells"]
                    [resolved.cell_idx as usize]
                    .clone(),
                _ => proof_step_for(&squad, &resolved).clone(),
            }
        };
        let data = match field {
            Some(f) => walk_field(&data, f)?,
            None => data,
        };
        if data.is_object() || data.is_array() {
            println!(
                "{}",
                serde_json::to_string_pretty(&data).unwrap_or_default()
            );
        } else {
            println!("{}", plain(&data));
        }
        Ok(())
    })
}

fn plain(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn walk_field(data: &Value, field: &str) -> Result<Value, SelectorError> {
    let mut current = data.clone();
    for part in field.split('.') {
        current = match &current {
            Value::Array(arr) => part
                .parse::<usize>()
                .ok()
                .and_then(|i| arr.get(i).cloned())
                .ok_or_else(|| SelectorError(format!("no field '{field}' (failed at '{part}')")))?,
            Value::Object(_) => current
                .get(part)
                .cloned()
                .ok_or_else(|| SelectorError(format!("no field '{field}' (failed at '{part}')")))?,
            _ => {
                return Err(SelectorError(format!(
                    "no field '{field}' (failed at '{part}')"
                )));
            }
        };
    }
    Ok(current)
}

// ---- cartographer -----------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct CartographerArgs {
    pub entity: Option<String>,
    pub for_selector: Option<String>,
    pub squad_id: Option<String>,
    pub task: Option<String>,
    pub cell_id: Option<String>,
    pub guardian_id: Option<String>,
    pub source: Option<String>,
    pub scope: Option<String>,
    pub level: Option<String>,
    pub q: Option<String>,
    pub limit: i64,
    pub offset: i64,
    pub ascending: bool,
}

pub fn parse_cartographer(scanner: &mut Scanner) -> super::Command {
    let entity = scanner.take_value("--entity").ok().flatten();
    let for_selector = scanner.take_value("--for").ok().flatten();
    let squad_id = scanner.take_value("--squad").ok().flatten();
    let task = scanner.take_value("--task").ok().flatten();
    let cell_id = scanner.take_value("--cell").ok().flatten();
    let guardian_id = scanner.take_value("--guardian").ok().flatten();
    let source = scanner.take_value("--source").ok().flatten();
    let scope = scanner.take_value("--scope").ok().flatten();
    let level = scanner.take_value("--level").ok().flatten();
    let q = scanner.take_value("--q").ok().flatten();
    let limit = scanner
        .take_parsed::<i64>("--limit")
        .ok()
        .flatten()
        .unwrap_or(100);
    let offset = scanner
        .take_parsed::<i64>("--offset")
        .ok()
        .flatten()
        .unwrap_or(0);
    let ascending = scanner.take_bool("--ascending");
    super::Command::Cartographer(CartographerArgs {
        entity,
        for_selector,
        squad_id,
        task,
        cell_id,
        guardian_id,
        source,
        scope,
        level,
        q,
        limit,
        offset,
        ascending,
    })
}

pub fn cmd_cartographer(opts: &GlobalOpts, args: CartographerArgs) -> i32 {
    let client = opts.client();
    run_and_report(opts, None, || {
        let mut entity = args.entity.clone();
        if let Some(sel) = &args.for_selector {
            let resolved = selector::resolve_squad_selector(&client, sel)?;
            entity = Some(from_resolved_selector(&resolved).to_string());
        }
        let filters = CartographerFilters {
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
        let page = client.cartographer(filters)?;
        emit(opts, &page, render_cartographer_page);
        Ok(())
    })
}

fn render_cartographer_page(page: &Value) {
    let rows = page["rows"].as_array().cloned().unwrap_or_default();
    if rows.is_empty() {
        println!("no matching events");
        return;
    }
    for row in &rows {
        let mut bits = vec![format!(
            "[{}] {:<7} {}",
            row["at_ms"],
            row["level"].as_str().unwrap_or_default().to_uppercase(),
            row["source"].as_str().unwrap_or_default()
        )];
        if let Some(s) = row["scope"].as_str() {
            bits.push(format!("scope={s}"));
        }
        if let Some(t) = row["task"].as_str() {
            bits.push(format!("task={t}"));
        }
        if let Some(s) = row["cell_id"].as_str() {
            bits.push(format!("cell={s}"));
        }
        println!(
            "{}: {}",
            bits.join(" "),
            row["message"].as_str().unwrap_or_default()
        );
        if let Some(p) = row["log_path"].as_str() {
            println!("    (log: {p})");
        }
    }
    println!("({} of {} total)", rows.len(), page["total"]);
}

// ---- history --------------------------------------------------------------

pub fn cmd_history(opts: &GlobalOpts, selector: &str) -> i32 {
    let client = opts.client();
    run_and_report(opts, None, || {
        let resolved = selector::resolve_squad_selector(&client, selector)?;
        if resolved.kind != "cell" && resolved.kind != "proof" {
            return Err(SelectorError(format!(
                "'{selector}' is a {} selector -- history targets a cell or proof step",
                resolved.kind
            ))
            .into());
        }
        let pane = history_pane(&client, &resolved)?;
        if pane["active"].as_bool().unwrap_or(false) {
            let content = pane["content"].as_str().unwrap_or_default();
            print_history_content(content);
        } else {
            let (content, _found) = history_ghost_or_output(&client, &resolved)?;
            print_history_content(&content);
        }
        Ok(())
    })
}

fn print_history_content(content: &str) {
    if content.is_empty() {
        println!("(no history recorded yet)");
        return;
    }
    // RAL-247: defense-in-depth — the daemon redacts at its read/serve paths,
    // but a CLI talking to an older daemon must still never print a
    // credential value to the terminal.
    let redacted = ralphus_core::redact::redact_secrets(content);
    if content.ends_with('\n') {
        print!("{redacted}");
    } else {
        println!("{redacted}");
    }
}

fn history_pane(client: &DaemonClient, resolved: &ResolvedSelector) -> Result<Value, DaemonError> {
    if resolved.kind == "cell" {
        client.cell_pane(
            &resolved.squad_id,
            resolved.task_idx,
            resolved.cell_idx,
            20000,
        )
    } else {
        client.proof_pane(
            &resolved.squad_id,
            resolved.task_idx,
            &resolved.proof_scope,
            resolved.cell_idx,
            resolved.proof_idx,
            20000,
        )
    }
}

fn history_ghost_or_output(
    client: &DaemonClient,
    resolved: &ResolvedSelector,
) -> Result<(String, bool), DaemonError> {
    if resolved.kind == "cell" {
        let uri = format!(
            "cell:{}:{}:{}",
            resolved.squad_id, resolved.task_idx, resolved.cell_idx
        );
        match client.ghost_get(&uri) {
            Ok(ghost) => Ok((
                ghost["content"].as_str().unwrap_or_default().to_string(),
                true,
            )),
            Err(e) if e.status_code == Some(404) => Ok((String::new(), false)),
            Err(e) => Err(e),
        }
    } else {
        let squad = client.squad(&resolved.squad_id)?;
        let output = proof_step_for(&squad, resolved)["output"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        let found = !output.is_empty();
        Ok((output, found))
    }
}

// ---- listen -----------------------------------------------------------------

pub fn parse_listen(scanner: &mut Scanner) -> super::Command {
    let until = scanner.take_value("--until").ok().flatten();
    let timeout = scanner.take_parsed::<f64>("--timeout").ok().flatten();
    let selector = scanner.clone().remaining().into_iter().next();
    match (selector, until) {
        (Some(selector), Some(until)) => super::Command::Listen {
            selector,
            until,
            timeout,
        },
        _ => super::Command::UsageError(
            "listen requires a <selector> argument and --until STATUS".to_string(),
        ),
    }
}

/// Polls `selector` until it reaches `target` status (case-insensitive),
/// printing the final status and returning 0, or giving up after `timeout`
/// seconds (exit 1). Never panics: selector/daemon errors are handled here.
pub fn cmd_listen(opts: &GlobalOpts, selector: &str, until: &str, timeout: Option<f64>) -> i32 {
    let client = opts.client();
    let target = until.to_lowercase();
    let start = std::time::Instant::now();
    loop {
        match listen_status(&client, selector) {
            Ok((kind, status)) => {
                if status.to_lowercase() == target {
                    emit(
                        opts,
                        &serde_json::json!({"selector": selector, "kind": kind, "status": status}),
                        |d| {
                            println!(
                                "{}: {}",
                                d["selector"].as_str().unwrap_or_default(),
                                d["status"].as_str().unwrap_or_default()
                            );
                        },
                    );
                    return 0;
                }
            }
            Err(e) => {
                e.print(opts.json, None);
                return e.exit_code();
            }
        }
        if let Some(t) = timeout {
            if start.elapsed().as_secs_f64() >= t {
                println!("error: timed out after {t}s waiting for '{until}'");
                return 1;
            }
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

fn listen_status(client: &DaemonClient, selector: &str) -> Result<(String, String), CommandError> {
    if looks_like_guardian_selector(selector) {
        let resolved = selector::resolve_guardian_selector(
            client,
            selector,
            selector::DEFAULT_REVIEW_LIST_HINT,
        )?;
        let guardian = client.guardian_get(&resolved.guardian_id)?;
        if let Some(branch_id) = &resolved.branch_id {
            let branch = guardian["branches"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|b| b["id"].as_str() == Some(branch_id.as_str()))
                .ok_or_else(|| SelectorError(format!("no branch '{branch_id}' in this review")))?;
            return Ok((
                "review-worktree".to_string(),
                branch["merge_status"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
            ));
        }
        return Ok((
            "review".to_string(),
            guardian["status"].as_str().unwrap_or_default().to_string(),
        ));
    }
    let resolved = selector::resolve_squad_selector(client, selector)?;
    let squad = client.squad(&resolved.squad_id)?;
    let (kind, status) =
        match resolved.kind.as_str() {
            "squad" => (
                "squad",
                squad["state"].as_str().unwrap_or_default().to_string(),
            ),
            "task" => (
                "task",
                squad["tasks"][resolved.task_idx as usize]["state"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
            ),
            "cell" => {
                (
                    "cell",
                    squad["tasks"][resolved.task_idx as usize]["cells"][resolved.cell_idx as usize]
                        ["state"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string(),
                )
            }
            _ => (
                "proof",
                proof_step_for(&squad, &resolved)["state"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
            ),
        };
    Ok((kind.to_string(), status))
}

// ---- retry ------------------------------------------------------------------

pub fn cmd_retry(opts: &GlobalOpts, squad_id: &str) -> i32 {
    let client = opts.client();
    run_and_report(opts, None, || {
        let result = client.retry_squad(squad_id)?;
        emit(opts, &result, |r| println!("{squad_id} -> {}", r["state"]));
        Ok(())
    })
}

// ---- clear ------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct ClearArgs {
    pub status: Option<String>,
    pub all: bool,
    pub yes: bool,
    pub keep_temporary: bool,
}

pub fn parse_clear(scanner: &mut Scanner) -> super::Command {
    let status = scanner.take_value("--status").ok().flatten();
    let all = scanner.take_bool("--all");
    let yes = scanner.take_bool("--yes");
    let keep_temporary = scanner.take_bool("--keep-temporary");
    super::Command::Clear(ClearArgs {
        status,
        all,
        yes,
        keep_temporary,
    })
}

pub fn cmd_clear(opts: &GlobalOpts, args: ClearArgs) -> i32 {
    let mut states: Vec<String> = Vec::new();
    if let Some(status) = &args.status {
        states = status
            .split(',')
            .map(|s| s.trim().to_lowercase())
            .filter(|s| !s.is_empty())
            .collect();
        let invalid: Vec<&String> = states
            .iter()
            .filter(|s| !SQUAD_STATES.contains(&s.as_str()))
            .collect();
        if !invalid.is_empty() {
            println!(
                "error: unknown status {} (valid: {})",
                invalid
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
                SQUAD_STATES.join(", ")
            );
            return 2;
        }
    }
    if !args.all && states.is_empty() {
        println!("error: pass --all to clear everything, or --status to filter by squad state");
        return 2;
    }
    if !args.yes {
        let what = if states.is_empty() {
            "ALL tasks and reviews".to_string()
        } else {
            format!("squads in states [{}]", states.join(", "))
        };
        print!("Delete {what}? This cannot be undone. [y/N] ");
        use std::io::Write as _;
        let _ = std::io::stdout().flush();
        let mut answer = String::new();
        let _ = std::io::stdin().read_line(&mut answer);
        if !matches!(answer.trim().to_lowercase().as_str(), "y" | "yes") {
            println!("aborted");
            return 1;
        }
    }
    let client = opts.client();
    run_and_report(opts, None, || {
        let states_opt = if states.is_empty() {
            None
        } else {
            Some(states.as_slice())
        };
        let result = client.clear(states_opt, args.keep_temporary)?;
        emit(opts, &result, |r| {
            println!(
                "cleared {} squad(s), {} review(s); {} worktree(s) purged",
                r["squads_deleted"].as_i64().unwrap_or(0),
                r["guardians_deleted"].as_i64().unwrap_or(0),
                r["worktrees_purged"].as_i64().unwrap_or(0)
            );
        });
        Ok(())
    })
}

// ---- check health -----------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct CheckArgs {
    pub enable_developer_checks: bool,
}

pub fn parse_check(scanner: &mut Scanner) -> super::Command {
    let tail = scanner.clone().remaining();
    let mut inner = Scanner::new(if tail.first().map(String::as_str) == Some("health") {
        &tail[1..]
    } else {
        &tail[..]
    });
    let enable_developer_checks = inner.take_bool("--enable-developer-checks");
    super::Command::Check(CheckArgs {
        enable_developer_checks,
    })
}

pub fn cmd_check(opts: &GlobalOpts, args: CheckArgs) -> i32 {
    let symbol = |s: &str| match s {
        "pass" => "OK  ",
        "warn" => "WARN",
        "fail" => "FAIL",
        _ => "?",
    };
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let results = crate::health::run_checks(&opts.daemon_url, &cwd, args.enable_developer_checks);
    for (section, title) in [
        (crate::health::CORE, "Core"),
        (crate::health::DEVELOPER, "Developer"),
    ] {
        let section_results: Vec<_> = results.iter().filter(|r| r.section == section).collect();
        if section_results.is_empty() {
            continue;
        }
        println!("{title}:");
        for r in section_results {
            println!("  [{}] {}: {}", symbol(r.status), r.name, r.detail);
        }
    }

    let file_issues = crate::config::validate_config_files(&cwd, true);
    if !file_issues.is_empty() {
        println!("\nConfiguration file issues:");
        for fi in &file_issues {
            println!("  {}  ({})", fi.path.display(), fi.label);
            if let Some(syn) = &fi.syntax_error {
                println!("    - TOML syntax error: {syn}");
            }
            for issue in &fi.issues {
                println!("    - {issue}");
            }
        }
    }

    let failed = results.iter().filter(|r| r.is_fail()).count() + file_issues.len();
    if failed > 0 {
        println!("\n{failed} check(s) failed.");
        1
    } else {
        0
    }
}

// ---- completion / configuration / initialize --------------------------------

pub fn cmd_completion() -> i32 {
    println!("# ralphus shell completion is not yet ported in this Rust build.");
    0
}

pub fn cmd_configuration(_opts: &GlobalOpts) -> i32 {
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let config = crate::config::load_config(&cwd, true);

    println!("Sources (in resolution order, later wins):");
    if config.sources.is_empty() {
        println!("  (none)");
    } else {
        for (i, src) in config.sources.iter().enumerate() {
            let label = config
                .source_labels
                .iter()
                .find(|(p, _)| p == src)
                .map(|(_, l)| l.as_str())
                .unwrap_or("unknown");
            println!("  {}. {}  ({label})", i + 1, src.display());
        }
    }

    let cwd_toml = cwd.join(".ralphus.toml");
    if cwd_toml.exists() && !config.sources.contains(&cwd_toml) {
        println!(
            "\nNote: {} exists but is not in the resolution chain.",
            cwd_toml.display()
        );
        println!("      Add it to RALPHUS_CONFIGURATION_PATH to include it.");
    }

    let prov = |key: &str| -> String {
        config
            .provenance
            .iter()
            .find(|(k, _)| *k == key)
            .and_then(|(_, v)| v.as_ref())
            .map_or_else(
                || "default".to_string(),
                |p| format!("from {}", p.display()),
            )
    };

    println!("\nResolved values:");
    println!(
        "  task.maximum_timeout_seconds  = {}  ({})",
        config.task.maximum_timeout_seconds,
        prov("task.maximum_timeout_seconds")
    );
    println!(
        "  daemon.log_path               = {}  ({})",
        config.daemon.log_path.as_deref().unwrap_or("not set"),
        prov("daemon.log_path")
    );
    println!(
        "  daemon.log_level              = {}  ({})",
        config.daemon.log_level.as_deref().unwrap_or("not set"),
        prov("daemon.log_level")
    );
    0
}

pub fn cmd_initialize_git(path: Option<String>) -> i32 {
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
        .output();
    let Ok(probe) = probe else {
        println!("error: could not run git");
        return 2;
    };
    if !probe.status.success() || String::from_utf8_lossy(&probe.stdout).trim() != "true" {
        println!(
            "error: {} is not inside a git working tree (run this from a repository, or pass --path)",
            target.display()
        );
        return 2;
    }
    for (key, value) in [("rerere.enabled", "true"), ("rerere.autoupdate", "true")] {
        let result = std::process::Command::new("git")
            .args(["-C", &target.to_string_lossy(), "config", key, value])
            .output();
        match result {
            Ok(out) if out.status.success() => println!("set {key} = {value}"),
            Ok(out) => {
                println!(
                    "error: git config {key} failed: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                );
                return 1;
            }
            Err(e) => {
                println!("error: git config {key} failed: {e}");
                return 1;
            }
        }
    }
    println!(
        "git rerere enabled: conflict resolutions during review rebases will now be recorded and replayed automatically."
    );
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_matches_star_and_question_and_class() {
        assert!(glob_matches("*.toml", "a.toml"));
        assert!(!glob_matches("*.toml", "a.txt"));
        assert!(glob_matches("a?.toml", "ab.toml"));
        assert!(glob_matches("[ab].toml", "a.toml"));
        assert!(!glob_matches("[ab].toml", "c.toml"));
    }

    #[test]
    fn looks_like_guardian_selector_detects_legacy_forms() {
        assert!(looks_like_guardian_selector("@my-review"));
        assert!(looks_like_guardian_selector("guardian-1~2"));
        assert!(!looks_like_guardian_selector("squad-1/task/0"));
    }

    #[test]
    fn walk_field_indexes_arrays_and_objects() {
        let data = serde_json::json!({"tasks": [{"state": "done"}]});
        assert_eq!(
            walk_field(&data, "tasks.0.state").unwrap(),
            serde_json::json!("done")
        );
        assert!(walk_field(&data, "tasks.5.state").is_err());
    }

    #[test]
    fn resolve_submit_sources_passes_through_explicit_paths() {
        let (sources, is_batch) =
            resolve_submit_sources(&["a.toml".to_string(), "b.toml".to_string()]).unwrap();
        assert_eq!(sources, vec!["a.toml".to_string(), "b.toml".to_string()]);
        assert!(!is_batch);
    }
}
