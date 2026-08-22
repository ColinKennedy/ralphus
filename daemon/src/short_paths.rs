//! Short, path-safe worktree naming (RAL-211).
//!
//! All of ralphus's own git worktrees live under one root, `.git/.ralphus/`
//! (inside `.git/`, so git already excludes it from the working tree and it
//! needs no `.gitignore` entry), split into two short-named groups so a
//! deeply nested repository root still leaves room for its deepest tracked
//! file within Windows' 260-character `MAX_PATH`:
//!
//! - `.git/.ralphus/g/g<n>/` — a guardian review, `<n>` its numeric id with
//!   zero-padding stripped. `wt-<short>` inside it is one branch's worktree,
//!   `<short>` a truncated form of the branch name; `review` is the combined
//!   (finished-stack) worktree.
//! - `.git/.ralphus/w/<short>/` — a task worktree
//!   (`ralphus:new-worktree/<branch>`), `<short>` again a truncated form of
//!   the branch name.
//!
//! This module holds the pure logic for that layout, kept free of any `git`
//! invocation so it is unit-testable without a repository. [`crate::worktrees`]
//! and [`crate::guardian_merge`] wire it into the actual worktree-creation
//! call sites. [`render_readme`] writes a human-readable explanation of this
//! layout to `.git/.ralphus/README.md`.
//!
//! Git branch names are **not** shortened by this module — only these
//! directory names are.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// How many characters of a sanitized branch name [`short_name`] keeps before
/// looking for a place to cut.
const TRUNCATE_AT: usize = 12;

/// Sanitize `raw` for use as a single path component: anything other than an
/// ASCII letter, digit, `-`, `_`, or `.` becomes `-` (this is what turns a
/// branch like `feature/x` into `feature-x` rather than a nested directory).
fn sanitize(raw: &str) -> String {
    raw.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// Truncate a sanitized branch name to at most [`TRUNCATE_AT`] characters,
/// without ending mid-word.
///
/// Takes the first 12 characters, then checks whether that cut lands exactly
/// on a word boundary — i.e. the next character (the 13th) is itself a `-` or
/// `_`, meaning nothing was split. If so, the full 12 characters are kept
/// as-is. Otherwise the cut would sever a word (e.g. `RAL-184-remo|te`), so it
/// backs up to the last separator at or before the 12-character mark. A name
/// with no separator anywhere in that window is hard-truncated at 12.
fn truncate(sanitized: &str) -> String {
    let chars: Vec<char> = sanitized.chars().collect();
    if chars.len() <= TRUNCATE_AT {
        return sanitized.to_string();
    }
    if matches!(chars[TRUNCATE_AT], '-' | '_') {
        return chars[..TRUNCATE_AT].iter().collect();
    }
    match chars[..TRUNCATE_AT]
        .iter()
        .rposition(|c| matches!(c, '-' | '_'))
    {
        Some(idx) => chars[..idx].iter().collect(),
        None => chars[..TRUNCATE_AT].iter().collect(),
    }
}

/// The short, path-safe form of a branch name, per the truncation rule above.
///
/// Deterministic and collision-*unaware* — the same input always produces the
/// same output, but two different branch names can produce the same short
/// name. Disambiguating that is layered on top rather than folded in here, so
/// this stays a pure function of one branch name: [`dedupe_short_names`] does
/// it for a guardian's fixed branch list (`g/g<n>/wt-<short>`), and
/// `worktrees::resolve_task_worktree_dir` does it for task worktrees
/// (`w/<short>`) by querying live `git worktree list` state instead, since
/// those branches are discovered one at a time across squads rather than
/// known up front.
#[must_use]
pub(crate) fn short_name(raw: &str) -> String {
    let truncated = truncate(&sanitize(raw));
    let trimmed = truncated.trim_end_matches(['-', '_', '.']);
    if trimmed.is_empty() {
        "wt".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Assign every branch in `branches` a short directory name, appending `-2`,
/// `-3`, ... on a collision so two branches that truncate to the same short
/// name never target the same directory.
///
/// **Order matters and must be stable across calls for the same guardian**:
/// the first branch (in iteration order) to reach a given short name keeps it
/// bare; a later one gets the next free suffix. Callers must always pass
/// branches in the same order (position order) so a rebuild assigns the exact
/// same name to the exact same branch as the build before it — otherwise an
/// existing worktree on disk would silently end up targeted at the wrong
/// branch.
#[must_use]
pub(crate) fn dedupe_short_names<'a, I>(branches: I) -> HashMap<String, String>
where
    I: IntoIterator<Item = &'a str>,
{
    let mut used: HashSet<String> = HashSet::new();
    let mut out = HashMap::new();
    for branch in branches {
        let base = short_name(branch);
        let mut candidate = base.clone();
        let mut n = 2;
        while used.contains(&candidate) {
            candidate = format!("{base}-{n}");
            n += 1;
        }
        used.insert(candidate.clone());
        out.insert(branch.to_string(), candidate);
    }
    out
}

/// The short `g<n>` form of a guardian id (`guardian-000000000056` -> `g56`),
/// with zero-padding stripped.
#[must_use]
pub(crate) fn guardian_short_id(id: &str) -> String {
    let num = id.strip_prefix("guardian-").unwrap_or(id);
    let trimmed = num.trim_start_matches('0');
    if trimmed.is_empty() {
        "g0".to_string()
    } else {
        format!("g{trimmed}")
    }
}

/// The root of ralphus's own worktree/admin state inside a project — `.git/`
/// is already excluded from the working tree, so nothing here needs a
/// `.gitignore` entry.
#[must_use]
pub(crate) fn ralphus_root(git_root: &Path) -> PathBuf {
    git_root.join(".git").join(".ralphus")
}

/// Whether the worktree at `path` (as printed by `git worktree list
/// --porcelain`) belongs to the guardian identified by `guardian_dir` (its
/// `.git/.ralphus/g/g<n>` directory) or `id` (its full id, for the legacy
/// `.ralphus_guardian/<id>` layout).
///
/// Path-component exact, not substring: `g56`'s directory is never a prefix
/// match for a worktree actually rooted under `g560`'s, because this compares
/// whole components, and the legacy-layout fallback checks a full path
/// *component* equal to `id` rather than `path.contains(id)`. A plain
/// substring check (the pre-RAL-211 behavior) would have let a `g56` cleanup
/// destroy `g560`'s worktree.
///
/// Checked two ways: a plain [`Path::starts_with`] against `guardian_dir`
/// (the cheap, common-case check), OR `path` containing the contiguous
/// component run `.git/.ralphus/g/<short-id>` anywhere in it (`path` is
/// always that run plus one more trailing component, the worktree's own
/// `wt-<branch>`/`review` directory).
///
/// The second check exists because `guardian_dir` is built in-process from
/// paths like `std::env::temp_dir()`, while `path` comes straight from git,
/// which always canonicalizes its own output to the OS's long form. On a
/// machine whose `TEMP`/`USERPROFILE` env var is itself an 8.3 short name
/// (e.g. `C:\Users\KORINK~1\...`), those two disagree on that one component
/// (`KORINK~1` vs `korinkite`) even though they name the same directory, so
/// the `starts_with` check silently never matches — which meant
/// `cleanup_review_worktrees` on such a machine could not recognize any of
/// its own worktrees, left their branches undeleted, and let a stale
/// `guardian/<id>/wt-<branch>` ref carry old content into the next rebuild.
/// Matching on the `.git/.ralphus/g/<short-id>` run alone sidesteps that
/// mismatch, since every component in it is a ralphus-generated name, never
/// an OS-assigned short name.
#[must_use]
pub(crate) fn worktree_belongs_to_guardian(path: &str, guardian_dir: &Path, id: &str) -> bool {
    let p = Path::new(path);
    if p.starts_with(guardian_dir) {
        return true;
    }
    let short_id = guardian_short_id(id);
    let needle = [".git", ".ralphus", "g", short_id.as_str()];
    let p_components: Vec<&std::ffi::OsStr> = p.components().map(|c| c.as_os_str()).collect();
    p_components.windows(needle.len()).any(|w| {
        w.iter()
            .zip(needle.iter())
            .all(|(pc, nc)| *pc == std::ffi::OsStr::new(nc))
    }) || p
        .components()
        .any(|c| c.as_os_str().to_string_lossy() == id)
}

/// Pure budget check: given the raw NUL-separated `git ls-tree -r -z
/// --name-only <ref>` output for the tree about to be checked out, and the
/// worktree directory that will hold it, fail if their combined length would
/// exceed `limit` characters (Windows' `MAX_PATH` is 260, when this is
/// enforced at all — see call sites).
///
/// # Errors
/// When the projected path length exceeds `limit`. The message spells out
/// every term of the arithmetic so a user can see exactly what to shorten.
pub(crate) fn check_worktree_path_budget(
    worktree_dir: &Path,
    tree_listing: &str,
    limit: usize,
) -> Result<(), String> {
    let deepest = tree_listing.split('\0').map(str::len).max().unwrap_or(0);
    let wt_len = worktree_dir.to_string_lossy().len();
    let total = wt_len + 1 + deepest;
    if total > limit {
        return Err(format!(
            "worktree path would exceed the {limit}-character path limit: \"{}\" is {wt_len} \
             characters, plus a separator, plus the deepest tracked file at {deepest} \
             characters, totals {total}. Move the repository closer to the filesystem root, or \
             shorten its path.",
            worktree_dir.display()
        ));
    }
    Ok(())
}

/// Render `.git/.ralphus/README.md`: an explanation of the directory scheme
/// plus the current short-name -> branch mappings, for a human who opens the
/// folder. `mappings` is `(short_path, branch_or_description)` in whatever
/// order the caller wants displayed.
#[must_use]
pub(crate) fn render_readme(mappings: &[(String, String)]) -> String {
    let mut out = String::new();
    out.push_str("# ralphus worktree state\n\n");
    out.push_str(
        "This directory holds ralphus's own git worktrees, under short, truncated \
         names so a deeply nested repository still fits inside Windows' 260-character \
         path limit (RAL-211). It lives inside `.git/`, which git already excludes from \
         the working tree, so nothing here needs a `.gitignore` entry.\n\n",
    );
    out.push_str("Layout:\n\n");
    out.push_str("- `w/<short>` -- a task worktree (`ralphus:new-worktree/<branch>`)\n");
    out.push_str("- `g/g<n>/wt-<short>` -- a guardian review's per-branch worktree\n");
    out.push_str("- `g/g<n>/review` -- a guardian review's combined (finished-stack) worktree\n\n");
    out.push_str(
        "Branch names themselves are never shortened -- only these directory names \
         are. This file is regenerated on every merge; it is for humans only, not read \
         by ralphus itself.\n\n",
    );
    if mappings.is_empty() {
        out.push_str("No worktrees are currently registered.\n");
        return out;
    }
    out.push_str("## Current mappings\n\n");
    for (short, branch) in mappings {
        out.push_str(&format!("- `{short}` -> `{branch}`\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_name_worked_examples_from_the_ticket() {
        assert_eq!(
            short_name("RAL-184-remote-build-auto-provisioning"),
            "RAL-184"
        );
        assert_eq!(short_name("RAL-121-speed_up_review_page"), "RAL-121");
        assert_eq!(short_name("improve_wasd_input_feel"), "improve_wasd");
    }

    #[test]
    fn short_name_keeps_short_branches_unchanged() {
        assert_eq!(short_name("feat-a"), "feat-a");
        assert_eq!(short_name("x"), "x");
    }

    #[test]
    fn short_name_hard_truncates_when_no_separator_is_in_the_window() {
        // 12 'a's followed by more 'a's -- no '-'/'_' anywhere to back up to.
        assert_eq!(short_name("aaaaaaaaaaaaaaaa"), "aaaaaaaaaaaa");
    }

    #[test]
    fn short_name_sanitizes_slashes_and_other_unsafe_characters() {
        assert_eq!(short_name("feature/x"), "feature-x");
        // Sanitizes to "feature-build-" (14 chars), then backs up to the
        // separator at index 7 since index 12 ('d') is mid-word.
        assert_eq!(short_name("feature/build?"), "feature");
    }

    #[test]
    fn short_name_never_ends_with_a_trailing_separator() {
        // Truncating at the window boundary can land right on a separator;
        // the result must not end with it.
        assert_eq!(short_name("abcdefghijk-foo"), "abcdefghijk");
    }

    #[test]
    fn dedupe_short_names_is_a_no_op_when_nothing_collides() {
        let out = dedupe_short_names(["RAL-184-remote-build", "RAL-121-speed-up"]);
        assert_eq!(
            out.get("RAL-184-remote-build").map(String::as_str),
            Some("RAL-184")
        );
        assert_eq!(
            out.get("RAL-121-speed-up").map(String::as_str),
            Some("RAL-121")
        );
    }

    #[test]
    fn dedupe_short_names_appends_an_incrementing_suffix_on_collision() {
        // All three truncate to "improve-wasd" (they only differ after the
        // 12-character truncation window).
        let out = dedupe_short_names(["improve-wasd-a", "improve-wasd-b", "improve-wasd-c"]);
        assert_eq!(out["improve-wasd-a"], "improve-wasd");
        assert_eq!(out["improve-wasd-b"], "improve-wasd-2");
        assert_eq!(out["improve-wasd-c"], "improve-wasd-3");
    }

    #[test]
    fn dedupe_short_names_is_stable_regardless_of_hashmap_iteration_by_depending_only_on_input_order()
     {
        // Same input order twice must produce identical output -- this is the
        // "stable across re-merges" property the ticket calls out.
        let branches = ["improve-wasd-a", "improve-wasd-b"];
        let first = dedupe_short_names(branches);
        let second = dedupe_short_names(branches);
        assert_eq!(first, second);
    }

    #[test]
    fn guardian_short_id_strips_the_prefix_and_zero_padding() {
        assert_eq!(guardian_short_id("guardian-000000000056"), "g56");
        assert_eq!(guardian_short_id("guardian-000000000097"), "g97");
        assert_eq!(guardian_short_id("guardian-000000000000"), "g0");
    }

    #[test]
    fn worktree_belongs_to_guardian_does_not_confuse_g56_with_g560() {
        let g56_dir = Path::new("/repo/.git/.ralphus/g/g56");
        // A worktree actually rooted under g560 must not be matched by g56's
        // check -- the pre-RAL-211 substring bug this guards against.
        assert!(!worktree_belongs_to_guardian(
            "/repo/.git/.ralphus/g/g560/wt-RAL-121",
            g56_dir,
            "guardian-000000000056",
        ));
        assert!(worktree_belongs_to_guardian(
            "/repo/.git/.ralphus/g/g56/wt-RAL-121",
            g56_dir,
            "guardian-000000000056",
        ));
    }

    #[test]
    fn worktree_belongs_to_guardian_still_matches_the_legacy_full_id_layout() {
        let g56_dir = Path::new("/repo/.git/.ralphus/g/g56");
        assert!(worktree_belongs_to_guardian(
            "/repo/.git/.ralphus_guardian/guardian-000000000056/wt-RAL-121",
            g56_dir,
            "guardian-000000000056",
        ));
    }

    #[test]
    fn check_worktree_path_budget_passes_when_under_the_limit() {
        let listing = "src/lib.rs\0src/main.rs\0";
        assert!(check_worktree_path_budget(Path::new("/short"), listing, 260).is_ok());
    }

    #[test]
    fn check_worktree_path_budget_fails_with_the_arithmetic_spelled_out() {
        let deep = "a".repeat(50);
        let listing = format!("src/{deep}.rs\0");
        let err = check_worktree_path_budget(
            Path::new("/repo/.git/.ralphus/g/g56/wt-RAL-121"),
            &listing,
            60,
        )
        .expect_err("must fail when over budget");
        assert!(err.contains("60-character"), "{err}");
        assert!(err.contains("exceed"), "{err}");
    }

    #[test]
    fn render_readme_lists_current_mappings() {
        let out = render_readme(&[(
            "g/g56/wt-RAL-121".to_string(),
            "RAL-121-speed-up".to_string(),
        )]);
        assert!(out.contains("g/g56/wt-RAL-121"));
        assert!(out.contains("RAL-121-speed-up"));
    }

    #[test]
    fn render_readme_handles_no_mappings() {
        let out = render_readme(&[]);
        assert!(out.contains("No worktrees are currently registered."));
    }
}
