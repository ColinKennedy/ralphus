//! Ghost memory store (RAL-136): an ephemeral, queryable record a task session
//! or review worktree publishes so downstream work doesn't have to re-derive
//! it from scratch.
//!
//! A ghost is **not** a changelog — anything recoverable from `git log`/the
//! diff is already cheap to get. The point is to capture what a session
//! learned that the diff alone can't show: where it struggled, issues it
//! noticed but didn't fix, open questions for whoever picks up dependent
//! work next.
//!
//! - **Data model** (`impl Store` below): CRUD over the `ghosts` table
//!   (schema created in `store.rs`). Mirrors how `pr.rs`/`guardian.rs` add
//!   `Store` methods from their own module rather than `store.rs` itself.
//! - **Keying**: one row per owner, addressed by a stable `owner_uri` —
//!   [`session_uri`] for a task session (`run_id`/`task_idx`/`session_idx`),
//!   [`review_uri`] for a review worktree (`guardian_id`/optional branch).
//!   Both task sessions and review worktrees live in the same table since a
//!   session's dependency-graph lookup (one level up — see
//!   `scheduler.rs::run_session_worker`) and a review's explicit publish both
//!   go through the exact same [`Store::upsert_ghost`]/[`Store::get_ghost`]
//!   pair; keeping them in one table is what lets a ghost be copied between
//!   the two kinds (`Store::copy_ghost`) without a cross-database join.
//! - **One row per owner, merged on rewrite**: writing a ghost for a URI that
//!   already has one does not append a second row — [`merge_content`] folds
//!   the new text onto the old one (capped, keeping the most recent content)
//!   so a restarted session's ghost is always the single rolled-up record for
//!   that owner.
//! - **Staleness**: best-effort only, never re-validated. `revision` is an
//!   opaque, VCS-agnostic marker (currently a git commit sha when the owner's
//!   working directory is a git repo; `None` otherwise) a consumer *could*
//!   reason about later — nothing in this module enforces or checks it.

use std::path::Path;

use rusqlite::{OptionalExtension, params};
use serde::Serialize;

use crate::store::{Result, Store, now_ms};

/// Ghosts are advisory handoff notes, not a document store — keep them small
/// enough to be cheap context, not a growing blob. Not a strict token budget,
/// just a sane ceiling so [`merge_content`] has something to trim against.
pub const MAX_CONTENT_CHARS: usize = 4000;

/// A ghost's owner kind.
pub const KIND_SESSION: &str = "session";
/// A ghost's owner kind.
pub const KIND_REVIEW: &str = "review";

/// One row of the `ghosts` table, as returned to API/internal consumers.
#[derive(Debug, Clone, Serialize)]
pub struct GhostView {
    /// Stable identifier of the session/review that wrote this ghost.
    pub owner_uri: String,
    /// `"session"` or `"review"`.
    pub kind: String,
    /// Owning run id, for a `"session"` ghost (cascade-deleted with the run).
    pub run_id: Option<String>,
    /// Owning guardian id, for a `"review"` ghost (cascade-deleted with the
    /// guardian).
    pub guardian_id: Option<String>,
    /// The handoff note itself.
    pub content: String,
    /// Opaque, VCS-agnostic revision marker captured when this ghost was last
    /// written (e.g. a git commit sha), for best-effort staleness reasoning.
    /// `None` when no VCS-derived identifier was available.
    pub revision: Option<String>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

struct GhostRow {
    owner_uri: String,
    kind: String,
    run_id: Option<String>,
    guardian_id: Option<String>,
    content: String,
    revision: Option<String>,
    created_at_ms: i64,
    updated_at_ms: i64,
}

impl From<GhostRow> for GhostView {
    fn from(r: GhostRow) -> Self {
        Self {
            owner_uri: r.owner_uri,
            kind: r.kind,
            run_id: r.run_id,
            guardian_id: r.guardian_id,
            content: r.content,
            revision: r.revision,
            created_at_ms: r.created_at_ms,
            updated_at_ms: r.updated_at_ms,
        }
    }
}

const GHOST_COLUMNS: &str =
    "owner_uri, kind, run_id, guardian_id, content, revision, created_at_ms, updated_at_ms";

fn map_ghost_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<GhostRow> {
    Ok(GhostRow {
        owner_uri: r.get(0)?,
        kind: r.get(1)?,
        run_id: r.get(2)?,
        guardian_id: r.get(3)?,
        content: r.get(4)?,
        revision: r.get(5)?,
        created_at_ms: r.get(6)?,
        updated_at_ms: r.get(7)?,
    })
}

/// The stable owner URI for a task session's ghost.
#[must_use]
pub fn session_uri(run_id: &str, task_idx: i64, session_idx: i64) -> String {
    format!("session:{run_id}:{task_idx}:{session_idx}")
}

/// The stable owner URI for a review worktree's ghost. `branch_id` is the
/// stacked branch's stable id (`branch-000000000042`, RAL-122); `None`
/// addresses the review's combined worktree as a whole.
#[must_use]
pub fn review_uri(guardian_id: &str, branch_id: Option<&str>) -> String {
    match branch_id {
        Some(b) => format!("review:{guardian_id}:{b}"),
        None => format!("review:{guardian_id}:combined"),
    }
}

/// Parse an owner URI (as produced by [`session_uri`]/[`review_uri`]) back
/// into `(kind, run_id, guardian_id)`. Used by callers that only have the
/// target URI in hand -- e.g. the `POST /api/ghosts/copy` HTTP handler --
/// rather than the individual components used to build it. `None` for a URI
/// that doesn't start with a recognised `session:`/`review:` prefix, or whose
/// id component is empty.
#[must_use]
pub fn parse_owner_uri(uri: &str) -> Option<(&'static str, Option<&str>, Option<&str>)> {
    if let Some(rest) = uri.strip_prefix("session:") {
        let run_id = rest.split(':').next()?;
        if run_id.is_empty() {
            return None;
        }
        Some((KIND_SESSION, Some(run_id), None))
    } else if let Some(rest) = uri.strip_prefix("review:") {
        let guardian_id = rest.split(':').next()?;
        if guardian_id.is_empty() {
            return None;
        }
        Some((KIND_REVIEW, None, Some(guardian_id)))
    } else {
        None
    }
}

/// Fold `new_content` onto `previous` (if any), so a rewritten ghost is
/// always one rolled-up record rather than an ever-growing log. The most
/// recent content is always kept in full when possible; if the combination
/// still exceeds [`MAX_CONTENT_CHARS`], older content is trimmed from the
/// front (to a line boundary) rather than truncating the newest text.
#[must_use]
pub fn merge_content(previous: Option<&str>, new_content: &str) -> String {
    let new_content = new_content.trim();
    let combined = match previous.map(str::trim) {
        Some(prev) if !prev.is_empty() => format!("{prev}\n---\n{new_content}"),
        _ => new_content.to_string(),
    };
    if combined.len() <= MAX_CONTENT_CHARS {
        return combined;
    }
    // Keep the tail (most recent content); drop whatever's cut off up to the
    // next line boundary so we don't start mid-sentence. `cut_from` is
    // nudged forward to the nearest char boundary first -- `combined.len()`
    // is a byte count, and an arbitrary byte offset can land inside a
    // multi-byte UTF-8 character.
    let min_cut = combined.len() - MAX_CONTENT_CHARS;
    let cut_from = (min_cut..=combined.len())
        .find(|&i| combined.is_char_boundary(i))
        .unwrap_or(combined.len());
    let tail = &combined[cut_from..];
    let tail = tail.find('\n').map_or(tail, |i| &tail[i + 1..]);
    format!("…(earlier notes truncated)…\n{tail}")
}

/// Best-effort revision marker for `cwd`: the current git commit sha, or
/// `None` if `cwd` isn't a git working tree (or git isn't available). This is
/// advisory only (see module docs on staleness) — never treated as a hard
/// error.
#[must_use]
pub fn current_revision(cwd: &str) -> Option<String> {
    if cwd.is_empty() {
        return None;
    }
    let sha = crate::guardian_merge::git(Path::new(cwd), &["rev-parse", "HEAD"]).ok()?;
    let sha = sha.trim();
    if sha.is_empty() {
        None
    } else {
        Some(sha.to_string())
    }
}

/// Build the context block to prepend to a session's prompt from its own
/// prior ghost (if any) and its direct dependencies' ghosts, labelled
/// `"<task>/<session>"`. Returns `None` when there is nothing to inject, so
/// callers can leave the prompt untouched rather than prepending an empty
/// (but visually noisy) block.
#[must_use]
pub fn format_context_block(
    own: Option<&GhostView>,
    parents: &[(String, GhostView)],
) -> Option<String> {
    if own.is_none() && parents.is_empty() {
        return None;
    }
    let mut out = String::from(
        "--- Prior context (best-effort notes from earlier work; not verified against the current files) ---\n",
    );
    if let Some(g) = own {
        out.push_str("Your own notes from a previous attempt at this session:\n");
        out.push_str(&g.content);
        out.push('\n');
    }
    for (label, g) in parents {
        out.push_str(&format!("Notes from dependency session '{label}':\n"));
        out.push_str(&g.content);
        out.push('\n');
    }
    out.push_str("--- End prior context ---\n\n");
    Some(out)
}

impl Store {
    /// Publish (or merge into) a ghost. If `owner_uri` already has a ghost,
    /// the new content is folded onto the existing one (`merge_content`)
    /// rather than replacing it or inserting a second row (Q4: one row per
    /// owner, rolling/merged on restart).
    pub fn upsert_ghost(
        &self,
        owner_uri: &str,
        kind: &str,
        run_id: Option<&str>,
        guardian_id: Option<&str>,
        new_content: &str,
        revision: Option<&str>,
    ) -> Result<GhostView> {
        let existing: Option<String> = self
            .conn
            .query_row(
                "SELECT content FROM ghosts WHERE owner_uri=?",
                params![owner_uri],
                |r| r.get(0),
            )
            .optional()?;
        let merged = merge_content(existing.as_deref(), new_content);
        let now = now_ms();
        self.conn.execute(
            "INSERT INTO ghosts(owner_uri, kind, run_id, guardian_id, content, revision, created_at_ms, updated_at_ms)
             VALUES (?,?,?,?,?,?,?,?)
             ON CONFLICT(owner_uri) DO UPDATE SET
                content=excluded.content,
                revision=excluded.revision,
                updated_at_ms=excluded.updated_at_ms",
            params![owner_uri, kind, run_id, guardian_id, merged, revision, now, now],
        )?;
        self.get_ghost(owner_uri).map(|g| g.expect("just written"))
    }

    /// Fetch one ghost by its owner URI, or `None` if it has never published
    /// one.
    pub fn get_ghost(&self, owner_uri: &str) -> Result<Option<GhostView>> {
        Ok(self
            .conn
            .query_row(
                &format!("SELECT {GHOST_COLUMNS} FROM ghosts WHERE owner_uri=?"),
                params![owner_uri],
                map_ghost_row,
            )
            .optional()?
            .map(GhostView::from))
    }

    /// Copy `source_uri`'s ghost onto `target_uri`, independent of the
    /// dependency graph (explicit seed, per the ticket's AC). Merges onto
    /// whatever `target_uri` already has, same as any other write. `NotFound`
    /// if `source_uri` has no ghost to copy.
    pub fn copy_ghost(
        &self,
        source_uri: &str,
        target_uri: &str,
        target_kind: &str,
        target_run_id: Option<&str>,
        target_guardian_id: Option<&str>,
    ) -> Result<GhostView> {
        let source = self
            .get_ghost(source_uri)?
            .ok_or(crate::store::StoreError::NotFound)?;
        self.upsert_ghost(
            target_uri,
            target_kind,
            target_run_id,
            target_guardian_id,
            &source.content,
            source.revision.as_deref(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_uri_is_stable_and_scoped_to_run() {
        assert_eq!(session_uri("run-1", 0, 2), "session:run-1:0:2");
        assert_ne!(session_uri("run-1", 0, 2), session_uri("run-2", 0, 2));
    }

    #[test]
    fn review_uri_distinguishes_branch_and_combined() {
        assert_eq!(
            review_uri("guardian-1", Some("branch-1")),
            "review:guardian-1:branch-1"
        );
        assert_eq!(review_uri("guardian-1", None), "review:guardian-1:combined");
    }

    #[test]
    fn parse_owner_uri_session_extracts_run_id() {
        let uri = session_uri("run-1", 3, 1);
        assert_eq!(
            parse_owner_uri(&uri),
            Some((KIND_SESSION, Some("run-1"), None))
        );
    }

    #[test]
    fn parse_owner_uri_review_extracts_guardian_id() {
        let uri = review_uri("guardian-1", Some("branch-1"));
        assert_eq!(
            parse_owner_uri(&uri),
            Some((KIND_REVIEW, None, Some("guardian-1")))
        );
        let combined = review_uri("guardian-1", None);
        assert_eq!(
            parse_owner_uri(&combined),
            Some((KIND_REVIEW, None, Some("guardian-1")))
        );
    }

    #[test]
    fn parse_owner_uri_rejects_unknown_prefix_or_empty_id() {
        assert_eq!(parse_owner_uri("bogus:foo"), None);
        assert_eq!(parse_owner_uri("session:"), None);
        assert_eq!(parse_owner_uri("review:"), None);
    }

    #[test]
    fn merge_content_with_no_previous_is_just_new() {
        assert_eq!(merge_content(None, "hello"), "hello");
        assert_eq!(merge_content(Some(""), "hello"), "hello");
        assert_eq!(merge_content(Some("   "), "hello"), "hello");
    }

    #[test]
    fn merge_content_combines_previous_and_new() {
        let merged = merge_content(Some("old note"), "new note");
        assert_eq!(merged, "old note\n---\nnew note");
    }

    #[test]
    fn merge_content_trims_oversized_combination_keeping_the_tail() {
        let previous = "a".repeat(MAX_CONTENT_CHARS);
        let new_content = "the newest and most important bit";
        let merged = merge_content(Some(&previous), new_content);
        assert!(merged.len() <= MAX_CONTENT_CHARS + "…(earlier notes truncated)…\n".len());
        assert!(
            merged.ends_with(new_content),
            "must keep the newest content in full: {merged}"
        );
        assert!(merged.starts_with("…(earlier notes truncated)…"));
    }

    #[test]
    fn merge_content_trims_without_panicking_on_multibyte_boundary() {
        // Every char is 3 bytes (UTF-8), chosen so a naive byte-offset cut
        // from the end would very likely land mid-character.
        let previous = "日".repeat(MAX_CONTENT_CHARS);
        let merged = merge_content(Some(&previous), "new note");
        assert!(merged.ends_with("new note"));
    }

    #[test]
    fn current_revision_none_for_non_git_dir() {
        let dir = std::env::temp_dir().join(format!("ralphus-ghost-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(current_revision(dir.to_str().unwrap()), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn current_revision_empty_cwd_is_none() {
        assert_eq!(current_revision(""), None);
    }

    fn store() -> Store {
        Store::open_in_memory().unwrap()
    }

    /// Insert a minimal `runs` row so ghosts can carry a real `run_id` FK.
    fn seed_run(s: &Store, id: &str) {
        s.conn
            .execute(
                "INSERT INTO runs(id, state, created_at_ms, updated_at_ms) VALUES (?,'pending',0,0)",
                params![id],
            )
            .unwrap();
    }

    /// Insert a minimal `guardians` row so ghosts can carry a real
    /// `guardian_id` FK.
    fn seed_guardian(s: &Store, id: &str) {
        s.conn
            .execute(
                "INSERT INTO guardians(id, name, base_branch, git_root, status, created_at_ms, updated_at_ms)
                 VALUES (?,'g','main','/repo','collecting',0,0)",
                params![id],
            )
            .unwrap();
    }

    #[test]
    fn upsert_then_get_round_trips() {
        let s = store();
        seed_run(&s, "run-1");
        let uri = session_uri("run-1", 0, 0);
        let g = s
            .upsert_ghost(
                &uri,
                KIND_SESSION,
                Some("run-1"),
                None,
                "first note",
                Some("abc123"),
            )
            .unwrap();
        assert_eq!(g.content, "first note");
        assert_eq!(g.revision.as_deref(), Some("abc123"));
        let fetched = s.get_ghost(&uri).unwrap().unwrap();
        assert_eq!(fetched.content, "first note");
    }

    #[test]
    fn get_ghost_missing_uri_is_none() {
        let s = store();
        assert!(s.get_ghost("session:nope:0:0").unwrap().is_none());
    }

    #[test]
    fn second_upsert_merges_rather_than_inserting_a_second_row() {
        let s = store();
        seed_run(&s, "run-1");
        let uri = session_uri("run-1", 0, 0);
        s.upsert_ghost(&uri, KIND_SESSION, Some("run-1"), None, "first note", None)
            .unwrap();
        let g = s
            .upsert_ghost(&uri, KIND_SESSION, Some("run-1"), None, "second note", None)
            .unwrap();
        assert_eq!(g.content, "first note\n---\nsecond note");
        let count: i64 = s
            .conn
            .query_row("SELECT COUNT(*) FROM ghosts", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn copy_ghost_seeds_another_owner_independent_of_the_graph() {
        let s = store();
        seed_run(&s, "run-1");
        seed_run(&s, "run-2");
        let src = session_uri("run-1", 0, 0);
        let dst = session_uri("run-2", 3, 1);
        s.upsert_ghost(
            &src,
            KIND_SESSION,
            Some("run-1"),
            None,
            "handoff note",
            Some("sha1"),
        )
        .unwrap();
        let copied = s
            .copy_ghost(&src, &dst, KIND_SESSION, Some("run-2"), None)
            .unwrap();
        assert_eq!(copied.content, "handoff note");
        assert_eq!(copied.revision.as_deref(), Some("sha1"));
        // Source is untouched.
        assert_eq!(s.get_ghost(&src).unwrap().unwrap().content, "handoff note");
    }

    #[test]
    fn copy_ghost_missing_source_is_not_found() {
        let s = store();
        seed_run(&s, "run-1");
        let err = s
            .copy_ghost(
                "session:nope:0:0",
                &session_uri("run-1", 0, 0),
                KIND_SESSION,
                Some("run-1"),
                None,
            )
            .unwrap_err();
        assert!(matches!(err, crate::store::StoreError::NotFound));
    }

    #[test]
    fn review_ghost_carries_guardian_id() {
        let s = store();
        seed_guardian(&s, "guardian-1");
        let uri = review_uri("guardian-1", Some("branch-1"));
        let g = s
            .upsert_ghost(
                &uri,
                KIND_REVIEW,
                None,
                Some("guardian-1"),
                "resolver note",
                None,
            )
            .unwrap();
        assert_eq!(g.guardian_id.as_deref(), Some("guardian-1"));
        assert_eq!(g.run_id, None);
    }

    fn ghost_view(content: &str) -> GhostView {
        GhostView {
            owner_uri: "session:run-1:0:0".to_string(),
            kind: KIND_SESSION.to_string(),
            run_id: Some("run-1".to_string()),
            guardian_id: None,
            content: content.to_string(),
            revision: None,
            created_at_ms: 0,
            updated_at_ms: 0,
        }
    }

    #[test]
    fn format_context_block_none_when_nothing_to_inject() {
        assert_eq!(format_context_block(None, &[]), None);
    }

    #[test]
    fn format_context_block_includes_own_and_parent_notes() {
        let own = ghost_view("watch out for the flaky test");
        let parents = vec![(
            "build/compile".to_string(),
            ghost_view("left a TODO in main.rs"),
        )];
        let block = format_context_block(Some(&own), &parents).unwrap();
        assert!(block.contains("watch out for the flaky test"));
        assert!(block.contains("build/compile"));
        assert!(block.contains("left a TODO in main.rs"));
    }

    #[test]
    fn format_context_block_own_only_omits_dependency_section() {
        let own = ghost_view("own note");
        let block = format_context_block(Some(&own), &[]).unwrap();
        assert!(block.contains("own note"));
        assert!(!block.contains("dependency session"));
    }
}
