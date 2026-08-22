//! Selector grammar: a single, path-shaped way to address any
//! run/task/session/verify or review/branch entity from the CLI. Ported from
//! `cli/src/ralphus/selector.py`. Parsing (`parse_run_selector`/
//! `parse_guardian_selector`) never touches the network; resolution
//! (`resolve_run_selector`/`resolve_guardian_selector`) fetches the owning
//! run/guardian view once and matches name segments against it.
//!
//! Reuses `ralphus_core::uri` for the RAL-188 URI grammar itself (that
//! crate already carries the parser as the Rust twin of `cli/src/ralphus/
//! uri.py`) -- this module owns *resolution* (URI/legacy selector ->
//! `task_idx`/`session_idx`/`verify_idx`, branch ids), same split as Python.

use ralphus_core::uri::{self, INDEX_SIGIL_TOKEN, RalphusUri, Segment};
use serde_json::Value;

use crate::client::DaemonClient;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectorError(pub String);

impl std::fmt::Display for SelectorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for SelectorError {}

impl From<uri::UriError> for SelectorError {
    fn from(e: uri::UriError) -> Self {
        Self(e.0)
    }
}

// ---- run/task/session/verify selectors ---------------------------------

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RawRunSelector {
    pub run_id: String,
    pub task: Option<String>,
    pub session: Option<String>,
    pub verify_scope: Option<String>,
    pub verify: Option<String>,
    pub run_label: Option<String>,
    pub uri_form: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedSelector {
    pub kind: String,
    pub run_id: String,
    pub task_idx: i64,
    pub session_idx: i64,
    pub verify_idx: i64,
    pub verify_scope: String,
}

impl ResolvedSelector {
    fn run(run_id: String) -> Self {
        Self {
            kind: "run".to_string(),
            run_id,
            task_idx: 0,
            session_idx: -1,
            verify_idx: -1,
            verify_scope: String::new(),
        }
    }
}

fn segment_token(segment: &Segment) -> String {
    match segment.index {
        Some(idx) => format!("{INDEX_SIGIL_TOKEN}{idx}"),
        None => segment.name.clone().unwrap_or_default(),
    }
}

fn run_selector_from_uri(parsed: &RalphusUri, raw: &str) -> Result<RawRunSelector, SelectorError> {
    if parsed.kinds().first() != Some(&"RUN") {
        return Err(SelectorError(format!(
            "'{raw}' addresses a review, not a run/task/session/verify"
        )));
    }
    let run_seg = parsed
        .segment("RUN")
        .ok_or_else(|| SelectorError(format!("'{raw}': missing RUN segment")))?;
    if run_seg.index.is_some() {
        return Err(SelectorError(format!(
            "'{raw}': RUN[{INDEX_SIGIL_TOKEN}N] is not addressable -- a run has no stable \
             position. Use its label or id (and ideally '?id=<run id>')."
        )));
    }
    let task = parsed.segment("TASK");
    let session = parsed.segment("SESSION");
    let verify = parsed.segment("VERIFY");
    Ok(RawRunSelector {
        run_id: parsed.id().unwrap_or("").to_string(),
        run_label: run_seg.name.clone(),
        uri_form: true,
        task: task.map(segment_token),
        session: session.map(segment_token),
        verify_scope: verify
            .map(|_| if session.is_some() { "session" } else { "task" }.to_string()),
        verify: verify.map(segment_token),
    })
}

pub fn parse_run_selector(raw: &str) -> Result<RawRunSelector, SelectorError> {
    if uri::looks_like_uri(raw) {
        let parsed = uri::parse_uri(raw)?;
        return run_selector_from_uri(&parsed, raw);
    }

    let segs: Vec<&str> = raw.split('/').filter(|s| !s.is_empty()).collect();
    let Some((run_id, rest)) = segs.split_first() else {
        return Err(SelectorError("empty selector".to_string()));
    };
    let run_id = (*run_id).to_string();
    match rest {
        [] => Ok(RawRunSelector {
            run_id,
            ..Default::default()
        }),
        [task] => Ok(RawRunSelector {
            run_id,
            task: Some((*task).to_string()),
            ..Default::default()
        }),
        [task, "verify"] => {
            let _ = task;
            Err(SelectorError(format!(
                "'{raw}': 'verify' needs an index, e.g. .../verify/0"
            )))
        }
        [task, session] => Ok(RawRunSelector {
            run_id,
            task: Some((*task).to_string()),
            session: Some((*session).to_string()),
            ..Default::default()
        }),
        [task, "verify", idx] => Ok(RawRunSelector {
            run_id,
            task: Some((*task).to_string()),
            verify_scope: Some("task".to_string()),
            verify: Some((*idx).to_string()),
            ..Default::default()
        }),
        [task, session, "verify", idx] => Ok(RawRunSelector {
            run_id,
            task: Some((*task).to_string()),
            session: Some((*session).to_string()),
            verify_scope: Some("session".to_string()),
            verify: Some((*idx).to_string()),
            ..Default::default()
        }),
        _ => Err(SelectorError(format!("cannot parse selector '{raw}'"))),
    }
}

/// Resolves `raw` to an index into `candidates`. Legacy form: a bare int is a
/// position (bounds-checked); anything else matches by name. URI form: only
/// a `~N` token is a position -- a bare `0` addresses the entity *named*
/// `"0"`.
fn resolve_index(
    raw: &str,
    candidates: &[String],
    what: &str,
    uri_form: bool,
) -> Result<i64, SelectorError> {
    let positional: Option<&str> = if let Some(rest) = raw.strip_prefix(INDEX_SIGIL_TOKEN) {
        Some(rest)
    } else if !uri_form {
        Some(raw)
    } else {
        None
    };

    if let Some(text) = positional {
        match text.parse::<i64>() {
            Ok(idx) => {
                if idx < 0 || idx as usize >= candidates.len() {
                    return Err(SelectorError(format!(
                        "{what} index {idx} out of range (have {})",
                        candidates.len()
                    )));
                }
                return Ok(idx);
            }
            Err(_) if uri_form => {
                return Err(SelectorError(format!(
                    "{what} '{raw}': '{INDEX_SIGIL_TOKEN}' must be followed by a number"
                )));
            }
            Err(_) => {}
        }
    }

    let matches: Vec<usize> = candidates
        .iter()
        .enumerate()
        .filter(|(_, name)| !name.is_empty() && name.as_str() == raw)
        .map(|(i, _)| i)
        .collect();
    if matches.is_empty() {
        let options = candidates
            .iter()
            .enumerate()
            .map(|(i, c)| {
                if c.is_empty() {
                    format!("{INDEX_SIGIL_TOKEN}{i}")
                } else {
                    c.clone()
                }
            })
            .collect::<Vec<_>>()
            .join(", ");
        let options = if options.is_empty() {
            "(none)".to_string()
        } else {
            options
        };
        return Err(SelectorError(format!(
            "no {what} named '{raw}' (available: {options})"
        )));
    }
    if matches.len() > 1 {
        let hint = if uri_form {
            format!(
                "use {INDEX_SIGIL_TOKEN}{} (a positional index) instead",
                matches[0]
            )
        } else {
            "use an index".to_string()
        };
        return Err(SelectorError(format!(
            "'{raw}' matches {} {what}s at positions {matches:?} -- {hint}",
            matches.len()
        )));
    }
    Ok(matches[0] as i64)
}

fn verify_candidates(steps: &[Value]) -> Vec<String> {
    steps
        .iter()
        .map(|s| s["id"].as_str().unwrap_or("").to_string())
        .collect()
}

fn lookup_run_id(client: &DaemonClient, parsed: &RawRunSelector) -> Result<String, SelectorError> {
    let Some(label) = parsed.run_label.as_deref().filter(|l| !l.is_empty()) else {
        return Err(SelectorError("selector does not name a run".to_string()));
    };
    let tasks_view = client
        .tasks(None, None, None)
        .map_err(|e| SelectorError(e.to_string()))?;
    let runs = tasks_view["runs"].as_array().cloned().unwrap_or_default();
    let matches: Vec<&Value> = runs
        .iter()
        .filter(|r| r["label"].as_str() == Some(label) || r["id"].as_str() == Some(label))
        .collect();
    match matches.as_slice() {
        [] => Err(SelectorError(format!("no run labelled '{label}'"))),
        [one] => Ok(one["id"].as_str().unwrap_or_default().to_string()),
        many => {
            let ids = many
                .iter()
                .filter_map(|r| r["id"].as_str())
                .collect::<Vec<_>>()
                .join(", ");
            Err(SelectorError(format!(
                "'{label}' matches {} runs ({ids}) -- add '?id=<run id>' to say which one",
                many.len()
            )))
        }
    }
}

pub fn resolve_run_selector(
    client: &DaemonClient,
    raw: &str,
) -> Result<ResolvedSelector, SelectorError> {
    let parsed = parse_run_selector(raw)?;
    let run_id = if parsed.run_id.is_empty() {
        lookup_run_id(client, &parsed)?
    } else {
        parsed.run_id.clone()
    };
    let Some(task_raw) = &parsed.task else {
        return Ok(ResolvedSelector::run(run_id));
    };

    let run = client
        .run(&run_id)
        .map_err(|e| SelectorError(e.to_string()))?;
    let tasks = run["tasks"].as_array().cloned().unwrap_or_default();
    let task_names: Vec<String> = tasks
        .iter()
        .map(|t| t["name"].as_str().unwrap_or("").to_string())
        .collect();
    let task_idx = resolve_index(task_raw, &task_names, "task", parsed.uri_form)?;
    let task = &tasks[task_idx as usize];

    if parsed.verify_scope.as_deref() == Some("task") {
        let verify_raw = parsed.verify.as_deref().unwrap_or_default();
        let steps = task["verify"].as_array().cloned().unwrap_or_default();
        let verify_idx = resolve_index(
            verify_raw,
            &verify_candidates(&steps),
            "task verify",
            parsed.uri_form,
        )?;
        return Ok(ResolvedSelector {
            kind: "verify".to_string(),
            run_id,
            task_idx,
            session_idx: -1,
            verify_idx,
            verify_scope: "task".to_string(),
        });
    }

    let Some(session_raw) = &parsed.session else {
        return Ok(ResolvedSelector {
            kind: "task".to_string(),
            run_id,
            task_idx,
            session_idx: -1,
            verify_idx: -1,
            verify_scope: String::new(),
        });
    };

    let sessions = task["sessions"].as_array().cloned().unwrap_or_default();
    let session_names: Vec<String> = sessions
        .iter()
        .map(|s| {
            s["name"]
                .as_str()
                .or_else(|| s["id"].as_str())
                .unwrap_or("")
                .to_string()
        })
        .collect();
    let session_idx = resolve_index(session_raw, &session_names, "session", parsed.uri_form)?;

    if parsed.verify_scope.as_deref() == Some("session") {
        let verify_raw = parsed.verify.as_deref().unwrap_or_default();
        let session = &sessions[session_idx as usize];
        let steps = session["verify"].as_array().cloned().unwrap_or_default();
        let verify_idx = resolve_index(
            verify_raw,
            &verify_candidates(&steps),
            "session verify",
            parsed.uri_form,
        )?;
        return Ok(ResolvedSelector {
            kind: "verify".to_string(),
            run_id,
            task_idx,
            session_idx,
            verify_idx,
            verify_scope: "session".to_string(),
        });
    }

    Ok(ResolvedSelector {
        kind: "session".to_string(),
        run_id,
        task_idx,
        session_idx,
        verify_idx: -1,
        verify_scope: String::new(),
    })
}

// ---- review/branch selectors --------------------------------------------

const LEGACY_BRANCH_SEPARATOR: char = '#';

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RawGuardianSelector {
    pub head: String,
    pub branch: Option<String>,
    pub combined: bool,
    pub guardian_id: Option<String>,
    pub uri_form: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedGuardianSelector {
    pub guardian_id: String,
    pub branch_id: Option<String>,
    pub branch: Option<String>,
    pub combined: bool,
}

fn split_legacy_branch(raw: &str) -> (String, Option<String>) {
    let cuts: Vec<usize> = [INDEX_SIGIL_TOKEN, LEGACY_BRANCH_SEPARATOR]
        .iter()
        .filter_map(|token| raw.find(*token))
        .collect();
    let Some(&at) = cuts.iter().min() else {
        return (raw.to_string(), None);
    };
    (raw[..at].to_string(), Some(raw[at + 1..].to_string()))
}

pub fn parse_guardian_selector(raw: &str) -> Result<RawGuardianSelector, SelectorError> {
    if uri::looks_like_uri(raw) {
        let parsed = uri::parse_uri(raw)?;
        let kind = *parsed.kinds().first().unwrap_or(&"");
        if kind != "REVIEW" {
            return Err(SelectorError(format!(
                "'{raw}' addresses a {}, not a review",
                kind.to_lowercase()
            )));
        }
        let segment = &parsed.segments[0];
        if segment.index.is_some() {
            return Err(SelectorError(format!(
                "'{raw}': REVIEW[{INDEX_SIGIL_TOKEN}N] is not addressable -- a review has no \
                 stable position. Use its name or id (and ideally '?id=<guardian id>')."
            )));
        }
        return Ok(RawGuardianSelector {
            head: segment.name.clone().unwrap_or_default(),
            branch: parsed.get("worktree").map(str::to_string),
            combined: parsed.has("combined"),
            guardian_id: parsed.id().map(str::to_string),
            uri_form: true,
        });
    }

    if raw.is_empty() {
        return Err(SelectorError("empty selector".to_string()));
    }
    let (head, branch) = split_legacy_branch(raw);
    if head.is_empty() {
        return Err(SelectorError(format!("cannot parse selector '{raw}'")));
    }
    Ok(RawGuardianSelector {
        head,
        branch,
        ..Default::default()
    })
}

pub const DEFAULT_REVIEW_LIST_HINT: &str = "ralphus review list";

fn lookup_guardian_id(
    client: &DaemonClient,
    parsed: &RawGuardianSelector,
    list_hint: &str,
) -> Result<String, SelectorError> {
    if let Some(id) = &parsed.guardian_id {
        return Ok(id.clone());
    }
    if !parsed.uri_form && !parsed.head.starts_with('@') {
        return Ok(parsed.head.clone());
    }

    let name = parsed.head.strip_prefix('@').unwrap_or(&parsed.head);
    let guardians = client
        .guardian_list()
        .map_err(|e| SelectorError(e.to_string()))?;
    let list = guardians.as_array().cloned().unwrap_or_default();
    let matches: Vec<&Value> = list
        .iter()
        .filter(|g| {
            g["name"].as_str() == Some(name) || (parsed.uri_form && g["id"].as_str() == Some(name))
        })
        .collect();
    match matches.as_slice() {
        [] => Err(SelectorError(format!(
            "no review named '{name}' (run '{list_hint}' to see candidates)"
        ))),
        [one] => Ok(one["id"].as_str().unwrap_or_default().to_string()),
        many => {
            let ids = many
                .iter()
                .filter_map(|g| g["id"].as_str())
                .collect::<Vec<_>>()
                .join(", ");
            let hint = if parsed.uri_form {
                format!("add '?id=<guardian id>' to say which one ({ids})")
            } else {
                "use the id instead".to_string()
            };
            Err(SelectorError(format!(
                "'{name}' matches {} reviews -- {hint}",
                many.len()
            )))
        }
    }
}

pub fn resolve_guardian_selector(
    client: &DaemonClient,
    raw: &str,
    list_hint: &str,
) -> Result<ResolvedGuardianSelector, SelectorError> {
    let parsed = parse_guardian_selector(raw)?;
    let guardian_id = lookup_guardian_id(client, &parsed, list_hint)?;

    let Some(pos_or_branch) = &parsed.branch else {
        return Ok(ResolvedGuardianSelector {
            guardian_id,
            branch_id: None,
            branch: None,
            combined: parsed.combined,
        });
    };

    let guardian = client
        .guardian_get(&guardian_id)
        .map_err(|e| SelectorError(e.to_string()))?;
    let branches = guardian["branches"].as_array().cloned().unwrap_or_default();

    let positional: Option<&str> = if let Some(rest) = pos_or_branch.strip_prefix(INDEX_SIGIL_TOKEN)
    {
        Some(rest)
    } else if !parsed.uri_form {
        Some(pos_or_branch.as_str())
    } else {
        None
    };
    let mut position: Option<i64> = None;
    if let Some(text) = positional {
        match text.parse::<i64>() {
            Ok(p) => position = Some(p),
            Err(_) if parsed.uri_form => {
                return Err(SelectorError(format!(
                    "'?worktree={pos_or_branch}': '{INDEX_SIGIL_TOKEN}' must be followed by a position"
                )));
            }
            Err(_) => {}
        }
    }

    let (branch_id, branch_name) = if let Some(position) = position {
        let found = branches
            .iter()
            .find(|b| b["position"].as_i64() == Some(position));
        let found = found.ok_or_else(|| {
            SelectorError(format!("no branch at position {position} in this review"))
        })?;
        (
            found["id"].as_str().unwrap_or_default().to_string(),
            found["branch"].as_str().unwrap_or_default().to_string(),
        )
    } else {
        let by_id: Vec<&Value> = branches
            .iter()
            .filter(|b| b["id"].as_str() == Some(pos_or_branch.as_str()))
            .collect();
        let matches: Vec<&Value> = if by_id.is_empty() {
            branches
                .iter()
                .filter(|b| b["branch"].as_str() == Some(pos_or_branch.as_str()))
                .collect()
        } else {
            by_id
        };
        match matches.as_slice() {
            [] => {
                let options = branches
                    .iter()
                    .filter_map(|b| b["branch"].as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                let options = if options.is_empty() {
                    "(none)".to_string()
                } else {
                    options
                };
                return Err(SelectorError(format!(
                    "no branch '{pos_or_branch}' in this review (available: {options})"
                )));
            }
            [one] => (
                one["id"].as_str().unwrap_or_default().to_string(),
                one["branch"].as_str().unwrap_or_default().to_string(),
            ),
            _ => {
                return Err(SelectorError(format!(
                    "'{pos_or_branch}' matches multiple branches -- use a position"
                )));
            }
        }
    };

    Ok(ResolvedGuardianSelector {
        guardian_id,
        branch_id: Some(branch_id),
        branch: Some(branch_name),
        combined: false,
    })
}

// ---- URI production (RAL-188 §C.3: ralphus always emits the ?id= sidecar) --

/// A path segment's addressing value: by name, or (when the entity has none
/// of its own) by position -- mirrors Python's `str | int` union for this.
enum Addr {
    Name(String),
    Index(i64),
}

impl Addr {
    fn from_name_or_index(name: Option<&str>, index: i64) -> Self {
        match name.filter(|n| !n.is_empty()) {
            Some(n) => Self::Name(n.to_string()),
            None => Self::Index(index),
        }
    }

    fn segment(&self, kind: &str) -> Segment {
        match self {
            Self::Name(n) => Segment::named(kind, n.clone()),
            Self::Index(i) => Segment::positional(kind, (*i).max(0) as usize),
        }
    }
}

/// Renders the canonical ralphus URI for whatever `resolved` addresses
/// inside `run` (a `/api/runs/{id}` view).
#[must_use]
pub fn run_view_uri(run: &Value, resolved: &ResolvedSelector) -> String {
    let run_id = run["id"].as_str().unwrap_or(&resolved.run_id).to_string();
    let label_raw = run["label"]
        .as_str()
        .filter(|l| !l.is_empty())
        .unwrap_or(&run_id);

    let mut segments = vec![Segment::named("RUN", label_raw.to_string())];
    let mut query: Vec<(String, Option<String>)> = vec![("id".to_string(), Some(run_id.clone()))];

    if resolved.kind == "run" {
        return RalphusUri { segments, query }.to_string();
    }

    let tasks = run["tasks"].as_array().cloned().unwrap_or_default();
    let task_view = tasks
        .get(resolved.task_idx as usize)
        .cloned()
        .unwrap_or(Value::Null);
    let task_addr = Addr::from_name_or_index(task_view["name"].as_str(), resolved.task_idx);
    segments.push(task_addr.segment("TASK"));

    let mut steps: Vec<Value> = Vec::new();
    if resolved.verify_scope == "task" {
        steps = task_view["verify"].as_array().cloned().unwrap_or_default();
    } else {
        let sessions = task_view["sessions"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        if resolved.session_idx >= 0 && (resolved.session_idx as usize) < sessions.len() {
            let session_view = &sessions[resolved.session_idx as usize];
            let name = session_view["name"]
                .as_str()
                .or_else(|| session_view["id"].as_str());
            segments.push(Addr::from_name_or_index(name, resolved.session_idx).segment("SESSION"));
            steps = session_view["verify"]
                .as_array()
                .cloned()
                .unwrap_or_default();
        }
    }

    if resolved.kind == "verify" {
        let step = steps
            .get(resolved.verify_idx as usize)
            .cloned()
            .unwrap_or(Value::Null);
        segments.push(
            Addr::from_name_or_index(step["id"].as_str(), resolved.verify_idx).segment("VERIFY"),
        );
    }

    let _ = &mut query;
    RalphusUri { segments, query }.to_string()
}

/// Renders the canonical ralphus URI for a review, optionally narrowed to
/// the branch/combined worktree `resolved` addresses.
#[must_use]
pub fn guardian_view_uri(guardian: &Value, resolved: Option<&ResolvedGuardianSelector>) -> String {
    let guardian_id = guardian["id"].as_str().unwrap_or_default().to_string();
    let name = guardian["name"]
        .as_str()
        .filter(|n| !n.is_empty())
        .map_or_else(|| guardian_id.clone(), str::to_string);
    let id_value = if guardian_id.is_empty() {
        resolved.map(|r| r.guardian_id.clone()).unwrap_or_default()
    } else {
        guardian_id
    };
    let mut query: Vec<(String, Option<String>)> = vec![("id".to_string(), Some(id_value))];
    if let Some(resolved) = resolved {
        if let Some(branch) = &resolved.branch {
            query.push(("worktree".to_string(), Some(branch.clone())));
        }
        if resolved.combined {
            query.push(("combined".to_string(), None));
        }
    }
    RalphusUri {
        segments: vec![Segment::named("REVIEW", name)],
        query,
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_run_selector_legacy_bare_run() {
        let sel = parse_run_selector("run-123").unwrap();
        assert_eq!(sel.run_id, "run-123");
        assert_eq!(sel.task, None);
    }

    #[test]
    fn parse_run_selector_legacy_task_and_session() {
        let sel = parse_run_selector("run-123/0/1").unwrap();
        assert_eq!(sel.task.as_deref(), Some("0"));
        assert_eq!(sel.session.as_deref(), Some("1"));
    }

    #[test]
    fn parse_run_selector_legacy_task_verify() {
        let sel = parse_run_selector("run-123/0/verify/2").unwrap();
        assert_eq!(sel.verify_scope.as_deref(), Some("task"));
        assert_eq!(sel.verify.as_deref(), Some("2"));
    }

    #[test]
    fn parse_run_selector_legacy_session_verify() {
        let sel = parse_run_selector("run-123/0/1/verify/2").unwrap();
        assert_eq!(sel.verify_scope.as_deref(), Some("session"));
        assert_eq!(sel.session.as_deref(), Some("1"));
    }

    #[test]
    fn parse_run_selector_rejects_bare_verify_no_index() {
        assert!(parse_run_selector("run-123/0/verify").is_err());
    }

    #[test]
    fn parse_run_selector_uri_form() {
        let sel =
            parse_run_selector("ralphus:/RUN[my run]/TASK[build]?id=run-000000000151").unwrap();
        assert!(sel.uri_form);
        assert_eq!(sel.run_id, "run-000000000151");
        assert_eq!(sel.task.as_deref(), Some("build"));
    }

    #[test]
    fn parse_run_selector_uri_rejects_positional_run() {
        let err = parse_run_selector("ralphus:/RUN[~0]").unwrap_err();
        assert!(err.0.contains("not addressable"));
    }

    #[test]
    fn resolve_index_legacy_bare_int_is_position() {
        let candidates = vec!["a".to_string(), "b".to_string()];
        assert_eq!(resolve_index("1", &candidates, "task", false).unwrap(), 1);
    }

    #[test]
    fn resolve_index_uri_form_bare_token_is_name_not_position() {
        let candidates = vec!["0".to_string(), "b".to_string()];
        assert_eq!(resolve_index("0", &candidates, "task", true).unwrap(), 0);
    }

    #[test]
    fn resolve_index_uri_form_sigil_is_position() {
        let candidates = vec!["a".to_string(), "b".to_string()];
        assert_eq!(resolve_index("~1", &candidates, "task", true).unwrap(), 1);
    }

    #[test]
    fn resolve_index_ambiguous_name_errors() {
        let candidates = vec!["x".to_string(), "x".to_string()];
        let err = resolve_index("x", &candidates, "task", false).unwrap_err();
        assert!(err.0.contains("matches 2"));
    }

    #[test]
    fn parse_guardian_selector_legacy_plain_id() {
        let sel = parse_guardian_selector("guardian-1").unwrap();
        assert_eq!(sel.head, "guardian-1");
        assert_eq!(sel.branch, None);
    }

    #[test]
    fn parse_guardian_selector_legacy_branch_tilde() {
        let sel = parse_guardian_selector("guardian-1~2").unwrap();
        assert_eq!(sel.head, "guardian-1");
        assert_eq!(sel.branch.as_deref(), Some("2"));
    }

    #[test]
    fn parse_guardian_selector_legacy_branch_hash_earliest_wins() {
        let sel = parse_guardian_selector("@my review#fix~1").unwrap();
        assert_eq!(sel.head, "@my review");
        assert_eq!(sel.branch.as_deref(), Some("fix~1"));
    }

    #[test]
    fn parse_guardian_selector_uri_form() {
        let sel = parse_guardian_selector("ralphus:/REVIEW[my review]?id=guardian-3&worktree=~2")
            .unwrap();
        assert!(sel.uri_form);
        assert_eq!(sel.guardian_id.as_deref(), Some("guardian-3"));
        assert_eq!(sel.branch.as_deref(), Some("~2"));
    }

    #[test]
    fn run_view_uri_renders_run_only() {
        let run = serde_json::json!({"id": "run-1", "label": "my run"});
        let resolved = ResolvedSelector::run("run-1".to_string());
        let out = run_view_uri(&run, &resolved);
        assert_eq!(out, "ralphus:/RUN[my run]?id=run-1");
    }

    #[test]
    fn guardian_view_uri_renders_with_branch() {
        let guardian = serde_json::json!({"id": "g1", "name": "my review"});
        let resolved = ResolvedGuardianSelector {
            guardian_id: "g1".to_string(),
            branch_id: Some("b1".to_string()),
            branch: Some("feature".to_string()),
            combined: false,
        };
        let out = guardian_view_uri(&guardian, Some(&resolved));
        assert_eq!(out, "ralphus:/REVIEW[my review]?id=g1&worktree=feature");
    }
}
