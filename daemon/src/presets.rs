//! Preset registry: named bundles of field defaults an author references
//! from a task's, cell's, or proof step's `extends` array via a
//! `<<ralphus:presets/<name>>>` sentinel (see
//! [`ralphus_core::schema::parse_preset_sentinel`]), so common boilerplate
//! (e.g. a "commit and push" `system_prompt`, or a "complex task" context
//! sizing triple) doesn't need to be retyped into every cell.
//!
//! Store-backed and CLI/UI-mutable -- mirrors `crate::triage`'s type
//! registry (not `crate::agent_profiles`'s config-file-only pattern), since
//! a preset is meant to be added/edited/removed at runtime, not
//! redeployed.
//!
//! Applying a preset is a one-time, submit-time stamp: [`apply_presets`]
//! fills a scalar field only when the entity left it unset (an explicit value
//! wins), always applies a `prompt`/`system_prompt` template (an author's own
//! text the template doesn't reference is appended after it), and silently
//! skips any preset field that doesn't exist
//! on the entity kind it's applied to (e.g. `system_prompt` via a
//! task-level `extends` -- `system_prompt` only exists on `CellDef`; see
//! each field's doc comment on `TaskDef`/`CellDef`/`ProofStep` for the
//! exact applicability). Once stamped, the resulting value is
//! indistinguishable from one the author typed directly -- presets carry no
//! provenance and are never re-applied on a restart.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use rusqlite::params;
use serde::Serialize;

use ralphus_core::schema::{
    CellDef, ProofStep, ScopeLevel, TaskDef, TaskFile, parse_preset_sentinel,
};
use ralphus_core::validate::{ErrorKind, ValidationError};

use crate::store::{Result as StoreResult, Store, now_ms};

/// A registered preset: a name plus the (all optional) field values it
/// supplies. Every field mirrors a [`CellDef`]/[`TaskDef`]/[`ProofStep`]
/// field of the same name -- see [`apply_presets`] for which entity kinds
/// honor which.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct PresetView {
    pub name: String,
    /// A prompt template: stamped into the entity's own `prompt`, with
    /// `<<ralphus:linked-field/...>>` references expanded (see
    /// [`apply_presets`]).
    pub prompt: Option<String>,
    /// A system-prompt template, expanded the same way as `prompt`.
    pub system_prompt: Option<String>,
    pub system_prompt_position: Option<String>,
    pub maximum_context: Option<u64>,
    pub auto_compact_threshold: Option<u64>,
    pub maximum_tool_output_tokens: Option<u64>,
    pub created_at_ms: i64,
    /// Where this preset came from; on-disk presets are read-only.
    pub source: PresetSource,
    /// The file an on-disk preset was read from.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

/// Where a [`PresetView`] is defined.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PresetSource {
    /// The daemon's database: editable and removable at runtime.
    #[default]
    Db,
    /// A file under a `preset_paths` entry of the global config: read-only.
    Disk,
}

/// Starter presets seeded once, the first time the `presets` table is
/// created (`Store::init_schema`, mirroring
/// `crate::triage::DEFAULT_TRIAGE_TYPES`'s seeding pattern) -- never
/// re-seeded afterward, so deregistering one of these is honored across
/// every later restart.
pub struct PresetSeed {
    pub name: &'static str,
    pub prompt: Option<&'static str>,
    pub system_prompt: Option<&'static str>,
    pub system_prompt_position: Option<&'static str>,
    pub maximum_context: Option<u64>,
    pub auto_compact_threshold: Option<u64>,
    pub maximum_tool_output_tokens: Option<u64>,
}

pub const DEFAULT_PRESETS: &[PresetSeed] = &[
    PresetSeed {
        name: "easy_task",
        prompt: None,
        system_prompt: None,
        system_prompt_position: None,
        maximum_context: Some(75_000),
        auto_compact_threshold: Some(51_000),
        maximum_tool_output_tokens: Some(8_000),
    },
    PresetSeed {
        name: "medium_task",
        prompt: None,
        system_prompt: None,
        system_prompt_position: None,
        maximum_context: Some(120_000),
        auto_compact_threshold: Some(86_000),
        maximum_tool_output_tokens: Some(12_000),
    },
    PresetSeed {
        name: "complex_task",
        prompt: None,
        system_prompt: None,
        system_prompt_position: None,
        maximum_context: Some(200_000),
        auto_compact_threshold: Some(150_000),
        maximum_tool_output_tokens: Some(15_000),
    },
    PresetSeed {
        name: "no_git_commit",
        prompt: None,
        system_prompt: Some(
            "Do NOT commit and do NOT push under any circumstances. You are working in a \
             dedicated git worktree. You may run read-only git commands. Implement the ticket \
             completely, follow all applicable AGENTS.md instructions, run relevant formatters, \
             linters, and tests, and keep changes in this worktree.",
        ),
        system_prompt_position: Some(ralphus_core::schema::SYSTEM_PROMPT_POSITION_APPEND),
        maximum_context: None,
        auto_compact_threshold: None,
        maximum_tool_output_tokens: None,
    },
    PresetSeed {
        name: "commit_and_push",
        prompt: None,
        system_prompt: Some(
            "Do NOT run formatters, linters, or tests. Just stage the intended source changes, \
             commit, and push.",
        ),
        system_prompt_position: Some(ralphus_core::schema::SYSTEM_PROMPT_POSITION_APPEND),
        maximum_context: None,
        auto_compact_threshold: None,
        maximum_tool_output_tokens: None,
    },
];

// ── Registry ─────────────────────────────────────────────────────────────

impl Store {
    /// Register (or update) a preset. Upserts on `name`, matching
    /// [`Store::register_triage_type`]'s behavior.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn register_preset(&self, view: &PresetView) -> StoreResult<()> {
        let name = view.name.trim();
        self.conn.execute(
            "INSERT INTO presets(name, prompt, system_prompt, system_prompt_position, maximum_context, auto_compact_threshold, maximum_tool_output_tokens, created_at_ms)
             VALUES(?,?,?,?,?,?,?,?)
             ON CONFLICT(name) DO UPDATE SET
                prompt=excluded.prompt,
                system_prompt=excluded.system_prompt,
                system_prompt_position=excluded.system_prompt_position,
                maximum_context=excluded.maximum_context,
                auto_compact_threshold=excluded.auto_compact_threshold,
                maximum_tool_output_tokens=excluded.maximum_tool_output_tokens",
            params![
                name,
                view.prompt,
                view.system_prompt,
                view.system_prompt_position,
                view.maximum_context
                    .map(|v| i64::try_from(v).unwrap_or(i64::MAX)),
                view.auto_compact_threshold
                    .map(|v| i64::try_from(v).unwrap_or(i64::MAX)),
                view.maximum_tool_output_tokens
                    .map(|v| i64::try_from(v).unwrap_or(i64::MAX)),
                now_ms(),
            ],
        )?;
        crate::rlog!(INFO, "ralphus [store] preset {name:?} registered");
        let _ = self.cartographer_log(crate::cartographer::CartographerEntry {
            level: crate::logging::LogLevel::INFO,
            source: "store",
            message: "preset registered",
            scope: Some("presets"),
            squad_id: None,
            guardian_id: None,
            cell_id: None,
            task: None,
            log_path: None,
            payload: serde_json::json!({ "name": name }),
            admin_only: false,
        });
        Ok(())
    }

    /// Every registered preset, alphabetical by name.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn list_presets(&self) -> StoreResult<Vec<PresetView>> {
        let mut stmt = self.conn.prepare(
            "SELECT name, prompt, system_prompt, system_prompt_position, maximum_context, auto_compact_threshold, maximum_tool_output_tokens, created_at_ms
             FROM presets ORDER BY name",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(PresetView {
                    name: r.get(0)?,
                    prompt: r.get(1)?,
                    system_prompt: r.get(2)?,
                    system_prompt_position: r.get(3)?,
                    maximum_context: r
                        .get::<_, Option<i64>>(4)?
                        .map(|v| u64::try_from(v).unwrap_or(0)),
                    auto_compact_threshold: r
                        .get::<_, Option<i64>>(5)?
                        .map(|v| u64::try_from(v).unwrap_or(0)),
                    maximum_tool_output_tokens: r
                        .get::<_, Option<i64>>(6)?
                        .map(|v| u64::try_from(v).unwrap_or(0)),
                    created_at_ms: r.get(7)?,
                    source: PresetSource::Db,
                    path: None,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// One preset by exact name, or `None`.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn get_preset(&self, name: &str) -> StoreResult<Option<PresetView>> {
        Ok(self
            .list_presets()?
            .into_iter()
            .find(|p| p.name == name.trim()))
    }

    /// Remove a preset. Unlike [`Store::deregister_triage_type`], no preset
    /// is structurally required to exist, so this is a plain remove: `true`
    /// if a row was deleted, `false` if `name` wasn't registered.
    ///
    /// Deliberately does not check whether any historical submission
    /// referenced this preset -- same rationale as
    /// [`Store::deregister_triage_type`]: a past squad already had its
    /// fields stamped at submit time, and a historical record should not
    /// block cleaning up the registry.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn deregister_preset(&self, name: &str) -> StoreResult<bool> {
        let name = name.trim();
        let n = self
            .conn
            .execute("DELETE FROM presets WHERE name = ?", params![name])?;
        if n > 0 {
            crate::rlog!(INFO, "ralphus [store] preset {name:?} removed");
            let _ = self.cartographer_log(crate::cartographer::CartographerEntry {
                level: crate::logging::LogLevel::INFO,
                source: "store",
                message: "preset removed",
                scope: Some("presets"),
                squad_id: None,
                guardian_id: None,
                cell_id: None,
                task: None,
                log_path: None,
                payload: serde_json::json!({ "name": name }),
                admin_only: false,
            });
        }
        Ok(n > 0)
    }
}

// ── Submit-time registry validation ─────────────────────────────────────────

/// Best-effort 1-based line number of `sentinel`'s (the full, quoted
/// `<<ralphus:presets/<name>>>` string as it appears in an `extends` array)
/// occurrence in the raw TOML, scanning forward from `search_from_line` so
/// repeated identical values in different `extends` arrays each resolve to
/// their own occurrence. Mirrors `crate::triage::find_triage_type_line`'s
/// same-shaped best-effort scan -- good enough for "roughly which line", not
/// a byte-exact guarantee.
fn find_preset_sentinel_line(
    raw_toml: &str,
    sentinel: &str,
    search_from_line: usize,
) -> Option<u32> {
    let needle = format!("\"{sentinel}\"");
    for (i, line) in raw_toml.lines().enumerate().skip(search_from_line) {
        if line.contains(&needle) {
            return Some((i + 1) as u32);
        }
    }
    None
}

/// Validate one entity's `extends` list against the registered presets,
/// appending any "not registered" errors to `errors` and advancing
/// `search_from_line` past each error found (see
/// [`find_preset_sentinel_line`]).
fn check_extends_entries(
    raw_toml: &str,
    path: &str,
    extends: &[String],
    known_names: &[&str],
    search_from_line: &mut usize,
    errors: &mut Vec<ValidationError>,
) {
    for raw in extends {
        // A malformed/unwrapped entry was already rejected by
        // `core::validate::check_extends`.
        let Some(name) = parse_preset_sentinel(raw) else {
            continue;
        };
        if known_names.contains(&name) {
            continue;
        }
        let line = find_preset_sentinel_line(raw_toml, raw, *search_from_line);
        if let Some(l) = line {
            *search_from_line = l as usize;
        }
        errors.push(ValidationError {
            path: format!("{path}.extends"),
            kind: ErrorKind::InvalidValue,
            message: format!(
                "extends preset \"{name}\" is not registered (registered: {})",
                if known_names.is_empty() {
                    "none".to_string()
                } else {
                    known_names.join(", ")
                }
            ),
            line,
        });
    }
}

/// Validate every task's, cell's, and proof step's `extends` list (RAL-…)
/// against the daemon's preset registry -- `core::validate::check_extends`
/// only checked shape (a wrapped `<<ralphus:presets/<name>>>` sentinel with
/// a non-empty name), since `core` has no store access. A submission naming
/// an unregistered preset fails here, listing the currently registered
/// presets plus the offending line number (best-effort; see
/// [`find_preset_sentinel_line`]).
#[must_use]
pub fn validate_task_file_presets(
    store: &Store,
    raw_toml: &str,
    file: &TaskFile,
) -> Vec<ValidationError> {
    let mut errors = Vec::new();
    let registered = effective_presets(store).unwrap_or_default();
    let known_names: Vec<&str> = registered.iter().map(|p| p.name.as_str()).collect();
    let mut search_from_line = 0usize;

    for (task_idx, task) in file.task.iter().enumerate() {
        check_extends_entries(
            raw_toml,
            &format!("task[{task_idx}]"),
            &task.extends,
            &known_names,
            &mut search_from_line,
            &mut errors,
        );
        for (proof_idx, proof) in task.proof.iter().enumerate() {
            check_extends_entries(
                raw_toml,
                &format!("task[{task_idx}].proof[{proof_idx}]"),
                &proof.extends,
                &known_names,
                &mut search_from_line,
                &mut errors,
            );
        }
        for (cell_idx, cell) in task.cell.iter().enumerate() {
            check_extends_entries(
                raw_toml,
                &format!("task[{task_idx}].cell[{cell_idx}]"),
                &cell.extends,
                &known_names,
                &mut search_from_line,
                &mut errors,
            );
            for (proof_idx, proof) in cell.proof.iter().enumerate() {
                check_extends_entries(
                    raw_toml,
                    &format!("task[{task_idx}].cell[{cell_idx}].proof[{proof_idx}]"),
                    &proof.extends,
                    &known_names,
                    &mut search_from_line,
                    &mut errors,
                );
            }
        }
    }
    errors
}

// ── On-disk presets ─────────────────────────────────────────────────────────

/// One on-disk preset file. Every field is optional and mirrors the
/// same-named [`PresetView`] field; an unknown key is rejected so a typo
/// shows up as a warning instead of silently doing nothing.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct DiskPreset {
    prompt: Option<String>,
    system_prompt: Option<String>,
    system_prompt_position: Option<String>,
    maximum_context: Option<u64>,
    auto_compact_threshold: Option<u64>,
    maximum_tool_output_tokens: Option<u64>,
}

/// Collect every `*.toml` file at or under `path` (sorted, so the result is
/// deterministic), pairing each with the preset name its location implies:
/// the file's path relative to the directory it was found under, minus the
/// `.toml` extension, with `/` between segments. A directory `presets/`
/// holding `roles/foo.toml` therefore defines the preset `roles/foo` -- the
/// namespace is nothing but the sub-directory. A file named directly gets
/// its own stem as its name.
fn collect_disk_files(path: &Path, out: &mut Vec<(String, PathBuf)>, warnings: &mut Vec<String>) {
    fn walk(
        dir: &Path,
        prefix: &str,
        out: &mut Vec<(String, PathBuf)>,
        warnings: &mut Vec<String>,
    ) {
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(e) => {
                warnings.push(format!(
                    "cannot read preset directory {}: {e}",
                    dir.display()
                ));
                return;
            }
        };
        let mut entries: Vec<_> = entries.flatten().collect();
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for entry in entries {
            let p = entry.path();
            let file_name = entry.file_name().to_string_lossy().into_owned();
            if p.is_dir() {
                walk(&p, &format!("{prefix}{file_name}/"), out, warnings);
            } else if let Some(stem) = file_name.strip_suffix(".toml") {
                out.push((format!("{prefix}{stem}"), p));
            }
        }
    }

    if path.is_dir() {
        walk(path, "", out, warnings);
    } else if path.is_file() {
        match path.file_stem().and_then(|s| s.to_str()) {
            Some(stem) => out.push((stem.to_string(), path.to_path_buf())),
            None => warnings.push(format!("preset file {} has no usable name", path.display())),
        }
    } else {
        warnings.push(format!("preset path {} does not exist", path.display()));
    }
}

/// Read every preset defined under `roots` (see [`collect_disk_files`]).
/// A file that fails to parse, or whose name or values are invalid, is
/// skipped with a warning rather than failing the whole load -- a broken
/// preset file must not stop the daemon accepting submissions. When two
/// files define the same name the one listed first in `roots` wins.
#[must_use]
pub fn load_disk_presets(roots: &[PathBuf]) -> (Vec<PresetView>, Vec<String>) {
    let mut files = Vec::new();
    let mut warnings = Vec::new();
    for root in roots {
        collect_disk_files(root, &mut files, &mut warnings);
    }
    let mut presets: Vec<PresetView> = Vec::new();
    for (name, path) in files {
        if !ralphus_core::schema::is_valid_preset_name(&name) {
            warnings.push(format!(
                "preset file {} implies the invalid preset name {name:?}",
                path.display()
            ));
            continue;
        }
        if presets.iter().any(|p| p.name == name) {
            warnings.push(format!(
                "preset {name:?} in {} is shadowed by an earlier definition",
                path.display()
            ));
            continue;
        }
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) => {
                warnings.push(format!("cannot read preset file {}: {e}", path.display()));
                continue;
            }
        };
        let parsed: DiskPreset = match toml::from_str(&text) {
            Ok(parsed) => parsed,
            Err(e) => {
                warnings.push(format!("invalid preset file {}: {e}", path.display()));
                continue;
            }
        };
        if let Some(pos) = &parsed.system_prompt_position {
            if pos != ralphus_core::schema::SYSTEM_PROMPT_POSITION_APPEND {
                warnings.push(format!(
                    "invalid preset file {}: 'system_prompt_position' must be \"{}\"",
                    path.display(),
                    ralphus_core::schema::SYSTEM_PROMPT_POSITION_APPEND
                ));
                continue;
            }
        }
        presets.push(PresetView {
            name,
            prompt: parsed.prompt,
            system_prompt: parsed.system_prompt,
            system_prompt_position: parsed.system_prompt_position,
            maximum_context: parsed.maximum_context,
            auto_compact_threshold: parsed.auto_compact_threshold,
            maximum_tool_output_tokens: parsed.maximum_tool_output_tokens,
            created_at_ms: 0,
            source: PresetSource::Disk,
            path: Some(path.display().to_string()),
        });
    }
    (presets, warnings)
}

/// Warnings already reported this process, so a registry read on every
/// submit or board poll reports each distinct problem once.
fn warn_once(message: &str) {
    static SEEN: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> =
        std::sync::OnceLock::new();
    let seen = SEEN.get_or_init(Default::default);
    let first = seen
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(message.to_string());
    if first {
        // ralphus[ignore-rlog-pair]: preset files are read without a Store and on the request path of every submit
        crate::rlog!(WARNING, "ralphus [presets] {message}");
    }
}

/// Merge the database presets with the on-disk presets read from `roots`.
/// A same-named on-disk preset takes precedence over the database one --
/// on-disk presets are read-only, so letting them win is what keeps them
/// authoritative. The result is alphabetical by name.
#[must_use]
pub fn merge_presets(db: Vec<PresetView>, roots: &[PathBuf]) -> Vec<PresetView> {
    let (disk, warnings) = load_disk_presets(roots);
    for w in &warnings {
        warn_once(w);
    }
    let mut merged = disk;
    for p in db {
        if !merged.iter().any(|d| d.name == p.name) {
            merged.push(p);
        }
    }
    merged.sort_by(|a, b| a.name.cmp(&b.name));
    merged
}

/// Every preset a submission may reference: the on-disk ones named by the
/// global config's `preset_paths`, plus the database ones.
///
/// # Errors
/// Propagates any SQLite failure.
pub fn effective_presets(store: &Store) -> StoreResult<Vec<PresetView>> {
    Ok(merge_presets(
        store.list_presets()?,
        &crate::config::load_preset_paths(),
    ))
}

/// The on-disk preset named `name`, if any -- consulted before a
/// registration or removal so a read-only preset is never "edited" into a
/// database row that would then be silently shadowed.
#[must_use]
pub fn disk_preset_named(name: &str) -> Option<PresetView> {
    load_disk_presets(&crate::config::load_preset_paths())
        .0
        .into_iter()
        .find(|p| p.name == name.trim())
}

// ── Application (submit-time field stamping) ────────────────────────────────

type PresetMap<'a> = HashMap<&'a str, &'a PresetView>;

/// The presets an entity's `extends` list names, in list order, resolved
/// against `by_name`. An unresolvable name is silently dropped here --
/// [`validate_task_file_presets`] must have already run and rejected the
/// submission otherwise, so by the time [`apply_presets`] runs every name is
/// guaranteed to resolve.
fn resolved_presets<'a>(extends: &[String], by_name: &PresetMap<'a>) -> Vec<&'a PresetView> {
    extends
        .iter()
        .filter_map(|raw| parse_preset_sentinel(raw))
        .filter_map(|name| by_name.get(name).copied())
        .collect()
}

/// The last (in `extends` list order) resolved preset that defines a `Some`
/// value for a field, per this crate's "last listed wins" rule.
fn last_defined<T>(presets: &[&PresetView], field: impl Fn(&PresetView) -> Option<T>) -> Option<T> {
    presets.iter().rev().find_map(|p| field(p))
}

/// The text fields a linked-field reference inside a preset template may
/// address, as they stand on one entity.
#[derive(Debug, Default, Clone)]
struct TextFrame {
    prompt: Option<String>,
    system_prompt: Option<String>,
}

impl TextFrame {
    fn field(&self, name: &str) -> Option<&str> {
        match name {
            "prompt" => self.prompt.as_deref(),
            "system_prompt" => self.system_prompt.as_deref(),
            _ => None,
        }
    }
}

/// The text standing in for a linked field that could not be resolved.
fn not_found_text(field: &str) -> String {
    format!("<field {field} was not found>")
}

/// Resolve one `<<ralphus:linked-field/<path>>>` body against `chain`
/// (outermost entity first, the entity being expanded last). `None` means
/// "not a reference this expander handles" and leaves the sentinel in
/// place -- notably a `?text=` query, and any field other than `prompt` /
/// `system_prompt`. A reference that is handled but cannot be satisfied
/// (nothing that many levels up, or the field is unset) yields
/// [`not_found_text`].
fn resolve_text_link(body: &str, chain: &[TextFrame], levels: &[ScopeLevel]) -> Option<String> {
    let link = ralphus_core::schema::parse_linked_field(body)?;
    if link.query.is_some() {
        return None;
    }
    let parsed = ralphus_core::schema::parse_linked_field_path(link.path).ok()?;
    if !matches!(parsed.field, "prompt" | "system_prompt") {
        return None;
    }
    let value = chain
        .len()
        .checked_sub(1 + parsed.resolve_ups(levels))
        .and_then(|i| chain.get(i))
        .and_then(|frame| frame.field(parsed.field));
    Some(value.map_or_else(|| not_found_text(parsed.field), str::to_string))
}

/// Expand every handled linked field in `template`. The replacement keeps
/// the indentation of the line the reference sits on: each line of a
/// multi-line value after the first is indented to match, so a reference
/// written indented inside a template stays indented once expanded. A
/// value's trailing newlines are dropped, so the template's own blank lines
/// (not the value's) decide the spacing that follows it.
fn expand_template(template: &str, chain: &[TextFrame], levels: &[ScopeLevel]) -> String {
    template
        .split('\n')
        .map(|line| {
            let indent: String = line
                .chars()
                .take_while(|c| matches!(c, ' ' | '\t'))
                .collect();
            let expanded: Result<String, std::convert::Infallible> =
                ralphus_core::schema::replace_text_placeholders(line, |body| {
                    Ok(resolve_text_link(body, chain, levels).map(|value| {
                        let value = value.trim_end_matches(['\n', '\r']);
                        value
                            .split('\n')
                            .enumerate()
                            .map(|(i, l)| {
                                if i == 0 || l.is_empty() {
                                    l.to_string()
                                } else {
                                    format!("{indent}{l}")
                                }
                            })
                            .collect::<Vec<_>>()
                            .join("\n")
                    }))
                });
            expanded.unwrap_or_else(|never| match never {})
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Whether `template` references `field` of the very entity it is being
/// applied to (`<<ralphus:linked-field/./<field>>>`), evaluating any
/// `..[kind]` steps against `levels`.
fn template_wraps_own(template: &str, field: &str, levels: &[ScopeLevel]) -> bool {
    ralphus_core::schema::text_placeholders(template)
        .into_iter()
        .filter_map(ralphus_core::schema::parse_linked_field)
        .filter(|link| link.query.is_none())
        .filter_map(|link| ralphus_core::schema::parse_linked_field_path(link.path).ok())
        .any(|p| p.resolve_ups(levels) == 0 && p.field == field)
}

/// Apply one preset text `template` to an entity's `field`, which currently
/// holds `own`.
///
/// * `own` unset: the expanded template fills it.
/// * `own` set, template references the entity's own `field` (any reference
///   that lands on the entity itself once `..[kind]` steps resolve): the
///   expanded template, with `own` spliced in where it is referenced.
/// * `own` set, template does **not** reference it: the expanded template,
///   a blank line, then `own` verbatim.
///
/// The same rule applies to every entity kind and every text field.
fn apply_text(
    own: &mut Option<String>,
    field: &str,
    template: Option<&str>,
    chain: &[TextFrame],
    levels: &[ScopeLevel],
) {
    let Some(template) = template else { return };
    let expanded = expand_template(template, chain, levels);
    *own = Some(match own.take() {
        Some(value) if !template_wraps_own(template, field, levels) => {
            format!("{expanded}\n\n{value}")
        }
        _ => expanded,
    });
}

fn apply_to_task(task: &mut TaskDef, by_name: &PresetMap<'_>) {
    let presets = resolved_presets(&task.extends, by_name);
    if presets.is_empty() {
        return;
    }
    if task.maximum_context.is_none() {
        task.maximum_context = last_defined(&presets, |p| p.maximum_context);
    }
    if task.auto_compact_threshold.is_none() {
        task.auto_compact_threshold = last_defined(&presets, |p| p.auto_compact_threshold);
    }
    if task.maximum_tool_output_tokens.is_none() {
        task.maximum_tool_output_tokens = last_defined(&presets, |p| p.maximum_tool_output_tokens);
    }
    // `prompt`/`system_prompt`/`system_prompt_position` don't exist on
    // `TaskDef` -- silently skipped, per this module's applicability rule.
}

/// Apply a cell's presets and return the cell's resulting text frame, which
/// is the parent frame its proof steps' `../` references resolve against.
fn apply_to_cell(cell: &mut CellDef, by_name: &PresetMap<'_>, task_frame: &TextFrame) -> TextFrame {
    let presets = resolved_presets(&cell.extends, by_name);
    if !presets.is_empty() {
        // References resolve against the values the author wrote, so a
        // `system_prompt` template's `./prompt` sees the cell's own prompt
        // rather than whatever the `prompt` template makes of it.
        let own = TextFrame {
            prompt: cell.prompt.clone(),
            system_prompt: cell.system_prompt.clone(),
        };
        let chain = [task_frame.clone(), own];
        let levels = [ScopeLevel::Task, ScopeLevel::Cell];
        apply_text(
            &mut cell.system_prompt,
            "system_prompt",
            last_defined(&presets, |p| p.system_prompt.clone()).as_deref(),
            &chain,
            &levels,
        );
        // A `command` cell has no prompt for a preset to fill or frame.
        if cell.command.is_none() {
            apply_text(
                &mut cell.prompt,
                "prompt",
                last_defined(&presets, |p| p.prompt.clone()).as_deref(),
                &chain,
                &levels,
            );
        }
        if cell.system_prompt_position.is_none() {
            cell.system_prompt_position =
                last_defined(&presets, |p| p.system_prompt_position.clone());
        }
        if cell.maximum_context.is_none() {
            cell.maximum_context = last_defined(&presets, |p| p.maximum_context);
        }
        if cell.auto_compact_threshold.is_none() {
            cell.auto_compact_threshold = last_defined(&presets, |p| p.auto_compact_threshold);
        }
        if cell.maximum_tool_output_tokens.is_none() {
            cell.maximum_tool_output_tokens =
                last_defined(&presets, |p| p.maximum_tool_output_tokens);
        }
    }
    TextFrame {
        prompt: cell.prompt.clone(),
        system_prompt: cell.system_prompt.clone(),
    }
}

fn apply_to_proof(
    proof: &mut ProofStep,
    by_name: &PresetMap<'_>,
    parent_frame: &TextFrame,
    parent_level: ScopeLevel,
) {
    let presets = resolved_presets(&proof.extends, by_name);
    if presets.is_empty() {
        return;
    }
    // Only an AI (`prompt`) proof has a prompt to fill or frame; a
    // `command`/`brain` proof is left alone.
    if proof.command.is_none() && proof.brain.is_none() {
        let own = TextFrame {
            prompt: proof.prompt.clone(),
            system_prompt: None,
        };
        let chain = [parent_frame.clone(), own];
        apply_text(
            &mut proof.prompt,
            "prompt",
            last_defined(&presets, |p| p.prompt.clone()).as_deref(),
            &chain,
            &[parent_level, ScopeLevel::ProofStep],
        );
    }
    if proof.maximum_tool_output_tokens.is_none() {
        proof.maximum_tool_output_tokens = last_defined(&presets, |p| p.maximum_tool_output_tokens);
    }
    // `system_prompt`/`system_prompt_position`/`maximum_context`/
    // `auto_compact_threshold` don't exist on `ProofStep` -- silently
    // skipped, per this module's applicability rule.
}

/// Stamp each task's, cell's, and proof step's named presets' field values
/// into its own fields, using the registry [`effective_presets`] returns.
/// See [`apply_presets_from`].
pub fn apply_presets(store: &Store, file: &mut TaskFile) {
    let registered = effective_presets(store).unwrap_or_default();
    apply_presets_from(&registered, file);
}

/// Stamp each task's, cell's, and proof step's named presets' field values
/// into its own fields. Called once, at submit time, before persistence --
/// mirrors `agent_profiles::resolve_agent_candidate_lists`'s
/// mutate-`file`-in-place shape. Requires [`validate_task_file_presets`] to
/// have already run and rejected the submission on any unregistered name,
/// so every resolution here is infallible.
///
/// Scalar fields only fill a field that is currently `None`, so a value the
/// author typed explicitly is never overridden; when more than one preset
/// in an entity's own `extends` list defines the same field, the last one
/// in the list wins. `prompt` and `system_prompt` are *templates* that are
/// always applied (see [`apply_text`]: the entity's own text is spliced in
/// where referenced, else appended after the template):
/// `<<ralphus:linked-field/<path>>>` references inside them
/// to a `prompt`/`system_prompt` field are expanded -- `./prompt` is the
/// entity's own prompt, `../prompt` its parent's (a proof step's cell) --
/// and a reference that cannot be resolved becomes
/// `<field prompt was not found>` rather than an error. A preset field that
/// doesn't exist on the entity kind it's applied to (e.g. `system_prompt`
/// via a task-level `extends`) is silently skipped. Because a task-level
/// fill only ever touches the task's own field, the existing task→cell
/// inheritance (`resolve_maximum_context`, `resolve_auto_compact_threshold`,
/// `resolve_cell_maximum_tool_output_tokens` in `crate::store`) still
/// cascades it to a cell exactly as it always has -- no changes needed
/// there.
pub fn apply_presets_from(registered: &[PresetView], file: &mut TaskFile) {
    let by_name: PresetMap<'_> = registered.iter().map(|p| (p.name.as_str(), p)).collect();
    let task_frame = TextFrame::default();

    for task in &mut file.task {
        apply_to_task(task, &by_name);
        for proof in &mut task.proof {
            apply_to_proof(proof, &by_name, &task_frame, ScopeLevel::Task);
        }
        for cell in &mut task.cell {
            let cell_frame = apply_to_cell(cell, &by_name, &task_frame);
            for proof in &mut cell.proof {
                apply_to_proof(proof, &by_name, &cell_frame, ScopeLevel::Cell);
            }
        }
    }
}

/// After [`apply_presets`], re-check the "a cell has a `prompt` or a
/// `command`; a proof step has exactly one kind" requirement for every
/// entity that relied on a preset to supply it -- `core::validate` waives
/// that requirement for an entity with a non-empty `extends`, since it
/// cannot see what the presets contain.
#[must_use]
pub fn check_required_fields_after_presets(file: &TaskFile) -> Vec<ValidationError> {
    let mut errors = Vec::new();
    for (task_idx, task) in file.task.iter().enumerate() {
        let mut check_proof = |path: String, proof: &ProofStep| {
            if proof.extends.is_empty() {
                return;
            }
            if proof.command.is_none() && proof.brain.is_none() && proof.prompt.is_none() {
                errors.push(ValidationError {
                    path,
                    kind: ErrorKind::MissingRequired,
                    message: "proof step requires one of: command, brain, prompt (none of its \
                              `extends` presets supplies a prompt)"
                        .to_string(),
                    line: None,
                });
            }
        };
        for (i, proof) in task.proof.iter().enumerate() {
            check_proof(format!("task[{task_idx}].proof[{i}]"), proof);
        }
        for (cell_idx, cell) in task.cell.iter().enumerate() {
            for (i, proof) in cell.proof.iter().enumerate() {
                check_proof(
                    format!("task[{task_idx}].cell[{cell_idx}].proof[{i}]"),
                    proof,
                );
            }
        }
        for (cell_idx, cell) in task.cell.iter().enumerate() {
            if !cell.extends.is_empty() && cell.prompt.is_none() && cell.command.is_none() {
                errors.push(ValidationError {
                    path: format!("task[{task_idx}].cell[{cell_idx}]"),
                    kind: ErrorKind::MissingRequired,
                    message: "cell requires either 'prompt' (AI-driven) or 'command' (shell \
                              command); none of its `extends` presets supplies a prompt"
                        .to_string(),
                    line: None,
                });
            }
        }
    }
    errors
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> Store {
        Store::open_in_memory().expect("in-memory store")
    }

    fn file_from_toml(raw: &str) -> TaskFile {
        toml::from_str(raw).expect("valid TOML")
    }

    #[test]
    fn default_presets_are_seeded_on_a_fresh_store() {
        let s = store();
        for seed in DEFAULT_PRESETS {
            assert!(
                s.get_preset(seed.name).unwrap().is_some(),
                "{:?} should be seeded by default",
                seed.name
            );
        }
        // Any preset, including a default one, is freely removable -- no
        // built-in-protection like `UNCLASSIFIED_TYPE`.
        assert!(s.deregister_preset(DEFAULT_PRESETS[0].name).unwrap());
        assert!(s.get_preset(DEFAULT_PRESETS[0].name).unwrap().is_none());
    }

    #[test]
    fn register_upserts_and_deregister_removes() {
        let s = store();
        s.register_preset(&PresetView {
            name: "custom".to_string(),
            maximum_context: Some(1),
            ..Default::default()
        })
        .unwrap();
        s.register_preset(&PresetView {
            name: "custom".to_string(),
            maximum_context: Some(2),
            ..Default::default()
        })
        .unwrap();
        let all = s.list_presets().unwrap();
        assert_eq!(
            all.iter().filter(|p| p.name == "custom").count(),
            1,
            "re-registering must upsert, not duplicate"
        );
        assert_eq!(
            s.get_preset("custom").unwrap().unwrap().maximum_context,
            Some(2)
        );
        assert!(s.deregister_preset("custom").unwrap());
        assert!(!s.deregister_preset("custom").unwrap());
    }

    #[test]
    fn validate_rejects_unregistered_preset_name() {
        let s = store();
        let raw = r#"
[[task]]
name = "t"

[[task.cell]]
cwd = "/tmp"
prompt = "hi"
extends = ["<<ralphus:presets/does-not-exist>>"]
"#;
        let file = file_from_toml(raw);
        let errors = validate_task_file_presets(&s, raw, &file);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].message.contains("does-not-exist"));
        assert!(errors[0].line.is_some());
    }

    #[test]
    fn validate_accepts_a_registered_preset() {
        let s = store();
        let raw = r#"
[[task]]
name = "t"

[[task.cell]]
cwd = "/tmp"
prompt = "hi"
extends = ["<<ralphus:presets/commit_and_push>>"]
"#;
        let file = file_from_toml(raw);
        assert!(validate_task_file_presets(&s, raw, &file).is_empty());
    }

    #[test]
    fn apply_presets_fills_only_unset_cell_fields() {
        let s = store();
        let raw = r#"
[[task]]
name = "t"

[[task.cell]]
cwd = "/tmp"
prompt = "hi"
system_prompt = "I am an override!"
extends = ["<<ralphus:presets/commit_and_push>>"]
"#;
        let mut file = file_from_toml(raw);
        apply_presets(&s, &mut file);
        let cell = &file.task[0].cell[0];
        let expected = format!(
            "{}\n\nI am an override!",
            DEFAULT_PRESETS[4].system_prompt.unwrap()
        );
        assert_eq!(cell.system_prompt.as_deref(), Some(expected.as_str()));
        assert_eq!(
            cell.system_prompt_position.as_deref(),
            Some(ralphus_core::schema::SYSTEM_PROMPT_POSITION_APPEND)
        );
    }

    #[test]
    fn apply_presets_last_listed_preset_wins_on_conflict() {
        let s = store();
        s.register_preset(&PresetView {
            name: "a".to_string(),
            system_prompt: Some("from a".to_string()),
            ..Default::default()
        })
        .unwrap();
        s.register_preset(&PresetView {
            name: "b".to_string(),
            system_prompt: Some("from b".to_string()),
            ..Default::default()
        })
        .unwrap();
        let raw = r#"
[[task]]
name = "t"

[[task.cell]]
cwd = "/tmp"
prompt = "hi"
extends = ["<<ralphus:presets/a>>", "<<ralphus:presets/b>>"]
"#;
        let mut file = file_from_toml(raw);
        apply_presets(&s, &mut file);
        assert_eq!(
            file.task[0].cell[0].system_prompt.as_deref(),
            Some("from b")
        );
    }

    #[test]
    fn apply_presets_skips_fields_not_applicable_to_task_level() {
        let s = store();
        let raw = r#"
[[task]]
name = "t"
extends = ["<<ralphus:presets/commit_and_push>>"]

[[task.cell]]
cwd = "/tmp"
prompt = "hi"
"#;
        let mut file = file_from_toml(raw);
        apply_presets(&s, &mut file);
        // `commit_and_push` only defines `system_prompt`/`system_prompt_position`,
        // neither of which exists on `TaskDef` -- nothing to observe on the
        // task itself, and the cell (no extends of its own) stays unset too.
        assert!(file.task[0].cell[0].system_prompt.is_none());
    }

    #[test]
    fn apply_presets_task_level_fill_cascades_to_cell_via_existing_inheritance() {
        let s = store();
        let raw = r#"
[[task]]
name = "t"
extends = ["<<ralphus:presets/complex_task>>"]

[[task.cell]]
cwd = "/tmp"
prompt = "hi"
"#;
        let mut file = file_from_toml(raw);
        apply_presets(&s, &mut file);
        assert_eq!(file.task[0].maximum_context, Some(200_000));
        // The cell itself is left unset by design -- inheritance to the
        // cell happens later, in `Store::insert_squad_with_id`'s existing
        // `resolve_maximum_context` fold, not here.
        assert!(file.task[0].cell[0].maximum_context.is_none());
    }

    // ── Roles, namespaces, templates, on-disk presets ───────────────────────

    fn preset(name: &str) -> PresetView {
        PresetView {
            name: name.to_string(),
            ..Default::default()
        }
    }

    fn applied(raw: &str, presets: &[PresetView]) -> TaskFile {
        let mut file = file_from_toml(raw);
        apply_presets_from(presets, &mut file);
        file
    }

    #[test]
    fn role_presets_are_seeded_under_the_roles_namespace() {
        let s = store();
        let roles: Vec<String> = s
            .list_presets()
            .unwrap()
            .into_iter()
            .map(|p| p.name)
            .filter(|n| n.starts_with("roles/"))
            .collect();
        assert_eq!(roles.len(), crate::preset_roles::ROLE_PRESETS.len());
        for expected in ["roles/reviewer", "roles/qa", "roles/ci-fixer", "roles/vp"] {
            assert!(roles.iter().any(|r| r == expected), "{expected} missing");
        }
        assert!(
            !roles
                .iter()
                .any(|r| r.contains("ml-engineer") || r.contains("prompt-engineer"))
        );
        for seed in crate::preset_roles::ROLE_PRESETS {
            assert!(ralphus_core::schema::is_valid_preset_name(seed.name));
            assert!(seed.system_prompt.is_some_and(|t| !t.trim().is_empty()));
            assert!(
                seed.prompt
                    .is_some_and(|t| t.contains("<<ralphus:linked-field/./prompt>>")),
                "{} should frame the entity's own prompt",
                seed.name
            );
        }
    }

    #[test]
    fn hierarchical_names_validate_and_apply() {
        let s = store();
        let raw = r#"
[[task]]
name = "t"

[[task.cell]]
cwd = "/tmp"
prompt = "hi"
extends = ["<<ralphus:presets/roles/reviewer>>"]
"#;
        let file = file_from_toml(raw);
        assert!(validate_task_file_presets(&s, raw, &file).is_empty());
        let presets = s.list_presets().unwrap();
        let out = applied(raw, &presets);
        let cell = &out.task[0].cell[0];
        assert!(
            cell.system_prompt
                .as_deref()
                .unwrap()
                .contains("code reviewer")
        );
        assert!(
            cell.prompt
                .as_deref()
                .unwrap()
                .starts_with("Review the following work.")
        );
        assert!(cell.prompt.as_deref().unwrap().contains("\nhi\n"));
    }

    #[test]
    fn linked_prompt_keeps_indentation_and_trailing_text() {
        let role = PresetView {
            system_prompt: Some(
                "I am a foo role and I am special!\n\n    <<ralphus:linked-field/./prompt>>\n\nMore text here"
                    .to_string(),
            ),
            ..preset("roles/foo")
        };
        let raw = "[[task]]\nname = \"t\"\n[[task.cell]]\ncwd = \"/tmp\"\nextends = [\"<<ralphus:presets/roles/foo>>\"]\nprompt = \"\"\"\nSome text here\nMore lines\n\"\"\"\n";
        let out = applied(raw, &[role]);
        assert_eq!(
            out.task[0].cell[0].system_prompt.as_deref(),
            Some(
                "I am a foo role and I am special!\n\n    Some text here\n    More lines\n\nMore text here"
            )
        );
        // The author's own prompt is untouched: the preset supplies no prompt
        // template, and only frames it from the system prompt.
        assert_eq!(
            out.task[0].cell[0].prompt.as_deref(),
            Some("Some text here\nMore lines\n")
        );
    }

    #[test]
    fn missing_linked_field_expands_to_a_generic_message() {
        let role = PresetView {
            system_prompt: Some("Context: <<ralphus:linked-field/../prompt>>".to_string()),
            ..preset("p")
        };
        let raw = "[[task]]\nname = \"t\"\n[[task.cell]]\ncwd = \"/tmp\"\nprompt = \"x\"\nextends = [\"<<ralphus:presets/p>>\"]\n";
        let out = applied(raw, &[role]);
        assert_eq!(
            out.task[0].cell[0].system_prompt.as_deref(),
            Some("Context: <field prompt was not found>")
        );
    }

    #[test]
    fn proof_preset_reaches_the_parent_cells_prompt() {
        let role = PresetView {
            prompt: Some("Check this work:\n  <<ralphus:linked-field/../prompt>>".to_string()),
            ..preset("roles/checker")
        };
        let raw = "[[task]]\nname = \"t\"\n[[task.cell]]\ncwd = \"/tmp\"\nprompt = \"build the thing\\nin two lines\"\n[[task.cell.proof]]\nextends = [\"<<ralphus:presets/roles/checker>>\"]\n";
        let out = applied(raw, &[role]);
        assert_eq!(
            out.task[0].cell[0].proof[0].prompt.as_deref(),
            Some("Check this work:\n  build the thing\n  in two lines")
        );
        assert!(check_required_fields_after_presets(&out).is_empty());
    }

    fn subject_role() -> PresetView {
        PresetView {
            prompt: Some("Review:\n<<ralphus:linked-field/./..[proof]/prompt>>".to_string()),
            ..preset("roles/subject")
        }
    }

    #[test]
    fn kind_step_template_resolves_to_the_subject_on_cell_and_cell_proof() {
        let raw = "[[task]]\nname = \"t\"\n[[task.cell]]\ncwd = \"/tmp\"\nprompt = \"the work\"\nextends = [\"<<ralphus:presets/roles/subject>>\"]\n[[task.cell]]\ncwd = \"/tmp\"\nprompt = \"parent work\"\n[[task.cell.proof]]\nextends = [\"<<ralphus:presets/roles/subject>>\"]\n";
        let out = applied(raw, &[subject_role()]);
        // On a cell the step is a no-op, so the template wraps the cell's own
        // prompt (not discarded).
        assert_eq!(
            out.task[0].cell[0].prompt.as_deref(),
            Some("Review:\nthe work")
        );
        assert_eq!(
            out.task[0].cell[1].proof[0].prompt.as_deref(),
            Some("Review:\nparent work")
        );
    }

    #[test]
    fn kind_step_template_is_not_found_on_a_task_level_proof() {
        let raw = "[[task]]\nname = \"t\"\n[[task.proof]]\nextends = [\"<<ralphus:presets/roles/subject>>\"]\n[[task.cell]]\ncwd = \"/tmp\"\nprompt = \"x\"\n";
        let out = applied(raw, &[subject_role()]);
        assert_eq!(
            out.task[0].proof[0].prompt.as_deref(),
            Some("Review:\n<field prompt was not found>")
        );
    }

    #[test]
    fn an_unreferenced_own_prompt_is_appended_after_the_template() {
        let plain = PresetView {
            prompt: Some("Preset prompt".to_string()),
            ..preset("plain")
        };
        let framing = PresetView {
            prompt: Some("Before\n<<ralphus:linked-field/./prompt>>\nAfter".to_string()),
            ..preset("framing")
        };
        let raw = "[[task]]\nname = \"t\"\n[[task.cell]]\ncwd = \"/tmp\"\nprompt = \"mine\"\nextends = [\"<<ralphus:presets/plain>>\"]\n[[task.cell]]\ncwd = \"/tmp\"\nprompt = \"mine\"\nextends = [\"<<ralphus:presets/framing>>\"]\n[[task.cell]]\ncwd = \"/tmp\"\nextends = [\"<<ralphus:presets/plain>>\"]\n";
        let out = applied(raw, &[plain, framing]);
        assert_eq!(
            out.task[0].cell[0].prompt.as_deref(),
            Some("Preset prompt\n\nmine")
        );
        assert_eq!(
            out.task[0].cell[1].prompt.as_deref(),
            Some("Before\nmine\nAfter")
        );
        assert_eq!(out.task[0].cell[2].prompt.as_deref(), Some("Preset prompt"));
    }

    #[test]
    fn a_cell_proof_with_its_own_prompt_gets_subject_then_own() {
        let raw = "[[task]]\nname = \"t\"\n[[task.cell]]\ncwd = \"/tmp\"\nprompt = \"parent work\"\n[[task.cell.proof]]\nprompt = \"focus on auth\"\nextends = [\"<<ralphus:presets/roles/subject>>\"]\n";
        let out = applied(raw, &[subject_role()]);
        assert_eq!(
            out.task[0].cell[0].proof[0].prompt.as_deref(),
            Some("Review:\nparent work\n\nfocus on auth")
        );
    }

    #[test]
    fn a_cell_with_a_subject_template_includes_its_prompt_once() {
        let raw = "[[task]]\nname = \"t\"\n[[task.cell]]\ncwd = \"/tmp\"\nprompt = \"the work\"\nextends = [\"<<ralphus:presets/roles/subject>>\"]\n";
        let out = applied(raw, &[subject_role()]);
        let prompt = out.task[0].cell[0].prompt.as_deref().unwrap();
        assert_eq!(prompt.matches("the work").count(), 1);
    }

    #[test]
    fn command_cell_is_never_given_a_prompt() {
        let role = PresetView {
            prompt: Some("p".to_string()),
            ..preset("p")
        };
        let raw = "[[task]]\nname = \"t\"\n[[task.cell]]\ncwd = \"/tmp\"\ncommand = \"true\"\nextends = [\"<<ralphus:presets/p>>\"]\n";
        let out = applied(raw, &[role]);
        assert!(out.task[0].cell[0].prompt.is_none());
    }

    #[test]
    fn a_preset_that_supplies_no_prompt_fails_the_post_apply_check() {
        let raw = "[[task]]\nname = \"t\"\n[[task.cell]]\ncwd = \"/tmp\"\nextends = [\"<<ralphus:presets/easy_task>>\"]\n";
        let s = store();
        let out = applied(raw, &s.list_presets().unwrap());
        let errors = check_required_fields_after_presets(&out);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].path, "task[0].cell[0]");
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ralphus-presets-{tag}-{}-{}",
            std::process::id(),
            now_ms()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn disk_presets_are_namespaced_by_subdirectory() {
        let dir = temp_dir("disk");
        std::fs::create_dir_all(dir.join("roles")).unwrap();
        std::fs::write(
            dir.join("roles/foo.toml"),
            "prompt = \"hello <<ralphus:linked-field/./prompt>>\"\nmaximum_context = 5\n",
        )
        .unwrap();
        std::fs::write(dir.join("top.toml"), "system_prompt = \"s\"\n").unwrap();
        std::fs::write(dir.join("notes.txt"), "ignored").unwrap();
        let (presets, warnings) = load_disk_presets(std::slice::from_ref(&dir));
        assert!(warnings.is_empty(), "{warnings:?}");
        let names: Vec<&str> = presets.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["roles/foo", "top"]);
        assert!(presets.iter().all(|p| p.source == PresetSource::Disk));
        assert_eq!(presets[0].maximum_context, Some(5));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_single_file_path_names_the_preset_after_its_stem() {
        let dir = temp_dir("file");
        let file = dir.join("solo.toml");
        std::fs::write(&file, "prompt = \"p\"\n").unwrap();
        let (presets, warnings) = load_disk_presets(&[file]);
        assert!(warnings.is_empty());
        assert_eq!(presets.len(), 1);
        assert_eq!(presets[0].name, "solo");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn broken_disk_files_warn_and_are_skipped() {
        let dir = temp_dir("broken");
        std::fs::write(dir.join("bad.toml"), "not toml [").unwrap();
        std::fs::write(dir.join("typo.toml"), "promt = \"x\"\n").unwrap();
        std::fs::write(
            dir.join("pos.toml"),
            "system_prompt_position = \"prepend\"\n",
        )
        .unwrap();
        std::fs::write(dir.join("good.toml"), "prompt = \"x\"\n").unwrap();
        let (presets, warnings) = load_disk_presets(&[dir.clone(), dir.join("missing")]);
        assert_eq!(presets.len(), 1);
        assert_eq!(presets[0].name, "good");
        assert_eq!(warnings.len(), 4, "{warnings:?}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn disk_presets_take_precedence_over_database_presets() {
        let dir = temp_dir("merge");
        std::fs::write(dir.join("shared.toml"), "prompt = \"from disk\"\n").unwrap();
        let db = vec![
            PresetView {
                prompt: Some("from db".to_string()),
                ..preset("shared")
            },
            preset("db_only"),
        ];
        let merged = merge_presets(db, std::slice::from_ref(&dir));
        let names: Vec<&str> = merged.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["db_only", "shared"]);
        let shared = merged.iter().find(|p| p.name == "shared").unwrap();
        assert_eq!(shared.prompt.as_deref(), Some("from disk"));
        assert_eq!(shared.source, PresetSource::Disk);
        let _ = std::fs::remove_dir_all(dir);
    }
}
