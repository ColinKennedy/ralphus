//! RAL-378: readable review-branch names.
//!
//! A review's branch used to be an internal ref -- `guardian/<id>/wt-<task
//! branch>` per stacked branch, `guardian/<id>/review` for a combined
//! worktree. Those are unambiguous but unreadable, and they force a review's
//! pull request onto a *second*, differently-named remote branch, with all
//! the reconciliation that implies. A readable `<task branch>-review` can be
//! pushed as-is and be the PR branch (see
//! `crate::guardian::GuardianView::effective_separate_pr_branch`).
//!
//! Moving the name into the user's own branch namespace costs the collision
//! immunity the `guardian/` prefix gave for free, so every name here is
//! resolved against the names already in use -- local refs, other reviews'
//! claimed names, and open PR aliases -- and suffixed `-2`, `-3`, ... until
//! it is unique. Because the resolution reads live state it is *not*
//! reproducible, which is exactly why the result is persisted
//! (`guardian_branches.review_branch_name`) and reused rather than recomputed
//! on later rebuilds.
//!
//! This module is deliberately pure: the "is this name taken" test is a
//! caller-supplied closure, so the git and SQL lookups stay in
//! `guardian_merge`/`guardian` where their errors can be handled.

/// The suffix appended to the source name. Hardcoded for now; a configurable
/// template is the intended follow-up once the naming has been exercised by
/// hand.
pub const REVIEW_SUFFIX: &str = "-review";

/// Longest source-name portion kept before [`REVIEW_SUFFIX`] is appended.
///
/// Branch names have no length limit in git itself, but a loose ref is a file
/// at `.git/refs/heads/<name>`, so an unbounded name can push a deep repo
/// path past Windows' `MAX_PATH`. 48 leaves every real task branch untouched
/// (the longest in this repo's own history is 41) while bounding the
/// pathological case: a review named from a whole sentence.
pub const MAX_SOURCE_CHARS: usize = 48;

/// Ceiling on how far [`resolve_unique`] will walk the `-2`, `-3`, ...
/// sequence before giving up. Reaching it means something is generating names
/// mechanically; failing loudly beats spinning.
pub const MAX_SUFFIX_ATTEMPTS: usize = 200;

/// The review-branch base name for a task branch: the branch's own name with
/// [`REVIEW_SUFFIX`] appended.
///
/// A task branch is already a valid ref name, so it is kept verbatim --
/// case and any `feature/` path segments included -- rather than slugified;
/// `RAL-121-fix` reads better as `RAL-121-fix-review` than as
/// `ral-121-fix-review`. Only the length cap and the ref-validity repair in
/// [`sanitize`] can alter it.
#[must_use]
pub fn base_from_task_branch(branch: &str) -> String {
    format!("{}{REVIEW_SUFFIX}", sanitize(&truncate(branch)))
}

/// The review-branch base name for a combined-worktree review, derived from
/// the review's own free-text `name`.
///
/// Unlike a task branch this is arbitrary user text ("RAL-239 squad/cell
/// rename"), so it is slugified to lowercase `[a-z0-9._-]` first. The result
/// is persisted and never recomputed, so renaming the review afterwards does
/// not move the branch.
#[must_use]
pub fn base_from_review_name(name: &str) -> String {
    let slug = truncate(&slugify(name));
    let slug = sanitize(&slug);
    if slug.is_empty() {
        // Nothing usable in the review's name at all. `review-review` would
        // read as a mistake, so the bare word stands in; the collision
        // suffixer keeps two such reviews apart.
        "review".to_string()
    } else {
        format!("{slug}{REVIEW_SUFFIX}")
    }
}

/// The `n`-th candidate for `base`: `n == 0` is `base` itself, `n == 1` is
/// `<base>-2`, and so on -- the human-facing numbering starts at 2 because
/// the unsuffixed name is conceptually the first.
#[must_use]
pub fn candidate(base: &str, n: usize) -> String {
    if n == 0 {
        base.to_string()
    } else {
        format!("{base}-{}", n + 1)
    }
}

/// The first [`candidate`] of `base` that `is_taken` rejects.
///
/// Returns `None` only after [`MAX_SUFFIX_ATTEMPTS`] consecutive collisions,
/// which callers should surface as a merge failure rather than papering over
/// -- silently reusing a taken name would force-push over somebody else's
/// branch.
pub fn resolve_unique(base: &str, mut is_taken: impl FnMut(&str) -> bool) -> Option<String> {
    (0..MAX_SUFFIX_ATTEMPTS)
        .map(|n| candidate(base, n))
        .find(|name| !is_taken(name))
}

/// Lowercase `name` and reduce it to `[a-z0-9._-]`, collapsing every run of
/// replaced characters into a single `-`.
fn slugify(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if matches!(c, '.' | '_' | '-') {
            out.push(c);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    out
}

/// Keep at most [`MAX_SOURCE_CHARS`] characters, cutting on a character
/// boundary.
fn truncate(name: &str) -> String {
    match name.char_indices().nth(MAX_SOURCE_CHARS) {
        Some((byte_idx, _)) => name[..byte_idx].to_string(),
        None => name.to_string(),
    }
}

/// Repair the sequences `git check-ref-format` rejects.
///
/// A task branch arrives already valid, but [`truncate`] can cut it into an
/// invalid shape (a trailing `/` or `.`, a severed `@{`), and a slugified
/// review name can contain `..` or start with `-`. Everything unrepresentable
/// is dropped rather than substituted, so the result stays readable.
fn sanitize(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for c in name.chars() {
        match c {
            // Rejected outright by `git check-ref-format`.
            '~' | '^' | ':' | '?' | '*' | '[' | '\\' | ' ' => out.push('-'),
            c if c.is_control() => out.push('-'),
            // `@{` is a reflog selector; `..` is a range operator.
            '{' if out.ends_with('@') => out.push('-'),
            '.' if out.ends_with('.') => {}
            // No empty path components, and no leading `/`.
            '/' if out.is_empty() || out.ends_with('/') => {}
            c => out.push(c),
        }
    }
    // A ref may not begin with `-` or `.`, nor end with `/`, `.` or `.lock`.
    // A trailing `-` is legal but would double up against `REVIEW_SUFFIX`, so
    // it goes too.
    let trimmed = out
        .trim_start_matches(['-', '.'])
        .trim_end_matches(['/', '.', '-']);
    let trimmed = trimmed.strip_suffix(".lock").unwrap_or(trimmed);
    trimmed.trim_end_matches(['/', '.', '-']).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_branch_keeps_its_case_and_path_segments() {
        assert_eq!(
            base_from_task_branch("RAL-121-fix-scheduler"),
            "RAL-121-fix-scheduler-review"
        );
        assert_eq!(
            base_from_task_branch("feature/nested/thing"),
            "feature/nested/thing-review"
        );
    }

    #[test]
    fn task_branch_longer_than_the_cap_is_truncated_to_a_valid_ref() {
        let long = "a".repeat(MAX_SOURCE_CHARS + 30);
        let out = base_from_task_branch(&long);
        assert_eq!(
            out,
            format!("{}{REVIEW_SUFFIX}", "a".repeat(MAX_SOURCE_CHARS))
        );
    }

    #[test]
    fn truncating_never_leaves_a_trailing_slash_or_dot() {
        // Cut lands exactly on the separator.
        let name = format!("{}/tail", "b".repeat(MAX_SOURCE_CHARS));
        assert_eq!(
            base_from_task_branch(&name),
            format!("{}{REVIEW_SUFFIX}", "b".repeat(MAX_SOURCE_CHARS))
        );
        let dotted = format!("{}.tail", "c".repeat(MAX_SOURCE_CHARS));
        assert!(!base_from_task_branch(&dotted).contains("."));
    }

    #[test]
    fn review_name_is_slugified() {
        assert_eq!(
            base_from_review_name("RAL-239 squad/cell/proof rename"),
            "ral-239-squad-cell-proof-rename-review"
        );
        assert_eq!(
            base_from_review_name("  Fix   the  thing  "),
            "fix-the-thing-review"
        );
    }

    #[test]
    fn review_name_with_nothing_usable_falls_back() {
        assert_eq!(base_from_review_name(""), "review");
        assert_eq!(base_from_review_name("???"), "review");
    }

    #[test]
    fn sanitize_rejects_every_shape_git_does() {
        assert_eq!(sanitize("a..b"), "a.b");
        assert_eq!(sanitize("a@{1}"), "a@-1}");
        assert_eq!(sanitize("-lead"), "lead");
        assert_eq!(sanitize("trail/"), "trail");
        assert_eq!(sanitize("trail."), "trail");
        assert_eq!(sanitize("x.lock"), "x");
        assert_eq!(sanitize("a//b"), "a/b");
        assert_eq!(sanitize("/lead"), "lead");
        assert_eq!(sanitize("a b"), "a-b");
        assert_eq!(sanitize("a~b^c:d?e*f[g"), "a-b-c-d-e-f-g");
    }

    #[test]
    fn candidate_numbering_starts_at_two() {
        assert_eq!(candidate("x-review", 0), "x-review");
        assert_eq!(candidate("x-review", 1), "x-review-2");
        assert_eq!(candidate("x-review", 2), "x-review-3");
    }

    #[test]
    fn resolve_unique_walks_past_taken_names() {
        let taken = ["x-review", "x-review-2"];
        assert_eq!(
            resolve_unique("x-review", |n| taken.contains(&n)),
            Some("x-review-3".to_string())
        );
    }

    #[test]
    fn resolve_unique_gives_up_rather_than_reusing_a_taken_name() {
        assert_eq!(resolve_unique("x-review", |_| true), None);
    }
}
