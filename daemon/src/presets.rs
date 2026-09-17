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
//! fills only a field an entity left unset, never overrides one the author
//! typed explicitly, and silently skips any preset field that doesn't exist
//! on the entity kind it's applied to (e.g. `system_prompt` via a
//! task-level `extends` -- `system_prompt` only exists on `CellDef`; see
//! each field's doc comment on `TaskDef`/`CellDef`/`ProofStep` for the
//! exact applicability). Once stamped, the resulting value is
//! indistinguishable from one the author typed directly -- presets carry no
//! provenance and are never re-applied on a restart.

use std::collections::HashMap;

use rusqlite::params;
use serde::Serialize;

use ralphus_core::schema::{CellDef, ProofStep, TaskDef, TaskFile, parse_preset_sentinel};
use ralphus_core::validate::{ErrorKind, ValidationError};

use crate::store::{Result as StoreResult, Store, now_ms};

/// A registered preset: a name plus the (all optional) field values it
/// supplies. Every field mirrors a [`CellDef`]/[`TaskDef`]/[`ProofStep`]
/// field of the same name -- see [`apply_presets`] for which entity kinds
/// honor which.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct PresetView {
    pub name: String,
    pub system_prompt: Option<String>,
    pub system_prompt_position: Option<String>,
    pub maximum_context: Option<u64>,
    pub auto_compact_threshold: Option<u64>,
    pub maximum_tool_output_tokens: Option<u64>,
    pub created_at_ms: i64,
}

/// Starter presets seeded once, the first time the `presets` table is
/// created (`Store::init_schema`, mirroring
/// `crate::triage::DEFAULT_TRIAGE_TYPES`'s seeding pattern) -- never
/// re-seeded afterward, so deregistering one of these is honored across
/// every later restart.
pub struct PresetSeed {
    pub name: &'static str,
    pub system_prompt: Option<&'static str>,
    pub system_prompt_position: Option<&'static str>,
    pub maximum_context: Option<u64>,
    pub auto_compact_threshold: Option<u64>,
    pub maximum_tool_output_tokens: Option<u64>,
}

pub const DEFAULT_PRESETS: &[PresetSeed] = &[
    PresetSeed {
        name: "easy_task",
        system_prompt: None,
        system_prompt_position: None,
        maximum_context: Some(75_000),
        auto_compact_threshold: Some(51_000),
        maximum_tool_output_tokens: Some(8_000),
    },
    PresetSeed {
        name: "medium_task",
        system_prompt: None,
        system_prompt_position: None,
        maximum_context: Some(120_000),
        auto_compact_threshold: Some(86_000),
        maximum_tool_output_tokens: Some(12_000),
    },
    PresetSeed {
        name: "complex_task",
        system_prompt: None,
        system_prompt_position: None,
        maximum_context: Some(200_000),
        auto_compact_threshold: Some(150_000),
        maximum_tool_output_tokens: Some(15_000),
    },
    PresetSeed {
        name: "no_git_commit",
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
            "INSERT INTO presets(name, system_prompt, system_prompt_position, maximum_context, auto_compact_threshold, maximum_tool_output_tokens, created_at_ms)
             VALUES(?,?,?,?,?,?,?)
             ON CONFLICT(name) DO UPDATE SET
                system_prompt=excluded.system_prompt,
                system_prompt_position=excluded.system_prompt_position,
                maximum_context=excluded.maximum_context,
                auto_compact_threshold=excluded.auto_compact_threshold,
                maximum_tool_output_tokens=excluded.maximum_tool_output_tokens",
            params![
                name,
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
            "SELECT name, system_prompt, system_prompt_position, maximum_context, auto_compact_threshold, maximum_tool_output_tokens, created_at_ms
             FROM presets ORDER BY name",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(PresetView {
                    name: r.get(0)?,
                    system_prompt: r.get(1)?,
                    system_prompt_position: r.get(2)?,
                    maximum_context: r
                        .get::<_, Option<i64>>(3)?
                        .map(|v| u64::try_from(v).unwrap_or(0)),
                    auto_compact_threshold: r
                        .get::<_, Option<i64>>(4)?
                        .map(|v| u64::try_from(v).unwrap_or(0)),
                    maximum_tool_output_tokens: r
                        .get::<_, Option<i64>>(5)?
                        .map(|v| u64::try_from(v).unwrap_or(0)),
                    created_at_ms: r.get(6)?,
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
    let registered = store.list_presets().unwrap_or_default();
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
    // `system_prompt`/`system_prompt_position` don't exist on `TaskDef` --
    // silently skipped, per this module's applicability rule.
}

fn apply_to_cell(cell: &mut CellDef, by_name: &PresetMap<'_>) {
    let presets = resolved_presets(&cell.extends, by_name);
    if presets.is_empty() {
        return;
    }
    if cell.system_prompt.is_none() {
        cell.system_prompt = last_defined(&presets, |p| p.system_prompt.clone());
    }
    if cell.system_prompt_position.is_none() {
        cell.system_prompt_position = last_defined(&presets, |p| p.system_prompt_position.clone());
    }
    if cell.maximum_context.is_none() {
        cell.maximum_context = last_defined(&presets, |p| p.maximum_context);
    }
    if cell.auto_compact_threshold.is_none() {
        cell.auto_compact_threshold = last_defined(&presets, |p| p.auto_compact_threshold);
    }
    if cell.maximum_tool_output_tokens.is_none() {
        cell.maximum_tool_output_tokens = last_defined(&presets, |p| p.maximum_tool_output_tokens);
    }
}

fn apply_to_proof(proof: &mut ProofStep, by_name: &PresetMap<'_>) {
    let presets = resolved_presets(&proof.extends, by_name);
    if presets.is_empty() {
        return;
    }
    if proof.maximum_tool_output_tokens.is_none() {
        proof.maximum_tool_output_tokens = last_defined(&presets, |p| p.maximum_tool_output_tokens);
    }
    // `system_prompt`/`system_prompt_position`/`maximum_context`/
    // `auto_compact_threshold` don't exist on `ProofStep` -- silently
    // skipped, per this module's applicability rule.
}

/// Stamp each task's, cell's, and proof step's named presets' field values
/// into any of its own fields still unset. Called once, at submit time,
/// before persistence -- mirrors
/// `agent_profiles::resolve_agent_candidate_lists`'s mutate-`file`-in-place
/// shape. Requires [`validate_task_file_presets`] to have already run and
/// rejected the submission on any unregistered name, so every resolution
/// here is infallible.
///
/// Only fills a field that is currently `None`, so a value the author typed
/// explicitly is never overridden; when more than one preset in an entity's
/// own `extends` list defines the same field, the last one in the list
/// wins. A preset field that doesn't exist on the entity kind it's applied
/// to (e.g. `system_prompt` via a task-level `extends`) is silently
/// skipped. Because a task-level fill only ever touches the task's own
/// field, the existing task→cell inheritance (`resolve_maximum_context`,
/// `resolve_auto_compact_threshold`,
/// `resolve_cell_maximum_tool_output_tokens` in `crate::store`) still
/// cascades it to a cell exactly as it always has -- no changes needed
/// there.
pub fn apply_presets(store: &Store, file: &mut TaskFile) {
    let registered = store.list_presets().unwrap_or_default();
    let by_name: PresetMap<'_> = registered.iter().map(|p| (p.name.as_str(), p)).collect();

    for task in &mut file.task {
        apply_to_task(task, &by_name);
        for proof in &mut task.proof {
            apply_to_proof(proof, &by_name);
        }
        for cell in &mut task.cell {
            apply_to_cell(cell, &by_name);
            for proof in &mut cell.proof {
                apply_to_proof(proof, &by_name);
            }
        }
    }
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
        assert_eq!(cell.system_prompt.as_deref(), Some("I am an override!"));
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
}
