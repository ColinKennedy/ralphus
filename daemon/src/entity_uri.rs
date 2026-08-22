//! A single-string URI scheme that addresses any entity in the daemon's
//! run/task/session/verify/guardian hierarchy uniformly (RAL-155 Q2).
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
//! run:<run_id>
//! task:<run_id>:<task_idx>
//! session:<run_id>:<task_idx>:<session_idx>
//! verify:<run_id>:<task_idx>:<verify_scope>:<session_idx>:<verify_idx>
//! guardian:<guardian_id>
//! ```
//!
//! `task_idx`/`session_idx`/`verify_idx` are the same 0-based indices already
//! used by the HTTP routes (`/api/runs/{id}/sessions/{ti}/{si}/...`) and by
//! [`crate::ghost::session_uri`] — this scheme deliberately reuses that
//! addressing convention rather than inventing a second one. `verify_scope`
//! is `"task"` or `"session"` (mirroring the `verifies` table's `scope`
//! column); `session_idx` is `-1` for a task-scope verify, matching
//! [`crate::store::Store::set_verify_state`]'s convention.
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
    /// The owning run id, for every kind except [`EntityUri::Guardian`].
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

    /// The guardian id, for [`EntityUri::Guardian`] only.
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

/// Parse an entity URI string, or `None` if it doesn't match a recognised
/// grammar (unknown kind, wrong arity, non-numeric index, empty id, or an
/// invalid `verify_scope`).
#[must_use]
pub fn parse(uri: &str) -> Option<EntityUri> {
    let mut parts = uri.split(':');
    let kind = parts.next()?;
    let result = match kind {
        "run" => {
            let run_id = non_empty(parts.next()?)?;
            EntityUri::Run {
                run_id: run_id.to_string(),
            }
        }
        "task" => {
            let run_id = non_empty(parts.next()?)?;
            let task_idx = parts.next()?.parse().ok()?;
            EntityUri::Task {
                run_id: run_id.to_string(),
                task_idx,
            }
        }
        "session" => {
            let run_id = non_empty(parts.next()?)?;
            let task_idx = parts.next()?.parse().ok()?;
            let session_idx = parts.next()?.parse().ok()?;
            EntityUri::Session {
                run_id: run_id.to_string(),
                task_idx,
                session_idx,
            }
        }
        "verify" => {
            let run_id = non_empty(parts.next()?)?;
            let task_idx = parts.next()?.parse().ok()?;
            let verify_scope = parts.next()?;
            if verify_scope != "task" && verify_scope != "session" {
                return None;
            }
            let session_idx = parts.next()?.parse().ok()?;
            let verify_idx = parts.next()?.parse().ok()?;
            EntityUri::Verify {
                run_id: run_id.to_string(),
                task_idx,
                verify_scope: verify_scope.to_string(),
                session_idx,
                verify_idx,
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
    fn task_scope_verify_uses_negative_one_session_idx_by_convention() {
        let s = "verify:run-1:0:task:-1:3";
        assert_eq!(
            parse(s),
            Some(EntityUri::Verify {
                run_id: "run-1".to_string(),
                task_idx: 0,
                verify_scope: "task".to_string(),
                session_idx: -1,
                verify_idx: 3,
            })
        );
    }

    #[test]
    fn run_id_accessor_covers_every_run_scoped_kind() {
        assert_eq!(parse("run:run-1").unwrap().run_id(), Some("run-1"));
        assert_eq!(parse("task:run-1:0").unwrap().run_id(), Some("run-1"));
        assert_eq!(parse("session:run-1:0:1").unwrap().run_id(), Some("run-1"));
        assert_eq!(
            parse("verify:run-1:0:task:-1:0").unwrap().run_id(),
            Some("run-1")
        );
        assert_eq!(parse("guardian:g-1").unwrap().run_id(), None);
    }

    #[test]
    fn guardian_id_accessor_only_set_for_guardian_kind() {
        assert_eq!(parse("guardian:g-1").unwrap().guardian_id(), Some("g-1"));
        assert_eq!(parse("run:run-1").unwrap().guardian_id(), None);
    }

    #[test]
    fn rejects_unknown_kind() {
        assert_eq!(parse("bogus:run-1"), None);
    }

    #[test]
    fn rejects_empty_ids() {
        assert_eq!(parse("run:"), None);
        assert_eq!(parse("guardian:"), None);
    }

    #[test]
    fn rejects_wrong_arity() {
        assert_eq!(parse("run:run-1:extra"), None);
        assert_eq!(parse("task:run-1"), None);
        assert_eq!(parse("session:run-1:0"), None);
    }

    #[test]
    fn rejects_non_numeric_indices() {
        assert_eq!(parse("task:run-1:not-a-number"), None);
        assert_eq!(parse("session:run-1:0:not-a-number"), None);
    }

    #[test]
    fn rejects_invalid_verify_scope() {
        assert_eq!(parse("verify:run-1:0:bogus:-1:0"), None);
    }
}
