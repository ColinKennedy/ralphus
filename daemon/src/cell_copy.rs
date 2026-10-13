//! RAL-641: copy one cell into a brand-new, one-task squad.
//!
//! The copy is rebuilt from the cell's stored rows (prompt or command, agent,
//! model, system prompt, subprojects, machine, cell-scope proofs) and submitted
//! through the normal submit path, so it is validated and materialized like any
//! other squad. Nothing that points at other cells (`depends_on`, a stacking
//! `upstream`) is carried over: those cells are not in the new squad.
//!
//! The new squad starts from a clean base ([`CopyBase::Clean`]) unless asked to
//! start from the copied cell's own branch ([`CopyBase::Branch`]). A cell
//! without a project has no worktree placeholder to rewrite, so it keeps the
//! `cwd` it had.

use ralphus_core::schema::{WORKTREE_PLACEHOLDER_PREFIX, WORKTREE_UPSTREAM_DEFAULT};
use rusqlite::params;
use toml::{Table, Value};

use crate::store::{CellRow, Result, Store, StoreError};

/// One cell-scope proof step of the cell being copied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CopyProof {
    /// The proof step's id, when it had one.
    pub id: Option<String>,
    /// `"command"` or `"prompt"`.
    pub kind: String,
    /// The command or prompt text.
    pub spec: String,
    /// The proof step's resolved agent.
    pub agent: String,
    /// The proof step's model, when it had one.
    pub model: Option<String>,
}

/// Everything the copy needs from the original cell.
#[derive(Debug, Clone)]
pub struct CellCopySource {
    /// Squad the cell belongs to.
    pub squad_id: String,
    /// Task index of the cell.
    pub task_idx: i64,
    /// Cell index within the task.
    pub cell_idx: i64,
    /// The registered project the cell's task names, if any.
    pub project: Option<String>,
    /// The stored cell.
    pub cell: CellRow,
    /// The cell's own proof steps, in order.
    pub proofs: Vec<CopyProof>,
}

/// Where the copy's worktree starts from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CopyBase {
    /// The project's default branch.
    Clean,
    /// The named branch, normally the copied cell's own.
    Branch(String),
}

impl Store {
    /// Load the definition of cell `(task_idx, cell_idx)` of `squad_id`.
    /// `Err(NotFound)` when the cell does not exist.
    pub fn cell_copy_source(
        &self,
        squad_id: &str,
        task_idx: i64,
        cell_idx: i64,
    ) -> Result<CellCopySource> {
        let cell = self
            .cells_of(squad_id)?
            .into_iter()
            .find(|c| c.task_idx == task_idx && c.idx == cell_idx)
            .ok_or(StoreError::NotFound)?;
        let project = self.task_project_at(squad_id, task_idx)?;
        let mut stmt = self.conn.prepare(
            "SELECT vid, kind, spec, agent, model FROM proofs
             WHERE squad_id=? AND task_idx=? AND scope='cell' AND cell_idx=?
             ORDER BY idx",
        )?;
        let proofs = stmt
            .query_map(params![squad_id, task_idx, cell_idx], |r| {
                Ok(CopyProof {
                    id: r.get(0)?,
                    kind: r.get(1)?,
                    spec: r.get(2)?,
                    agent: r.get(3)?,
                    model: r.get(4)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(CellCopySource {
            squad_id: squad_id.to_string(),
            task_idx,
            cell_idx,
            project,
            cell,
            proofs,
        })
    }
}

/// A branch-name-safe form of `s`.
fn slug(s: &str) -> String {
    let slug: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let slug = slug.trim_matches('-');
    if slug.is_empty() {
        "cell".to_string()
    } else {
        slug.to_string()
    }
}

/// The new squad's label.
#[must_use]
pub fn copy_label(source: &CellCopySource) -> String {
    format!(
        "copy of {}/{}/{}",
        source.squad_id, source.cell.task_name, source.cell.cell_id
    )
}

/// The one-task task file for the copy. `unique` makes the new worktree branch
/// name differ between copies of the same cell.
#[must_use]
pub fn build_copy_toml(source: &CellCopySource, base: &CopyBase, unique: &str) -> String {
    let cell = &source.cell;
    let mut task = Table::new();
    task.insert("name".into(), Value::String(cell.task_name.clone()));
    if let Some(project) = &source.project {
        task.insert("project".into(), Value::String(project.clone()));
    }

    let mut out = Table::new();
    out.insert("task".into(), Value::Array(vec![Value::Table(task)]));
    let task = out
        .get_mut("task")
        .and_then(Value::as_array_mut)
        .and_then(|a| a.first_mut())
        .and_then(Value::as_table_mut)
        .expect("task table was just inserted");

    let mut c = Table::new();
    c.insert("id".into(), Value::String(cell.cell_id.clone()));
    let cwd = if source.project.is_some() {
        let upstream = match base {
            CopyBase::Clean => WORKTREE_UPSTREAM_DEFAULT,
            CopyBase::Branch(b) => b.as_str(),
        };
        Some(format!(
            "<<{WORKTREE_PLACEHOLDER_PREFIX}{}-{}-copy-{unique}?upstream={upstream}>>",
            slug(&cell.task_name),
            slug(&cell.cell_id),
        ))
    } else {
        cell.cwd.clone()
    };
    if let Some(cwd) = cwd {
        c.insert("cwd".into(), Value::String(cwd));
    }
    if let Some(p) = &cell.prompt {
        c.insert("prompt".into(), Value::String(p.clone()));
    }
    if let Some(p) = &cell.command {
        c.insert("command".into(), Value::String(p.clone()));
        if let Some(mode) = &cell.mode {
            c.insert("mode".into(), Value::String(mode.clone()));
        }
        if let Some(n) = cell.remediation_attempts {
            c.insert("remediation_attempts".into(), Value::Integer(n));
        }
    }
    if cell.prompt.is_some() {
        c.insert("agent".into(), Value::String(cell.agent.clone()));
        if let Some(m) = &cell.model {
            c.insert("model".into(), Value::String(m.clone()));
        }
        if let Some(sp) = &cell.system_prompt {
            c.insert("system_prompt".into(), Value::String(sp.clone()));
            if let Some(pos) = &cell.system_prompt_position {
                c.insert("system_prompt_position".into(), Value::String(pos.clone()));
            }
        }
    }
    if !cell.subprojects.is_empty() {
        c.insert(
            "subprojects".into(),
            Value::Array(
                cell.subprojects
                    .iter()
                    .map(|s| Value::String(s.clone()))
                    .collect(),
            ),
        );
    }
    if let Some(m) = &cell.machine {
        c.insert("machine".into(), Value::String(m.clone()));
    }

    let proofs: Vec<Value> = source
        .proofs
        .iter()
        .map(|p| {
            let mut t = Table::new();
            if let Some(id) = &p.id {
                t.insert("id".into(), Value::String(id.clone()));
            }
            t.insert(p.kind.clone(), Value::String(p.spec.clone()));
            if p.kind == "command" {
                // The stored proof row keeps no remediation budget, so the
                // copy runs the command once rather than guessing one.
                t.insert("mode".into(), Value::String("raw".into()));
            }
            if p.kind == "prompt" {
                t.insert("agent".into(), Value::String(p.agent.clone()));
                if let Some(m) = &p.model {
                    t.insert("model".into(), Value::String(m.clone()));
                }
            }
            Value::Table(t)
        })
        .collect();
    if !proofs.is_empty() {
        c.insert("proof".into(), Value::Array(proofs));
    }
    task.insert("cell".into(), Value::Array(vec![Value::Table(c)]));
    toml::to_string(&out).expect("a table of strings and integers serializes")
}

#[cfg(test)]
mod tests {
    use super::*;
    use ralphus_core::schema::TaskFile;

    const ORIGINAL: &str = r#"
[[task]]
name = "build"
project = "demo"

[[task.cell]]
id = "compile"
cwd = "ralphus:new-worktree/build?upstream=<<default>>"
prompt = "make it \"compile\"\nplease"
agent = "claude-code"
model = "opus"
depends_on = ["other"]

[[task.cell.proof]]
id = "unit"
command = "cargo test"

[[task.cell.proof]]
id = "judge"
prompt = "is it good?"

[[task.cell]]
id = "other"
cwd = "ralphus:new-worktree/other?upstream=<<default>>"
command = "true"
"#;

    fn source() -> CellCopySource {
        let file: TaskFile = toml::from_str(ORIGINAL).unwrap();
        let mut store = Store::open_in_memory().unwrap();
        let squad = store.insert_squad(&file, Some("orig"), true).unwrap();
        store.cell_copy_source(&squad, 0, 0).unwrap()
    }

    #[test]
    fn copy_carries_the_cell_and_proofs_but_not_dependencies() {
        let src = source();
        let toml = build_copy_toml(&src, &CopyBase::Clean, "1");
        let report = ralphus_core::validate::validate_toml(&toml);
        assert!(
            report.is_ok(),
            "{:?}
{toml}",
            report.errors
        );
        let file: TaskFile = toml::from_str(&toml).unwrap();
        assert_eq!(file.task.len(), 1);
        let task = &file.task[0];
        assert_eq!(task.project.as_deref(), Some("demo"));
        assert_eq!(task.cell.len(), 1);
        let cell = &task.cell[0];
        assert_eq!(cell.prompt.as_deref(), Some("make it \"compile\"\nplease"));
        assert!(cell.depends_on.is_empty());
        assert_eq!(cell.proof.len(), 2);
        assert_eq!(cell.proof[0].command.as_deref(), Some("cargo test"));
        assert_eq!(cell.proof[1].prompt.as_deref(), Some("is it good?"));
        assert_eq!(
            cell.cwd.as_deref(),
            Some("<<ralphus:new-worktree/build-compile-copy-1?upstream=<<default>>>>")
        );
    }

    #[test]
    fn from_branch_starts_the_copy_on_the_given_branch() {
        let toml = build_copy_toml(&source(), &CopyBase::Branch("feat/x".into()), "2");
        assert!(toml.contains("?upstream=feat/x"), "{toml}");
        let report = ralphus_core::validate::validate_toml(&toml);
        assert!(report.is_ok(), "{:?}", report.errors);
    }

    #[test]
    fn a_missing_cell_is_not_found() {
        let store = Store::open_in_memory().unwrap();
        assert!(matches!(
            store.cell_copy_source("squad-x", 0, 0),
            Err(StoreError::NotFound)
        ));
    }
}
