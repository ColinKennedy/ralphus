//! Ghost memory store (RAL-136): an ephemeral, queryable record a task cell
//! or review worktree publishes so downstream work doesn't have to re-derive
//! it from scratch.
//!
//! A ghost is **not** a changelog — anything recoverable from `git log`/the
//! diff is already cheap to get. The point is to capture what a cell
//! learned that the diff alone can't show: where it struggled, issues it
//! noticed but didn't fix, open questions for whoever picks up dependent
//! work next.
//!
//! - **Data model** (`impl Store` below): CRUD over the `ghosts` table
//!   (schema created in `store.rs`). Mirrors how `pr.rs`/`guardian.rs` add
//!   `Store` methods from their own module rather than `store.rs` itself.
//! - **Keying**: one row per owner, addressed by a stable `owner_uri` —
//!   [`cell_uri`] for a task cell (`squad_id`/`task_idx`/`cell_idx`),
//!   [`review_uri`] for a review worktree (`guardian_id`/optional branch).
//!   Both task cells and review worktrees live in the same table since a
//!   cell's dependency-graph lookup (one level up — see
//!   `scheduler.rs::run_cell_worker`) and a review's explicit publish both
//!   go through the exact same [`Store::upsert_ghost`]/[`Store::get_ghost`]
//!   pair; keeping them in one table is what lets a ghost be copied between
//!   the two kinds (`Store::copy_ghost`) without a cross-database join.
//! - **One row per owner, merged on rewrite**: writing a ghost for a URI that
//!   already has one does not append a second row — [`merge_content`] folds
//!   the new text onto the old one (capped, keeping the most recent content)
//!   so a restarted cell's ghost is always the single rolled-up record for
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
pub const KIND_CELL: &str = "cell";
/// A ghost's owner kind.
pub const KIND_REVIEW: &str = "review";

/// One row of the `ghosts` table, as returned to API/internal consumers.
#[derive(Debug, Clone, Serialize)]
pub struct GhostView {
    /// Stable identifier of the cell/review that wrote this ghost.
    pub owner_uri: String,
    /// `"cell"` or `"review"`.
    pub kind: String,
    /// Owning squad id, for a `"cell"` ghost (cascade-deleted with the squad).
    pub squad_id: Option<String>,
    /// Owning guardian id, for a `"review"` ghost (cascade-deleted with the
    /// guardian).
    pub guardian_id: Option<String>,
    /// The handoff note itself.
    pub content: String,
    /// Free-form text a human typed into the restart popup (RAL-174), kept
    /// separate from `content` so it can be overwritten on every restart
    /// rather than folded/accumulated like the agent-authored notes above
    /// (see [`Store::set_ghost_user_note`]). `None` when no one has ever
    /// attached a restart note to this owner.
    pub user_note: Option<String>,
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
    squad_id: Option<String>,
    guardian_id: Option<String>,
    content: String,
    user_note: Option<String>,
    revision: Option<String>,
    created_at_ms: i64,
    updated_at_ms: i64,
}

impl From<GhostRow> for GhostView {
    fn from(r: GhostRow) -> Self {
        Self {
            owner_uri: r.owner_uri,
            kind: r.kind,
            squad_id: r.squad_id,
            guardian_id: r.guardian_id,
            content: r.content,
            user_note: r.user_note,
            revision: r.revision,
            created_at_ms: r.created_at_ms,
            updated_at_ms: r.updated_at_ms,
        }
    }
}

const GHOST_COLUMNS: &str = "owner_uri, kind, squad_id, guardian_id, content, user_note, revision, created_at_ms, updated_at_ms";

fn map_ghost_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<GhostRow> {
    Ok(GhostRow {
        owner_uri: r.get(0)?,
        kind: r.get(1)?,
        squad_id: r.get(2)?,
        guardian_id: r.get(3)?,
        content: r.get(4)?,
        user_note: r.get(5)?,
        revision: r.get(6)?,
        created_at_ms: r.get(7)?,
        updated_at_ms: r.get(8)?,
    })
}

/// The stable owner URI for a task cell's ghost.
#[must_use]
pub fn cell_uri(squad_id: &str, task_idx: i64, cell_idx: i64) -> String {
    format!("cell:{squad_id}:{task_idx}:{cell_idx}")
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

/// Parse an owner URI (as produced by [`cell_uri`]/[`review_uri`]) back
/// into `(kind, squad_id, guardian_id)`. Used by callers that only have the
/// target URI in hand -- e.g. the `POST /api/ghosts/copy` HTTP handler --
/// rather than the individual components used to build it. `None` for a URI
/// that doesn't start with a recognised `cell:`/`review:` prefix, or whose
/// id component is empty.
#[must_use]
pub fn parse_owner_uri(uri: &str) -> Option<(&'static str, Option<&str>, Option<&str>)> {
    if let Some(rest) = uri.strip_prefix("cell:") {
        let squad_id = rest.split(':').next()?;
        if squad_id.is_empty() {
            return None;
        }
        Some((KIND_CELL, Some(squad_id), None))
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

/// Advisory ghost note describing the *daemon-observed* (ground-truth, not
/// self-reported) outcome of a scope's proof/check steps (RAL-152). Callers
/// fold this onto the owning ghost with [`Store::upsert_ghost`] independent
/// of whatever the agent itself self-reported, so a restarted cell/resolver
/// gets a reliable signal even when the agent didn't report one -- or
/// didn't report one honestly. Phrased as a hint, not a guarantee: staleness
/// (module docs) applies just as much to a daemon-observed outcome as to a
/// self-reported one, since the code can change underneath it (e.g. a rebase
/// or conflict resolution) between when this was written and when it's read.
///
/// `passed`/`total` must satisfy `total > 0` and `passed <= total` -- callers
/// should skip writing a note entirely when no step actually ran, rather than
/// calling this with `total == 0`.
#[must_use]
pub fn proof_outcome_note(passed: usize, total: usize) -> String {
    debug_assert!(total > 0, "proof_outcome_note called with no steps run");
    debug_assert!(passed <= total, "passed count exceeds total");
    if passed == total {
        format!(
            "Daemon note (ground truth, not self-reported): the prior squad's {passed}/{total} \
             proof/check step(s) passed -- you internally validated that the code works. This \
             can go stale (e.g. a rebase or conflict resolution since this was written), so \
             re-test/re-verify the existing work first rather than assuming it's broken and \
             redoing it from scratch."
        )
    } else {
        format!(
            "Daemon note (ground truth, not self-reported): the prior squad's proof/check step(s) \
             did NOT all pass ({passed}/{total} passed) -- the existing work was not fully \
             validated. Investigate and fix the failure(s) before trusting or building further on \
             top of this code."
        )
    }
}

/// Build the context block to prepend to a cell's prompt from its own
/// prior ghost (if any) and its direct dependencies' ghosts, labelled
/// `"<task>/<cell>"`. Returns `None` when there is nothing to inject, so
/// callers can leave the prompt untouched rather than prepending an empty
/// (but visually noisy) block.
///
/// When the target cell's own ghost carries a human-authored restart note
/// (RAL-174), it is appended as its own distinct line at the very bottom of
/// the block -- after every agent-authored note, never interleaved with it
/// (Q1/Q2 of the ticket's interview).
#[must_use]
pub fn format_context_block(
    own: Option<&GhostView>,
    parents: &[(String, GhostView)],
) -> Option<String> {
    let own_note = own
        .and_then(|g| g.user_note.as_deref())
        .map(str::trim)
        .filter(|n| !n.is_empty());
    let own_content_present = own.is_some_and(|g| !g.content.trim().is_empty());
    if !own_content_present && own_note.is_none() && parents.is_empty() {
        return None;
    }
    let mut out = String::from(
        "--- Prior context (best-effort notes from earlier work; not verified against the current files) ---\n",
    );
    if own_content_present {
        out.push_str("Your own notes from a previous attempt at this cell:\n");
        out.push_str(&own.expect("checked above").content);
        out.push('\n');
    }
    for (label, g) in parents {
        out.push_str(&format!("Notes from dependency cell '{label}':\n"));
        out.push_str(&g.content);
        out.push('\n');
    }
    if let Some(note) = own_note {
        out.push_str(
            "A human wrote the following note when triggering this restart (not agent-authored, does not accumulate across restarts):\n",
        );
        out.push_str(note);
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
        squad_id: Option<&str>,
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
            "INSERT INTO ghosts(owner_uri, kind, squad_id, guardian_id, content, revision, created_at_ms, updated_at_ms)
             VALUES (?,?,?,?,?,?,?,?)
             ON CONFLICT(owner_uri) DO UPDATE SET
                content=excluded.content,
                revision=excluded.revision,
                updated_at_ms=excluded.updated_at_ms",
            params![owner_uri, kind, squad_id, guardian_id, merged, revision, now, now],
        )?;
        self.get_ghost(owner_uri).map(|g| g.expect("just written"))
    }

    /// Attach (or replace) a human-authored restart note (RAL-174) on the
    /// ghost for `owner_uri`, creating an (empty-content) ghost row first if
    /// one doesn't already exist. Unlike [`Store::upsert_ghost`]'s `content`,
    /// this *overwrites* rather than merges/accumulates -- each restart's
    /// note is one-time (Q5 of the ticket's interview); only the normal
    /// agent-authored `content` rolls up across restarts.
    pub fn set_ghost_user_note(
        &self,
        owner_uri: &str,
        kind: &str,
        squad_id: Option<&str>,
        guardian_id: Option<&str>,
        note: &str,
    ) -> Result<GhostView> {
        let now = now_ms();
        self.conn.execute(
            "INSERT INTO ghosts(owner_uri, kind, squad_id, guardian_id, content, user_note, created_at_ms, updated_at_ms)
             VALUES (?,?,?,?,'',?,?,?)
             ON CONFLICT(owner_uri) DO UPDATE SET
                user_note=excluded.user_note,
                updated_at_ms=excluded.updated_at_ms",
            params![owner_uri, kind, squad_id, guardian_id, note, now, now],
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
        target_squad_id: Option<&str>,
        target_guardian_id: Option<&str>,
    ) -> Result<GhostView> {
        let source = self
            .get_ghost(source_uri)?
            .ok_or(crate::store::StoreError::NotFound)?;
        self.upsert_ghost(
            target_uri,
            target_kind,
            target_squad_id,
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
    fn cell_uri_is_stable_and_scoped_to_squad() {
        assert_eq!(cell_uri("squad-1", 0, 2), "cell:squad-1:0:2");
        assert_ne!(cell_uri("squad-1", 0, 2), cell_uri("squad-2", 0, 2));
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
    fn parse_owner_uri_cell_extracts_squad_id() {
        let uri = cell_uri("squad-1", 3, 1);
        assert_eq!(
            parse_owner_uri(&uri),
            Some((KIND_CELL, Some("squad-1"), None))
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
        assert_eq!(parse_owner_uri("cell:"), None);
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

    /// Insert a minimal `squads` row so ghosts can carry a real `squad_id` FK.
    fn seed_squad(s: &Store, id: &str) {
        s.conn
            .execute(
                "INSERT INTO squads(id, state, created_at_ms, updated_at_ms) VALUES (?,'pending',0,0)",
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
        seed_squad(&s, "squad-1");
        let uri = cell_uri("squad-1", 0, 0);
        let g = s
            .upsert_ghost(
                &uri,
                KIND_CELL,
                Some("squad-1"),
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
        assert!(s.get_ghost("cell:nope:0:0").unwrap().is_none());
    }

    #[test]
    fn second_upsert_merges_rather_than_inserting_a_second_row() {
        let s = store();
        seed_squad(&s, "squad-1");
        let uri = cell_uri("squad-1", 0, 0);
        s.upsert_ghost(&uri, KIND_CELL, Some("squad-1"), None, "first note", None)
            .unwrap();
        let g = s
            .upsert_ghost(&uri, KIND_CELL, Some("squad-1"), None, "second note", None)
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
        seed_squad(&s, "squad-1");
        seed_squad(&s, "squad-2");
        let src = cell_uri("squad-1", 0, 0);
        let dst = cell_uri("squad-2", 3, 1);
        s.upsert_ghost(
            &src,
            KIND_CELL,
            Some("squad-1"),
            None,
            "handoff note",
            Some("sha1"),
        )
        .unwrap();
        let copied = s
            .copy_ghost(&src, &dst, KIND_CELL, Some("squad-2"), None)
            .unwrap();
        assert_eq!(copied.content, "handoff note");
        assert_eq!(copied.revision.as_deref(), Some("sha1"));
        // Source is untouched.
        assert_eq!(s.get_ghost(&src).unwrap().unwrap().content, "handoff note");
    }

    #[test]
    fn copy_ghost_missing_source_is_not_found() {
        let s = store();
        seed_squad(&s, "squad-1");
        let err = s
            .copy_ghost(
                "cell:nope:0:0",
                &cell_uri("squad-1", 0, 0),
                KIND_CELL,
                Some("squad-1"),
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
        assert_eq!(g.squad_id, None);
    }

    fn ghost_view(content: &str) -> GhostView {
        GhostView {
            owner_uri: "cell:squad-1:0:0".to_string(),
            kind: KIND_CELL.to_string(),
            squad_id: Some("squad-1".to_string()),
            guardian_id: None,
            content: content.to_string(),
            user_note: None,
            revision: None,
            created_at_ms: 0,
            updated_at_ms: 0,
        }
    }

    #[test]
    fn proof_outcome_note_all_passed_signals_validated() {
        let note = proof_outcome_note(2, 2);
        assert!(note.contains("2/2"));
        assert!(note.contains("internally validated"));
        assert!(note.contains("re-test/re-verify"));
    }

    #[test]
    fn proof_outcome_note_partial_pass_signals_not_validated() {
        let note = proof_outcome_note(1, 2);
        assert!(note.contains("1/2 passed"));
        assert!(note.contains("did NOT all pass"));
        assert!(!note.contains("internally validated"));
    }

    #[test]
    fn proof_outcome_note_all_failed_signals_not_validated() {
        let note = proof_outcome_note(0, 1);
        assert!(note.contains("0/1 passed"));
        assert!(note.contains("did NOT all pass"));
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
        assert!(!block.contains("dependency cell"));
    }

    #[test]
    fn set_ghost_user_note_creates_a_row_with_empty_content() {
        let s = store();
        seed_squad(&s, "squad-1");
        let uri = cell_uri("squad-1", 0, 0);
        let g = s
            .set_ghost_user_note(
                &uri,
                KIND_CELL,
                Some("squad-1"),
                None,
                "pick up where you left off",
            )
            .unwrap();
        assert_eq!(g.content, "");
        assert_eq!(g.user_note.as_deref(), Some("pick up where you left off"));
    }

    #[test]
    fn set_ghost_user_note_replaces_rather_than_accumulates() {
        let s = store();
        seed_squad(&s, "squad-1");
        let uri = cell_uri("squad-1", 0, 0);
        s.set_ghost_user_note(&uri, KIND_CELL, Some("squad-1"), None, "first restart note")
            .unwrap();
        let g = s
            .set_ghost_user_note(
                &uri,
                KIND_CELL,
                Some("squad-1"),
                None,
                "second restart note",
            )
            .unwrap();
        assert_eq!(g.user_note.as_deref(), Some("second restart note"));
        assert!(!g.user_note.unwrap().contains("first restart note"));
    }

    #[test]
    fn set_ghost_user_note_does_not_disturb_existing_agent_content() {
        let s = store();
        seed_squad(&s, "squad-1");
        let uri = cell_uri("squad-1", 0, 0);
        s.upsert_ghost(&uri, KIND_CELL, Some("squad-1"), None, "agent note", None)
            .unwrap();
        let g = s
            .set_ghost_user_note(&uri, KIND_CELL, Some("squad-1"), None, "human restart note")
            .unwrap();
        assert_eq!(g.content, "agent note");
        assert_eq!(g.user_note.as_deref(), Some("human restart note"));

        // A subsequent agent-authored write still merges/accumulates content
        // as before, leaving the (separately-overwritten) user note alone.
        let g2 = s
            .upsert_ghost(
                &uri,
                KIND_CELL,
                Some("squad-1"),
                None,
                "second agent note",
                None,
            )
            .unwrap();
        assert_eq!(g2.content, "agent note\n---\nsecond agent note");
        assert_eq!(g2.user_note.as_deref(), Some("human restart note"));
    }

    #[test]
    fn format_context_block_appends_user_note_after_agent_notes() {
        let mut own = ghost_view("agent note from last attempt");
        own.user_note = Some("you were stopped midway through the migration".to_string());
        let parents = vec![("build/compile".to_string(), ghost_view("left a TODO"))];
        let block = format_context_block(Some(&own), &parents).unwrap();
        let agent_pos = block.find("agent note from last attempt").unwrap();
        let parent_pos = block.find("left a TODO").unwrap();
        let note_pos = block
            .find("you were stopped midway through the migration")
            .unwrap();
        assert!(agent_pos < parent_pos);
        assert!(
            parent_pos < note_pos,
            "user note must come after parent notes"
        );
    }

    #[test]
    fn format_context_block_user_note_alone_does_not_render_empty_own_section() {
        let mut own = ghost_view("");
        own.user_note = Some("restart note with no prior agent content".to_string());
        let block = format_context_block(Some(&own), &[]).unwrap();
        assert!(!block.contains("Your own notes from a previous attempt"));
        assert!(block.contains("restart note with no prior agent content"));
    }

    #[test]
    fn format_context_block_blank_user_note_is_ignored() {
        let mut own = ghost_view("agent note");
        own.user_note = Some("   ".to_string());
        let block = format_context_block(Some(&own), &[]).unwrap();
        assert!(!block.contains("A human wrote the following note"));
    }
}
