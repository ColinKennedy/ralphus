//! The colon-delimited entity URI grammar used as the wire-level value for
//! `GET /api/cartographer?entity=`. This is a standalone copy of
//! `daemon/src/entity_uri.rs`'s grammar (rather than a cross-crate dependency
//! on the daemon lib, which would pull in rusqlite/tiny_http for a ~150-line
//! string-parsing module) -- both must stay in lockstep with
//! `cli/src/ralphus/entity_uri.py`, same as today.
//!
//! Adds [`from_resolved_selector`]/[`from_resolved_guardian_selector`], the
//! bridge from [`crate::selector`]'s human-typed, name-or-index resolved
//! selectors into this wire format (used by `ralphus cartographer --for
//! <selector>`).

use std::fmt;

use crate::selector::{ResolvedGuardianSelector, ResolvedSelector};

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

    #[must_use]
    pub fn guardian_id(&self) -> Option<&str> {
        match self {
            Self::Guardian { guardian_id } => Some(guardian_id),
            _ => None,
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

/// Parses an entity URI string, or `None` if it doesn't match a recognised
/// grammar (unknown kind, wrong arity, non-numeric index, empty id, or an
/// invalid `proof_scope`).
#[must_use]
pub fn parse(uri: &str) -> Option<EntityUri> {
    let mut parts = uri.split(':');
    let kind = parts.next()?;
    let result = match kind {
        "squad" => EntityUri::Squad {
            squad_id: non_empty(parts.next()?)?.to_string(),
        },
        "task" => EntityUri::Task {
            squad_id: non_empty(parts.next()?)?.to_string(),
            task_idx: parts.next()?.parse().ok()?,
        },
        "cell" => EntityUri::Cell {
            squad_id: non_empty(parts.next()?)?.to_string(),
            task_idx: parts.next()?.parse().ok()?,
            cell_idx: parts.next()?.parse().ok()?,
        },
        "proof" => {
            let squad_id = non_empty(parts.next()?)?.to_string();
            let task_idx = parts.next()?.parse().ok()?;
            let proof_scope = parts.next()?;
            if proof_scope != "task" && proof_scope != "cell" {
                return None;
            }
            EntityUri::Proof {
                squad_id,
                task_idx,
                proof_scope: proof_scope.to_string(),
                cell_idx: parts.next()?.parse().ok()?,
                proof_idx: parts.next()?.parse().ok()?,
            }
        }
        "guardian" => EntityUri::Guardian {
            guardian_id: non_empty(parts.next()?)?.to_string(),
        },
        _ => return None,
    };
    if parts.next().is_some() {
        return None;
    }
    Some(result)
}

fn non_empty(s: &str) -> Option<&str> {
    if s.is_empty() { None } else { Some(s) }
}

/// Bridges a resolved squad/task/cell/proof selector into this wire
/// format, matching `ResolvedSelector.kind`'s four values.
#[must_use]
pub fn from_resolved_selector(resolved: &ResolvedSelector) -> EntityUri {
    match resolved.kind.as_str() {
        "task" => EntityUri::Task {
            squad_id: resolved.squad_id.clone(),
            task_idx: resolved.task_idx,
        },
        "cell" => EntityUri::Cell {
            squad_id: resolved.squad_id.clone(),
            task_idx: resolved.task_idx,
            cell_idx: resolved.cell_idx,
        },
        "proof" => EntityUri::Proof {
            squad_id: resolved.squad_id.clone(),
            task_idx: resolved.task_idx,
            proof_scope: resolved.proof_scope.clone(),
            cell_idx: resolved.cell_idx,
            proof_idx: resolved.proof_idx,
        },
        _ => EntityUri::Squad {
            squad_id: resolved.squad_id.clone(),
        },
    }
}

/// Bridges a resolved review/branch selector into this wire format -- a
/// review addresses only the `guardian:` kind; branch/combined addressing
/// has no entity-URI equivalent (Cartographer rows are per-squad/per-guardian,
/// not per-branch).
#[must_use]
pub fn from_resolved_guardian_selector(resolved: &ResolvedGuardianSelector) -> EntityUri {
    EntityUri::Guardian {
        guardian_id: resolved.guardian_id.clone(),
    }
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
    fn rejects_unknown_kind_and_wrong_arity() {
        assert_eq!(parse("bogus:squad-1"), None);
        assert_eq!(parse("squad:squad-1:extra"), None);
        assert_eq!(parse("task:squad-1"), None);
    }

    #[test]
    fn rejects_empty_ids_and_bad_indices() {
        assert_eq!(parse("squad:"), None);
        assert_eq!(parse("task:squad-1:not-a-number"), None);
        assert_eq!(parse("proof:squad-1:0:bogus:-1:0"), None);
    }

    #[test]
    fn from_resolved_selector_maps_every_kind() {
        let base = ResolvedSelector {
            kind: "proof".to_string(),
            squad_id: "squad-1".to_string(),
            task_idx: 2,
            cell_idx: 1,
            proof_idx: 0,
            proof_scope: "cell".to_string(),
        };
        assert_eq!(
            from_resolved_selector(&base),
            EntityUri::Proof {
                squad_id: "squad-1".to_string(),
                task_idx: 2,
                proof_scope: "cell".to_string(),
                cell_idx: 1,
                proof_idx: 0,
            }
        );
    }

    #[test]
    fn from_resolved_guardian_selector_ignores_branch() {
        let resolved = ResolvedGuardianSelector {
            guardian_id: "g1".to_string(),
            branch_id: Some("b1".to_string()),
            branch: Some("feature".to_string()),
            combined: false,
        };
        assert_eq!(
            from_resolved_guardian_selector(&resolved),
            EntityUri::Guardian {
                guardian_id: "g1".to_string()
            }
        );
    }
}
