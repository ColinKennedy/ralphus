//! Concurrency-safe `git stash` helpers (RAL-283).
//!
//! Git has no per-worktree stash list — every linked worktree of a
//! repository shares one physical `.git` dir and therefore one `refs/stash`
//! ref/reflog. A bare `git stash push` / `git stash pop` pair is only safe if
//! nothing else can stash concurrently, which does not hold here: a guardian
//! applying feedback to one review branch and the scheduler rebasing another
//! cell's worktree can both stash in different worktrees of the same repo
//! around the same time, and a bare pop restores whatever is on top of the
//! shared stack — not necessarily the caller's own stash.
//!
//! The fix is a uniquified, named stash at every call site: push with
//! `git stash push -m <name>`, remember `<name>` for the duration of that
//! stash/restore window, and resolve the pop by listing the stash and
//! matching the message by exact string equality — never by popping
//! top-of-stack and never via `git stash pop stash^{/<pattern>}}`, whose
//! `<pattern>` is a POSIX extended regex against reflog messages (worktree
//! branch names routinely contain regex metacharacters like `.`/`+`/`[`).

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Build a stash name unique to this push, of the form `<scope>/<purpose>-<uniq>`.
///
/// `scope` identifies who/what is stashing (e.g. `guardian/<id>/<worktree-branch>`
/// or a bare worktree branch name); `purpose` is a fixed, call-site-specific
/// string (e.g. `feedback`, `rebase`). The trailing counter is what actually
/// guarantees no collision — the rest is there so a human reading
/// `git stash list` can tell stashes apart.
#[must_use]
pub(crate) fn unique_stash_name(scope: &str, purpose: &str) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    format!("{scope}/{purpose}-{nanos:x}-{n:x}")
}

/// Pop the exact stash previously pushed as `git stash push -m <name>`
/// (`name` from [`unique_stash_name`]), never the top of the stack.
///
/// `run` executes a `git` command (e.g. `crate::guardian_merge::git(root, args)`
/// or [`crate::workspace::Workspace::git`]) — injected so this works uniformly
/// for both local and remote-machine worktrees.
///
/// Resolves the slot via `git stash list --format=%gd %gs` and a plain Rust
/// string match against the reflog subject (git writes it as
/// `On <branch>: <name>`, so a slot matches when its subject either equals
/// `name` or ends with `": "` + `name`) rather than `stash^{/pattern}}`.
/// Fails loudly — rather than silently taking the first hit — if zero or
/// more than one stash matches `name`.
///
/// # Errors
/// If the stash list cannot be read, no stash matches `name`, more than one
/// stash matches `name`, or the `git stash pop` itself fails.
pub(crate) fn pop_named(
    run: impl Fn(&[&str]) -> Result<String, String>,
    name: &str,
) -> Result<(), String> {
    let list = run(&["stash", "list", "--format=%gd %gs"])?;
    let suffix = format!(": {name}");
    let matches: Vec<&str> = list
        .lines()
        .filter_map(|line| {
            let (slot, subject) = line.split_once(' ')?;
            // `On <branch>: <name>` — matched by suffix rather than by
            // splitting on the first `": "`, since a branch name may itself
            // contain a colon-space.
            (subject == name || subject.ends_with(&suffix)).then_some(slot)
        })
        .collect();
    match matches.as_slice() {
        [slot] => run(&["stash", "pop", slot]).map(|_| ()),
        [] => Err(format!("no stash found matching {name:?}")),
        _ => Err(format!(
            "found {} stashes matching {name:?}, expected 1",
            matches.len()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_unique_across_different_scopes_and_purposes() {
        let a = unique_stash_name("guardian/g1/wt-a", "feedback");
        let b = unique_stash_name("guardian/g2/wt-b", "feedback");
        let c = unique_stash_name("guardian/g1/wt-a", "rebase");
        assert_ne!(a, b);
        assert_ne!(a, c);
        assert_ne!(b, c);
    }

    #[test]
    fn the_same_call_site_called_twice_produces_two_distinct_names() {
        let first = unique_stash_name("wt-a", "rebase");
        let second = unique_stash_name("wt-a", "rebase");
        assert_ne!(first, second);
    }

    #[test]
    fn names_embed_the_scope_and_purpose_for_human_debugging() {
        let name = unique_stash_name("guardian/g1/wt-a", "feedback");
        assert!(name.starts_with("guardian/g1/wt-a/feedback-"), "{name}");
    }

    #[test]
    fn pop_named_resolves_the_exact_matching_slot_not_top_of_stack() {
        let calls = std::cell::RefCell::new(Vec::new());
        let run = |args: &[&str]| -> Result<String, String> {
            calls.borrow_mut().push(args.to_vec().join(" "));
            if args == ["stash", "list", "--format=%gd %gs"] {
                Ok("stash@{0} On wt-b: other/scope/rebase-1-1\n\
                    stash@{1} On wt-a: guardian/g1/wt-a/feedback-2-2"
                    .to_string())
            } else if args == ["stash", "pop", "stash@{1}"] {
                Ok(String::new())
            } else {
                panic!("unexpected git call: {args:?}")
            }
        };
        pop_named(run, "guardian/g1/wt-a/feedback-2-2").expect("pop the right slot");
    }

    #[test]
    fn pop_named_matches_when_the_branch_name_contains_a_colon() {
        let run = |args: &[&str]| -> Result<String, String> {
            if args == ["stash", "list", "--format=%gd %gs"] {
                Ok("stash@{0} On feat: odd/name: feat: odd/name/rebase-3-3".to_string())
            } else if args == ["stash", "pop", "stash@{0}"] {
                Ok(String::new())
            } else {
                panic!("unexpected git call: {args:?}")
            }
        };
        pop_named(run, "feat: odd/name/rebase-3-3").expect("pop despite the colon in the branch");
    }

    #[test]
    fn pop_named_fails_loudly_on_zero_matches() {
        let run = |args: &[&str]| -> Result<String, String> {
            if args == ["stash", "list", "--format=%gd %gs"] {
                Ok("stash@{0} On wt-b: something-else".to_string())
            } else {
                panic!("unexpected git call: {args:?}")
            }
        };
        let err = pop_named(run, "guardian/g1/wt-a/feedback-2-2").unwrap_err();
        assert!(err.contains("no stash found"), "{err}");
    }

    #[test]
    fn pop_named_fails_loudly_on_more_than_one_match() {
        let run = |args: &[&str]| -> Result<String, String> {
            if args == ["stash", "list", "--format=%gd %gs"] {
                Ok("stash@{0} On wt-a: dup-name\nstash@{1} On wt-a: dup-name".to_string())
            } else {
                panic!("unexpected git call: {args:?}")
            }
        };
        let err = pop_named(run, "dup-name").unwrap_err();
        assert!(err.contains("found 2 stashes"), "{err}");
    }
}
