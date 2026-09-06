//! A single-string URI scheme that addresses any entity in the daemon's
//! squad/task/cell/proof/guardian hierarchy uniformly (RAL-155 Q2).
//!
//! This is shared, cross-cutting infrastructure — not local to any one
//! feature — used by the CLI (`ralphus cartographer --entity ...`), the GUI
//! (the uber-log-viewer's Cartographer lookups), and as the `entity=` query
//! filter on `GET /api/cartographer` (see `daemon/src/server.rs`).
//!
//! Grammar (colon-separated, kind-prefixed, strict arity — no trailing
//! segments allowed):
//!
//! ```text
//! squad:<squad_id>
//! task:<squad_id>:<task_idx>
//! cell:<squad_id>:<task_idx>:<cell_idx>
//! proof:<squad_id>:<task_idx>:<proof_scope>:<cell_idx>:<proof_idx>
//! guardian:<guardian_id>
//! ```
//!
//! `task_idx`/`cell_idx`/`proof_idx` are the same 0-based indices already
//! used by the HTTP routes (`/api/squads/{id}/cells/{ti}/{si}/...`) and by
//! [`crate::ghost::cell_uri`] — this scheme deliberately reuses that
//! addressing convention rather than inventing a second one. `proof_scope`
//! is `"task"` or `"cell"` (mirroring the `proofs` table's `scope`
//! column); `cell_idx` is `-1` for a task-scope proof, matching
//! [`crate::store::Store::set_proof_state`]'s convention.
//!
//! An `EntityUri` only carries the addressing coordinates parsed out of the
//! string — it does not resolve them against the store (e.g. a `task_idx` is
//! not translated to the task's name here). Callers that need to turn an
//! `EntityUri` into a Cartographer filter do that translation themselves
//! (see `server.rs::cartographer_query`'s `entity=` handling), since it
//! requires a store lookup this module deliberately has no access to.

use std::fmt;

/// A parsed entity URI. See the module docs for the grammar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntityUri {
    Squad {
        squad_id: String,
    },
    Task {
        squad_id: String,
        task_idx: i64,
    },
    Cell {
        squad_id: String,
        task_idx: i64,
        cell_idx: i64,
    },
    Proof {
        squad_id: String,
        task_idx: i64,
        proof_scope: String,
        cell_idx: i64,
        proof_idx: i64,
    },
    Guardian {
        guardian_id: String,
    },
}

impl EntityUri {
    /// The owning squad id, for every kind except [`EntityUri::Guardian`].
    #[must_use]
    pub fn squad_id(&self) -> Option<&str> {
        match self {
            Self::Squad { squad_id }
            | Self::Task { squad_id, .. }
            | Self::Cell { squad_id, .. }
            | Self::Proof { squad_id, .. } => Some(squad_id),
            Self::Guardian { .. } => None,
        }
    }

    /// The guardian id, for [`EntityUri::Guardian`] only.
    #[must_use]
    pub fn guardian_id(&self) -> Option<&str> {
        match self {
            Self::Guardian { guardian_id } => Some(guardian_id),
            _ => None,
        }
    }

    /// Whether watching `self` should also notify about `other` — the
    /// parent-cascades-to-children rule Monitor watches rely on. An entity
    /// always covers itself; a `Squad` covers everything under its
    /// `squad_id`; a `Task` covers the `Cell`s/`Proof`s under its
    /// `(squad_id, task_idx)`; a `Cell` covers only the cell-scoped `Proof`s
    /// under its `(squad_id, task_idx, cell_idx)`. `Proof` and `Guardian`
    /// are leaves — they cover only themselves.
    #[must_use]
    pub fn covers(&self, other: &Self) -> bool {
        if self == other {
            return true;
        }
        match self {
            Self::Squad { squad_id } => other.squad_id() == Some(squad_id.as_str()),
            Self::Task { squad_id, task_idx } => match other {
                Self::Cell {
                    squad_id: s2,
                    task_idx: t2,
                    ..
                }
                | Self::Proof {
                    squad_id: s2,
                    task_idx: t2,
                    ..
                } => s2 == squad_id && t2 == task_idx,
                _ => false,
            },
            Self::Cell {
                squad_id,
                task_idx,
                cell_idx,
            } => match other {
                Self::Proof {
                    squad_id: s2,
                    task_idx: t2,
                    proof_scope,
                    cell_idx: c2,
                    ..
                } => proof_scope == "cell" && s2 == squad_id && t2 == task_idx && c2 == cell_idx,
                _ => false,
            },
            Self::Proof { .. } | Self::Guardian { .. } => false,
        }
    }
}

impl fmt::Display for EntityUri {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Squad { squad_id } => write!(f, "squad:{squad_id}"),
            Self::Task { squad_id, task_idx } => write!(f, "task:{squad_id}:{task_idx}"),
            Self::Cell {
                squad_id,
                task_idx,
                cell_idx,
            } => write!(f, "cell:{squad_id}:{task_idx}:{cell_idx}"),
            Self::Proof {
                squad_id,
                task_idx,
                proof_scope,
                cell_idx,
                proof_idx,
            } => write!(
                f,
                "proof:{squad_id}:{task_idx}:{proof_scope}:{cell_idx}:{proof_idx}"
            ),
            Self::Guardian { guardian_id } => write!(f, "guardian:{guardian_id}"),
        }
    }
}

/// Parse an entity URI string, or `None` if it doesn't match a recognised
/// grammar (unknown kind, wrong arity, non-numeric index, empty id, or an
/// invalid `proof_scope`).
#[must_use]
pub fn parse(uri: &str) -> Option<EntityUri> {
    let mut parts = uri.split(':');
    let kind = parts.next()?;
    let result = match kind {
        "squad" => {
            let squad_id = non_empty(parts.next()?)?;
            EntityUri::Squad {
                squad_id: squad_id.to_string(),
            }
        }
        "task" => {
            let squad_id = non_empty(parts.next()?)?;
            let task_idx = parts.next()?.parse().ok()?;
            EntityUri::Task {
                squad_id: squad_id.to_string(),
                task_idx,
            }
        }
        "cell" => {
            let squad_id = non_empty(parts.next()?)?;
            let task_idx = parts.next()?.parse().ok()?;
            let cell_idx = parts.next()?.parse().ok()?;
            EntityUri::Cell {
                squad_id: squad_id.to_string(),
                task_idx,
                cell_idx,
            }
        }
        "proof" => {
            let squad_id = non_empty(parts.next()?)?;
            let task_idx = parts.next()?.parse().ok()?;
            let proof_scope = parts.next()?;
            if proof_scope != "task" && proof_scope != "cell" {
                return None;
            }
            let cell_idx = parts.next()?.parse().ok()?;
            let proof_idx = parts.next()?.parse().ok()?;
            EntityUri::Proof {
                squad_id: squad_id.to_string(),
                task_idx,
                proof_scope: proof_scope.to_string(),
                cell_idx,
                proof_idx,
            }
        }
        "guardian" => {
            let guardian_id = non_empty(parts.next()?)?;
            EntityUri::Guardian {
                guardian_id: guardian_id.to_string(),
            }
        }
        _ => return None,
    };
    // Strict arity: no trailing segments left over.
    if parts.next().is_some() {
        return None;
    }
    Some(result)
}

fn non_empty(s: &str) -> Option<&str> {
    if s.is_empty() { None } else { Some(s) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_every_kind() {
        let cases = [
            EntityUri::Squad {
                squad_id: "squad-1".to_string(),
            },
            EntityUri::Task {
                squad_id: "squad-1".to_string(),
                task_idx: 2,
            },
            EntityUri::Cell {
                squad_id: "squad-1".to_string(),
                task_idx: 2,
                cell_idx: 0,
            },
            EntityUri::Proof {
                squad_id: "squad-1".to_string(),
                task_idx: 2,
                proof_scope: "cell".to_string(),
                cell_idx: 0,
                proof_idx: 1,
            },
            EntityUri::Guardian {
                guardian_id: "guardian-1".to_string(),
            },
        ];
        for uri in cases {
            let s = uri.to_string();
            assert_eq!(parse(&s), Some(uri), "round-trip failed for {s}");
        }
    }

    #[test]
    fn task_scope_proof_uses_negative_one_cell_idx_by_convention() {
        let s = "proof:squad-1:0:task:-1:3";
        assert_eq!(
            parse(s),
            Some(EntityUri::Proof {
                squad_id: "squad-1".to_string(),
                task_idx: 0,
                proof_scope: "task".to_string(),
                cell_idx: -1,
                proof_idx: 3,
            })
        );
    }

    #[test]
    fn squad_id_accessor_covers_every_squad_scoped_kind() {
        assert_eq!(parse("squad:squad-1").unwrap().squad_id(), Some("squad-1"));
        assert_eq!(parse("task:squad-1:0").unwrap().squad_id(), Some("squad-1"));
        assert_eq!(
            parse("cell:squad-1:0:1").unwrap().squad_id(),
            Some("squad-1")
        );
        assert_eq!(
            parse("proof:squad-1:0:task:-1:0").unwrap().squad_id(),
            Some("squad-1")
        );
        assert_eq!(parse("guardian:g-1").unwrap().squad_id(), None);
    }

    #[test]
    fn guardian_id_accessor_only_set_for_guardian_kind() {
        assert_eq!(parse("guardian:g-1").unwrap().guardian_id(), Some("g-1"));
        assert_eq!(parse("squad:squad-1").unwrap().guardian_id(), None);
    }

    #[test]
    fn rejects_unknown_kind() {
        assert_eq!(parse("bogus:squad-1"), None);
    }

    #[test]
    fn rejects_empty_ids() {
        assert_eq!(parse("squad:"), None);
        assert_eq!(parse("guardian:"), None);
    }

    #[test]
    fn rejects_wrong_arity() {
        assert_eq!(parse("squad:squad-1:extra"), None);
        assert_eq!(parse("task:squad-1"), None);
        assert_eq!(parse("cell:squad-1:0"), None);
    }

    #[test]
    fn rejects_non_numeric_indices() {
        assert_eq!(parse("task:squad-1:not-a-number"), None);
        assert_eq!(parse("cell:squad-1:0:not-a-number"), None);
    }

    #[test]
    fn rejects_invalid_proof_scope() {
        assert_eq!(parse("proof:squad-1:0:bogus:-1:0"), None);
    }

    #[test]
    fn covers_is_reflexive_for_every_kind() {
        let cases = [
            "squad:squad-1",
            "task:squad-1:0",
            "cell:squad-1:0:1",
            "proof:squad-1:0:cell:1:0",
            "guardian:g-1",
        ];
        for uri in cases {
            let parsed = parse(uri).unwrap();
            assert!(parsed.covers(&parsed), "{uri} should cover itself");
        }
    }

    #[test]
    fn squad_covers_every_descendant_in_the_same_squad() {
        let squad = parse("squad:squad-1").unwrap();
        assert!(squad.covers(&parse("task:squad-1:0").unwrap()));
        assert!(squad.covers(&parse("cell:squad-1:2:1").unwrap()));
        assert!(squad.covers(&parse("proof:squad-1:2:cell:1:0").unwrap()));
        assert!(!squad.covers(&parse("task:squad-2:0").unwrap()));
        assert!(!squad.covers(&parse("guardian:g-1").unwrap()));
    }

    #[test]
    fn task_covers_only_its_own_cells_and_proofs() {
        let task = parse("task:squad-1:2").unwrap();
        assert!(task.covers(&parse("cell:squad-1:2:0").unwrap()));
        assert!(task.covers(&parse("proof:squad-1:2:task:-1:0").unwrap()));
        assert!(!task.covers(&parse("cell:squad-1:3:0").unwrap()));
        assert!(!task.covers(&parse("squad:squad-1").unwrap()));
    }

    #[test]
    fn cell_covers_only_its_own_cell_scoped_proofs() {
        let cell = parse("cell:squad-1:2:1").unwrap();
        assert!(cell.covers(&parse("proof:squad-1:2:cell:1:0").unwrap()));
        assert!(!cell.covers(&parse("proof:squad-1:2:cell:0:0").unwrap()));
        assert!(!cell.covers(&parse("proof:squad-1:2:task:-1:0").unwrap()));
        assert!(!cell.covers(&parse("task:squad-1:2").unwrap()));
    }

    #[test]
    fn proof_and_guardian_are_leaves() {
        let proof = parse("proof:squad-1:0:cell:0:0").unwrap();
        assert!(!proof.covers(&parse("squad:squad-1").unwrap()));
        let guardian = parse("guardian:g-1").unwrap();
        assert!(!guardian.covers(&parse("guardian:g-2").unwrap()));
    }
}
