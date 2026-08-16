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
        if key != "default" && key != "task" && key != "review" {
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
    validate_review_blocks(table.get("review"), &mut ctx);

    report
}

// ── Allowed key sets (mirror old:src/tasks/validate.rs) ──────────────────────

const DEFAULT_KEYS: &[&str] = &["depends_on"];
const TASK_KEYS: &[&str] = &[
    "name",
    "project",
    "agent",
    "model",
    "machine",
    "args",
    "budget_tokens",
    "maximum_budget_usd",
    "max_retries",
    "priority",
    "timeout_minutes",
    "depends_on",
    "environment",
    "session",
    "verify",
];
const SESSION_KEYS: &[&str] = &[
    "id",
    "name",
    "role",
    "cwd",
    "subprojects",
    "prompt",
    "command",
    "depends_on",
    "agent",
    "model",
    "machine",
    "system_prompt",
    "system_prompt_position",
    "args",
    "budget_tokens",
    "maximum_budget_usd",
    "timeout_minutes",
    "priority",
    "environment",
    "verify",
    "review",
    "upstream",
];
const REVIEW_KEYS: &[&str] = &["id", "name", "agent", "model", "machine", "base", "action"];
const REVIEW_ACTION_KEYS: &[&str] = &["label", "prompt", "command", "cleanup_command", "input"];
const REVIEW_ACTION_INPUT_KEYS: &[&str] = &["name", "message", "default"];
const VERIFY_KEYS: &[&str] = &[
    "id",
    "command",
    "brain",
    "prompt",
    "model",
    "machine",
    "system_prompt",
    "system_prompt_position",
    "arguments",
    "budget_tokens",
    "timeout_minutes",
    "requires_approval",
    "restart_on",
];

// ── Type expectations ────────────────────────────────────────────────────────

#[derive(Clone, Copy)]
enum Ty {
    Str,
    Bool,
    Int,
    Float,
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
        Ty::Float => v.is_float() || v.is_integer(),
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
        Ty::Float => "number",
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

/// Reject a numeric key whose value is present, well-typed, and `<= 0`.
/// Silently returns for a missing or wrong-typed value -- `check_type` already
/// reports the latter separately.
fn check_positive_number(
    ctx: &mut Ctx,
    table: &toml::Table,
    key: &str,
    path: &str,
    header: Option<u32>,
) {
    let Some(v) = table.get(key) else { return };
    let Some(n) = v.as_float().or_else(|| v.as_integer().map(|i| i as f64)) else {
        return;
    };
    if n <= 0.0 {
        let line = ctx.key_line(header, key);
        ctx.error(
            &format!("{path}.{key}"),
            ErrorKind::InvalidValue,
            format!("'{key}' must be greater than 0"),
            line,
        );
    }
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
        check_machine(ctx, table, &path, header);
        check_type(ctx, table, "args", Ty::StrArray, &path, header);
        check_type(ctx, table, "budget_tokens", Ty::Int, &path, header);
        check_type(ctx, table, "maximum_budget_usd", Ty::Float, &path, header);
        check_positive_number(ctx, table, "maximum_budget_usd", &path, header);
        check_type(ctx, table, "max_retries", Ty::Int, &path, header);
        check_type(ctx, table, "priority", Ty::Int, &path, header);
        check_type(ctx, table, "timeout_minutes", Ty::Int, &path, header);
        check_type(ctx, table, "depends_on", Ty::StrArray, &path, header);
        check_environment(ctx, table, &path, header);

        let task_agent = table.get("agent").and_then(toml::Value::as_str);
        let task_project = table.get("project").and_then(toml::Value::as_str);
        validate_sessions(
            table.get("session"),
            t,
            &path,
            task_agent,
            task_project,
            header,
            ctx,
        );
        validate_verify_array(table.get("verify"), &format!("{path}.verify"), ctx);
    }
}

// ── [[task.session]] ─────────────────────────────────────────────────────────

fn validate_sessions(
    value: Option<&toml::Value>,
    task_idx: usize,
    task_path: &str,
    task_agent: Option<&str>,
    task_project: Option<&str>,
    task_header: Option<u32>,
    ctx: &mut Ctx,
) {
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
    let mut has_placeholder = false;

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
            Some(v) => {
                // A non-empty string (the two guarded arms above handled empty /
                // non-string cases): check whether it's a worktree placeholder.
                if v.as_str()
                    .and_then(crate::schema::parse_worktree_placeholder)
                    .is_some()
                {
                    has_placeholder = true;
                }
            }
        }

        check_type(ctx, table, "subprojects", Ty::StrArray, &path, header);
        check_subprojects(ctx, table, &path, header);

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
        check_machine(ctx, table, &path, header);
        check_type(ctx, table, "system_prompt", Ty::Str, &path, header);
        check_type(ctx, table, "system_prompt_position", Ty::Str, &path, header);
        check_system_prompt(ctx, table, task_agent, &path, header);
        check_type(ctx, table, "args", Ty::StrArray, &path, header);
        check_type(ctx, table, "budget_tokens", Ty::Int, &path, header);
        check_type(ctx, table, "maximum_budget_usd", Ty::Float, &path, header);
        check_positive_number(ctx, table, "maximum_budget_usd", &path, header);
        check_type(ctx, table, "timeout_minutes", Ty::Int, &path, header);
        check_type(ctx, table, "priority", Ty::Int, &path, header);
        check_type(ctx, table, "depends_on", Ty::StrArray, &path, header);
        check_environment(ctx, table, &path, header);

        if let Some(deps) = table.get("depends_on").and_then(toml::Value::as_array) {
            for dep in deps.iter().filter_map(toml::Value::as_str) {
                dep_edges.push((s, dep.to_string()));
            }
        }

        check_type(ctx, table, "upstream", Ty::Str, &path, header);
        check_upstream(ctx, table, &path, header);

        check_type(ctx, table, "review", Ty::Str, &path, header);

        validate_verify_array(table.get("verify"), &format!("{path}.verify"), ctx);
    }

    if has_placeholder && task_project.is_none() {
        ctx.error(
            task_path,
            ErrorKind::MissingRequired,
            "task 'project' is required when any session uses a placeholder cwd \
             (\"ralphus:new-worktree/<branch>\")",
            task_header,
        );
    }

    check_session_deps(&ids, &dep_edges, arr.len(), task_path, task_idx, ctx);
}

/// Validate the `subprojects` array (RAL-23): each element must be a
/// non-empty relative path with no `..` components so it cannot escape the
/// repository root.
fn check_subprojects(ctx: &mut Ctx, table: &toml::Table, path: &str, header: Option<u32>) {
    let Some(arr) = table.get("subprojects").and_then(toml::Value::as_array) else {
        return;
    };
    for sp in arr.iter().filter_map(toml::Value::as_str) {
        if sp.trim().is_empty() {
            let line = ctx.key_line(header, "subprojects");
            ctx.error(
                &format!("{path}.subprojects"),
                ErrorKind::InvalidValue,
                "'subprojects' entries must not be empty strings",
                line,
            );
        } else if sp.starts_with('/') || sp.starts_with('\\') {
            let line = ctx.key_line(header, "subprojects");
            ctx.error(
                &format!("{path}.subprojects"),
                ErrorKind::InvalidValue,
                "'subprojects' entries must be relative paths (no leading '/' or '\\')",
                line,
            );
        } else if sp.split(['/', '\\']).any(|seg| seg == "..") {
            let line = ctx.key_line(header, "subprojects");
            ctx.error(
                &format!("{path}.subprojects"),
                ErrorKind::InvalidValue,
                "'subprojects' entries must not contain '..' path components",
                line,
            );
        }
    }
}

/// Whether `key` is a syntactically valid environment-variable name
/// (`[A-Za-z_][A-Za-z0-9_]*`). Mirrors `daemon::config::is_valid_env_key`
/// exactly (`core` cannot depend on `daemon`, so the check is duplicated
/// rather than shared) -- both exist to stop an env-override key from
/// smuggling shell metacharacters into the `$env:KEY = ...;` prefix
/// `daemon::tmux::build_command_line_with_env` embeds ahead of a
/// tmux-wrapped runner invocation (RAL-172, RAL-150).
fn is_valid_env_key(key: &str) -> bool {
    let mut chars = key.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Validate an `environment` table (RAL-172): must be a table whose values
/// are all strings and whose keys are all valid environment-variable
/// identifiers. Silently returns when the key is absent.
fn check_environment(ctx: &mut Ctx, table: &toml::Table, path: &str, header: Option<u32>) {
    let Some(v) = table.get("environment") else {
        return;
    };
    let Some(env_table) = v.as_table() else {
        let line = ctx.key_line(header, "environment");
        ctx.error(
            &format!("{path}.environment"),
            ErrorKind::WrongType,
            format!(
                "key \"environment\" must be a table of string values, found {}",
                type_name(v)
            ),
            line,
        );
        return;
    };
    for (key, value) in env_table {
        if !value.is_str() {
            let line = ctx.key_line(header, "environment");
            ctx.error(
                &format!("{path}.environment.{key}"),
                ErrorKind::WrongType,
                format!(
                    "environment value for \"{key}\" must be a string, found {}",
                    type_name(value)
                ),
                line,
            );
        }
        if !is_valid_env_key(key) {
            let line = ctx.key_line(header, "environment");
            ctx.error(
                &format!("{path}.environment.{key}"),
                ErrorKind::InvalidValue,
                format!(
                    "environment key \"{key}\" is not a valid identifier \
                     (must match [A-Za-z_][A-Za-z0-9_]*)"
                ),
                line,
            );
        }
    }
}

/// Enforce the appended-system-prompt rules (RAL-5). `system_prompt` and
/// `system_prompt_position` are only accepted for backends with a real
/// delivery mechanism — see
/// [`agent_supports_system_prompt`](crate::schema::agent_supports_system_prompt)
/// — and the position, when set, must be the `"append"` sentinel. The
/// effective agent is the session's own `agent`, falling back to the
/// task-level `agent`, then [`DEFAULT_AGENT`](crate::schema::DEFAULT_AGENT).
/// Validate a `machine` value's *syntax* (RAL-185).
///
/// Type-checks it as a string, then requires it to be either the literal
/// `"local"` or a well-formed `scheme:uri`. Whether `scheme` names a
/// *registered* provider is deliberately NOT checked here: the registry lives
/// in the daemon's store, which `core` cannot see, so that check happens at
/// submit time — the same split already used for a task's `project`.
fn check_machine(ctx: &mut Ctx, table: &toml::Table, path: &str, header: Option<u32>) {
    let Some(v) = table.get("machine") else {
        return;
    };
    check_type(ctx, table, "machine", Ty::Str, path, header);
    let Some(raw) = v.as_str() else { return };
    let Err(e) = crate::schema::parse_machine(raw) else {
        return;
    };
    use crate::schema::MachineParseError as E;
    let detail = match e {
        E::Empty => "must not be empty".to_string(),
        E::MissingScheme => {
            "must be \"local\" or \"<provider>:<uri>\" (e.g. \"incredibuild:A\")".to_string()
        }
        E::EmptyScheme => "provider name (before the ':') must not be empty".to_string(),
        E::EmptyUri => "value after the ':' must not be empty".to_string(),
        E::SchemeTooShort => format!(
            "provider name must be at least {} characters — a bare drive letter like \"C:\\\\...\" is a path, not a machine",
            crate::schema::MIN_MACHINE_SCHEME_LEN
        ),
        E::InvalidSchemeChar(c) => {
            format!("provider name may only contain letters, digits, '_' and '-' (found {c:?})")
        }
    };
    let line = ctx.key_line(header, "machine");
    ctx.error(
        &format!("{path}.machine"),
        ErrorKind::InvalidValue,
        format!("invalid machine {raw:?}: {detail}"),
        line,
    );
}

fn check_system_prompt(
    ctx: &mut Ctx,
    table: &toml::Table,
    task_agent: Option<&str>,
    path: &str,
    header: Option<u32>,
) {
    let has_prompt = table.contains_key("system_prompt");
    let has_position = table.contains_key("system_prompt_position");
    if !has_prompt && !has_position {
        return;
    }

    let agent = table
        .get("agent")
        .and_then(toml::Value::as_str)
        .or(task_agent)
        .unwrap_or(crate::schema::DEFAULT_AGENT);
    if !crate::schema::agent_supports_system_prompt(agent) {
        let key = if has_prompt {
            "system_prompt"
        } else {
            "system_prompt_position"
        };
        let line = ctx.key_line(header, key);
        ctx.error(
            &format!("{path}.{key}"),
            ErrorKind::InvalidValue,
            format!(
                "'system_prompt'/'system_prompt_position' are only supported for the \
                 'claude-code'/'codex' agents right now, not '{agent}'"
            ),
            line,
        );
    }

    if let Some(pos) = table
        .get("system_prompt_position")
        .and_then(toml::Value::as_str)
    {
        if pos != crate::schema::SYSTEM_PROMPT_POSITION_APPEND {
            let line = ctx.key_line(header, "system_prompt_position");
            ctx.error(
                &format!("{path}.system_prompt_position"),
                ErrorKind::InvalidValue,
                format!(
                    "'system_prompt_position' must be \"{}\" (the only supported position), got \"{pos}\"",
                    crate::schema::SYSTEM_PROMPT_POSITION_APPEND
                ),
                line,
            );
        }
    }
}

/// Validate top-level `[[review]]` blocks. Only the TOML shape is checked here;
/// whether a session's `review` id actually matches a declared review is a
/// daemon-level preflight.
fn validate_review_blocks(value: Option<&toml::Value>, ctx: &mut Ctx) {
    let Some(value) = value else { return };
    let Some(arr) = value.as_array() else {
        ctx.error(
            "review",
            ErrorKind::WrongType,
            "[[review]] must be an array of tables",
            None,
        );
        return;
    };
    for (r, item) in arr.iter().enumerate() {
        let rpath = format!("review[{r}]");
        let Some(table) = item.as_table() else {
            ctx.error(
                &rpath,
                ErrorKind::WrongType,
                "each [[review]] must be a table",
                None,
            );
            continue;
        };
        let header = ctx.idx.review_line(r);
        unknown_keys(ctx, table, REVIEW_KEYS, &rpath, header);
        check_type(ctx, table, "id", Ty::Str, &rpath, header);
        check_type(ctx, table, "name", Ty::Str, &rpath, header);
        check_type(ctx, table, "agent", Ty::Str, &rpath, header);
        check_type(ctx, table, "model", Ty::Str, &rpath, header);
        check_type(ctx, table, "base", Ty::Str, &rpath, header);
        check_machine(ctx, table, &rpath, header);
        // A `ralphus:`-scheme id must be a well-formed review-link placeholder:
        // `ralphus:new-review/<key>` with a non-empty slug key. Any submission
        // that repeats the same key attaches to one shared guardian.
        if let Some(id) = table.get("id").and_then(toml::Value::as_str) {
            if id.starts_with("ralphus:") {
                let ok = crate::schema::review_link_key(id).is_some_and(|key| {
                    key.chars()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
                });
                if !ok {
                    ctx.error(
                        &format!("{rpath}.id"),
                        ErrorKind::InvalidValue,
                        "review link id must be 'ralphus:new-review/<key>' with a non-empty key of letters, digits, '-', '_' or '.'",
                        None,
                    );
                }
            }
        }
        validate_review_action_array(table.get("action"), &format!("{rpath}.action"), ctx);
    }
}

/// Validate `[[review.action]]` entries. Each entry must have `label` plus exactly
/// one of `prompt` or `command`.
fn validate_review_action_array(value: Option<&toml::Value>, path: &str, ctx: &mut Ctx) {
    let Some(value) = value else { return };
    let Some(arr) = value.as_array() else {
        ctx.error(
            path,
            ErrorKind::WrongType,
            "[[review.action]] must be an array of tables",
            None,
        );
        return;
    };
    for (a, item) in arr.iter().enumerate() {
        let apath = format!("{path}[{a}]");
        let Some(table) = item.as_table() else {
            ctx.error(
                &apath,
                ErrorKind::WrongType,
                "each [[review.action]] must be a table",
                None,
            );
            continue;
        };
        unknown_keys(ctx, table, REVIEW_ACTION_KEYS, &apath, None);

        // `label` is required.
        match table.get("label") {
            None => ctx.error(
                &apath,
                ErrorKind::MissingRequired,
                "review action requires a 'label'",
                None,
            ),
            Some(v) if !v.is_str() => check_type(ctx, table, "label", Ty::Str, &apath, None),
            Some(toml::Value::String(s)) if s.trim().is_empty() => ctx.error(
                &format!("{apath}.label"),
                ErrorKind::InvalidValue,
                "review action 'label' must not be empty",
                None,
            ),
            Some(_) => {}
        }

        // Exactly one of `prompt` or `command` is required.
        let has_prompt = table.contains_key("prompt");
        let has_command = table.contains_key("command");
        match (has_prompt, has_command) {
            (true, true) => ctx.error(
                &apath,
                ErrorKind::ConflictingKeys,
                "review action cannot set both 'prompt' and 'command'; use exactly one",
                None,
            ),
            (false, false) => ctx.error(
                &apath,
                ErrorKind::MissingRequired,
                "review action requires either 'prompt' or 'command'",
                None,
            ),
            _ => {
                check_type(ctx, table, "prompt", Ty::Str, &apath, None);
                check_type(ctx, table, "command", Ty::Str, &apath, None);
            }
        }

        // `cleanup_command` is optional and independent of the prompt/command
        // XOR above -- it coexists with either.
        check_type(ctx, table, "cleanup_command", Ty::Str, &apath, None);

        validate_review_action_input_array(table.get("input"), &format!("{apath}.input"), ctx);
    }
}

/// Validate `[[review.action.input]]` entries. Each entry must have `name`
/// and `message`; `default` is optional (defaults to an empty string).
fn validate_review_action_input_array(value: Option<&toml::Value>, path: &str, ctx: &mut Ctx) {
    let Some(value) = value else { return };
    let Some(arr) = value.as_array() else {
        ctx.error(
            path,
            ErrorKind::WrongType,
            "[[review.action.input]] must be an array of tables",
            None,
        );
        return;
    };
    for (i, item) in arr.iter().enumerate() {
        let ipath = format!("{path}[{i}]");
        let Some(table) = item.as_table() else {
            ctx.error(
                &ipath,
                ErrorKind::WrongType,
                "each [[review.action.input]] must be a table",
                None,
            );
            continue;
        };
        unknown_keys(ctx, table, REVIEW_ACTION_INPUT_KEYS, &ipath, None);

        match table.get("name") {
            None => ctx.error(
                &ipath,
                ErrorKind::MissingRequired,
                "review action input requires a 'name'",
                None,
            ),
            Some(v) if !v.is_str() => check_type(ctx, table, "name", Ty::Str, &ipath, None),
            Some(toml::Value::String(s)) if s.trim().is_empty() => ctx.error(
                &format!("{ipath}.name"),
                ErrorKind::InvalidValue,
                "review action input 'name' must not be empty",
                None,
            ),
            Some(_) => {}
        }

        match table.get("message") {
            None => ctx.error(
                &ipath,
                ErrorKind::MissingRequired,
                "review action input requires a 'message'",
                None,
            ),
            Some(v) if !v.is_str() => check_type(ctx, table, "message", Ty::Str, &ipath, None),
            Some(_) => {}
        }

        check_type(ctx, table, "default", Ty::Str, &ipath, None);
    }
}

/// Validate the `upstream` field of a session. When it uses the
/// `<<task:...>>` sentinel the inner reference must be non-empty.
fn check_upstream(ctx: &mut Ctx, table: &toml::Table, path: &str, header: Option<u32>) {
    let Some(val) = table.get("upstream").and_then(toml::Value::as_str) else {
        return;
    };
    if val.trim().is_empty() {
        let line = ctx.key_line(header, "upstream");
        ctx.error(
            &format!("{path}.upstream"),
            ErrorKind::InvalidValue,
            "'upstream' must not be empty",
            line,
        );
        return;
    }
    if let Some(inner) = val
        .strip_prefix(crate::schema::UPSTREAM_TASK_REF_PREFIX)
        .and_then(|s| s.strip_suffix(">>"))
    {
        if inner.trim().is_empty() {
            let line = ctx.key_line(header, "upstream");
            ctx.error(
                &format!("{path}.upstream"),
                ErrorKind::InvalidValue,
                "'upstream' <<task:...>> sentinel requires a non-empty task reference",
                line,
            );
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
        check_machine(ctx, table, &vpath, None);

        let kinds = ["command", "brain", "prompt"];
        let set: Vec<&str> = kinds
            .iter()
            .copied()
            .filter(|k| table.contains_key(*k))
            .collect();
        match set.len() {
            0 => ctx.error(
                &vpath,
                ErrorKind::MissingRequired,
                "verify step requires exactly one of: command, brain, prompt",
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
        check_type(ctx, table, "budget_tokens", Ty::Int, &vpath, None);
        check_type(ctx, table, "timeout_minutes", Ty::Int, &vpath, None);
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
    review_lines: Vec<u32>,
    /// (task_idx, session_idx) -> line
    session_lines: HashMap<(usize, usize), u32>,
}

impl HeaderIndex {
    fn scan(raw: &str) -> Self {
        let mut default_lines = Vec::new();
        let mut task_lines = Vec::new();
        let mut review_lines = Vec::new();
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
                Some("[[review]]") => review_lines.push(ln),
                _ => {}
            }
        }
        Self {
            default_lines,
            task_lines,
            review_lines,
            session_lines,
        }
    }

    fn default_line(&self, d: usize) -> Option<u32> {
        self.default_lines.get(d).copied()
    }

    fn task_line(&self, t: usize) -> Option<u32> {
        self.task_lines.get(t).copied()
    }

    fn review_line(&self, r: usize) -> Option<u32> {
        self.review_lines.get(r).copied()
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
    fn priority_accepted_on_task_and_session() {
        let src = "[[task]]\nname=\"t\"\npriority=2\n[[task.session]]\ncwd=\"/r\"\nprompt=\"p\"\npriority=0\n";
        let r = validate_toml(src);
        assert!(r.is_ok(), "{:?}", r.errors);
    }

    #[test]
    fn priority_wrong_type_reported() {
        let src =
            "[[task]]\nname=\"t\"\npriority=\"high\"\n[[task.session]]\ncwd=\"/r\"\nprompt=\"p\"\n";
        let r = validate_toml(src);
        assert!(
            r.errors
                .iter()
                .any(|e| e.kind == ErrorKind::WrongType && e.message.contains("priority")),
            "{:?}",
            r.errors
        );
    }

    #[test]
    fn wrong_type_for_budget() {
        let src = "[[task]]\nname=\"t\"\nbudget_tokens=\"lots\"\n[[task.session]]\ncwd=\"/r\"\nprompt=\"p\"\n";
        let r = validate_toml(src);
        assert!(
            r.errors
                .iter()
                .any(|e| e.kind == ErrorKind::WrongType && e.message.contains("budget_tokens"))
        );
    }

    #[test]
    fn maximum_budget_usd_accepted_on_task_and_session() {
        let src = "[[task]]\nname=\"t\"\nmaximum_budget_usd=5.0\n[[task.session]]\ncwd=\"/r\"\nprompt=\"p\"\nmaximum_budget_usd=1.5\n";
        let r = validate_toml(src);
        assert!(r.is_ok(), "{:?}", r.errors);
    }

    #[test]
    fn maximum_budget_usd_accepts_bare_integer() {
        let src = "[[task]]\nname=\"t\"\nmaximum_budget_usd=5\n[[task.session]]\ncwd=\"/r\"\nprompt=\"p\"\n";
        let r = validate_toml(src);
        assert!(r.is_ok(), "{:?}", r.errors);
    }

    #[test]
    fn maximum_budget_usd_wrong_type_reported() {
        let src = "[[task]]\nname=\"t\"\nmaximum_budget_usd=\"lots\"\n[[task.session]]\ncwd=\"/r\"\nprompt=\"p\"\n";
        let r = validate_toml(src);
        assert!(
            r.errors.iter().any(|e| e.kind == ErrorKind::WrongType
                && e.message.contains("maximum_budget_usd")),
            "{:?}",
            r.errors
        );
    }

    #[test]
    fn maximum_budget_usd_zero_rejected() {
        let src = "[[task]]\nname=\"t\"\nmaximum_budget_usd=0.0\n[[task.session]]\ncwd=\"/r\"\nprompt=\"p\"\n";
        let r = validate_toml(src);
        assert!(
            r.errors
                .iter()
                .any(|e| e.kind == ErrorKind::InvalidValue
                    && e.message.contains("maximum_budget_usd")),
            "{:?}",
            r.errors
        );
    }

    #[test]
    fn maximum_budget_usd_negative_rejected() {
        let src = "[[task]]\nname=\"t\"\n[[task.session]]\ncwd=\"/r\"\nprompt=\"p\"\nmaximum_budget_usd=-1.0\n";
        let r = validate_toml(src);
        assert!(
            r.errors
                .iter()
                .any(|e| e.kind == ErrorKind::InvalidValue
                    && e.message.contains("maximum_budget_usd")),
            "{:?}",
            r.errors
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
    fn environment_accepted_on_task_and_session() {
        let src = "[[task]]\nname=\"t\"\nenvironment={A=\"1\"}\n[[task.session]]\ncwd=\"/r\"\nprompt=\"p\"\nenvironment={B=\"2\"}\n";
        let r = validate_toml(src);
        assert!(r.is_ok(), "{:?}", r.errors);
    }

    #[test]
    fn environment_wrong_type_reported() {
        let src = "[[task]]\nname=\"t\"\nenvironment=\"nope\"\n[[task.session]]\ncwd=\"/r\"\nprompt=\"p\"\n";
        let r = validate_toml(src);
        assert!(
            r.errors
                .iter()
                .any(|e| e.kind == ErrorKind::WrongType && e.message.contains("environment")),
            "{:?}",
            r.errors
        );
    }

    #[test]
    fn environment_non_string_value_reported() {
        let src =
            "[[task]]\nname=\"t\"\n[[task.session]]\ncwd=\"/r\"\nprompt=\"p\"\nenvironment={A=1}\n";
        let r = validate_toml(src);
        assert!(
            r.errors
                .iter()
                .any(|e| e.kind == ErrorKind::WrongType && e.message.contains('A')),
            "{:?}",
            r.errors
        );
    }

    #[test]
    fn environment_invalid_key_reported() {
        let src = "[[task]]\nname=\"t\"\n[[task.session]]\ncwd=\"/r\"\nprompt=\"p\"\nenvironment={\"1BAD\"=\"x\"}\n";
        let r = validate_toml(src);
        assert!(
            r.errors
                .iter()
                .any(|e| e.kind == ErrorKind::InvalidValue && e.message.contains("1BAD")),
            "{:?}",
            r.errors
        );
    }

    #[test]
    fn system_prompt_valid_for_claude_code() {
        let src = "[[task]]\nname=\"t\"\n[[task.session]]\ncwd=\"/r\"\nprompt=\"p\"\nagent=\"claude-code\"\nsystem_prompt=\"be terse\"\nsystem_prompt_position=\"append\"\n";
        let r = validate_toml(src);
        assert!(r.is_ok(), "{:?}", r.errors);
    }

    #[test]
    fn system_prompt_valid_for_codex() {
        // Codex has no dedicated system-prompt flag but delivers `system_prompt`
        // via `-c developer_instructions=...` (see `CodexBackend`), so it's
        // accepted the same as claude-code.
        let src = "[[task]]\nname=\"t\"\n[[task.session]]\ncwd=\"/r\"\nprompt=\"p\"\nagent=\"codex\"\nsystem_prompt=\"be terse\"\nsystem_prompt_position=\"append\"\n";
        let r = validate_toml(src);
        assert!(r.is_ok(), "{:?}", r.errors);
    }

    #[test]
    fn system_prompt_valid_for_codex_cli_alias() {
        let src = "[[task]]\nname=\"t\"\n[[task.session]]\ncwd=\"/r\"\nprompt=\"p\"\nagent=\"codex-cli\"\nsystem_prompt=\"be terse\"\n";
        let r = validate_toml(src);
        assert!(r.is_ok(), "{:?}", r.errors);
    }

    #[test]
    fn system_prompt_inherits_task_agent() {
        // agent set at the task level (claude-code); the session omits it.
        let src = "[[task]]\nname=\"t\"\nagent=\"claude-code\"\n[[task.session]]\ncwd=\"/r\"\nprompt=\"p\"\nsystem_prompt=\"be terse\"\n";
        let r = validate_toml(src);
        assert!(r.is_ok(), "{:?}", r.errors);
    }

    #[test]
    fn system_prompt_rejected_for_default_agent() {
        // No agent set anywhere → resolves to the default "claude", not claude-code.
        let src = "[[task]]\nname=\"t\"\n[[task.session]]\ncwd=\"/r\"\nprompt=\"p\"\nsystem_prompt=\"be terse\"\n";
        let r = validate_toml(src);
        assert!(
            r.errors.iter().any(|e| e.kind == ErrorKind::InvalidValue
                && e.message.contains("only supported for the")),
            "{:?}",
            r.errors
        );
    }

    #[test]
    fn system_prompt_rejected_for_ollama() {
        let src = "[[task]]\nname=\"t\"\n[[task.session]]\ncwd=\"/r\"\nprompt=\"p\"\nagent=\"ollama\"\nsystem_prompt=\"be terse\"\n";
        let r = validate_toml(src);
        assert!(
            r.errors
                .iter()
                .any(|e| e.kind == ErrorKind::InvalidValue && e.message.contains("ollama")),
            "{:?}",
            r.errors
        );
    }

    #[test]
    fn system_prompt_position_rejects_unknown_value() {
        let src = "[[task]]\nname=\"t\"\n[[task.session]]\ncwd=\"/r\"\nprompt=\"p\"\nagent=\"claude-code\"\nsystem_prompt=\"x\"\nsystem_prompt_position=\"prepend\"\n";
        let r = validate_toml(src);
        assert!(
            r.errors.iter().any(|e| e.kind == ErrorKind::InvalidValue
                && e.message.contains("system_prompt_position")),
            "{:?}",
            r.errors
        );
    }

    #[test]
    fn system_prompt_wrong_type_reported() {
        let src = "[[task]]\nname=\"t\"\n[[task.session]]\ncwd=\"/r\"\nprompt=\"p\"\nagent=\"claude-code\"\nsystem_prompt=123\n";
        let r = validate_toml(src);
        assert!(r.errors.iter().any(|e| e.kind == ErrorKind::WrongType));
    }

    #[test]
    fn toplevel_review_block_is_valid() {
        let src = "[[task]]\nname=\"t\"\n[[task.session]]\ncwd=\"/r\"\nprompt=\"p\"\nreview=\"be\"\n[[review]]\nid=\"be\"\n";
        assert!(
            validate_toml(src).is_ok(),
            "{:?}",
            validate_toml(src).errors
        );
    }

    // ── machine (RAL-185) ─────────────────────────────────────────────────

    #[test]
    fn machine_is_valid_at_every_level() {
        let src = "[[task]]\nname=\"t\"\nmachine=\"incredibuild:A\"\n\
                   [[task.session]]\ncwd=\"/r\"\nprompt=\"p\"\nmachine=\"incredibuild:A\"\nreview=\"r\"\n\
                   [[task.session.verify]]\ncommand=\"cargo test\"\nmachine=\"incredibuild:A\"\n\
                   [[task.verify]]\ncommand=\"cargo fmt\"\nmachine=\"local\"\n\
                   [[review]]\nid=\"r\"\nmachine=\"incredibuild:C\"\n";
        assert!(
            validate_toml(src).is_ok(),
            "{:?}",
            validate_toml(src).errors
        );
    }

    #[test]
    fn machine_without_a_scheme_is_rejected() {
        let src = "[[task]]\nname=\"t\"\nmachine=\"incredibuild\"\n[[task.session]]\ncwd=\"/r\"\nprompt=\"p\"\n";
        let r = validate_toml(src);
        let e = r
            .errors
            .iter()
            .find(|e| e.path == "task[0].machine")
            .expect("machine error");
        assert_eq!(e.kind, ErrorKind::InvalidValue);
        assert!(e.message.contains("incredibuild:A"), "{}", e.message);
        assert_eq!(e.line, Some(3), "must point at the machine line");
    }

    #[test]
    fn machine_with_empty_uri_is_rejected() {
        let src = "[[task]]\nname=\"t\"\nmachine=\"incredibuild:\"\n[[task.session]]\ncwd=\"/r\"\nprompt=\"p\"\n";
        let r = validate_toml(src);
        assert!(
            r.errors
                .iter()
                .any(|e| e.path == "task[0].machine" && e.message.contains("must not be empty")),
            "{:?}",
            r.errors
        );
    }

    #[test]
    fn machine_that_is_actually_a_windows_path_gets_a_clear_error() {
        // Without the minimum-scheme-length rule this parses as provider "C",
        // and the user's first clue would be an unrelated "provider C is not
        // registered" at submit time.
        let src = "[[task]]\nname=\"t\"\nmachine=\"C:\\\\build\\\\wt\"\n[[task.session]]\ncwd=\"/r\"\nprompt=\"p\"\n";
        let r = validate_toml(src);
        let e = r
            .errors
            .iter()
            .find(|e| e.path == "task[0].machine")
            .expect("machine error");
        assert!(e.message.contains("path, not a machine"), "{}", e.message);
    }

    #[test]
    fn machine_wrong_type_is_reported() {
        let src = "[[task]]\nname=\"t\"\nmachine=42\n[[task.session]]\ncwd=\"/r\"\nprompt=\"p\"\n";
        let r = validate_toml(src);
        assert!(
            r.errors
                .iter()
                .any(|e| e.path == "task[0].machine" && e.kind == ErrorKind::WrongType),
            "{:?}",
            r.errors
        );
    }

    #[test]
    fn a_task_file_that_never_mentions_machine_still_validates() {
        // Regression guard: `machine` is optional everywhere and defaults to
        // local, so every pre-RAL-185 task file must keep validating untouched.
        let src = "[[task]]\nname=\"t\"\n[[task.session]]\ncwd=\"/r\"\nprompt=\"p\"\n";
        assert!(
            validate_toml(src).is_ok(),
            "{:?}",
            validate_toml(src).errors
        );
    }

    #[test]
    fn review_with_action_command_is_valid() {
        let src = "[[task]]\nname=\"t\"\n[[task.session]]\ncwd=\"/r\"\nprompt=\"p\"\nreview=\"r\"\n[[review]]\nid=\"r\"\n[[review.action]]\nlabel=\"Run tests\"\ncommand=\"cargo test\"\n";
        assert!(
            validate_toml(src).is_ok(),
            "{:?}",
            validate_toml(src).errors
        );
    }

    #[test]
    fn review_with_action_prompt_is_valid() {
        let src = "[[task]]\nname=\"t\"\n[[task.session]]\ncwd=\"/r\"\nprompt=\"p\"\nreview=\"r\"\n[[review]]\nid=\"r\"\n[[review.action]]\nlabel=\"Check UI\"\nprompt=\"Open localhost:3000 and verify the wizard\"\n";
        assert!(
            validate_toml(src).is_ok(),
            "{:?}",
            validate_toml(src).errors
        );
    }

    #[test]
    fn review_action_both_prompt_and_command_rejected() {
        let src = "[[task]]\nname=\"t\"\n[[task.session]]\ncwd=\"/r\"\nprompt=\"p\"\nreview=\"r\"\n[[review]]\nid=\"r\"\n[[review.action]]\nlabel=\"Check\"\nprompt=\"do x\"\ncommand=\"do y\"\n";
        let r = validate_toml(src);
        assert!(
            r.errors
                .iter()
                .any(|e| e.kind == ErrorKind::ConflictingKeys),
            "{:?}",
            r.errors
        );
    }

    #[test]
    fn review_action_missing_both_prompt_and_command_rejected() {
        let src = "[[task]]\nname=\"t\"\n[[task.session]]\ncwd=\"/r\"\nprompt=\"p\"\nreview=\"r\"\n[[review]]\nid=\"r\"\n[[review.action]]\nlabel=\"Check\"\n";
        let r = validate_toml(src);
        assert!(
            r.errors
                .iter()
                .any(|e| e.kind == ErrorKind::MissingRequired && e.message.contains("prompt")),
            "{:?}",
            r.errors
        );
    }

    #[test]
    fn review_action_missing_label_rejected() {
        let src = "[[task]]\nname=\"t\"\n[[task.session]]\ncwd=\"/r\"\nprompt=\"p\"\nreview=\"r\"\n[[review]]\nid=\"r\"\n[[review.action]]\ncommand=\"cargo test\"\n";
        let r = validate_toml(src);
        assert!(
            r.errors
                .iter()
                .any(|e| e.kind == ErrorKind::MissingRequired && e.message.contains("label")),
            "{:?}",
            r.errors
        );
    }

    #[test]
    fn review_action_unknown_key_rejected() {
        let src = "[[task]]\nname=\"t\"\n[[task.session]]\ncwd=\"/r\"\nprompt=\"p\"\nreview=\"r\"\n[[review]]\nid=\"r\"\n[[review.action]]\nlabel=\"x\"\ncommand=\"y\"\nfoo=\"bar\"\n";
        let r = validate_toml(src);
        assert!(
            r.errors
                .iter()
                .any(|e| e.kind == ErrorKind::UnknownKey && e.message.contains("foo")),
            "{:?}",
            r.errors
        );
    }

    #[test]
    fn review_action_cleanup_command_alone_is_valid() {
        let src = "[[task]]\nname=\"t\"\n[[task.session]]\ncwd=\"/r\"\nprompt=\"p\"\nreview=\"r\"\n[[review]]\nid=\"r\"\n[[review.action]]\nlabel=\"Serve\"\ncommand=\"ralphus-daemon serve --port {port}\"\ncleanup_command=\"ralphus-daemon stop --port {port}\"\n";
        assert!(
            validate_toml(src).is_ok(),
            "{:?}",
            validate_toml(src).errors
        );
    }

    #[test]
    fn review_action_input_is_valid() {
        let src = "[[task]]\nname=\"t\"\n[[task.session]]\ncwd=\"/r\"\nprompt=\"p\"\nreview=\"r\"\n[[review]]\nid=\"r\"\n[[review.action]]\nlabel=\"Serve\"\ncommand=\"ralphus-daemon serve --port {port}\"\n[[review.action.input]]\nname=\"port\"\nmessage=\"Port for the daemon\"\ndefault=\"7890\"\n";
        assert!(
            validate_toml(src).is_ok(),
            "{:?}",
            validate_toml(src).errors
        );
    }

    #[test]
    fn review_action_input_missing_name_rejected() {
        let src = "[[task]]\nname=\"t\"\n[[task.session]]\ncwd=\"/r\"\nprompt=\"p\"\nreview=\"r\"\n[[review]]\nid=\"r\"\n[[review.action]]\nlabel=\"Serve\"\ncommand=\"ralphus-daemon serve --port {port}\"\n[[review.action.input]]\nmessage=\"Port for the daemon\"\n";
        let r = validate_toml(src);
        assert!(
            r.errors
                .iter()
                .any(|e| e.kind == ErrorKind::MissingRequired && e.message.contains("name")),
            "{:?}",
            r.errors
        );
    }

    #[test]
    fn review_action_input_missing_message_rejected() {
        let src = "[[task]]\nname=\"t\"\n[[task.session]]\ncwd=\"/r\"\nprompt=\"p\"\nreview=\"r\"\n[[review]]\nid=\"r\"\n[[review.action]]\nlabel=\"Serve\"\ncommand=\"ralphus-daemon serve --port {port}\"\n[[review.action.input]]\nname=\"port\"\n";
        let r = validate_toml(src);
        assert!(
            r.errors
                .iter()
                .any(|e| e.kind == ErrorKind::MissingRequired && e.message.contains("message")),
            "{:?}",
            r.errors
        );
    }

    #[test]
    fn review_action_input_unknown_key_rejected() {
        let src = "[[task]]\nname=\"t\"\n[[task.session]]\ncwd=\"/r\"\nprompt=\"p\"\nreview=\"r\"\n[[review]]\nid=\"r\"\n[[review.action]]\nlabel=\"Serve\"\ncommand=\"ralphus-daemon serve --port {port}\"\n[[review.action.input]]\nname=\"port\"\nmessage=\"Port\"\nfoo=\"bar\"\n";
        let r = validate_toml(src);
        assert!(
            r.errors
                .iter()
                .any(|e| e.kind == ErrorKind::UnknownKey && e.message.contains("foo")),
            "{:?}",
            r.errors
        );
    }

    #[test]
    fn review_link_placeholder_id_is_valid() {
        let src = "[[task]]\nname=\"t\"\n[[task.session]]\ncwd=\"/r\"\nprompt=\"p\"\nreview=\"ralphus:new-review/ral-batch\"\n[[review]]\nid=\"ralphus:new-review/ral-batch\"\n";
        assert!(
            validate_toml(src).is_ok(),
            "{:?}",
            validate_toml(src).errors
        );
    }

    #[test]
    fn malformed_review_link_id_reported() {
        // Right scheme, but empty key after the slash.
        let src = "[[task]]\nname=\"t\"\n[[task.session]]\ncwd=\"/r\"\nprompt=\"p\"\n[[review]]\nid=\"ralphus:new-review/\"\n";
        let r = validate_toml(src);
        assert!(
            r.errors
                .iter()
                .any(|e| e.kind == ErrorKind::InvalidValue && e.message.contains("review link id")),
            "{:?}",
            r.errors
        );
    }

    #[test]
    fn wrong_scheme_review_link_id_reported() {
        // `ralphus:` scheme but not the `new-review/` form.
        let src = "[[task]]\nname=\"t\"\n[[task.session]]\ncwd=\"/r\"\nprompt=\"p\"\n[[review]]\nid=\"ralphus:review/xyz\"\n";
        let r = validate_toml(src);
        assert!(
            r.errors
                .iter()
                .any(|e| e.kind == ErrorKind::InvalidValue && e.message.contains("review link id")),
            "{:?}",
            r.errors
        );
    }

    #[test]
    fn review_unknown_key_reported() {
        let src = "[[task]]\nname=\"t\"\n[[task.session]]\ncwd=\"/r\"\nprompt=\"p\"\n[[review]]\nbranch=\"x\"\n";
        let r = validate_toml(src);
        assert!(
            r.errors
                .iter()
                .any(|e| e.kind == ErrorKind::UnknownKey && e.message.contains("branch"))
        );
    }

    #[test]
    fn review_wrong_type_reported() {
        let src = "[[task]]\nname=\"t\"\n[[task.session]]\ncwd=\"/r\"\nprompt=\"p\"\n[[review]]\nid=123\n";
        let r = validate_toml(src);
        assert!(r.errors.iter().any(|e| e.kind == ErrorKind::WrongType));
    }

    #[test]
    fn session_review_must_be_string_not_table() {
        // A session's `review` field opts into a top-level [[review]] block by id
        // and must be a string; a table value is rejected.
        let src =
            "[[task]]\nname=\"t\"\n[[task.session]]\ncwd=\"/r\"\nprompt=\"p\"\nreview={id=\"x\"}\n";
        let r = validate_toml(src);
        assert!(
            r.errors.iter().any(|e| e.kind == ErrorKind::WrongType),
            "a table-valued session 'review' must be rejected: {:?}",
            r.errors
        );
    }

    #[test]
    fn review_without_action_block_works_identically() {
        // A [[review]] with no [[review.action]] sub-blocks must still validate.
        let src = "[[task]]\nname=\"t\"\n[[task.session]]\ncwd=\"/r\"\nprompt=\"p\"\nreview=\"r\"\n[[review]]\nid=\"r\"\nagent=\"claude\"\nmodel=\"claude-opus-4-8\"\n";
        assert!(
            validate_toml(src).is_ok(),
            "{:?}",
            validate_toml(src).errors
        );
    }

    #[test]
    fn subprojects_single_valid() {
        let src = "[[task]]\nname=\"t\"\n[[task.session]]\ncwd=\"/r\"\nprompt=\"p\"\nsubprojects=[\"packages/foo\"]\n";
        assert!(
            validate_toml(src).is_ok(),
            "{:?}",
            validate_toml(src).errors
        );
    }

    #[test]
    fn subprojects_multiple_valid() {
        let src = "[[task]]\nname=\"t\"\n[[task.session]]\ncwd=\"/r\"\nprompt=\"p\"\nsubprojects=[\"packages/foo\",\"packages/bar\"]\n";
        assert!(
            validate_toml(src).is_ok(),
            "{:?}",
            validate_toml(src).errors
        );
    }

    #[test]
    fn subprojects_nested_path_valid() {
        let src = "[[task]]\nname=\"t\"\n[[task.session]]\ncwd=\"/r\"\nprompt=\"p\"\nsubprojects=[\"a/b/c\"]\n";
        assert!(
            validate_toml(src).is_ok(),
            "{:?}",
            validate_toml(src).errors
        );
    }

    #[test]
    fn subprojects_empty_entry_rejected() {
        let src = "[[task]]\nname=\"t\"\n[[task.session]]\ncwd=\"/r\"\nprompt=\"p\"\nsubprojects=[\"\"]\n";
        let r = validate_toml(src);
        assert!(
            r.errors
                .iter()
                .any(|e| e.kind == ErrorKind::InvalidValue && e.message.contains("subprojects")),
            "{:?}",
            r.errors
        );
    }

    #[test]
    fn subprojects_absolute_path_rejected() {
        let src = "[[task]]\nname=\"t\"\n[[task.session]]\ncwd=\"/r\"\nprompt=\"p\"\nsubprojects=[\"/packages/foo\"]\n";
        let r = validate_toml(src);
        assert!(
            r.errors
                .iter()
                .any(|e| e.kind == ErrorKind::InvalidValue && e.message.contains("relative")),
            "{:?}",
            r.errors
        );
    }

    #[test]
    fn subprojects_dotdot_rejected() {
        let src = "[[task]]\nname=\"t\"\n[[task.session]]\ncwd=\"/r\"\nprompt=\"p\"\nsubprojects=[\"packages/../etc\"]\n";
        let r = validate_toml(src);
        assert!(
            r.errors
                .iter()
                .any(|e| e.kind == ErrorKind::InvalidValue && e.message.contains("..")),
            "{:?}",
            r.errors
        );
    }

    #[test]
    fn subprojects_wrong_type_rejected() {
        let src =
            "[[task]]\nname=\"t\"\n[[task.session]]\ncwd=\"/r\"\nprompt=\"p\"\nsubprojects=123\n";
        let r = validate_toml(src);
        assert!(
            r.errors.iter().any(|e| e.kind == ErrorKind::WrongType),
            "{:?}",
            r.errors
        );
    }

    #[test]
    fn subprojects_string_not_array_rejected() {
        // Passing a plain string instead of an array should be caught as WrongType
        let src = "[[task]]\nname=\"t\"\n[[task.session]]\ncwd=\"/r\"\nprompt=\"p\"\nsubprojects=\"packages/foo\"\n";
        let r = validate_toml(src);
        assert!(
            r.errors.iter().any(|e| e.kind == ErrorKind::WrongType),
            "{:?}",
            r.errors
        );
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

    // ── upstream field (RAL-50) ───────────────────────────────────────────────

    #[test]
    fn upstream_task_sentinel_is_valid() {
        let src = "[[task]]\nname=\"t\"\n[[task.session]]\ncwd=\"/r\"\nprompt=\"p\"\nupstream=\"<<task:task-a>>\"\n";
        let r = validate_toml(src);
        assert!(r.is_ok(), "{:?}", r.errors);
    }

    #[test]
    fn upstream_task_session_sentinel_is_valid() {
        let src = "[[task]]\nname=\"t\"\n[[task.session]]\ncwd=\"/r\"\nprompt=\"p\"\nupstream=\"<<task:task-a/session-1>>\"\n";
        let r = validate_toml(src);
        assert!(r.is_ok(), "{:?}", r.errors);
    }

    #[test]
    fn upstream_empty_sentinel_ref_is_rejected() {
        let src = "[[task]]\nname=\"t\"\n[[task.session]]\ncwd=\"/r\"\nprompt=\"p\"\nupstream=\"<<task:>>\"\n";
        let r = validate_toml(src);
        assert!(
            r.errors
                .iter()
                .any(|e| e.kind == ErrorKind::InvalidValue && e.message.contains("<<task:")),
            "{:?}",
            r.errors
        );
    }

    #[test]
    fn upstream_empty_string_is_rejected() {
        let src =
            "[[task]]\nname=\"t\"\n[[task.session]]\ncwd=\"/r\"\nprompt=\"p\"\nupstream=\"\"\n";
        let r = validate_toml(src);
        assert!(
            r.errors
                .iter()
                .any(|e| e.kind == ErrorKind::InvalidValue && e.message.contains("upstream")),
            "{:?}",
            r.errors
        );
    }

    #[test]
    fn upstream_wrong_type_rejected() {
        let src =
            "[[task]]\nname=\"t\"\n[[task.session]]\ncwd=\"/r\"\nprompt=\"p\"\nupstream=123\n";
        let r = validate_toml(src);
        assert!(r.errors.iter().any(|e| e.kind == ErrorKind::WrongType));
    }

    // ── worktree placeholder cwd (RAL-100) ────────────────────────────────────

    #[test]
    fn placeholder_cwd_with_project_is_valid() {
        let src = "[[task]]\nname=\"t\"\nproject=\"my-project\"\n[[task.session]]\ncwd=\"ralphus:new-worktree/feat\"\nprompt=\"p\"\n";
        let r = validate_toml(src);
        assert!(r.is_ok(), "{:?}", r.errors);
    }

    #[test]
    fn placeholder_cwd_without_project_is_rejected() {
        let src = "[[task]]\nname=\"t\"\n[[task.session]]\ncwd=\"ralphus:new-worktree/feat\"\nprompt=\"p\"\n";
        let r = validate_toml(src);
        assert!(
            r.errors.iter().any(|e| e.kind == ErrorKind::MissingRequired
                && e.message.contains("project")
                && e.message.contains("placeholder")),
            "{:?}",
            r.errors
        );
    }

    #[test]
    fn old_style_project_prefixed_cwd_is_treated_as_a_plain_path() {
        // The pre-RAL-100-redesign `<project>:worktree/<branch>` scheme no
        // longer parses as a placeholder, so it doesn't require 'project' to
        // be set -- it's just an (unusual, but not our concern here) literal
        // cwd string.
        let src = "[[task]]\nname=\"t\"\n[[task.session]]\ncwd=\"my-project:worktree/feat\"\nprompt=\"p\"\n";
        let r = validate_toml(src);
        assert!(r.is_ok(), "{:?}", r.errors);
    }

    #[test]
    fn plain_path_cwd_does_not_require_project() {
        let src = "[[task]]\nname=\"t\"\n[[task.session]]\ncwd=\"/repo\"\nprompt=\"p\"\n";
        assert!(
            validate_toml(src).is_ok(),
            "{:?}",
            validate_toml(src).errors
        );
    }

    #[test]
    fn upstream_unknown_key_would_have_been_caught() {
        // Regression guard: "upstream" must be in SESSION_KEYS so it is NOT
        // reported as an unknown key.
        let src = "[[task]]\nname=\"t\"\n[[task.session]]\ncwd=\"/r\"\nprompt=\"p\"\nupstream=\"<<task:dep>>\"\n";
        let r = validate_toml(src);
        assert!(
            !r.errors
                .iter()
                .any(|e| e.kind == ErrorKind::UnknownKey && e.message.contains("upstream")),
            "upstream must not be reported as unknown: {:?}",
            r.errors
        );
    }
}
