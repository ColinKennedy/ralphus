//! Task-file validation.
//!
//! Ported (behaviour, not verbatim) from `old:src/tasks/validate.rs`, which the
//! research flagged as the strongest part of the predecessor. It validates the
//! *raw* `toml::Value` rather than the deserialized structs, so it can report
//! unknown keys, wrong types, and precise 1-based line numbers that a plain
//! serde error would not surface.
//!
//! Scope note: cross-squad / cross-blob reference resolution (which needs the
//! daemon's database) is deferred to a later phase. This module validates a
//! single submission's structure, types, required fields, mutually-exclusive
//! keys, `restart_on` grammar, and within-task cell dependency cycles.

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
    /// An `upstream = "<<task:...>>"` sentinel references a task (or
    /// task/cell) that does not exist anywhere in this submission.
    UnknownTaskRef,
}

/// A single validation finding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidationError {
    /// Dotted path to the offending element, e.g. `task[0].cell[1].cwd`.
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
    "no_commit_required",
    "cell",
    "proof",
];
const CELL_KEYS: &[&str] = &[
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
    "proof",
    "review",
    "upstream",
];
const REVIEW_KEYS: &[&str] = &[
    "id",
    "name",
    "agent",
    "model",
    "machine",
    "base",
    "action",
    "maximum_budget_usd",
];
const REVIEW_ACTION_KEYS: &[&str] = &["label", "prompt", "command", "cleanup_command", "input"];
const REVIEW_ACTION_INPUT_KEYS: &[&str] = &["name", "message", "default"];
const PROOF_KEYS: &[&str] = &[
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
    "environment",
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

    // Submission-wide index (every [[task]] in `arr`, not just the one being
    // walked) so an `upstream = "<<task:...>>"` sentinel can be checked
    // against a task anywhere in this submission -- including a task that
    // appears later in the array, or in a different file that `ralphus
    // submit a.toml b.toml` already joined into this same `arr` before
    // `validate_toml` ever saw it (see `build_task_cell_index`'s doc comment).
    let task_cell_index = build_task_cell_index(arr);

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
        check_type(ctx, table, "no_commit_required", Ty::Bool, &path, header);

        let task_agent = table.get("agent").and_then(toml::Value::as_str);
        let task_project = table.get("project").and_then(toml::Value::as_str);
        validate_cells(
            table.get("cell"),
            t,
            &path,
            task_agent,
            task_project,
            header,
            &task_cell_index,
            ctx,
        );
        validate_proof_array(table.get("proof"), &format!("{path}.proof"), ctx);
    }
}

// ── [[task.cell]] ─────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
fn validate_cells(
    value: Option<&toml::Value>,
    task_idx: usize,
    task_path: &str,
    task_agent: Option<&str>,
    task_project: Option<&str>,
    task_header: Option<u32>,
    task_cell_index: &HashMap<String, HashSet<String>>,
    ctx: &mut Ctx,
) {
    let Some(value) = value else { return };
    let Some(arr) = value.as_array() else {
        ctx.error(
            &format!("{task_path}.cell"),
            ErrorKind::WrongType,
            "cell must be an array of tables",
            None,
        );
        return;
    };

    let mut ids: HashMap<String, usize> = HashMap::new();
    let mut dep_edges: Vec<(usize, String)> = Vec::new();
    let mut has_placeholder = false;

    for (s, item) in arr.iter().enumerate() {
        let path = format!("{task_path}.cell[{s}]");
        let Some(table) = item.as_table() else {
            ctx.error(
                &path,
                ErrorKind::WrongType,
                "each cell must be a table",
                None,
            );
            continue;
        };
        let header = ctx.idx.cell_line(task_idx, s);
        unknown_keys(ctx, table, CELL_KEYS, &path, header);

        // RAL-224: `cwd` is deliberately syntax-only (present/non-empty/string),
        // unlike `check_subprojects` below which also rejects a leading '/' or
        // '\' and any '..' segment. A traversal check / sensitive-path denylist
        // for `cwd` was considered and explicitly rejected -- the stakeholder
        // decision was to rely on RAL-219's submission-time authentication (only
        // authenticated callers can submit a task at all) as the real gate,
        // rather than layering path-content rules on top. Revisit if RAL-219
        // ever introduces a lower-trust authenticated tier that shouldn't be
        // able to point `cwd` anywhere on disk.
        match table.get("cwd") {
            None => ctx.error(
                &path,
                ErrorKind::MissingRequired,
                "'cwd' is required for every cell (absolute path to the worktree)",
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
                if let Some(s) = v.as_str() {
                    if crate::schema::parse_worktree_placeholder(s).is_some() {
                        has_placeholder = true;
                        let line = ctx.key_line(header, "cwd");
                        match crate::schema::parse_worktree_placeholder_upstream(s) {
                            None => ctx.error(
                                &format!("{path}.cwd"),
                                ErrorKind::MissingRequired,
                                "a \"ralphus:new-worktree/<branch>\" placeholder cwd requires \
                                 an explicit \"?upstream=<upstream>\" suffix, e.g. \
                                 \"ralphus:new-worktree/<branch>?upstream=main\" (or the \
                                 \"?upstream=<<default>>\" sentinel), so ralphus knows what \
                                 the branch tracks instead of guessing from HEAD",
                                line,
                            ),
                            // A `<<...>>` value is only ever a reserved sentinel:
                            // any other one is a typo or an unsupported sentinel,
                            // and must fail fast here (before it ever reaches the
                            // daemon) with the valid options spelled out.
                            Some(upstream) if upstream.starts_with("<<") => {
                                if !crate::schema::is_worktree_upstream_sentinel(upstream) {
                                    let options =
                                        crate::schema::WORKTREE_UPSTREAM_SENTINELS.join(", ");
                                    ctx.error(
                                        &format!("{path}.cwd"),
                                        ErrorKind::InvalidValue,
                                        format!(
                                            "\"?upstream={upstream}\" is not a supported \
                                             sentinel. Choose one of {options} (alphabetical) \
                                             or use an existing branch name as the literal \
                                             tracking target, e.g. \"?upstream=main\""
                                        ),
                                        line,
                                    );
                                }
                            }
                            Some(_) => {}
                        }
                    }
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
                "cell cannot set both 'prompt' (AI-driven) and 'command' (deterministic); use one",
                header,
            ),
            (false, false) => ctx.error(
                &path,
                ErrorKind::MissingRequired,
                "cell requires either 'prompt' (AI-driven) or 'command' (shell command)",
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
                        format!("cell id \"{id}\" must not contain '/'"),
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
        check_upstream_task_ref_exists(ctx, table, &path, header, task_cell_index);

        check_type(ctx, table, "review", Ty::Str, &path, header);

        validate_proof_array(table.get("proof"), &format!("{path}.proof"), ctx);
    }

    if has_placeholder && task_project.is_none() {
        ctx.error(
            task_path,
            ErrorKind::MissingRequired,
            "task 'project' is required when any cell uses a placeholder cwd \
             (\"ralphus:new-worktree/<branch>\")",
            task_header,
        );
    }

    check_cell_deps(&ids, &dep_edges, arr.len(), task_path, task_idx, ctx);
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
/// smuggling shell metacharacters into the `$env:KEY = ...;` prefix the
/// daemon's tmux delivery embeds: the `-e KEY=value` flag passed to
/// `new-session` on Windows (RAL-247) and the `KEY='...'` assignment prefix
/// [`daemon::tmux::build_command_line_with_env`] inlines for POSIX RAL-150
/// (RAL-172, RAL-150).
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

/// Enforce the appended-system-prompt rules (RAL-5). `system_prompt` and
/// `system_prompt_position` are only accepted for backends with a real
/// delivery mechanism — see
/// [`agent_supports_system_prompt`](crate::schema::agent_supports_system_prompt)
/// — and the position, when set, must be the `"append"` sentinel. The
/// effective agent is the cell's own `agent`, falling back to the
/// task-level `agent`, then [`DEFAULT_AGENT`](crate::schema::DEFAULT_AGENT).
///
/// This only rejects agent names `core` can classify itself --
/// [`RESERVED_AGENT_NAMES`](crate::schema::RESERVED_AGENT_NAMES), the
/// built-in backends and their aliases. A name outside that set may be a
/// custom `[agent.profiles.*]` entry whose backend `core` cannot see (same
/// split as [`check_machine`]'s provider-registry check); that case is
/// deferred to `daemon::agent_profiles::validate_task_file_profiles` at
/// submit time, once the daemon has resolved the profile's real backend.
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
    if crate::schema::RESERVED_AGENT_NAMES.contains(&agent)
        && !crate::schema::agent_supports_system_prompt(agent)
    {
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
/// whether a cell's `review` id actually matches a declared review is a
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
        check_type(ctx, table, "maximum_budget_usd", Ty::Float, &rpath, header);
        check_positive_number(ctx, table, "maximum_budget_usd", &rpath, header);
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

/// Validate the `upstream` field of a cell. When it uses the
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

/// Build a submission-wide index of task name -> its cells' `id`s, from
/// every `[[task]]` in `arr` (not just one). This is what lets
/// [`check_upstream_task_ref_exists`] resolve an `upstream = "<<task:...>>"`
/// sentinel against a task anywhere in the submission, regardless of which
/// `[[task]]` block it physically appears before or after -- and, since
/// `ralphus submit a.toml b.toml` joins multiple files' raw TOML text into
/// one string before this module ever sees it (`texts.join("\n\n")` in
/// `cli-rs`'s `cmd_submit`), a cross-*file* reference within one submission
/// resolves here too, for free: by the time `validate_toml` runs, `arr` is
/// already every `[[task]]` from every joined file, indistinguishable from
/// one file with the same content.
///
/// Malformed entries (a non-table task, a missing/empty name, a cell id
/// containing `/`) are skipped rather than erroring here -- those are each
/// already reported by their own dedicated check elsewhere in this module;
/// this index only needs to know what a valid `upstream` reference COULD
/// legitimately resolve to.
fn build_task_cell_index(arr: &[toml::Value]) -> HashMap<String, HashSet<String>> {
    let mut index: HashMap<String, HashSet<String>> = HashMap::new();
    for item in arr {
        let Some(table) = item.as_table() else {
            continue;
        };
        let Some(name) = table.get("name").and_then(toml::Value::as_str) else {
            continue;
        };
        if name.trim().is_empty() {
            continue;
        }
        let cells = index.entry(name.to_string()).or_default();
        let Some(cell_arr) = table.get("cell").and_then(toml::Value::as_array) else {
            continue;
        };
        for cell_item in cell_arr {
            let Some(cell_table) = cell_item.as_table() else {
                continue;
            };
            if let Some(id) = cell_table.get("id").and_then(toml::Value::as_str) {
                if !id.is_empty() && !id.contains('/') {
                    cells.insert(id.to_string());
                }
            }
        }
    }
    index
}

/// Check that an `upstream = "<<task:task-name>>"` (or
/// `"<<task:task-name/cell-id>>"`) sentinel references a task -- and cell,
/// when given -- that actually exists in `task_cell_index`. Previously this
/// went unchecked entirely: a typo'd or nonexistent task name validated
/// clean and only surfaced at run time as a silent no-op (the rebase this
/// sentinel is supposed to trigger just never fires --
/// `daemon/src/scheduler.rs`'s `try_upstream_rebase` returns `None` via `?`
/// the same way it does for "no upstream set at all", so nothing in the
/// squad's own output distinguishes "intentionally no upstream" from "typo'd
/// upstream").
///
/// A non-sentinel `upstream` value (empty, or not `<<task:...>>`-shaped) is
/// out of scope here -- `check_upstream` already reports an empty value, and
/// a plain literal branch name is syntactically valid (even though nothing
/// currently acts on it; see the `upstream` field's doc comment).
fn check_upstream_task_ref_exists(
    ctx: &mut Ctx,
    table: &toml::Table,
    path: &str,
    header: Option<u32>,
    task_cell_index: &HashMap<String, HashSet<String>>,
) {
    let Some(val) = table.get("upstream").and_then(toml::Value::as_str) else {
        return;
    };
    let Some(inner) = crate::schema::parse_upstream_task_ref(val) else {
        return;
    };
    if inner.trim().is_empty() {
        return; // already reported by check_upstream
    }
    let (task_name, cell_id) = inner
        .split_once('/')
        .map_or((inner, None), |(t, c)| (t, Some(c)));
    let line = ctx.key_line(header, "upstream");
    match task_cell_index.get(task_name) {
        None => ctx.error(
            &format!("{path}.upstream"),
            ErrorKind::UnknownTaskRef,
            format!("'upstream' <<task:...>> references unknown task \"{task_name}\""),
            line,
        ),
        Some(cells) => {
            if let Some(cid) = cell_id {
                if !cells.contains(cid) {
                    ctx.error(
                        &format!("{path}.upstream"),
                        ErrorKind::UnknownTaskRef,
                        format!(
                            "'upstream' <<task:...>> references unknown cell \"{cid}\" \
                             in task \"{task_name}\""
                        ),
                        line,
                    );
                }
            }
        }
    }
}

/// Resolve within-task cell dependencies and detect cycles. Cross-task refs
/// (those containing `/`) are left for a later phase to resolve against the DB.
fn check_cell_deps(
    ids: &HashMap<String, usize>,
    edges: &[(usize, String)],
    cell_count: usize,
    task_path: &str,
    task_idx: usize,
    ctx: &mut Ctx,
) {
    let mut adj: Vec<Vec<usize>> = vec![Vec::new(); cell_count];
    for (from, dep) in edges {
        if dep.contains('/') {
            continue; // cross-task ref, deferred.
        }
        match ids.get(dep) {
            Some(&to) => adj[*from].push(to),
            None => {
                let line = ctx.idx.cell_line(task_idx, *from);
                ctx.error(
                    &format!("{task_path}.cell[{from}].depends_on"),
                    ErrorKind::IntraTaskRefNotFound,
                    format!("depends_on references unknown cell id \"{dep}\""),
                    line,
                );
            }
        }
    }

    if has_cycle(&adj) {
        let line = ctx.idx.task_line(task_idx);
        ctx.error(
            &format!("{task_path}.cell"),
            ErrorKind::InvalidValue,
            "circular dependency detected among cells",
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

// ── proof steps ─────────────────────────────────────────────────────────────

fn validate_proof_array(value: Option<&toml::Value>, path: &str, ctx: &mut Ctx) {
    let Some(value) = value else { return };
    let Some(arr) = value.as_array() else {
        ctx.error(
            path,
            ErrorKind::WrongType,
            "proof must be an array of tables",
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
                "each proof step must be a table",
                None,
            );
            continue;
        };
        unknown_keys(ctx, table, PROOF_KEYS, &vpath, None);
        check_machine(ctx, table, &vpath, None);
        // RAL-191: a proof step carries its own `environment`, validated with
        // exactly the same key/value rules as a task's or cell's.
        check_environment(ctx, table, &vpath, None);

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
                "proof step requires exactly one of: command, brain, prompt",
                None,
            ),
            1 => {}
            _ => ctx.error(
                &vpath,
                ErrorKind::ConflictingKeys,
                format!(
                    "proof step sets multiple kinds ({}); use exactly one",
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
/// `task/cell/proof[?on=pass|fail|both]` with `task/*` and `task/cell/*`
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
        [_task, _cell, _proof] => Ok(()),
        _ => Err(format!(
            "restart_on \"{spec}\" must be task/cell/proof or a task/* / task/cell/* wildcard"
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
    /// (task_idx, cell_idx) -> line
    cell_lines: HashMap<(usize, usize), u32>,
}

impl HeaderIndex {
    fn scan(raw: &str) -> Self {
        let mut default_lines = Vec::new();
        let mut task_lines = Vec::new();
        let mut review_lines = Vec::new();
        let mut cell_lines = HashMap::new();
        let mut cur_task: isize = -1;
        let mut cur_cell: isize = -1;

        for (i, line) in raw.lines().enumerate() {
            let ln = u32::try_from(i + 1).unwrap_or(u32::MAX);
            match normalize_header(line).as_deref() {
                Some("[[default]]") => default_lines.push(ln),
                Some("[[task]]") => {
                    cur_task += 1;
                    cur_cell = -1;
                    task_lines.push(ln);
                }
                Some("[[task.cell]]") => {
                    cur_cell += 1;
                    if let (Ok(t), Ok(s)) = (usize::try_from(cur_task), usize::try_from(cur_cell)) {
                        cell_lines.insert((t, s), ln);
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
            cell_lines,
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

    fn cell_line(&self, t: usize, s: usize) -> Option<u32> {
        self.cell_lines.get(&(t, s)).copied()
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
/// whitespace, e.g. `[[ task . cell ]]` -> `[[task.cell]]`. Returns `None`
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
[[task.cell]]
cwd = "/repo"
prompt = "make it build"
[[task.cell.proof]]
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
            "[[task]]\nname = \"t\"\nbudgt_usd = 1.0\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\n";
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
        let src = "[[task]]\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\n";
        let r = validate_toml(src);
        assert!(
            r.errors
                .iter()
                .any(|e| e.kind == ErrorKind::MissingRequired && e.message.contains("name"))
        );
    }

    #[test]
    fn cell_needs_cwd() {
        let src = "[[task]]\nname=\"t\"\n[[task.cell]]\nprompt=\"p\"\n";
        let r = validate_toml(src);
        assert!(
            r.errors
                .iter()
                .any(|e| e.kind == ErrorKind::MissingRequired && e.message.contains("cwd"))
        );
    }

    #[test]
    fn prompt_and_command_conflict() {
        let src = "[[task]]\nname=\"t\"\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\ncommand=\"c\"\n";
        let r = validate_toml(src);
        assert!(
            r.errors
                .iter()
                .any(|e| e.kind == ErrorKind::ConflictingKeys)
        );
    }

    #[test]
    fn cell_needs_prompt_or_command() {
        let src = "[[task]]\nname=\"t\"\n[[task.cell]]\ncwd=\"/r\"\n";
        let r = validate_toml(src);
        assert!(
            r.errors
                .iter()
                .any(|e| e.kind == ErrorKind::MissingRequired && e.message.contains("prompt"))
        );
    }

    #[test]
    fn command_only_cell_is_valid() {
        let src = "[[task]]\nname=\"t\"\n[[task.cell]]\ncwd=\"/r\"\ncommand=\"cargo build\"\n";
        assert!(validate_toml(src).is_ok());
    }

    #[test]
    fn placeholder_cwd_accepts_reserved_upstream_sentinels() {
        for upstream in ["<<default>>", "<<current_branch>>"] {
            let src = format!(
                "[[task]]\nname=\"t\"\nproject=\"my-project\"\n[[task.cell]]\ncwd=\"ralphus:new-worktree/feat?upstream={upstream}\"\nprompt=\"p\"\n"
            );
            let r = validate_toml(&src);
            assert!(
                r.errors.iter().all(|e| e.kind != ErrorKind::MissingRequired
                    && !e.message.contains("not a supported sentinel")),
                "sentinel {upstream} must pass, got {:?}",
                r.errors
            );
        }
    }

    #[test]
    fn placeholder_cwd_rejects_an_unknown_sentinel_with_an_actionable_message() {
        let src = "[[task]]\nname=\"t\"\nproject=\"my-project\"\n[[task.cell]]\ncwd=\"ralphus:new-worktree/feat?upstream=<<wat>>\"\nprompt=\"p\"\n";
        let r = validate_toml(src);
        let e = r
            .errors
            .iter()
            .find(|e| e.kind == ErrorKind::InvalidValue)
            .expect("unknown sentinel must be an InvalidValue error");
        // Lists the valid options alphabetically and suggests a literal branch.
        assert!(
            e.message.contains("not a supported sentinel")
                && e.message.contains("<<current_branch>>")
                && e.message.contains("<<default>>")
                && e.message.contains("?upstream=main"),
            "{}",
            e.message
        );
    }

    #[test]
    fn no_commit_required_accepted_as_bool() {
        let src = "[[task]]\nname=\"t\"\nno_commit_required=true\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\n";
        let r = validate_toml(src);
        assert!(r.is_ok(), "{:?}", r.errors);
    }

    #[test]
    fn no_commit_required_wrong_type_reported() {
        let src = "[[task]]\nname=\"t\"\nno_commit_required=\"yes\"\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\n";
        let r = validate_toml(src);
        assert!(
            r.errors.iter().any(
                |e| e.kind == ErrorKind::WrongType && e.message.contains("no_commit_required")
            ),
            "{:?}",
            r.errors
        );
    }

    #[test]
    fn priority_accepted_on_task_and_cell() {
        let src = "[[task]]\nname=\"t\"\npriority=2\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\npriority=0\n";
        let r = validate_toml(src);
        assert!(r.is_ok(), "{:?}", r.errors);
    }

    #[test]
    fn priority_wrong_type_reported() {
        let src =
            "[[task]]\nname=\"t\"\npriority=\"high\"\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\n";
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
        let src = "[[task]]\nname=\"t\"\nbudget_tokens=\"lots\"\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\n";
        let r = validate_toml(src);
        assert!(
            r.errors
                .iter()
                .any(|e| e.kind == ErrorKind::WrongType && e.message.contains("budget_tokens"))
        );
    }

    #[test]
    fn maximum_budget_usd_accepted_on_task_and_cell() {
        let src = "[[task]]\nname=\"t\"\nmaximum_budget_usd=5.0\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\nmaximum_budget_usd=1.5\n";
        let r = validate_toml(src);
        assert!(r.is_ok(), "{:?}", r.errors);
    }

    #[test]
    fn maximum_budget_usd_accepts_bare_integer() {
        let src =
            "[[task]]\nname=\"t\"\nmaximum_budget_usd=5\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\n";
        let r = validate_toml(src);
        assert!(r.is_ok(), "{:?}", r.errors);
    }

    #[test]
    fn maximum_budget_usd_wrong_type_reported() {
        let src = "[[task]]\nname=\"t\"\nmaximum_budget_usd=\"lots\"\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\n";
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
        let src = "[[task]]\nname=\"t\"\nmaximum_budget_usd=0.0\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\n";
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
        let src = "[[task]]\nname=\"t\"\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\nmaximum_budget_usd=-1.0\n";
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

    // ── RAL-193: [[review]] maximum_budget_usd ───────────────────────────

    #[test]
    fn review_maximum_budget_usd_accepted() {
        let src = "[[task]]\nname=\"t\"\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\nreview=\"r\"\n[[review]]\nid=\"r\"\nmaximum_budget_usd=10.0\n";
        let r = validate_toml(src);
        assert!(r.is_ok(), "{:?}", r.errors);
    }

    #[test]
    fn review_maximum_budget_usd_wrong_type_reported() {
        let src = "[[task]]\nname=\"t\"\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\nreview=\"r\"\n[[review]]\nid=\"r\"\nmaximum_budget_usd=\"lots\"\n";
        let r = validate_toml(src);
        assert!(
            r.errors.iter().any(|e| e.kind == ErrorKind::WrongType
                && e.message.contains("maximum_budget_usd")),
            "{:?}",
            r.errors
        );
    }

    #[test]
    fn review_maximum_budget_usd_zero_rejected() {
        let src = "[[task]]\nname=\"t\"\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\nreview=\"r\"\n[[review]]\nid=\"r\"\nmaximum_budget_usd=0.0\n";
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
    fn proof_needs_exactly_one_kind() {
        let none = "[[task]]\nname=\"t\"\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\n[[task.cell.proof]]\nid=\"v\"\n";
        assert!(
            validate_toml(none)
                .errors
                .iter()
                .any(|e| e.kind == ErrorKind::MissingRequired && e.message.contains("exactly one"))
        );

        let many = "[[task]]\nname=\"t\"\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\n[[task.cell.proof]]\ncommand=\"c\"\nbrain=\"b\"\n";
        assert!(
            validate_toml(many)
                .errors
                .iter()
                .any(|e| e.kind == ErrorKind::ConflictingKeys)
        );
    }

    #[test]
    fn duplicate_task_names() {
        let src = "[[task]]\nname=\"t\"\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\n[[task]]\nname=\"t\"\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\n";
        let r = validate_toml(src);
        assert!(
            r.errors
                .iter()
                .any(|e| e.kind == ErrorKind::InvalidValue && e.message.contains("duplicate"))
        );
    }

    #[test]
    fn intra_task_dep_not_found() {
        let src = "[[task]]\nname=\"t\"\n[[task.cell]]\nid=\"a\"\ncwd=\"/r\"\nprompt=\"p\"\ndepends_on=[\"ghost\"]\n";
        let r = validate_toml(src);
        assert!(
            r.errors
                .iter()
                .any(|e| e.kind == ErrorKind::IntraTaskRefNotFound)
        );
    }

    #[test]
    fn cross_task_dep_is_not_flagged_here() {
        let src = "[[task]]\nname=\"t\"\n[[task.cell]]\nid=\"a\"\ncwd=\"/r\"\nprompt=\"p\"\ndepends_on=[\"other/b\"]\n";
        let r = validate_toml(src);
        assert!(
            !r.errors
                .iter()
                .any(|e| e.kind == ErrorKind::IntraTaskRefNotFound)
        );
    }

    // ── upstream <<task:...>> existence checks ─────────────────────────────

    #[test]
    fn upstream_task_ref_resolves_within_same_file() {
        let src = "[[task]]\nname=\"a\"\n[[task.cell]]\nid=\"work\"\ncwd=\"/r\"\nprompt=\"p\"\n\
                    [[task]]\nname=\"b\"\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\nupstream=\"<<task:a>>\"\n";
        let r = validate_toml(src);
        assert!(
            !r.errors.iter().any(|e| e.kind == ErrorKind::UnknownTaskRef),
            "{:?}",
            r.errors
        );
    }

    #[test]
    fn upstream_task_cell_ref_resolves_within_same_file() {
        let src = "[[task]]\nname=\"a\"\n[[task.cell]]\nid=\"work\"\ncwd=\"/r\"\nprompt=\"p\"\n\
                    [[task]]\nname=\"b\"\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\nupstream=\"<<task:a/work>>\"\n";
        let r = validate_toml(src);
        assert!(
            !r.errors.iter().any(|e| e.kind == ErrorKind::UnknownTaskRef),
            "{:?}",
            r.errors
        );
    }

    #[test]
    fn upstream_task_ref_unknown_task_is_flagged() {
        let src = "[[task]]\nname=\"b\"\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\nupstream=\"<<task:ghost>>\"\n";
        let r = validate_toml(src);
        assert!(
            r.errors.iter().any(|e| e.kind == ErrorKind::UnknownTaskRef
                && e.message.contains("unknown task \"ghost\"")),
            "{:?}",
            r.errors
        );
    }

    #[test]
    fn upstream_task_ref_unknown_cell_is_flagged() {
        let src = "[[task]]\nname=\"a\"\n[[task.cell]]\nid=\"work\"\ncwd=\"/r\"\nprompt=\"p\"\n\
                    [[task]]\nname=\"b\"\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\nupstream=\"<<task:a/ghost>>\"\n";
        let r = validate_toml(src);
        assert!(
            r.errors.iter().any(|e| e.kind == ErrorKind::UnknownTaskRef
                && e.message.contains("unknown cell \"ghost\" in task \"a\"")),
            "{:?}",
            r.errors
        );
    }

    #[test]
    fn upstream_task_ref_forward_reference_within_same_file_is_fine() {
        // "a" references "b", which is declared LATER in the same array --
        // the index is built from the whole array up front, so declaration
        // order must not matter.
        let src = "[[task]]\nname=\"a\"\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\nupstream=\"<<task:b>>\"\n\
                    [[task]]\nname=\"b\"\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\n";
        let r = validate_toml(src);
        assert!(
            !r.errors.iter().any(|e| e.kind == ErrorKind::UnknownTaskRef),
            "{:?}",
            r.errors
        );
    }

    #[test]
    fn upstream_task_ref_resolves_across_joined_files() {
        // Mirrors what `ralphus submit a.toml b.toml` does client-side:
        // read each file's TOML text independently and join with a blank
        // line into ONE combined submission before validating -- so a task
        // declared in "file one" must be a valid `upstream` target for a
        // cell declared in "file two", once joined.
        let file_a = "[[task]]\nname=\"upstream-task\"\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\n";
        let file_b = "[[task]]\nname=\"downstream-task\"\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\nupstream=\"<<task:upstream-task>>\"\n";
        let combined = format!("{file_a}\n\n{file_b}");
        let r = validate_toml(&combined);
        assert!(
            !r.errors.iter().any(|e| e.kind == ErrorKind::UnknownTaskRef),
            "{:?}",
            r.errors
        );
    }

    #[test]
    fn upstream_task_ref_unknown_across_joined_files_is_still_flagged() {
        // Same join as above, but the referenced task exists in neither
        // joined file -- joining two files must not accidentally make an
        // otherwise-invalid reference look resolvable.
        let file_a = "[[task]]\nname=\"unrelated-task\"\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\n";
        let file_b = "[[task]]\nname=\"downstream-task\"\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\nupstream=\"<<task:ghost>>\"\n";
        let combined = format!("{file_a}\n\n{file_b}");
        let r = validate_toml(&combined);
        assert!(
            r.errors.iter().any(|e| e.kind == ErrorKind::UnknownTaskRef
                && e.message.contains("unknown task \"ghost\"")),
            "{:?}",
            r.errors
        );
    }

    #[test]
    fn upstream_plain_branch_name_is_not_checked_against_the_task_index() {
        // A literal (non-sentinel) `upstream` value is syntactically valid
        // today (see the field's own doc comment -- nothing currently acts
        // on it, but `check_upstream` doesn't reject it either), so it must
        // never be flagged as an unknown task ref just because it isn't a
        // task name.
        let src = "[[task]]\nname=\"a\"\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\nupstream=\"origin/some-branch\"\n";
        let r = validate_toml(src);
        assert!(
            !r.errors.iter().any(|e| e.kind == ErrorKind::UnknownTaskRef),
            "{:?}",
            r.errors
        );
    }

    #[test]
    fn dependency_cycle_detected() {
        let src = "[[task]]\nname=\"t\"\n[[task.cell]]\nid=\"a\"\ncwd=\"/r\"\nprompt=\"p\"\ndepends_on=[\"b\"]\n[[task.cell]]\nid=\"b\"\ncwd=\"/r\"\nprompt=\"p\"\ndepends_on=[\"a\"]\n";
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
        let src = "[[task]]\nname=\"t\"\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\n[[task.cell.proof]]\ncommand=\"c\"\nrestart_on=[\"nope\"]\n";
        let r = validate_toml(src);
        assert!(
            r.errors
                .iter()
                .any(|e| e.kind == ErrorKind::InvalidValue && e.message.contains("restart_on"))
        );
    }

    #[test]
    fn environment_accepted_on_task_and_cell() {
        let src = "[[task]]\nname=\"t\"\nenvironment={A=\"1\"}\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\nenvironment={B=\"2\"}\n";
        let r = validate_toml(src);
        assert!(r.is_ok(), "{:?}", r.errors);
    }

    #[test]
    fn environment_accepted_on_proof_steps() {
        // RAL-191: `environment` on `[[task.proof]]` / `[[task.cell.proof]]`.
        let src = "[[task]]\nname=\"t\"\n[[task.proof]]\ncommand=\"c\"\nenvironment={A=\"1\"}\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\n[[task.cell.proof]]\ncommand=\"d\"\nenvironment={B=\"2\"}\n";
        let r = validate_toml(src);
        assert!(r.is_ok(), "{:?}", r.errors);
    }

    #[test]
    fn environment_invalid_key_on_a_proof_step_reported() {
        // The same key/value rules apply at the proof layer -- an invalid
        // identifier here would otherwise reach `build_command_line_with_env`.
        let src = "[[task]]\nname=\"t\"\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\n[[task.cell.proof]]\ncommand=\"c\"\nenvironment={\"BAD-KEY\"=\"x\"}\n";
        let r = validate_toml(src);
        assert!(
            r.errors
                .iter()
                .any(|e| e.kind == ErrorKind::InvalidValue && e.message.contains("BAD-KEY")),
            "{:?}",
            r.errors
        );
    }

    #[test]
    fn environment_non_string_value_on_a_proof_step_reported() {
        let src = "[[task]]\nname=\"t\"\n[[task.proof]]\ncommand=\"c\"\nenvironment={A=1}\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\n";
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
    fn environment_wrong_type_reported() {
        let src =
            "[[task]]\nname=\"t\"\nenvironment=\"nope\"\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\n";
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
            "[[task]]\nname=\"t\"\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\nenvironment={A=1}\n";
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
        let src = "[[task]]\nname=\"t\"\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\nenvironment={\"1BAD\"=\"x\"}\n";
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
        let src = "[[task]]\nname=\"t\"\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\nagent=\"claude-code\"\nsystem_prompt=\"be terse\"\nsystem_prompt_position=\"append\"\n";
        let r = validate_toml(src);
        assert!(r.is_ok(), "{:?}", r.errors);
    }

    #[test]
    fn system_prompt_valid_for_codex() {
        // Codex has no dedicated system-prompt flag but delivers `system_prompt`
        // via `-c developer_instructions=...` (see `CodexBackend`), so it's
        // accepted the same as claude-code.
        let src = "[[task]]\nname=\"t\"\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\nagent=\"codex\"\nsystem_prompt=\"be terse\"\nsystem_prompt_position=\"append\"\n";
        let r = validate_toml(src);
        assert!(r.is_ok(), "{:?}", r.errors);
    }

    #[test]
    fn system_prompt_valid_for_codex_cli_alias() {
        let src = "[[task]]\nname=\"t\"\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\nagent=\"codex-cli\"\nsystem_prompt=\"be terse\"\n";
        let r = validate_toml(src);
        assert!(r.is_ok(), "{:?}", r.errors);
    }

    #[test]
    fn system_prompt_inherits_task_agent() {
        // agent set at the task level (claude-code); the cell omits it.
        let src = "[[task]]\nname=\"t\"\nagent=\"claude-code\"\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\nsystem_prompt=\"be terse\"\n";
        let r = validate_toml(src);
        assert!(r.is_ok(), "{:?}", r.errors);
    }

    #[test]
    fn system_prompt_rejected_for_default_agent() {
        // No agent set anywhere → resolves to the default "claude", not claude-code.
        let src = "[[task]]\nname=\"t\"\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\nsystem_prompt=\"be terse\"\n";
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
        let src = "[[task]]\nname=\"t\"\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\nagent=\"ollama\"\nsystem_prompt=\"be terse\"\n";
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
    fn system_prompt_deferred_to_daemon_for_custom_agent_profile() {
        // "openrouter-deepseek" isn't a RESERVED_AGENT_NAMES entry, so `core`
        // can't tell whether it's a custom `[agent.profiles.*]` resolving to a
        // system_prompt-capable backend (e.g. claude-code) -- it must not
        // reject offline. The daemon checks the resolved backend instead, see
        // `daemon::agent_profiles::validate_task_file_profiles`.
        let src = "[[task]]\nname=\"t\"\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\nagent=\"openrouter-deepseek\"\nsystem_prompt=\"be terse\"\nsystem_prompt_position=\"append\"\n";
        let r = validate_toml(src);
        assert!(r.is_ok(), "{:?}", r.errors);
    }

    #[test]
    fn system_prompt_position_rejects_unknown_value() {
        let src = "[[task]]\nname=\"t\"\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\nagent=\"claude-code\"\nsystem_prompt=\"x\"\nsystem_prompt_position=\"prepend\"\n";
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
        let src = "[[task]]\nname=\"t\"\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\nagent=\"claude-code\"\nsystem_prompt=123\n";
        let r = validate_toml(src);
        assert!(r.errors.iter().any(|e| e.kind == ErrorKind::WrongType));
    }

    #[test]
    fn toplevel_review_block_is_valid() {
        let src = "[[task]]\nname=\"t\"\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\nreview=\"be\"\n[[review]]\nid=\"be\"\n";
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
                   [[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\nmachine=\"incredibuild:A\"\nreview=\"r\"\n\
                   [[task.cell.proof]]\ncommand=\"cargo test\"\nmachine=\"incredibuild:A\"\n\
                   [[task.proof]]\ncommand=\"cargo fmt\"\nmachine=\"local\"\n\
                   [[review]]\nid=\"r\"\nmachine=\"incredibuild:C\"\n";
        assert!(
            validate_toml(src).is_ok(),
            "{:?}",
            validate_toml(src).errors
        );
    }

    #[test]
    fn machine_without_a_scheme_is_rejected() {
        let src = "[[task]]\nname=\"t\"\nmachine=\"incredibuild\"\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\n";
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
        let src = "[[task]]\nname=\"t\"\nmachine=\"incredibuild:\"\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\n";
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
        let src = "[[task]]\nname=\"t\"\nmachine=\"C:\\\\build\\\\wt\"\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\n";
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
        let src = "[[task]]\nname=\"t\"\nmachine=42\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\n";
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
        let src = "[[task]]\nname=\"t\"\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\n";
        assert!(
            validate_toml(src).is_ok(),
            "{:?}",
            validate_toml(src).errors
        );
    }

    #[test]
    fn review_with_action_command_is_valid() {
        let src = "[[task]]\nname=\"t\"\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\nreview=\"r\"\n[[review]]\nid=\"r\"\n[[review.action]]\nlabel=\"Run tests\"\ncommand=\"cargo test\"\n";
        assert!(
            validate_toml(src).is_ok(),
            "{:?}",
            validate_toml(src).errors
        );
    }

    #[test]
    fn review_with_action_prompt_is_valid() {
        let src = "[[task]]\nname=\"t\"\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\nreview=\"r\"\n[[review]]\nid=\"r\"\n[[review.action]]\nlabel=\"Check UI\"\nprompt=\"Open localhost:3000 and verify the wizard\"\n";
        assert!(
            validate_toml(src).is_ok(),
            "{:?}",
            validate_toml(src).errors
        );
    }

    #[test]
    fn review_action_both_prompt_and_command_rejected() {
        let src = "[[task]]\nname=\"t\"\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\nreview=\"r\"\n[[review]]\nid=\"r\"\n[[review.action]]\nlabel=\"Check\"\nprompt=\"do x\"\ncommand=\"do y\"\n";
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
        let src = "[[task]]\nname=\"t\"\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\nreview=\"r\"\n[[review]]\nid=\"r\"\n[[review.action]]\nlabel=\"Check\"\n";
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
        let src = "[[task]]\nname=\"t\"\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\nreview=\"r\"\n[[review]]\nid=\"r\"\n[[review.action]]\ncommand=\"cargo test\"\n";
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
        let src = "[[task]]\nname=\"t\"\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\nreview=\"r\"\n[[review]]\nid=\"r\"\n[[review.action]]\nlabel=\"x\"\ncommand=\"y\"\nfoo=\"bar\"\n";
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
        let src = "[[task]]\nname=\"t\"\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\nreview=\"r\"\n[[review]]\nid=\"r\"\n[[review.action]]\nlabel=\"Serve\"\ncommand=\"ralphus-daemon serve --port {port}\"\ncleanup_command=\"ralphus-daemon stop --port {port}\"\n";
        assert!(
            validate_toml(src).is_ok(),
            "{:?}",
            validate_toml(src).errors
        );
    }

    #[test]
    fn review_action_input_is_valid() {
        let src = "[[task]]\nname=\"t\"\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\nreview=\"r\"\n[[review]]\nid=\"r\"\n[[review.action]]\nlabel=\"Serve\"\ncommand=\"ralphus-daemon serve --port {port}\"\n[[review.action.input]]\nname=\"port\"\nmessage=\"Port for the daemon\"\ndefault=\"7890\"\n";
        assert!(
            validate_toml(src).is_ok(),
            "{:?}",
            validate_toml(src).errors
        );
    }

    #[test]
    fn review_action_input_missing_name_rejected() {
        let src = "[[task]]\nname=\"t\"\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\nreview=\"r\"\n[[review]]\nid=\"r\"\n[[review.action]]\nlabel=\"Serve\"\ncommand=\"ralphus-daemon serve --port {port}\"\n[[review.action.input]]\nmessage=\"Port for the daemon\"\n";
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
        let src = "[[task]]\nname=\"t\"\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\nreview=\"r\"\n[[review]]\nid=\"r\"\n[[review.action]]\nlabel=\"Serve\"\ncommand=\"ralphus-daemon serve --port {port}\"\n[[review.action.input]]\nname=\"port\"\n";
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
        let src = "[[task]]\nname=\"t\"\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\nreview=\"r\"\n[[review]]\nid=\"r\"\n[[review.action]]\nlabel=\"Serve\"\ncommand=\"ralphus-daemon serve --port {port}\"\n[[review.action.input]]\nname=\"port\"\nmessage=\"Port\"\nfoo=\"bar\"\n";
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
        let src = "[[task]]\nname=\"t\"\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\nreview=\"ralphus:new-review/ral-batch\"\n[[review]]\nid=\"ralphus:new-review/ral-batch\"\n";
        assert!(
            validate_toml(src).is_ok(),
            "{:?}",
            validate_toml(src).errors
        );
    }

    #[test]
    fn malformed_review_link_id_reported() {
        // Right scheme, but empty key after the slash.
        let src = "[[task]]\nname=\"t\"\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\n[[review]]\nid=\"ralphus:new-review/\"\n";
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
        let src = "[[task]]\nname=\"t\"\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\n[[review]]\nid=\"ralphus:review/xyz\"\n";
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
        let src = "[[task]]\nname=\"t\"\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\n[[review]]\nbranch=\"x\"\n";
        let r = validate_toml(src);
        assert!(
            r.errors
                .iter()
                .any(|e| e.kind == ErrorKind::UnknownKey && e.message.contains("branch"))
        );
    }

    #[test]
    fn review_wrong_type_reported() {
        let src =
            "[[task]]\nname=\"t\"\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\n[[review]]\nid=123\n";
        let r = validate_toml(src);
        assert!(r.errors.iter().any(|e| e.kind == ErrorKind::WrongType));
    }

    #[test]
    fn cell_review_must_be_string_not_table() {
        // A cell's `review` field opts into a top-level [[review]] block by id
        // and must be a string; a table value is rejected.
        let src =
            "[[task]]\nname=\"t\"\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\nreview={id=\"x\"}\n";
        let r = validate_toml(src);
        assert!(
            r.errors.iter().any(|e| e.kind == ErrorKind::WrongType),
            "a table-valued cell 'review' must be rejected: {:?}",
            r.errors
        );
    }

    #[test]
    fn review_without_action_block_works_identically() {
        // A [[review]] with no [[review.action]] sub-blocks must still validate.
        let src = "[[task]]\nname=\"t\"\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\nreview=\"r\"\n[[review]]\nid=\"r\"\nagent=\"claude\"\nmodel=\"claude-opus-4-8\"\n";
        assert!(
            validate_toml(src).is_ok(),
            "{:?}",
            validate_toml(src).errors
        );
    }

    #[test]
    fn subprojects_single_valid() {
        let src = "[[task]]\nname=\"t\"\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\nsubprojects=[\"packages/foo\"]\n";
        assert!(
            validate_toml(src).is_ok(),
            "{:?}",
            validate_toml(src).errors
        );
    }

    #[test]
    fn subprojects_multiple_valid() {
        let src = "[[task]]\nname=\"t\"\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\nsubprojects=[\"packages/foo\",\"packages/bar\"]\n";
        assert!(
            validate_toml(src).is_ok(),
            "{:?}",
            validate_toml(src).errors
        );
    }

    #[test]
    fn subprojects_nested_path_valid() {
        let src = "[[task]]\nname=\"t\"\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\nsubprojects=[\"a/b/c\"]\n";
        assert!(
            validate_toml(src).is_ok(),
            "{:?}",
            validate_toml(src).errors
        );
    }

    #[test]
    fn subprojects_empty_entry_rejected() {
        let src =
            "[[task]]\nname=\"t\"\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\nsubprojects=[\"\"]\n";
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
        let src = "[[task]]\nname=\"t\"\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\nsubprojects=[\"/packages/foo\"]\n";
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
        let src = "[[task]]\nname=\"t\"\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\nsubprojects=[\"packages/../etc\"]\n";
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
            "[[task]]\nname=\"t\"\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\nsubprojects=123\n";
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
        let src = "[[task]]\nname=\"t\"\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\nsubprojects=\"packages/foo\"\n";
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
        let src = "[[task]]\nname=\"task-a\"\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\n\
                    [[task]]\nname=\"t\"\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\nupstream=\"<<task:task-a>>\"\n";
        let r = validate_toml(src);
        assert!(r.is_ok(), "{:?}", r.errors);
    }

    #[test]
    fn upstream_task_cell_sentinel_is_valid() {
        let src = "[[task]]\nname=\"task-a\"\n[[task.cell]]\nid=\"cell-1\"\ncwd=\"/r\"\nprompt=\"p\"\n\
                    [[task]]\nname=\"t\"\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\nupstream=\"<<task:task-a/cell-1>>\"\n";
        let r = validate_toml(src);
        assert!(r.is_ok(), "{:?}", r.errors);
    }

    #[test]
    fn upstream_empty_sentinel_ref_is_rejected() {
        let src = "[[task]]\nname=\"t\"\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\nupstream=\"<<task:>>\"\n";
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
        let src = "[[task]]\nname=\"t\"\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\nupstream=\"\"\n";
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
        let src = "[[task]]\nname=\"t\"\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\nupstream=123\n";
        let r = validate_toml(src);
        assert!(r.errors.iter().any(|e| e.kind == ErrorKind::WrongType));
    }

    // ── worktree placeholder cwd (RAL-100) ────────────────────────────────────

    #[test]
    fn placeholder_cwd_with_project_is_valid() {
        let src = "[[task]]\nname=\"t\"\nproject=\"my-project\"\n[[task.cell]]\ncwd=\"ralphus:new-worktree/feat?upstream=main\"\nprompt=\"p\"\n";
        let r = validate_toml(src);
        assert!(r.is_ok(), "{:?}", r.errors);
    }

    #[test]
    fn placeholder_cwd_without_project_is_rejected() {
        let src = "[[task]]\nname=\"t\"\n[[task.cell]]\ncwd=\"ralphus:new-worktree/feat?upstream=main\"\nprompt=\"p\"\n";
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
    fn placeholder_cwd_without_upstream_is_rejected() {
        let src = "[[task]]\nname=\"t\"\nproject=\"my-project\"\n[[task.cell]]\ncwd=\"ralphus:new-worktree/feat\"\nprompt=\"p\"\n";
        let r = validate_toml(src);
        assert!(
            r.errors
                .iter()
                .any(|e| e.kind == ErrorKind::MissingRequired && e.message.contains("upstream")),
            "{:?}",
            r.errors
        );
    }

    #[test]
    fn placeholder_cwd_with_empty_upstream_value_is_rejected() {
        let src = "[[task]]\nname=\"t\"\nproject=\"my-project\"\n[[task.cell]]\ncwd=\"ralphus:new-worktree/feat?upstream=\"\nprompt=\"p\"\n";
        let r = validate_toml(src);
        assert!(
            r.errors
                .iter()
                .any(|e| e.kind == ErrorKind::MissingRequired && e.message.contains("upstream")),
            "{:?}",
            r.errors
        );
    }

    #[test]
    fn placeholder_cwd_with_remote_qualified_upstream_is_valid() {
        let src = "[[task]]\nname=\"t\"\nproject=\"my-project\"\n[[task.cell]]\ncwd=\"ralphus:new-worktree/origin/feature/x?upstream=origin/blah\"\nprompt=\"p\"\n";
        let r = validate_toml(src);
        assert!(r.is_ok(), "{:?}", r.errors);
    }

    #[test]
    fn old_style_project_prefixed_cwd_is_treated_as_a_plain_path() {
        // The pre-RAL-100-redesign `<project>:worktree/<branch>` scheme no
        // longer parses as a placeholder, so it doesn't require 'project' to
        // be set -- it's just an (unusual, but not our concern here) literal
        // cwd string.
        let src =
            "[[task]]\nname=\"t\"\n[[task.cell]]\ncwd=\"my-project:worktree/feat\"\nprompt=\"p\"\n";
        let r = validate_toml(src);
        assert!(r.is_ok(), "{:?}", r.errors);
    }

    #[test]
    fn plain_path_cwd_does_not_require_project() {
        let src = "[[task]]\nname=\"t\"\n[[task.cell]]\ncwd=\"/repo\"\nprompt=\"p\"\n";
        assert!(
            validate_toml(src).is_ok(),
            "{:?}",
            validate_toml(src).errors
        );
    }

    #[test]
    fn upstream_unknown_key_would_have_been_caught() {
        // Regression guard: "upstream" must be in CELL_KEYS so it is NOT
        // reported as an unknown key.
        let src = "[[task]]\nname=\"t\"\n[[task.cell]]\ncwd=\"/r\"\nprompt=\"p\"\nupstream=\"<<task:dep>>\"\n";
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
