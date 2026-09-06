//! Shared plumbing for the read-only resolved-environment listings (RAL-324):
//! `ralphus squad env`, `task env`, `cell env`, `proof env`, and `review env`.
//!
//! Not a command group of its own -- each verb lives under the noun it
//! addresses, the way every other per-entity CLI verb does. What is shared is
//! the fetch (one `GET .../env` against the daemon) and the rendering, so all
//! five print the same table.
//!
//! **Redaction is not done here.** The daemon resolves the layers and masks
//! every value whose name is registered in the Secrets tab before serializing
//! (see `ralphus_daemon::env_view`), so this CLI never receives a registered
//! secret's value and cannot become a bypass for the board's identical view.

use serde_json::Value;

use crate::args::GlobalOpts;
use crate::commands::{CommandError, emit, run_and_report};
use crate::flags::Scanner;
use crate::selector::{ResolvedGuardianSelector, ResolvedSelector};

/// Reads `--scope <name>`, validating it against the surfaces this command
/// offers. Returns the default when the flag is absent.
pub fn take_scope(
    scanner: &mut Scanner,
    default: &str,
    allowed: &[&str],
) -> Result<String, String> {
    let raw = scanner
        .take_value("--scope")
        .ok()
        .flatten()
        .unwrap_or_else(|| default.to_string());
    if allowed.contains(&raw.as_str()) {
        Ok(raw)
    } else {
        Err(format!(
            "unknown --scope {raw:?} (expected one of: {})",
            allowed.join(", ")
        ))
    }
}

/// The daemon path serving a squad's resolved environment.
#[must_use]
pub fn squad_path(squad_id: &str) -> String {
    format!("/api/squads/{squad_id}/env")
}

/// The daemon path for a task's resolved environment: `scope` is `task` (what
/// its cells inherit) or `proof` (what its task-scoped proof steps inherit).
#[must_use]
pub fn task_path(resolved: &ResolvedSelector, scope: &str) -> String {
    let suffix = if scope == "proof" { "/proof" } else { "" };
    format!(
        "/api/squads/{}/tasks/{}{suffix}/env",
        resolved.squad_id, resolved.task_idx
    )
}

/// The daemon path for a cell's resolved environment: `scope` is `cell` (the
/// cell's own subprocess) or `proof` (what its own proof steps inherit).
#[must_use]
pub fn cell_path(resolved: &ResolvedSelector, scope: &str) -> String {
    let suffix = if scope == "proof" { "/proof" } else { "" };
    format!(
        "/api/squads/{}/cells/{}/{}{suffix}/env",
        resolved.squad_id, resolved.task_idx, resolved.cell_idx
    )
}

/// The daemon path for one individual proof step's resolved environment. A
/// task-scoped step is addressed under `tasks/{ti}` and a cell-scoped one
/// under `cells/{ti}/{si}` -- the same split the `POST .../proof/{vi}/env`
/// routes use.
#[must_use]
pub fn proof_path(resolved: &ResolvedSelector) -> String {
    if resolved.proof_scope == "task" {
        format!(
            "/api/squads/{}/tasks/{}/proof/{}/env",
            resolved.squad_id, resolved.task_idx, resolved.proof_idx
        )
    } else {
        format!(
            "/api/squads/{}/cells/{}/{}/proof/{}/env",
            resolved.squad_id, resolved.task_idx, resolved.cell_idx, resolved.proof_idx
        )
    }
}

/// The default review surface for a selector: the worktree when the selector
/// named a branch, else the finalize-time build step.
#[must_use]
pub fn default_review_scope(resolved: &ResolvedGuardianSelector) -> String {
    if resolved.branch_id.is_some() {
        "worktree".to_string()
    } else {
        "build".to_string()
    }
}

/// The daemon path for one review surface's resolved environment.
///
/// # Errors
/// [`CommandError::Usage`] when `scope` is `worktree` but the selector did not
/// address a branch -- there is no single worktree to resolve in that case.
pub fn review_path(
    resolved: &ResolvedGuardianSelector,
    scope: &str,
    selector: &str,
) -> Result<String, CommandError> {
    if scope == "worktree" {
        let Some(branch_id) = resolved.branch_id.as_deref() else {
            return Err(CommandError::Usage(format!(
                "--scope worktree needs a branch: address one as '{selector}#0' or \
                 '{selector}/<branch>'"
            )));
        };
        return Ok(format!(
            "/api/guardians/{}/branches/{branch_id}/env",
            resolved.guardian_id
        ));
    }
    let leaf = match scope {
        "build" => "build-env",
        // The check gates run under the build step's own override layer, so
        // this is a distinct entry point onto the same stored layer -- see
        // `ralphus_daemon::env_view::ReviewStep`.
        "tests" => "tests-env",
        _ => "manual-checks-env",
    };
    Ok(format!("/api/guardians/{}/{leaf}", resolved.guardian_id))
}

/// Fetches and emits one resolved-environment view from `api_path`.
pub fn show(opts: &GlobalOpts, api_path: String) -> i32 {
    let client = opts.client();
    run_and_report(opts, None, || {
        let view = client.env_view(&api_path)?;
        emit(opts, &view, render);
        Ok(())
    })
}

/// Human rendering: the surface, its layers lowest-first, then one row per
/// resolved variable naming which layer won.
pub fn render(view: &Value) {
    crate::output::print_kv(&[
        ("surface", string_of(view, "label")),
        ("layers", array_of(view, "layers").join(" < ")),
    ]);
    println!();
    let vars = view["vars"].as_array().cloned().unwrap_or_default();
    if vars.is_empty() {
        println!("(no environment variables resolve here)");
    } else {
        let rows: Vec<Vec<String>> = vars
            .iter()
            .map(|v| {
                vec![
                    string_of(v, "name"),
                    string_of(v, "value"),
                    string_of(v, "source"),
                    if v["redacted"].as_bool().unwrap_or(false) {
                        "secret".to_string()
                    } else {
                        String::new()
                    },
                ]
            })
            .collect();
        crate::output::print_table(&["name", "value", "from", ""], &rows);
    }
    println!();
    if let Some(warning) = view["warning"].as_str() {
        println!("warning: {warning}");
    }
    println!("note: {}", string_of(view, "note"));
}

fn string_of(value: &Value, key: &str) -> String {
    value[key].as_str().unwrap_or_default().to_string()
}

fn array_of(value: &Value, key: &str) -> Vec<String> {
    value[key]
        .as_array()
        .map(|items| {
            items
                .iter()
                .map(|i| i.as_str().unwrap_or_default().to_string())
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scanner(args: &[&str]) -> Scanner {
        let owned: Vec<String> = args.iter().map(|s| (*s).to_string()).collect();
        Scanner::new(&owned)
    }

    #[test]
    fn take_scope_falls_back_to_the_default() {
        let mut s = scanner(&["squad-1"]);
        assert_eq!(
            take_scope(&mut s, "cell", &["cell", "proof"]).unwrap(),
            "cell"
        );
    }

    #[test]
    fn take_scope_accepts_an_offered_value_and_rejects_others() {
        let mut s = scanner(&["squad-1", "--scope", "proof"]);
        assert_eq!(
            take_scope(&mut s, "cell", &["cell", "proof"]).unwrap(),
            "proof"
        );
        let mut s = scanner(&["squad-1", "--scope", "nope"]);
        let err = take_scope(&mut s, "cell", &["cell", "proof"]).unwrap_err();
        assert!(err.contains("cell, proof"), "{err}");
    }

    fn cell_selector() -> ResolvedSelector {
        ResolvedSelector {
            kind: "cell".to_string(),
            squad_id: "squad-1".to_string(),
            task_idx: 2,
            cell_idx: 3,
            proof_idx: 4,
            proof_scope: "cell".to_string(),
        }
    }

    #[test]
    fn paths_match_the_routes_the_same_surfaces_are_set_on() {
        let s = cell_selector();
        assert_eq!(squad_path("squad-1"), "/api/squads/squad-1/env");
        assert_eq!(task_path(&s, "task"), "/api/squads/squad-1/tasks/2/env");
        assert_eq!(
            task_path(&s, "proof"),
            "/api/squads/squad-1/tasks/2/proof/env"
        );
        assert_eq!(cell_path(&s, "cell"), "/api/squads/squad-1/cells/2/3/env");
        assert_eq!(
            cell_path(&s, "proof"),
            "/api/squads/squad-1/cells/2/3/proof/env"
        );
        assert_eq!(proof_path(&s), "/api/squads/squad-1/cells/2/3/proof/4/env");
        let task_scoped = ResolvedSelector {
            proof_scope: "task".to_string(),
            ..cell_selector()
        };
        assert_eq!(
            proof_path(&task_scoped),
            "/api/squads/squad-1/tasks/2/proof/4/env"
        );
    }

    #[test]
    fn review_paths_cover_each_surface_and_require_a_branch_for_the_worktree() {
        let no_branch = ResolvedGuardianSelector {
            guardian_id: "guardian-1".to_string(),
            branch_id: None,
            branch: None,
            combined: false,
        };
        assert_eq!(default_review_scope(&no_branch), "build");
        assert_eq!(
            review_path(&no_branch, "build", "g1").unwrap(),
            "/api/guardians/guardian-1/build-env"
        );
        assert_eq!(
            review_path(&no_branch, "tests", "g1").unwrap(),
            "/api/guardians/guardian-1/tests-env"
        );
        assert_eq!(
            review_path(&no_branch, "manual-checks", "g1").unwrap(),
            "/api/guardians/guardian-1/manual-checks-env"
        );
        assert!(review_path(&no_branch, "worktree", "g1").is_err());

        let with_branch = ResolvedGuardianSelector {
            branch_id: Some("branch-7".to_string()),
            ..no_branch
        };
        assert_eq!(default_review_scope(&with_branch), "worktree");
        assert_eq!(
            review_path(&with_branch, "worktree", "g1").unwrap(),
            "/api/guardians/guardian-1/branches/branch-7/env"
        );
    }

    #[test]
    fn render_handles_an_empty_and_a_populated_view() {
        render(&serde_json::json!({
            "label": "squad squad-1", "layers": ["squad"], "vars": [],
            "redacted_count": 0, "note": "n", "warning": null,
        }));
        render(&serde_json::json!({
            "label": "cell 0/0 of squad-1",
            "layers": ["squad", "cell"],
            "vars": [{"name": "A", "value": "1", "source": "cell", "redacted": false}],
            "redacted_count": 0,
            "note": "n",
            "warning": "profile missing",
        }));
    }
}
