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
    Run {
        run_id: String,
    },
    Task {
        run_id: String,
        task_idx: i64,
    },
    Session {
        run_id: String,
        task_idx: i64,
        session_idx: i64,
    },
    Verify {
        run_id: String,
        task_idx: i64,
        verify_scope: String,
        session_idx: i64,
        verify_idx: i64,
    },
    Guardian {
        guardian_id: String,
    },
}

impl EntityUri {
    #[must_use]
    pub fn run_id(&self) -> Option<&str> {
        match self {
            Self::Run { run_id }
            | Self::Task { run_id, .. }
            | Self::Session { run_id, .. }
            | Self::Verify { run_id, .. } => Some(run_id),
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
            Self::Run { run_id } => write!(f, "run:{run_id}"),
            Self::Task { run_id, task_idx } => write!(f, "task:{run_id}:{task_idx}"),
            Self::Session {
                run_id,
                task_idx,
                session_idx,
            } => write!(f, "session:{run_id}:{task_idx}:{session_idx}"),
            Self::Verify {
                run_id,
                task_idx,
                verify_scope,
                session_idx,
                verify_idx,
            } => write!(
                f,
                "verify:{run_id}:{task_idx}:{verify_scope}:{session_idx}:{verify_idx}"
            ),
            Self::Guardian { guardian_id } => write!(f, "guardian:{guardian_id}"),
        }
    }
}

/// Parses an entity URI string, or `None` if it doesn't match a recognised
/// grammar (unknown kind, wrong arity, non-numeric index, empty id, or an
/// invalid `verify_scope`).
#[must_use]
pub fn parse(uri: &str) -> Option<EntityUri> {
    let mut parts = uri.split(':');
    let kind = parts.next()?;
    let result = match kind {
        "run" => EntityUri::Run {
            run_id: non_empty(parts.next()?)?.to_string(),
        },
        "task" => EntityUri::Task {
            run_id: non_empty(parts.next()?)?.to_string(),
            task_idx: parts.next()?.parse().ok()?,
        },
        "session" => EntityUri::Session {
            run_id: non_empty(parts.next()?)?.to_string(),
            task_idx: parts.next()?.parse().ok()?,
            session_idx: parts.next()?.parse().ok()?,
        },
        "verify" => {
            let run_id = non_empty(parts.next()?)?.to_string();
            let task_idx = parts.next()?.parse().ok()?;
            let verify_scope = parts.next()?;
            if verify_scope != "task" && verify_scope != "session" {
                return None;
            }
            EntityUri::Verify {
                run_id,
                task_idx,
                verify_scope: verify_scope.to_string(),
                session_idx: parts.next()?.parse().ok()?,
                verify_idx: parts.next()?.parse().ok()?,
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

/// Bridges a resolved run/task/session/verify selector into this wire
/// format, matching `ResolvedSelector.kind`'s four values.
#[must_use]
pub fn from_resolved_selector(resolved: &ResolvedSelector) -> EntityUri {
    match resolved.kind.as_str() {
        "task" => EntityUri::Task {
            run_id: resolved.run_id.clone(),
            task_idx: resolved.task_idx,
        },
        "session" => EntityUri::Session {
            run_id: resolved.run_id.clone(),
            task_idx: resolved.task_idx,
            session_idx: resolved.session_idx,
        },
        "verify" => EntityUri::Verify {
            run_id: resolved.run_id.clone(),
            task_idx: resolved.task_idx,
            verify_scope: resolved.verify_scope.clone(),
            session_idx: resolved.session_idx,
            verify_idx: resolved.verify_idx,
        },
        _ => EntityUri::Run {
            run_id: resolved.run_id.clone(),
        },
    }
}

/// Bridges a resolved review/branch selector into this wire format -- a
/// review addresses only the `guardian:` kind; branch/combined addressing
/// has no entity-URI equivalent (Cartographer rows are per-run/per-guardian,
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
            EntityUri::Run {
                run_id: "run-1".to_string(),
            },
            EntityUri::Task {
                run_id: "run-1".to_string(),
                task_idx: 2,
            },
            EntityUri::Session {
                run_id: "run-1".to_string(),
                task_idx: 2,
                session_idx: 0,
            },
            EntityUri::Verify {
                run_id: "run-1".to_string(),
                task_idx: 2,
                verify_scope: "session".to_string(),
                session_idx: 0,
                verify_idx: 1,
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
        assert_eq!(parse("bogus:run-1"), None);
        assert_eq!(parse("run:run-1:extra"), None);
        assert_eq!(parse("task:run-1"), None);
    }

    #[test]
    fn rejects_empty_ids_and_bad_indices() {
        assert_eq!(parse("run:"), None);
        assert_eq!(parse("task:run-1:not-a-number"), None);
        assert_eq!(parse("verify:run-1:0:bogus:-1:0"), None);
    }

    #[test]
    fn from_resolved_selector_maps_every_kind() {
        let base = ResolvedSelector {
            kind: "verify".to_string(),
            run_id: "run-1".to_string(),
            task_idx: 2,
            session_idx: 1,
            verify_idx: 0,
            verify_scope: "session".to_string(),
        };
        assert_eq!(
            from_resolved_selector(&base),
            EntityUri::Verify {
                run_id: "run-1".to_string(),
                task_idx: 2,
                verify_scope: "session".to_string(),
                session_idx: 1,
                verify_idx: 0,
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
