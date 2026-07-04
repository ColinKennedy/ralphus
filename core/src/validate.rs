//! Task-file validation.
//!
//! Ported (behaviour, not verbatim) from `old:src/tasks/validate.rs`, which the
//! research flagged as the strongest part of the predecessor. It validates the
//! *raw* `toml::Value` rather than the deserialized structs, so it can report
//! unknown keys, wrong types, and precise 1-based line numbers that a plain
//! serde error would not surface.
//!
//! Scope note: cross-run / cross-blob reference resolution (which needs the
//! daemon's database) is deferred to a later phase. This module validates a
//! single submission's structure, types, required fields, mutually-exclusive
//! keys, `restart_on` grammar, and within-task session dependency cycles.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

// ── Public types ────────────────────────────────────────────────────────────

/// The category of a validation finding. Stable machine strings via serde.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    /// A required key is absent.
    MissingRequired,
    /// A key is not recognized at this level.
    UnknownKey,
    /// A key has the wrong TOML value type.
    WrongType,
    /// A value is recognized but not allowed (empty, bad enum, bad grammar).
    InvalidValue,
    /// Two keys that must not coexist were both set.
    ConflictingKeys,
    /// A within-task dependency references something that does not exist.
    IntraTaskRefNotFound,
}

/// A single validation finding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidationError {
    /// Dotted path to the offending element, e.g. `task[0].session[1].cwd`.
    pub path: String,
    /// The category of finding.
    pub kind: ErrorKind,
    /// Human-readable explanation.
    pub message: String,
    /// 1-based source line, when determinable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
}

/// The result of validating a submission: hard errors plus non-fatal warnings.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct ValidationReport {
    /// Findings that make the submission invalid.
    pub errors: Vec<ValidationError>,
    /// Non-fatal findings.
    pub warnings: Vec<ValidationError>,
}

impl ValidationReport {
    /// An empty report.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// True when there are no hard errors.
    #[must_use]
    pub fn is_ok(&self) -> bool {
        self.errors.is_empty()
    }

    fn error(&mut self, path: &str, kind: ErrorKind, msg: impl Into<String>, line: Option<u32>) {
        self.errors.push(ValidationError {
            path: path.to_string(),
            kind,
            message: msg.into(),
            line,
        });
    }
}

/// Shared validation context: the raw source, the line index, and the report
/// being built. Bundled so helper functions take one `ctx` instead of threading
/// three parameters through every call.
struct Ctx<'a> {
    raw: &'a str,
    idx: HeaderIndex,
    report: &'a mut ValidationReport,
}

impl Ctx<'_> {
    fn error(&mut self, path: &str, kind: ErrorKind, msg: impl Into<String>, line: Option<u32>) {
        self.report.error(path, kind, msg, line);
    }

    fn key_line(&self, header: Option<u32>, key: &str) -> Option<u32> {
        self.idx.find_key_line(self.raw, header, key)
    }
}

/// Validate a single submission's TOML text.
#[must_use]
pub fn validate_toml(raw: &str) -> ValidationReport {
    let mut report = ValidationReport::new();

    let value = match toml::from_str::<toml::Value>(raw) {
        Ok(v) => v,
        Err(e) => {
            let line = e.span().map(|s| byte_to_line(raw, s.start));
            report.error(
                "<file>",
                ErrorKind::InvalidValue,
                format!("TOML parse error: {e}"),
                line,
            );
            return report;
        }
    };
    let Some(table) = value.as_table() else {
        report.error(
            "<file>",
            ErrorKind::WrongType,
            "top level must be a table",
            Some(1),
        );
        return report;
    };

    let idx = HeaderIndex::scan(raw);
    let mut ctx = Ctx {
        raw,
        idx,
        report: &mut report,
    };

    for key in table.keys() {
        if key != "default" && key != "task" {
            let line = ctx.idx.find_toplevel_key(ctx.raw, key);
            ctx.error(
                key,
                ErrorKind::UnknownKey,
                format!("unknown top-level key \"{key}\""),
                line,
            );
        }
    }

    validate_defaults(table.get("default"), &mut ctx);
    validate_tasks(table.get("task"), &mut ctx);

    report
}

// ── Allowed key sets (mirror old:src/tasks/validate.rs) ──────────────────────

const DEFAULT_KEYS: &[&str] = &["depends_on"];
const TASK_KEYS: &[&str] = &[
    "name",
    "project",
    "agent",
    "model",
    "args",
    "budget_usd",
    "max_retries",
    "timeout_min",
    "depends_on",
    "session",
    "verify",
];
const SESSION_KEYS: &[&str] = &[
    "id",
    "role",
    "cwd",
    "prompt",
    "command",
    "depends_on",
    "agent",
    "model",
    "args",
    "budget_usd",
    "verify",
    "review",
];
const REVIEW_KEYS: &[&str] = &["id", "name", "base"];
const VERIFY_KEYS: &[&str] = &[
    "id",
    "command",
    "brain",
    "agent",
    "model",
    "arguments",
    "budget_usd",
    "requires_approval",
    "restart_on",
];

// ── Type expectations ────────────────────────────────────────────────────────

#[derive(Clone, Copy)]
enum Ty {
    Str,
    Bool,
    Int,
    Num,
    StrArray,
}

fn type_name(v: &toml::Value) -> &'static str {
    match v {
        toml::Value::String(_) => "string",
        toml::Value::Integer(_) => "integer",
        toml::Value::Float(_) => "float",
        toml::Value::Boolean(_) => "boolean",
        toml::Value::Datetime(_) => "datetime",
        toml::Value::Array(_) => "array",
        toml::Value::Table(_) => "table",
    }
}

/// Check a key's type if present. Emits `WrongType` on mismatch.
fn check_type(
    ctx: &mut Ctx,
    table: &toml::Table,
    key: &str,
    ty: Ty,
    path: &str,
    header: Option<u32>,
) {
    let Some(v) = table.get(key) else { return };
    let ok = match ty {
        Ty::Str => v.is_str(),
        Ty::Bool => v.is_bool(),
        Ty::Int => v.is_integer(),
        Ty::Num => v.is_integer() || v.is_float(),
        Ty::StrArray => v
            .as_array()
            .is_some_and(|a| a.iter().all(toml::Value::is_str)),
    };
    if ok {
        return;
    }
    let expected = match ty {
        Ty::Str => "string",
        Ty::Bool => "boolean",
        Ty::Int => "integer",
        Ty::Num => "number",
        Ty::StrArray => "array of strings",
    };
    let line = ctx.key_line(header, key);
    ctx.error(
        &format!("{path}.{key}"),
        ErrorKind::WrongType,
        format!("key \"{key}\" must be a {expected}, found {}", type_name(v)),
        line,
    );
}

fn unknown_keys(
    ctx: &mut Ctx,
    table: &toml::Table,
    allowed: &[&str],
    path: &str,
    header: Option<u32>,
) {
    for key in table.keys() {
        if !allowed.contains(&key.as_str()) {
            let line = ctx.key_line(header, key);
            ctx.error(
                &format!("{path}.{key}"),
                ErrorKind::UnknownKey,
                format!("unknown key \"{key}\""),
                line,
            );
        }
    }
}

// ── [[default]] ──────────────────────────────────────────────────────────────

fn validate_defaults(value: Option<&toml::Value>, ctx: &mut Ctx) {
    let Some(value) = value else { return };
    let Some(arr) = value.as_array() else {
        ctx.error(
            "default",
            ErrorKind::WrongType,
            "[[default]] must be an array of tables",
            Some(1),
        );
        return;
    };
    for (d, item) in arr.iter().enumerate() {
        let path = format!("default[{d}]");
        let Some(table) = item.as_table() else {
            ctx.error(
                &path,
                ErrorKind::WrongType,
                "each [[default]] must be a table",
                None,
            );
            continue;
        };
        let header = ctx.idx.default_line(d);
        unknown_keys(ctx, table, DEFAULT_KEYS, &path, header);
        check_type(ctx, table, "depends_on", Ty::StrArray, &path, header);
    }
}

// ── [[task]] ─────────────────────────────────────────────────────────────────

fn validate_tasks(value: Option<&toml::Value>, ctx: &mut Ctx) {
    let Some(value) = value else {
        ctx.error(
            "task",
            ErrorKind::MissingRequired,
            "at least one [[task]] is required",
            Some(1),
        );
        return;
    };
    let Some(arr) = value.as_array() else {
        ctx.error(
            "task",
            ErrorKind::WrongType,
            "[[task]] must be an array of tables",
            Some(1),
        );
        return;
    };
    if arr.is_empty() {
        ctx.error(
            "task",
            ErrorKind::MissingRequired,
            "at least one [[task]] is required",
            Some(1),
        );
        return;
    }

    let mut task_names: HashSet<String> = HashSet::new();
    for (t, item) in arr.iter().enumerate() {
        let path = format!("task[{t}]");
        let Some(table) = item.as_table() else {
            ctx.error(
                &path,
                ErrorKind::WrongType,
                "each [[task]] must be a table",
                None,
            );
            continue;
        };
        let header = ctx.idx.task_line(t);
        unknown_keys(ctx, table, TASK_KEYS, &path, header);

        match table.get("name") {
            None => ctx.error(
                &path,
                ErrorKind::MissingRequired,
                "task requires a 'name'",
                header,
            ),
            Some(toml::Value::String(s)) if s.trim().is_empty() => {
                let line = ctx.key_line(header, "name");
                ctx.error(
                    &format!("{path}.name"),
                    ErrorKind::InvalidValue,
                    "task 'name' must not be empty",
                    line,
                );
            }
            Some(toml::Value::String(s)) => {
                if !task_names.insert(s.clone()) {
                    let line = ctx.key_line(header, "name");
                    ctx.error(
                        &format!("{path}.name"),
                        ErrorKind::InvalidValue,
                        format!("duplicate task name \"{s}\""),
                        line,
                    );
                }
            }
            Some(_) => check_type(ctx, table, "name", Ty::Str, &path, header),
        }

        check_type(ctx, table, "project", Ty::Str, &path, header);
        check_type(ctx, table, "agent", Ty::Str, &path, header);
        check_type(ctx, table, "model", Ty::Str, &path, header);
        check_type(ctx, table, "args", Ty::StrArray, &path, header);
        check_type(ctx, table, "budget_usd", Ty::Num, &path, header);
        check_type(ctx, table, "max_retries", Ty::Int, &path, header);
        check_type(ctx, table, "timeout_min", Ty::Int, &path, header);
        check_type(ctx, table, "depends_on", Ty::StrArray, &path, header);

        validate_sessions(table.get("session"), t, &path, ctx);
        validate_verify_array(table.get("verify"), &format!("{path}.verify"), ctx);
    }
}

// ── [[task.session]] ─────────────────────────────────────────────────────────

fn validate_sessions(value: Option<&toml::Value>, task_idx: usize, task_path: &str, ctx: &mut Ctx) {
    let Some(value) = value else { return };
    let Some(arr) = value.as_array() else {
        ctx.error(
            &format!("{task_path}.session"),
            ErrorKind::WrongType,
            "session must be an array of tables",
            None,
        );
        return;
    };

    let mut ids: HashMap<String, usize> = HashMap::new();
    let mut dep_edges: Vec<(usize, String)> = Vec::new();

    for (s, item) in arr.iter().enumerate() {
        let path = format!("{task_path}.session[{s}]");
        let Some(table) = item.as_table() else {
            ctx.error(
                &path,
                ErrorKind::WrongType,
                "each session must be a table",
                None,
            );
            continue;
        };
        let header = ctx.idx.session_line(task_idx, s);
        unknown_keys(ctx, table, SESSION_KEYS, &path, header);

        match table.get("cwd") {
            None => ctx.error(
                &path,
                ErrorKind::MissingRequired,
                "'cwd' is required for every session (absolute path to the worktree)",
                header,
            ),
            Some(toml::Value::String(c)) if c.trim().is_empty() => {
                let line = ctx.key_line(header, "cwd");
                ctx.error(
                    &format!("{path}.cwd"),
                    ErrorKind::InvalidValue,
                    "'cwd' must not be empty",
                    line,
                );
            }
            Some(v) if !v.is_str() => check_type(ctx, table, "cwd", Ty::Str, &path, header),
            Some(_) => {}
        }

        let has_prompt = table.contains_key("prompt");
        let has_command = table.contains_key("command");
        match (has_prompt, has_command) {
            (true, true) => ctx.error(
                &path,
                ErrorKind::ConflictingKeys,
                "session cannot set both 'prompt' (AI-driven) and 'command' (deterministic); use one",
                header,
            ),
            (false, false) => ctx.error(
                &path,
                ErrorKind::MissingRequired,
                "session requires either 'prompt' (AI-driven) or 'command' (shell command)",
                header,
            ),
            _ => {
                check_type(ctx, table, "prompt", Ty::Str, &path, header);
                check_type(ctx, table, "command", Ty::Str, &path, header);
            }
        }

        if let Some(id_v) = table.get("id") {
            if let Some(id) = id_v.as_str() {
                if id.contains('/') {
                    let line = ctx.key_line(header, "id");
                    ctx.error(
                        &format!("{path}.id"),
                        ErrorKind::InvalidValue,
                        format!("session id \"{id}\" must not contain '/'"),
                        line,
                    );
                } else {
                    ids.insert(id.to_string(), s);
                }
            } else {
                check_type(ctx, table, "id", Ty::Str, &path, header);
            }
        }

        check_type(ctx, table, "role", Ty::Str, &path, header);
        check_type(ctx, table, "agent", Ty::Str, &path, header);
        check_type(ctx, table, "model", Ty::Str, &path, header);
        check_type(ctx, table, "args", Ty::StrArray, &path, header);
        check_type(ctx, table, "budget_usd", Ty::Num, &path, header);
        check_type(ctx, table, "depends_on", Ty::StrArray, &path, header);

        if let Some(deps) = table.get("depends_on").and_then(toml::Value::as_array) {
            for dep in deps.iter().filter_map(toml::Value::as_str) {
                dep_edges.push((s, dep.to_string()));
            }
        }

        validate_verify_array(table.get("verify"), &format!("{path}.verify"), ctx);
        validate_review_array(table.get("review"), &format!("{path}.review"), ctx);
    }

    check_session_deps(&ids, &dep_edges, arr.len(), task_path, task_idx, ctx);
}

/// Validate `[[task.session.review]]` entries. Only the TOML shape is checked
/// here (keys, types, non-empty `base`); whether a `<<upstream>>` base actually
/// resolves against the worktree is a git-aware daemon preflight, not a pure
/// check — see `REVIEWS.local.md`.
fn validate_review_array(value: Option<&toml::Value>, path: &str, ctx: &mut Ctx) {
    let Some(value) = value else { return };
    let Some(arr) = value.as_array() else {
        ctx.error(
            path,
            ErrorKind::WrongType,
            "review must be an array of tables",
            None,
        );
        return;
    };
    for (r, item) in arr.iter().enumerate() {
        let rpath = format!("{path}[{r}]");
        let Some(table) = item.as_table() else {
            ctx.error(
                &rpath,
                ErrorKind::WrongType,
                "each review must be a table",
                None,
            );
            continue;
        };
        unknown_keys(ctx, table, REVIEW_KEYS, &rpath, None);
        check_type(ctx, table, "id", Ty::Str, &rpath, None);
        check_type(ctx, table, "name", Ty::Str, &rpath, None);
        check_type(ctx, table, "base", Ty::Str, &rpath, None);
        if let Some(base) = table.get("base").and_then(toml::Value::as_str) {
            if base.trim().is_empty() {
                ctx.error(
                    &format!("{rpath}.base"),
                    ErrorKind::InvalidValue,
                    "review 'base' must not be empty (a branch name or the '<<upstream>>' sentinel)",
                    None,
                );
            }
        }
    }
}

/// Resolve within-task session dependencies and detect cycles. Cross-task refs
/// (those containing `/`) are left for a later phase to resolve against the DB.
fn check_session_deps(
    ids: &HashMap<String, usize>,
    edges: &[(usize, String)],
    session_count: usize,
    task_path: &str,
    task_idx: usize,
    ctx: &mut Ctx,
) {
    let mut adj: Vec<Vec<usize>> = vec![Vec::new(); session_count];
    for (from, dep) in edges {
        if dep.contains('/') {
            continue; // cross-task ref, deferred.
        }
        match ids.get(dep) {
            Some(&to) => adj[*from].push(to),
            None => {
                let line = ctx.idx.session_line(task_idx, *from);
                ctx.error(
                    &format!("{task_path}.session[{from}].depends_on"),
                    ErrorKind::IntraTaskRefNotFound,
                    format!("depends_on references unknown session id \"{dep}\""),
                    line,
                );
            }
        }
    }

    if has_cycle(&adj) {
        let line = ctx.idx.task_line(task_idx);
        ctx.error(
            &format!("{task_path}.session"),
            ErrorKind::InvalidValue,
            "circular dependency detected among sessions",
            line,
        );
    }
}

/// Standard three-colour DFS cycle detection over an adjacency list.
fn has_cycle(adj: &[Vec<usize>]) -> bool {
    #[derive(Clone, Copy, PartialEq)]
    enum Mark {
        White,
        Gray,
        Black,
    }
    fn dfs(n: usize, adj: &[Vec<usize>], marks: &mut [Mark]) -> bool {
        marks[n] = Mark::Gray;
        for &m in &adj[n] {
            let mark = marks[m];
            if mark == Mark::Gray || (mark == Mark::White && dfs(m, adj, marks)) {
                return true;
            }
        }
        marks[n] = Mark::Black;
        false
    }
    let mut marks = vec![Mark::White; adj.len()];
    (0..adj.len()).any(|n| marks[n] == Mark::White && dfs(n, adj, &mut marks))
}

// ── verify steps ─────────────────────────────────────────────────────────────

fn validate_verify_array(value: Option<&toml::Value>, path: &str, ctx: &mut Ctx) {
    let Some(value) = value else { return };
    let Some(arr) = value.as_array() else {
        ctx.error(
            path,
            ErrorKind::WrongType,
            "verify must be an array of tables",
            None,
        );
        return;
    };
    for (v, item) in arr.iter().enumerate() {
        let vpath = format!("{path}[{v}]");
        let Some(table) = item.as_table() else {
            ctx.error(
                &vpath,
                ErrorKind::WrongType,
                "each verify step must be a table",
                None,
            );
            continue;
        };
        unknown_keys(ctx, table, VERIFY_KEYS, &vpath, None);

        let kinds = ["command", "brain", "agent"];
        let set: Vec<&str> = kinds
            .iter()
            .copied()
            .filter(|k| table.contains_key(*k))
            .collect();
        match set.len() {
            0 => ctx.error(
                &vpath,
                ErrorKind::MissingRequired,
                "verify step requires exactly one of: command, brain, agent",
                None,
            ),
            1 => {}
            _ => ctx.error(
                &vpath,
                ErrorKind::ConflictingKeys,
                format!(
                    "verify step sets multiple kinds ({}); use exactly one",
                    set.join(", ")
                ),
                None,
            ),
        }

        check_type(ctx, table, "requires_approval", Ty::Bool, &vpath, None);
        check_type(ctx, table, "budget_usd", Ty::Num, &vpath, None);
        check_type(ctx, table, "arguments", Ty::StrArray, &vpath, None);
        check_type(ctx, table, "restart_on", Ty::StrArray, &vpath, None);

        if let Some(restarts) = table.get("restart_on").and_then(toml::Value::as_array) {
            for r in restarts.iter().filter_map(toml::Value::as_str) {
                if let Err(msg) = check_restart_grammar(r) {
                    ctx.error(
                        &format!("{vpath}.restart_on"),
                        ErrorKind::InvalidValue,
                        msg,
                        None,
                    );
                }
            }
        }
    }
}

/// Validate a single `restart_on` reference against the grammar
/// `task/session/verify[?on=pass|fail|both]` with `task/*` and `task/session/*`
/// wildcards allowed.
fn check_restart_grammar(spec: &str) -> Result<(), String> {
    let (path, filter) = match spec.split_once("?on=") {
        Some((p, f)) => (p, Some(f)),
        None => (spec, None),
    };
    if let Some(f) = filter {
        if !matches!(f, "pass" | "fail" | "both") {
            return Err(format!(
                "restart_on filter must be on=pass|fail|both, got \"{f}\""
            ));
        }
    }
    let segs: Vec<&str> = path.split('/').collect();
    if segs.iter().any(|s| s.is_empty()) {
        return Err(format!("restart_on \"{spec}\" has an empty path segment"));
    }
    match segs.as_slice() {
        [_task, last] if *last == "*" => Ok(()),
        [_task, _session, _verify] => Ok(()),
        _ => Err(format!(
            "restart_on \"{spec}\" must be task/session/verify or a task/* / task/session/* wildcard"
        )),
    }
}

// ── Line-number index ────────────────────────────────────────────────────────

/// Maps array-of-table indices to their 1-based header line, and finds the line
/// of a scalar key inside a table's body. Built by a single raw-text pass, so it
/// works even for the tables serde would happily accept.
struct HeaderIndex {
    default_lines: Vec<u32>,
    task_lines: Vec<u32>,
    /// (task_idx, session_idx) -> line
    session_lines: HashMap<(usize, usize), u32>,
}

impl HeaderIndex {
    fn scan(raw: &str) -> Self {
        let mut default_lines = Vec::new();
        let mut task_lines = Vec::new();
        let mut session_lines = HashMap::new();
        let mut cur_task: isize = -1;
        let mut cur_session: isize = -1;

        for (i, line) in raw.lines().enumerate() {
            let ln = u32::try_from(i + 1).unwrap_or(u32::MAX);
            match normalize_header(line).as_deref() {
                Some("[[default]]") => default_lines.push(ln),
                Some("[[task]]") => {
                    cur_task += 1;
                    cur_session = -1;
                    task_lines.push(ln);
                }
                Some("[[task.session]]") => {
                    cur_session += 1;
                    if let (Ok(t), Ok(s)) =
                        (usize::try_from(cur_task), usize::try_from(cur_session))
                    {
                        session_lines.insert((t, s), ln);
                    }
                }
                _ => {}
            }
        }
        Self {
            default_lines,
            task_lines,
            session_lines,
        }
    }

    fn default_line(&self, d: usize) -> Option<u32> {
        self.default_lines.get(d).copied()
    }

    fn task_line(&self, t: usize) -> Option<u32> {
        self.task_lines.get(t).copied()
    }

    fn session_line(&self, t: usize, s: usize) -> Option<u32> {
        self.session_lines.get(&(t, s)).copied()
    }

    /// Find the line of `key = ...` in the scalar body starting just after
    /// `header`, stopping at the next `[`-prefixed header. Falls back to the
    /// header line itself when the key can't be located.
    fn find_key_line(&self, raw: &str, header: Option<u32>, key: &str) -> Option<u32> {
        let start = header? as usize; // 1-based header line == index of first body line
        for (i, line) in raw.lines().enumerate().skip(start) {
            let trimmed = line.trim_start();
            if trimmed.starts_with('[') {
                break;
            }
            if key_assignment_matches(trimmed, key) {
                return u32::try_from(i + 1).ok();
            }
        }
        header
    }

    /// Find a top-level key assignment that appears before the first header.
    fn find_toplevel_key(&self, raw: &str, key: &str) -> Option<u32> {
        for (i, line) in raw.lines().enumerate() {
            let trimmed = line.trim_start();
            if trimmed.starts_with('[') {
                break;
            }
            if key_assignment_matches(trimmed, key) {
                return u32::try_from(i + 1).ok();
            }
        }
        None
    }
}

/// True if `line` is `key` followed by optional whitespace and `=`.
fn key_assignment_matches(line: &str, key: &str) -> bool {
    let Some(rest) = line.strip_prefix(key) else {
        return false;
    };
    rest.trim_start().starts_with('=')
}

/// Normalize a potential array-of-table header line by removing inner
/// whitespace, e.g. `[[ task . session ]]` -> `[[task.session]]`. Returns `None`
/// if the line is not an `[[...]]` header.
fn normalize_header(line: &str) -> Option<String> {
    let t = line.trim();
    if !(t.starts_with("[[") && t.ends_with("]]")) {
        return None;
    }
    Some(t.chars().filter(|c| !c.is_whitespace()).collect())
}

/// Convert a byte offset into a 1-based line number.
fn byte_to_line(raw: &str, offset: usize) -> u32 {
    let capped = offset.min(raw.len());
    let line = raw[..capped].bytes().filter(|b| *b == b'\n').count() + 1;
    u32::try_from(line).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(report: &ValidationReport) -> Vec<ErrorKind> {
        report.errors.iter().map(|e| e.kind).collect()
    }

    const GOOD: &str = r#"
[[task]]
name = "build"
[[task.session]]
cwd = "/repo"
prompt = "make it build"
[[task.session.verify]]
command = "cargo build"
"#;

    #[test]
    fn good_file_passes() {
        let r = validate_toml(GOOD);
        assert!(r.is_ok(), "expected ok, got {:?}", r.errors);
    }

    #[test]
    fn empty_requires_a_task() {
        let r = validate_toml("");
        assert!(!r.is_ok());
        assert_eq!(kinds(&r), vec![ErrorKind::MissingRequired]);
    }

    #[test]
    fn unknown_task_key_reports_line() {
        let src =
            "[[task]]\nname = \"t\"\nbudgt_usd = 1.0\n[[task.session]]\ncwd=\"/r\"\nprompt=\"p\"\n";
        let r = validate_toml(src);
        let e = r
            .errors
            .iter()
            .find(|e| e.kind == ErrorKind::UnknownKey)
            .expect("unknown key");
        assert!(e.message.contains("budgt_usd"));
        assert_eq!(e.line, Some(3));
    }

    #[test]
    fn missing_name() {
        let src = "[[task]]\n[[task.session]]\ncwd=\"/r\"\nprompt=\"p\"\n";
        let r = validate_toml(src);
        assert!(
            r.errors
                .iter()
                .any(|e| e.kind == ErrorKind::MissingRequired && e.message.contains("name"))
        );
    }

    #[test]
    fn session_needs_cwd() {
        let src = "[[task]]\nname=\"t\"\n[[task.session]]\nprompt=\"p\"\n";
        let r = validate_toml(src);
        assert!(
            r.errors
                .iter()
                .any(|e| e.kind == ErrorKind::MissingRequired && e.message.contains("cwd"))
        );
    }

    #[test]
    fn prompt_and_command_conflict() {
        let src =
            "[[task]]\nname=\"t\"\n[[task.session]]\ncwd=\"/r\"\nprompt=\"p\"\ncommand=\"c\"\n";
        let r = validate_toml(src);
        assert!(
            r.errors
                .iter()
                .any(|e| e.kind == ErrorKind::ConflictingKeys)
        );
    }

    #[test]
    fn session_needs_prompt_or_command() {
        let src = "[[task]]\nname=\"t\"\n[[task.session]]\ncwd=\"/r\"\n";
        let r = validate_toml(src);
        assert!(
            r.errors
                .iter()
                .any(|e| e.kind == ErrorKind::MissingRequired && e.message.contains("prompt"))
        );
    }

    #[test]
    fn command_only_session_is_valid() {
        let src = "[[task]]\nname=\"t\"\n[[task.session]]\ncwd=\"/r\"\ncommand=\"cargo build\"\n";
        assert!(validate_toml(src).is_ok());
    }

    #[test]
    fn wrong_type_for_budget() {
        let src = "[[task]]\nname=\"t\"\nbudget_usd=\"lots\"\n[[task.session]]\ncwd=\"/r\"\nprompt=\"p\"\n";
        let r = validate_toml(src);
        assert!(
            r.errors
                .iter()
                .any(|e| e.kind == ErrorKind::WrongType && e.message.contains("budget_usd"))
        );
    }

    #[test]
    fn verify_needs_exactly_one_kind() {
        let none = "[[task]]\nname=\"t\"\n[[task.session]]\ncwd=\"/r\"\nprompt=\"p\"\n[[task.session.verify]]\nid=\"v\"\n";
        assert!(
            validate_toml(none)
                .errors
                .iter()
                .any(|e| e.kind == ErrorKind::MissingRequired && e.message.contains("exactly one"))
        );

        let many = "[[task]]\nname=\"t\"\n[[task.session]]\ncwd=\"/r\"\nprompt=\"p\"\n[[task.session.verify]]\ncommand=\"c\"\nbrain=\"b\"\n";
        assert!(
            validate_toml(many)
                .errors
                .iter()
                .any(|e| e.kind == ErrorKind::ConflictingKeys)
        );
    }

    #[test]
    fn duplicate_task_names() {
        let src = "[[task]]\nname=\"t\"\n[[task.session]]\ncwd=\"/r\"\nprompt=\"p\"\n[[task]]\nname=\"t\"\n[[task.session]]\ncwd=\"/r\"\nprompt=\"p\"\n";
        let r = validate_toml(src);
        assert!(
            r.errors
                .iter()
                .any(|e| e.kind == ErrorKind::InvalidValue && e.message.contains("duplicate"))
        );
    }

    #[test]
    fn intra_task_dep_not_found() {
        let src = "[[task]]\nname=\"t\"\n[[task.session]]\nid=\"a\"\ncwd=\"/r\"\nprompt=\"p\"\ndepends_on=[\"ghost\"]\n";
        let r = validate_toml(src);
        assert!(
            r.errors
                .iter()
                .any(|e| e.kind == ErrorKind::IntraTaskRefNotFound)
        );
    }

    #[test]
    fn cross_task_dep_is_not_flagged_here() {
        let src = "[[task]]\nname=\"t\"\n[[task.session]]\nid=\"a\"\ncwd=\"/r\"\nprompt=\"p\"\ndepends_on=[\"other/b\"]\n";
        let r = validate_toml(src);
        assert!(
            !r.errors
                .iter()
                .any(|e| e.kind == ErrorKind::IntraTaskRefNotFound)
        );
    }

    #[test]
    fn dependency_cycle_detected() {
        let src = "[[task]]\nname=\"t\"\n[[task.session]]\nid=\"a\"\ncwd=\"/r\"\nprompt=\"p\"\ndepends_on=[\"b\"]\n[[task.session]]\nid=\"b\"\ncwd=\"/r\"\nprompt=\"p\"\ndepends_on=[\"a\"]\n";
        let r = validate_toml(src);
        assert!(r.errors.iter().any(|e| e.message.contains("circular")));
    }

    #[test]
    fn restart_on_grammar() {
        assert!(check_restart_grammar("a/b/c").is_ok());
        assert!(check_restart_grammar("a/b/c?on=pass").is_ok());
        assert!(check_restart_grammar("a/*").is_ok());
        assert!(check_restart_grammar("a/b/*").is_ok());
        assert!(check_restart_grammar("a/b/c?on=maybe").is_err());
        assert!(check_restart_grammar("a").is_err());
        assert!(check_restart_grammar("a//c").is_err());
    }

    #[test]
    fn restart_on_bad_grammar_reported() {
        let src = "[[task]]\nname=\"t\"\n[[task.session]]\ncwd=\"/r\"\nprompt=\"p\"\n[[task.session.verify]]\ncommand=\"c\"\nrestart_on=[\"nope\"]\n";
        let r = validate_toml(src);
        assert!(
            r.errors
                .iter()
                .any(|e| e.kind == ErrorKind::InvalidValue && e.message.contains("restart_on"))
        );
    }

    #[test]
    fn review_block_is_valid() {
        let src = "[[task]]\nname=\"t\"\n[[task.session]]\ncwd=\"/r\"\nprompt=\"p\"\n[[task.session.review]]\nid=\"be\"\nbase=\"<<upstream>>\"\n";
        assert!(
            validate_toml(src).is_ok(),
            "{:?}",
            validate_toml(src).errors
        );
    }

    #[test]
    fn review_unknown_key_reported() {
        let src = "[[task]]\nname=\"t\"\n[[task.session]]\ncwd=\"/r\"\nprompt=\"p\"\n[[task.session.review]]\nbranch=\"x\"\n";
        let r = validate_toml(src);
        assert!(
            r.errors
                .iter()
                .any(|e| e.kind == ErrorKind::UnknownKey && e.message.contains("branch"))
        );
    }

    #[test]
    fn review_empty_base_reported() {
        let src = "[[task]]\nname=\"t\"\n[[task.session]]\ncwd=\"/r\"\nprompt=\"p\"\n[[task.session.review]]\nbase=\"\"\n";
        let r = validate_toml(src);
        assert!(
            r.errors
                .iter()
                .any(|e| e.kind == ErrorKind::InvalidValue && e.message.contains("base"))
        );
    }

    #[test]
    fn review_wrong_type_reported() {
        let src = "[[task]]\nname=\"t\"\n[[task.session]]\ncwd=\"/r\"\nprompt=\"p\"\n[[task.session.review]]\nid=123\n";
        let r = validate_toml(src);
        assert!(r.errors.iter().any(|e| e.kind == ErrorKind::WrongType));
    }

    #[test]
    fn parse_error_has_line() {
        let src = "[[task]\nname = \"t\"\n";
        let r = validate_toml(src);
        assert!(!r.is_ok());
        assert_eq!(r.errors[0].kind, ErrorKind::InvalidValue);
    }

    #[test]
    fn report_serializes_to_json() {
        let r = validate_toml("");
        let json = serde_json::to_string(&r).expect("serialize");
        assert!(json.contains("missing_required"));
    }
}
