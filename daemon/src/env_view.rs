//! Read-only "resolved environment variables" views (RAL-324).
//!
//! Every surface that *takes* environment variables as input -- a squad, a
//! task, a cell, a proof step, and a review's worktree / auto-build / check-
//! gate / manual-checks steps -- resolves them from a different stack of
//! layers. The board and the CLI both need to answer "what will this actually
//! run with?", and they must answer it identically, so the resolution *and*
//! the redaction live here, behind the daemon's HTTP API, rather than being
//! reimplemented on either client.
//!
//! Redaction is name-based and comes from exactly one place: the user's
//! registered secret env-var **names** (RAL-281's Secrets tab,
//! [`crate::secret_env_names`]), applied through the shared
//! [`ralphus_core::redact::redact_env_value`]. There is deliberately no
//! "looks like a secret" name heuristic here -- a variable the Secrets tab
//! does not list is shown in the clear, which is why every view carries
//! [`EnvView::note`] saying so. Anything else would make the viewer disagree
//! with what that same list scrubs from pane text and terminal logs.
//!
//! What a view does **not** include, by design:
//! - the runner process's inherited OS environment (`PATH`, `HOME`, ...) --
//!   these views list the layers ralphus itself contributes on top of it;
//! - placeholder expansion (`{{worktree}}` and friends) -- values are shown
//!   as configured, and the scheduler materializes them at dispatch (see
//!   [`crate::worktrees::materialize_env_overrides`]);
//! - a key some layer tombstones away: it is not part of the resolved
//!   environment, so it is not listed as one.
//!
//! This module is strictly read-only. Setting/unsetting stays with the
//! existing `POST .../env` routes.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde::Serialize;

use ralphus_core::redact::redact_env_value;

use crate::store::{Result as StoreResult, Store, StoreError};

/// One layer's contribution to a single variable.
#[derive(Debug, Clone, Serialize)]
pub struct EnvLayerValue {
    /// Human label of the layer, matching an entry of [`EnvView::layers`].
    pub layer: String,
    /// The layer's value, already redacted; `None` when this layer tombstones
    /// the variable away (review layers only).
    pub value: Option<String>,
    /// Whether `value` is the [`ralphus_core::redact::REDACTED`] placeholder
    /// rather than the real value.
    pub redacted: bool,
    /// Whether this is the layer that won -- exactly one per row.
    pub effective: bool,
}

/// One resolved environment variable.
#[derive(Debug, Clone, Serialize)]
pub struct EnvVarRow {
    /// The variable's name, never redacted.
    pub name: String,
    /// The winning value, already redacted.
    pub value: String,
    /// Whether `name` is registered as secret, so `value` is a placeholder.
    pub redacted: bool,
    /// Label of the layer the winning value came from.
    pub source: String,
    /// Every layer that mentions `name`, lowest-precedence first. One entry
    /// for a variable set at a single scope, more when a scope shadows its
    /// parent.
    pub layers: Vec<EnvLayerValue>,
}

/// A whole surface's resolved environment, as the board popup and
/// `ralphus env` both render it.
#[derive(Debug, Clone, Serialize)]
pub struct EnvView {
    /// Machine-readable scope id (`"cell"`, `"review-build"`, ...) -- the
    /// same token `ralphus env --scope` takes.
    pub scope: &'static str,
    /// Human label naming the specific surface, e.g. `cell 0/1 of squad-...`.
    pub label: String,
    /// Every layer that feeds this surface, lowest-precedence first.
    pub layers: Vec<String>,
    /// The resolved variables, alphabetical by name.
    pub vars: Vec<EnvVarRow>,
    /// How many of `vars` had their value masked by the Secrets list.
    pub redacted_count: usize,
    /// Why an unregistered secret-looking variable is still shown in full.
    pub note: &'static str,
    /// Set when a layer could not be resolved and was therefore left out
    /// (today: an agent profile whose backend/config does not resolve).
    pub warning: Option<String>,
}

/// The standing caveat every view carries -- see the module doc comment.
const NOTE: &str = "Values are masked only for env-var names registered in the Secrets tab. \
An unregistered variable is shown in full, including one that looks like a credential. \
The runner's inherited OS environment is not listed, and placeholders are shown unexpanded.";

/// A layer under construction: its label plus the entries it contributes.
/// `None` is a tombstone ("remove this inherited key"), which only the review
/// layers can produce.
type Layer = (String, BTreeMap<String, Option<String>>);

/// Promote a plain override map to a [`Layer`]'s entry shape.
fn plain(map: BTreeMap<String, String>) -> BTreeMap<String, Option<String>> {
    map.into_iter().map(|(k, v)| (k, Some(v))).collect()
}

/// Fold `layers` (lowest-precedence first) into the finished view, redacting
/// every value through the shared core getter against `secret_names`.
///
/// A later layer simply overwrites the winner, which is exactly how
/// `BTreeMap::extend` resolves precedence at dispatch -- the fold here mirrors
/// that rather than reimplementing a second precedence rule.
fn assemble(
    scope: &'static str,
    label: String,
    layers: Vec<Layer>,
    secret_names: &BTreeSet<String>,
    warning: Option<String>,
) -> EnvView {
    let mut contributions: BTreeMap<String, Vec<EnvLayerValue>> = BTreeMap::new();
    for (layer_label, entries) in &layers {
        for (name, value) in entries {
            let (shown, redacted) = match value {
                Some(v) => {
                    let (shown, redacted) = redact_env_value(name, v, secret_names);
                    (Some(shown), redacted)
                }
                None => (None, false),
            };
            contributions
                .entry(name.clone())
                .or_default()
                .push(EnvLayerValue {
                    layer: layer_label.clone(),
                    value: shown,
                    redacted,
                    effective: false,
                });
        }
    }

    let mut vars = Vec::new();
    for (name, mut rows) in contributions {
        let Some(last) = rows.last_mut() else {
            continue;
        };
        last.effective = true;
        // A tombstone winning means the variable is not in the resolved
        // environment at all -- there is nothing to list.
        let (Some(value), redacted, source) =
            (last.value.clone(), last.redacted, last.layer.clone())
        else {
            continue;
        };
        vars.push(EnvVarRow {
            name,
            value,
            redacted,
            source,
            layers: rows,
        });
    }
    let redacted_count = vars.iter().filter(|v| v.redacted).count();
    EnvView {
        scope,
        label,
        layers: layers.into_iter().map(|(l, _)| l).collect(),
        vars,
        redacted_count,
        note: NOTE,
        warning,
    }
}

/// The agent-profile `env` layer a cell (and any proof step borrowing that
/// cell's backend) runs under -- the lowest layer of all, since the scheduler
/// merges the override stack *on top* of it (see `scheduler::dispatch_cell`,
/// which does `selection.env` then `extend(env_overrides)`).
///
/// Returns the layer and, when resolution failed, the reason to surface as
/// [`EnvView::warning`]: a profile that doesn't resolve is reported rather
/// than silently dropped, because its absence changes what the view claims.
fn agent_profile_layer(agent: &str, cwd: &str) -> (Option<Layer>, Option<String>) {
    let cwd = if cwd.is_empty() { "." } else { cwd };
    match crate::agent_profiles::resolve_agent_for_path(agent, Path::new(cwd)) {
        Ok(selection) if selection.env.is_empty() => (None, None),
        Ok(selection) => (
            Some(("agent profile".to_string(), plain(selection.env))),
            None,
        ),
        Err(message) => (
            None,
            Some(format!(
                "the agent profile for {agent:?} does not resolve ({message}); \
                 its env layer is not included below"
            )),
        ),
    }
}

/// The `(agent, cwd)` a surface's backend resolves from: the cell itself for a
/// cell-scoped surface, and the task's *first* cell for a task-scoped one --
/// mirroring `scheduler::run_task_finalizer`, which borrows exactly that cell
/// for a task-level proof.
fn backend_of(
    store: &Store,
    squad_id: &str,
    task_idx: i64,
    cell_idx: Option<i64>,
) -> Option<(String, String)> {
    store
        .cells_of(squad_id)
        .ok()?
        .into_iter()
        .find(|c| c.task_idx == task_idx && cell_idx.is_none_or(|si| c.idx == si))
        .map(|c| (c.agent, c.cwd.unwrap_or_default()))
}

/// The `agent profile < squad < task [< cell]` prefix every task/cell surface
/// shares, so each view below only appends its own narrowest layers instead of
/// re-deriving the precedence order.
fn base_layers(
    store: &Store,
    squad_id: &str,
    task_idx: Option<i64>,
    cell_idx: Option<i64>,
) -> StoreResult<(Vec<Layer>, Option<String>)> {
    let (profile, warning) = match task_idx.and_then(|ti| backend_of(store, squad_id, ti, cell_idx))
    {
        Some((agent, cwd)) => agent_profile_layer(&agent, &cwd),
        None => (None, None),
    };
    let mut layers: Vec<Layer> = profile.into_iter().collect();
    layers.push((
        "squad".to_string(),
        plain(store.get_squad_env_overrides(squad_id)?),
    ));
    if let Some(ti) = task_idx {
        layers.push((
            "task".to_string(),
            plain(store.get_task_env_overrides(squad_id, ti)?),
        ));
        if let Some(si) = cell_idx {
            layers.push((
                "cell".to_string(),
                plain(store.get_cell_env_overrides(squad_id, ti, si)?),
            ));
        }
    }
    Ok((layers, warning))
}

/// The squad's own override layer -- the base of every task/cell/proof stack.
///
/// # Errors
/// [`StoreError::NotFound`] when no such squad exists.
pub fn squad_env(store: &Store, squad_id: &str) -> StoreResult<EnvView> {
    let secret_names = store.secret_env_names_cached()?;
    let (layers, warning) = base_layers(store, squad_id, None, None)?;
    Ok(assemble(
        "squad",
        format!("squad {squad_id}"),
        layers,
        &secret_names,
        warning,
    ))
}

/// `agent profile < squad < task` -- what every cell under this task starts
/// from.
///
/// # Errors
/// [`StoreError::NotFound`] when no such squad/task exists.
pub fn task_env(store: &Store, squad_id: &str, task_idx: i64) -> StoreResult<EnvView> {
    let secret_names = store.secret_env_names_cached()?;
    let (layers, warning) = base_layers(store, squad_id, Some(task_idx), None)?;
    Ok(assemble(
        "task",
        format!("task {task_idx} of squad {squad_id}"),
        layers,
        &secret_names,
        warning,
    ))
}

/// `agent profile < squad < task < task.proof` -- what every task-scoped proof
/// step inherits before its own per-step layer.
///
/// # Errors
/// [`StoreError::NotFound`] when no such squad/task exists.
pub fn task_proof_env(store: &Store, squad_id: &str, task_idx: i64) -> StoreResult<EnvView> {
    let secret_names = store.secret_env_names_cached()?;
    let (mut layers, warning) = base_layers(store, squad_id, Some(task_idx), None)?;
    layers.push((
        "task proof".to_string(),
        plain(store.get_task_proof_env_overrides(squad_id, task_idx)?),
    ));
    Ok(assemble(
        "task-proof",
        format!("task {task_idx} proof steps of squad {squad_id}"),
        layers,
        &secret_names,
        warning,
    ))
}

/// `agent profile < squad < task < cell` -- exactly what
/// `scheduler::dispatch_cell` hands the runner, minus placeholder expansion.
///
/// # Errors
/// [`StoreError::NotFound`] when no such squad/task/cell exists.
pub fn cell_env(
    store: &Store,
    squad_id: &str,
    task_idx: i64,
    cell_idx: i64,
) -> StoreResult<EnvView> {
    let secret_names = store.secret_env_names_cached()?;
    let (layers, warning) = base_layers(store, squad_id, Some(task_idx), Some(cell_idx))?;
    Ok(assemble(
        "cell",
        format!("cell {task_idx}/{cell_idx} of squad {squad_id}"),
        layers,
        &secret_names,
        warning,
    ))
}

/// `agent profile < squad < task < cell < cell.proof` -- what every proof step
/// under this cell inherits before its own per-step layer.
///
/// # Errors
/// [`StoreError::NotFound`] when no such squad/task/cell exists.
pub fn cell_proof_env(
    store: &Store,
    squad_id: &str,
    task_idx: i64,
    cell_idx: i64,
) -> StoreResult<EnvView> {
    let secret_names = store.secret_env_names_cached()?;
    let (mut layers, warning) = base_layers(store, squad_id, Some(task_idx), Some(cell_idx))?;
    layers.push((
        "cell proof".to_string(),
        plain(store.get_cell_proof_env_overrides(squad_id, task_idx, cell_idx)?),
    ));
    Ok(assemble(
        "cell-proof",
        format!("cell {task_idx}/{cell_idx} proof steps of squad {squad_id}"),
        layers,
        &secret_names,
        warning,
    ))
}

/// One individual proof step's fully resolved environment -- RAL-191's
/// narrowest layer on top of its scope's. `scope` is `"task"` or `"cell"`;
/// `cell_idx` is `-1` for a task-scoped step, matching the `proofs` table.
///
/// # Errors
/// [`StoreError::NotFound`] when no such squad/task/cell/proof step exists.
pub fn proof_step_env(
    store: &Store,
    squad_id: &str,
    task_idx: i64,
    scope: &str,
    cell_idx: i64,
    idx: i64,
) -> StoreResult<EnvView> {
    let secret_names = store.secret_env_names_cached()?;
    let cell = (scope == "cell").then_some(cell_idx);
    let (mut layers, warning) = base_layers(store, squad_id, Some(task_idx), cell)?;
    match cell {
        Some(si) => layers.push((
            "cell proof".to_string(),
            plain(store.get_cell_proof_env_overrides(squad_id, task_idx, si)?),
        )),
        None => layers.push((
            "task proof".to_string(),
            plain(store.get_task_proof_env_overrides(squad_id, task_idx)?),
        )),
    }
    layers.push((
        "proof step".to_string(),
        plain(store.get_proof_step_env_overrides(squad_id, task_idx, scope, cell_idx, idx)?),
    ));
    let label = match cell {
        Some(si) => format!("proof step {idx} of cell {task_idx}/{si} in squad {squad_id}"),
        None => format!("proof step {idx} of task {task_idx} in squad {squad_id}"),
    };
    Ok(assemble("proof", label, layers, &secret_names, warning))
}

/// A review worktree's environment: the source cell's resolved overrides with
/// this branch's own layer (which may tombstone a key) on top.
///
/// # Errors
/// [`StoreError::NotFound`] when no such guardian/branch exists.
pub fn review_worktree_env(
    store: &Store,
    guardian_id: &str,
    branch_id: &str,
) -> StoreResult<EnvView> {
    let secret_names = store.secret_env_names_cached()?;
    let guardian = store.get_guardian(guardian_id)?;
    let branch = guardian
        .branches
        .iter()
        .find(|b| b.id == branch_id)
        .ok_or(StoreError::NotFound)?;
    let layers = vec![
        (
            "source cell".to_string(),
            plain(branch.inherited_env.clone()),
        ),
        ("review worktree".to_string(), branch.env_overrides.clone()),
    ];
    Ok(assemble(
        "review-worktree",
        format!(
            "review worktree for branch {} of {guardian_id}",
            branch.branch
        ),
        layers,
        &secret_names,
        None,
    ))
}

/// Which combined-worktree step of a review to view. All three share the same
/// `combined_env` baseline; Build and Tests additionally share one stored
/// override layer, because the check gates run under the auto-build step's
/// environment (see `guardian_merge::final_checks`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewStep {
    /// The finalize-time auto-build step.
    Build,
    /// The check gates ("Tests") -- the build layer under a Tests-specific
    /// label, since they genuinely run under those overrides.
    Tests,
    /// The manual-checks step ("Run all" / an individual suggested command).
    ManualChecks,
}

impl ReviewStep {
    /// The `--scope` token / API scope id.
    #[must_use]
    pub const fn scope(self) -> &'static str {
        match self {
            Self::Build => "review-build",
            Self::Tests => "review-tests",
            Self::ManualChecks => "review-manual-checks",
        }
    }

    /// The layer label shown for this step's own override layer. Tests
    /// deliberately names the build layer: it is the same stored layer, and
    /// calling it something else would imply a second one exists to edit.
    #[must_use]
    const fn layer_label(self) -> &'static str {
        match self {
            Self::Build | Self::Tests => "review build step",
            Self::ManualChecks => "review manual checks",
        }
    }
}

/// A review's combined-worktree step environment: `combined_env` (the last
/// enabled branch's resolved environment) with that step's own override layer
/// on top.
///
/// # Errors
/// [`StoreError::NotFound`] when no such guardian exists.
pub fn review_step_env(store: &Store, guardian_id: &str, step: ReviewStep) -> StoreResult<EnvView> {
    let secret_names = store.secret_env_names_cached()?;
    let guardian = store.get_guardian(guardian_id)?;
    let own = match step {
        ReviewStep::Build | ReviewStep::Tests => guardian.build_env_overrides.clone(),
        ReviewStep::ManualChecks => guardian.manual_checks_env_overrides.clone(),
    };
    let layers = vec![
        (
            "combined worktree".to_string(),
            plain(guardian.combined_env.clone()),
        ),
        (step.layer_label().to_string(), own),
    ];
    let label = match step {
        ReviewStep::Build => format!("auto-build step of review {guardian_id}"),
        ReviewStep::Tests => format!("check gates (tests) of review {guardian_id}"),
        ReviewStep::ManualChecks => format!("manual checks of review {guardian_id}"),
    };
    Ok(assemble(step.scope(), label, layers, &secret_names, None))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ralphus_core::redact::REDACTED;

    /// A squad with one task, one cell, and one cell-scoped proof step --
    /// enough to exercise every non-review layer.
    fn seeded_store() -> (Store, String) {
        let mut store = Store::open_in_memory().unwrap();
        let toml = r#"
[[task]]
name = "build"
agent = "claude"

[[task.cell]]
id = "c1"
cwd = "."
command = "echo hi"

[[task.cell.proof]]
kind = "command"
command = "echo ok"
"#;
        let file: ralphus_core::schema::TaskFile = toml::from_str(toml).expect("valid toml");
        let id = store.insert_squad(&file, None, false).unwrap();
        (store, id)
    }

    fn value_of(view: &EnvView, name: &str) -> Option<String> {
        view.vars
            .iter()
            .find(|v| v.name == name)
            .map(|v| v.value.clone())
    }

    #[test]
    fn a_cell_view_shows_the_narrowest_layer_and_names_where_it_came_from() {
        let (store, id) = seeded_store();
        let set = |k: &str, v: &str| {
            let mut m = BTreeMap::new();
            m.insert(k.to_string(), v.to_string());
            m
        };
        store
            .set_squad_env_overrides(&id, &set("SHARED", "from-squad"), &[])
            .unwrap();
        store
            .set_task_env_overrides(&id, 0, &set("SHARED", "from-task"), &[])
            .unwrap();
        store
            .set_cell_env_overrides(&id, 0, 0, &set("SHARED", "from-cell"), &[])
            .unwrap();
        store
            .set_squad_env_overrides(&id, &set("ONLY_SQUAD", "s"), &[])
            .unwrap();

        let view = cell_env(&store, &id, 0, 0).unwrap();
        assert_eq!(value_of(&view, "SHARED").as_deref(), Some("from-cell"));
        assert_eq!(value_of(&view, "ONLY_SQUAD").as_deref(), Some("s"));
        let shared = view.vars.iter().find(|v| v.name == "SHARED").unwrap();
        assert_eq!(shared.source, "cell");
        assert_eq!(shared.layers.len(), 3, "every layer that set it is listed");
        assert!(shared.layers.last().unwrap().effective);
        assert!(!shared.layers[0].effective);
    }

    #[test]
    fn a_registered_secret_name_is_masked_but_an_unregistered_one_is_not() {
        let (store, id) = seeded_store();
        let mut set = BTreeMap::new();
        set.insert("MY_TOKEN".to_string(), "s3kr3t".to_string());
        set.insert("STRIPE_API_KEY".to_string(), "sk-live".to_string());
        store.set_squad_env_overrides(&id, &set, &[]).unwrap();
        store.add_secret_env_name("MY_TOKEN").unwrap();

        let view = squad_env(&store, &id).unwrap();
        assert_eq!(value_of(&view, "MY_TOKEN").as_deref(), Some(REDACTED));
        // RAL-324: the Secrets tab is the only source of truth, so an
        // unregistered credential-looking name is deliberately shown.
        assert_eq!(
            value_of(&view, "STRIPE_API_KEY").as_deref(),
            Some("sk-live")
        );
        assert_eq!(view.redacted_count, 1);
        assert!(view.note.contains("Secrets tab"));
    }

    #[test]
    fn a_proof_step_layers_its_own_overrides_over_its_scopes() {
        let (store, id) = seeded_store();
        let set = |k: &str, v: &str| {
            let mut m = BTreeMap::new();
            m.insert(k.to_string(), v.to_string());
            m
        };
        store
            .set_cell_proof_env_overrides(&id, 0, 0, &set("RUST_LOG", "warn"), &[])
            .unwrap();
        store
            .set_proof_step_env_overrides(&id, 0, "cell", 0, 0, &set("RUST_LOG", "debug"), &[])
            .unwrap();

        let scope_view = cell_proof_env(&store, &id, 0, 0).unwrap();
        assert_eq!(value_of(&scope_view, "RUST_LOG").as_deref(), Some("warn"));

        let step_view = proof_step_env(&store, &id, 0, "cell", 0, 0).unwrap();
        assert_eq!(value_of(&step_view, "RUST_LOG").as_deref(), Some("debug"));
        let row = step_view
            .vars
            .iter()
            .find(|v| v.name == "RUST_LOG")
            .unwrap();
        assert_eq!(row.source, "proof step");
    }

    #[test]
    fn an_unknown_squad_is_not_found() {
        let store = Store::open_in_memory().unwrap();
        assert!(matches!(
            squad_env(&store, "squad-nope").unwrap_err(),
            StoreError::NotFound
        ));
    }

    #[test]
    fn a_tombstoned_variable_is_not_listed_as_resolved() {
        let secret_names = BTreeSet::new();
        let layers = vec![
            (
                "source cell".to_string(),
                plain(
                    [("DROP_ME".to_string(), "value".to_string())]
                        .into_iter()
                        .collect(),
                ),
            ),
            (
                "review worktree".to_string(),
                [("DROP_ME".to_string(), None)].into_iter().collect(),
            ),
        ];
        let view = assemble(
            "review-worktree",
            "x".to_string(),
            layers,
            &secret_names,
            None,
        );
        assert!(view.vars.is_empty(), "a tombstoned key is not in the env");
    }

    #[test]
    fn tests_and_build_scopes_share_one_stored_override_layer() {
        assert_eq!(
            ReviewStep::Build.layer_label(),
            ReviewStep::Tests.layer_label()
        );
        assert_ne!(ReviewStep::Build.scope(), ReviewStep::Tests.scope());
    }
}
