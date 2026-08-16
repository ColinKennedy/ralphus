//! Guardian merge engine: build a linear review branch from the collected
//! feature branches by rebasing each one's own commits onto a growing stack in a
//! dedicated worktree.
//!
//! Each review branch is created at its feature's tip and rebased (`git rebase
//! --onto <prev> <base_sha> <rev>`) onto the previous branch, so the feature
//! branches themselves stay untouched. Rebasing — not a range cherry-pick — is
//! deliberate: commits already present on the stack (a shared or already-merged
//! commit) are dropped via patch-id instead of halting and silently collapsing a
//! branch to a no-op. The whole stack builds against ONE snapshotted base commit
//! (`base_sha`), recorded so a later shift in the base branch is detected and the
//! review auto-rebuilt (see [`review_maintenance`] / [`rebuild_on_base_shift`]).
//! When a branch conflicts, a `Runner` (the same agent runner the scheduler uses)
//! is asked to edit the conflicted files marker-free, after which the rebase
//! continues; a branch that still cannot be resolved marks the guardian
//! `MergeFailed`.
//!
//! On a *rebuild* (the base branch shifted), the previous build's resolved review
//! commits are carried forward: rather than re-deriving each branch from its
//! feature tip, the prior resolved commit is replayed onto the new base
//! (`git rebase --onto <new_prev> <old_upstream> <old_resolved>`). The replay
//! conflicts only on genuine new base deltas, so a conflict resolved on an earlier
//! build is not resolved again — no `git rerere` required. The feature branches
//! stay untouched throughout.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde::Deserialize;

use crate::guardian::{CheckInput, GuardianCheck, GuardianStatus, MergeStatus};
use crate::runner::{Runner, RunnerSpec};
use crate::scheduler::Semaphore;
use crate::server::Reply;
use crate::store::Store;
use crate::workspace::Workspace;

/// The `RunnerSpec.task` value used for every conflict-resolver invocation
/// (RAL-102). `server.rs`'s guardian-branch terminal/pane endpoints must pass
/// this exact string into `crate::tmux::session_name` to recompute the tmux
/// session name the resolver actually runs under — kept as a shared constant
/// rather than a literal duplicated in both files so the two can never drift.
pub(crate) const RESOLVER_TASK: &str = "resolve";

/// The `RunnerSpec.task` value used for the dedicated final-verification
/// invocation that follows a branch's fix pass (RAL-149) — a distinct LLM
/// call from [`RESOLVER_TASK`] so the indicator on the board reflects a real,
/// separate step rather than something implicitly bundled into the fix call.
pub(crate) const RESOLVER_VERIFY_TASK: &str = "resolve-verify";

/// The `RunnerSpec.task`/`session_id` values used for every manual-checks
/// generation invocation (RAL-88 follow-up). Mirrors [`RESOLVER_TASK`]'s
/// rationale: `server.rs`'s manual-checks terminal/pane endpoints must pass
/// these exact strings into `crate::tmux::session_name` to recompute the tmux
/// session name `generate_manual_commands` actually runs under.
pub(crate) const MANUAL_COMMANDS_TASK: &str = "manual_commands";
pub(crate) const MANUAL_COMMANDS_SESSION: &str = "manual-reviewer";
/// Task name for "set it for me" input resolution (RAL-164) -- see
/// [`resolve_check_input`].
pub(crate) const RESOLVE_INPUT_TASK: &str = "resolve_input";

// ---------------------------------------------------------------------------
// XML route-block helpers (RAL-35)
// ---------------------------------------------------------------------------

/// Parse `<route branch="BRANCH">...instructions...</route>` blocks from text.
/// Returns `(branch_name, instructions)` pairs in document order.
fn parse_route_blocks(text: &str) -> Vec<(String, String)> {
    let mut routes = Vec::new();
    let mut pos = 0;
    let close = "</route>";
    while let Some(tag_start) = text[pos..].find("<route ").map(|i| pos + i) {
        let Some(tag_end) = text[tag_start..].find('>').map(|i| tag_start + i) else {
            break;
        };
        let tag = &text[tag_start..=tag_end];
        let Some(branch) = extract_xml_attr(tag, "branch") else {
            pos = tag_end + 1;
            continue;
        };
        let content_start = tag_end + 1;
        let Some(close_offset) = text[content_start..].find(close) else {
            break;
        };
        let instructions = text[content_start..content_start + close_offset]
            .trim()
            .to_string();
        routes.push((branch, instructions));
        pos = content_start + close_offset + close.len();
    }
    routes
}

/// Extract the value of an XML attribute (`name="value"`) from a tag string.
fn extract_xml_attr(tag: &str, name: &str) -> Option<String> {
    let needle = format!("{name}=\"");
    let start = tag.find(needle.as_str())? + needle.len();
    let end = tag[start..].find('"')? + start;
    Some(tag[start..end].to_string())
}

/// Remove all `<route ...>...</route>` blocks from text; return the remainder,
/// trimmed. The resulting string is what is shown to the reviewer.
fn strip_route_blocks(text: &str) -> String {
    let mut result = String::new();
    let mut pos = 0;
    let close = "</route>";
    loop {
        let Some(tag_start) = text[pos..].find("<route ").map(|i| pos + i) else {
            result.push_str(&text[pos..]);
            break;
        };
        result.push_str(&text[pos..tag_start]);
        let Some(close_offset) = text[tag_start..].find(close).map(|i| tag_start + i) else {
            result.push_str(&text[tag_start..]);
            break;
        };
        pos = close_offset + close.len();
    }
    result.trim().to_string()
}

/// Return `true` when the user message expresses intent to skip committing.
///
/// Matches phrases like "don't commit", "do not commit", "don't add", and
/// "don't change git history" case-insensitively. When true, the Guardian
/// applies file edits to the working tree but does NOT run `git add`/`git
/// commit` — the changes remain as staged or unstaged working-tree edits.
fn is_no_commit_intent(text: &str) -> bool {
    let lower = text.to_lowercase();
    let phrases = [
        "don't commit",
        "do not commit",
        "don't add",
        "do not add",
        "don't change git",
        "do not change git",
        "without committing",
        "without a commit",
        "no commit",
        "skip commit",
        "don't stage",
        "do not stage",
    ];
    phrases.iter().any(|p| lower.contains(p))
}

/// Get the current HEAD commit hash in a worktree. Returns `None` if git fails.
fn head_hash(wt: &Workspace) -> Option<String> {
    wt.git(&["rev-parse", "HEAD"])
        .ok()
        .map(|s| s.trim().to_string())
}

/// Marker the conflict-resolver agent outputs after `git add -A` to signal the
/// orchestrator that the index is ready for `git rebase --continue`.
const STAGE_DONE_MARKER: &str = "RALPHUS_STAGE: DONE";

/// Run `git` with `args` in `root`, returning stdout on success or a message.
/// `GIT_EDITOR=true` keeps operations like `rebase --continue` from opening an
/// interactive editor.
pub(crate) fn git(root: &Path, args: &[&str]) -> std::result::Result<String, String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(root)
        .env("GIT_EDITOR", "true")
        .env("GIT_SEQUENCE_EDITOR", "true")
        .output()
        .map_err(|e| format!("could not run git: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

/// Re-register a worktree directory whose git tracking entry was removed (e.g.,
/// by `git worktree remove --force` when the directory could not be deleted
/// because it is another process's CWD on Windows). Recreates
/// `.git/worktrees/<name>/` and updates `<wt>/.git` so that `git -C wt`
/// commands work again.
///
/// `branch` is used to write a placeholder `HEAD` when the entry is being
/// created from scratch (missing `HEAD` → git refuses to open the gitdir).
fn relink_worktree(root: &Workspace, wt: &Workspace, branch: &str) -> Result<(), String> {
    let name = wt
        .root()
        .file_name()
        .ok_or_else(|| "worktree path has no filename component".to_string())?
        .to_string_lossy()
        .into_owned();
    // Path arithmetic only -- these never touch this host's disk when the
    // workspace is remote; the writes below go through the workspace.
    let entry_dir = root.root().join(".git").join("worktrees").join(&name);
    let wt_git_str = wt.root().join(".git").to_string_lossy().replace('\\', "/");
    root.write_file(
        entry_dir.join("gitdir"),
        &format!(
            "{wt_git_str}
"
        ),
    )?;
    root.write_file(
        entry_dir.join("commondir"),
        "../..
",
    )?;
    // Without HEAD, git refuses to open the gitdir ("not a git repository").
    // Only write the placeholder when HEAD is absent -- if the admin entry
    // already has one (partial-remove scenario), keep it untouched.
    let head_path = entry_dir.join("HEAD");
    if !root.exists(&head_path) {
        root.write_file(
            &head_path,
            &format!(
                "ref: refs/heads/{branch}
"
            ),
        )?;
    }
    let entry_str = entry_dir.to_string_lossy().replace('\\', "/");
    wt.write_file(
        wt.root().join(".git"),
        &format!(
            "gitdir: {entry_str}
"
        ),
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Worktree fail-state detection helpers
// ---------------------------------------------------------------------------

/// Whether `wt` looks like a valid *linked* git worktree: the directory must
/// contain a `.git` *file* (not a directory — directories mean a main repo)
/// whose content begins with `gitdir:`.
fn is_valid_linked_worktree(wt: &Workspace) -> bool {
    // A `.git` *file* (not a directory -- directories mean a main repo) whose
    // content begins with `gitdir:`. Reading it answers both questions at once,
    // and costs one round trip rather than a stat plus a read.
    wt.read_file(".git")
        .is_some_and(|s| s.trim_start().starts_with("gitdir:"))
}

/// Whether `rev` resolves to a commit in `root`. Accepts branch names, tag
/// names, raw SHAs, and any other form understood by `git rev-parse --verify`.
fn branch_exists(root: &Workspace, rev: &str) -> bool {
    root.git(&["rev-parse", "--verify", rev]).is_ok()
}

/// Whether `ancestor` is an ancestor of (or equal to) `descendant` in `root`.
/// `git merge-base --is-ancestor` exits 0 when true, so `git()` returns `Ok`
/// only in that case. Used to validate a carry-forward chain before replaying a
/// prior resolved review branch onto a shifted base.
fn is_ancestor(root: &Workspace, ancestor: &str, descendant: &str) -> bool {
    root.git(&["merge-base", "--is-ancestor", ancestor, descendant])
        .is_ok()
}

/// Delete every carry-forward protection ref (`refs/ralphus/carry/<id>/*`) in
/// `root`. Clears leftovers from a merge that was killed before its [`CarryRefs`]
/// guard ran, and runs when a guardian is deleted. Matching is path-component
/// exact, so `<id>` never captures another guardian whose id shares this prefix.
fn purge_carry_refs(root: &Workspace, id: &str) {
    let listed = git(
        root.root(),
        &[
            "for-each-ref",
            "--format=%(refname)",
            &format!("refs/ralphus/carry/{id}"),
        ],
    )
    .unwrap_or_default();
    for name in listed.lines().map(str::trim).filter(|s| !s.is_empty()) {
        let _ = root.git(&["update-ref", "-d", name]);
    }
}

/// RAII holder for carry-forward protection refs. Each pinned ref
/// (`refs/ralphus/carry/<id>/…`) keeps a commit the rebuild intends to replay
/// reachable — created BEFORE cleanup deletes the `guardian/<id>/*` branches, so
/// the commit is never dangling (and so never at risk from a concurrent `git gc`)
/// in the window between cleanup and re-checkout. Dropping the guard deletes every
/// ref it created, covering normal completion, early return, and panic-unwind, so
/// a build never leaks protection refs.
struct CarryRefs {
    pins: Vec<(PathBuf, String)>,
}

impl CarryRefs {
    fn new() -> Self {
        Self { pins: Vec::new() }
    }

    /// Pin `sha` under `refs/ralphus/carry/<id>/<slug>` in `root`. Best-effort:
    /// on failure the commit simply falls back to bare-SHA reachability (still
    /// valid until gc), so pinning never blocks a merge.
    fn pin(&mut self, root: &Path, id: &str, slug: &str, sha: &str) {
        let name = format!("refs/ralphus/carry/{id}/{slug}");
        if git(root, &["update-ref", &name, sha]).is_ok() {
            self.pins.push((root.to_path_buf(), name));
        }
    }
}

impl Drop for CarryRefs {
    fn drop(&mut self) {
        for (root, name) in &self.pins {
            let _ = git(root, &["update-ref", "-d", name]);
        }
    }
}

/// Whether the working tree in `wt` has any modification (staged, unstaged, or
/// untracked files). Returns `false` when `git status` fails.
fn worktree_has_changes(wt: &Workspace) -> bool {
    wt.git(&["status", "--porcelain"])
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false)
}

/// Find the worktree in `root`'s repo that is currently checked out on `branch`.
/// Parses `git worktree list --porcelain` and returns the first matching path,
/// or `None` when no worktree has that branch.
fn find_worktree_for_branch(root: &Workspace, branch: &str) -> Option<PathBuf> {
    let list = root.git(&["worktree", "list", "--porcelain"]).ok()?;
    let mut cur_path: Option<PathBuf> = None;
    for line in list.lines() {
        if let Some(path) = line.strip_prefix("worktree ") {
            cur_path = Some(PathBuf::from(path.trim()));
        } else if let Some(b) = line.strip_prefix("branch refs/heads/") {
            if b.trim() == branch {
                return cur_path;
            }
        }
    }
    None
}

/// Regenerate a review worktree when the feature branch `branch` is absent from
/// git. Locates an existing worktree checked out on `branch` (via
/// `git worktree list`), creates branch `rev` at its HEAD, and adds a worktree
/// at `wt`. Logs a warning that any prior review-branch commits are lost.
fn regen_from_feature_worktree(
    root: &Workspace,
    rev: &str,
    wt: &Workspace,
    branch: &str,
) -> std::result::Result<(), String> {
    let wt_str = wt.root().to_string_lossy().to_string();
    match find_worktree_for_branch(root, branch) {
        Some(feature_wt) => {
            let sha = git(&feature_wt, &["rev-parse", "HEAD"])
                .map(|s| s.trim().to_string())
                .map_err(|e| {
                    format!(
                        "cannot resolve HEAD of feature worktree at {}: {e}",
                        feature_wt.display()
                    )
                })?;
            crate::rlog!(
                WARNING,
                "ralphus [guardian] worktree recovery: feature branch '{branch}' is absent; \
                 creating '{rev}' from feature-worktree HEAD {sha} — prior review commits lost"
            );
            let _ = root.git(&["branch", "-D", rev]);
            root.git(&["branch", rev, &sha])
                .map_err(|e| format!("cannot create branch '{rev}' at {sha}: {e}"))?;
            root.git(&["worktree", "add", "--force", &wt_str, rev])
                .map(|_| ())
                .map_err(|e| format!("cannot add worktree at {wt_str}: {e}"))
        }
        None => Err(format!(
            "feature branch '{branch}' is absent from git and no worktree is checked \
             out on it; cannot recover review worktree at {wt_str}"
        )),
    }
}

// ---------------------------------------------------------------------------
// Worktree setup with exhaustive fail-state recovery
// ---------------------------------------------------------------------------

/// Set up `wt` as a worktree for `branch` under the review-branch name `rev`.
/// Equivalent to `git worktree add -B <rev> <wt> <branch>` on a clean slate,
/// but detects and recovers from all known fail-states before the add.
///
/// RAL-171: when `wt` is already a healthy linked worktree, this takes a fast
/// path — relink (idempotent) + `checkout -f -B rev branch` in place — instead
/// of the destructive remove/prune/re-add sequence below, which forces a full
/// re-checkout of every tracked file. That destructive sequence still runs,
/// unchanged, as a fallback whenever the fast path doesn't apply or fails, so
/// every fail-state below is still recovered exactly as documented:
///
/// 1. **Directory missing entirely** — creates a fresh worktree.
/// 2. **Directory not a git worktree** — `.git` file absent/malformed or a
///    directory (main-repo); relinks if the review branch survives, wipes and
///    regenerates otherwise.
/// 3. **Review branch missing** — `checkout -B` recreates it from `branch`.
/// 4. **Stale git tracking entry** — `git worktree prune` removes it so
///    `worktree add` does not reject the path as already registered.
/// 5. **Branch mismatch** — `checkout -f -B rev branch` corrects it.
/// 6. **Detached HEAD** — `checkout` reattaches to a named branch.
/// 7. **Worktree locked** — unlocked before removal.
///
/// When the feature branch `branch` is also gone, recovers from a worktree
/// still checked out on `branch` (found via `git worktree list`).
fn worktree_add_or_reset(
    root: &Workspace,
    rev: &str,
    wt: &Workspace,
    branch: &str,
) -> std::result::Result<(), String> {
    let wt_str = wt.root().to_string_lossy().to_string();

    if !wt.root().exists() {
        // [State 1] Directory missing — fast path.
        // Remove any stale tracking entry for this path (quick no-op when not
        // registered). This handles [State 4] when git's remove succeeds on a
        // ghost entry; if it does not, the lazy prune below is the fallback.
        let _ = root.git(&["worktree", "remove", "-f", &wt_str]);

        if branch_exists(root, branch) {
            // [State 4] If a stale tracking entry blocks the add, prune and retry.
            return root
                .git(&["worktree", "add", "-B", rev, &wt_str, branch])
                .or_else(|_| {
                    let _ = root.git(&["worktree", "prune"]);
                    root.git(&["worktree", "add", "-B", rev, &wt_str, branch])
                })
                .map(|_| ());
        }
        // Feature branch is also absent — find its worktree and regenerate.
        let _ = root.git(&["worktree", "prune"]);
        return regen_from_feature_worktree(root, rev, wt, branch);
    }

    // Directory survived (Windows CWD lock or external process).

    // Fast path (RAL-171): if the directory is already a healthy linked
    // worktree, reset it in place instead of falling through to the
    // destructive unlock/remove/prune/re-add sequence below. That sequence
    // deletes the whole worktree and repopulates it via `git worktree add`,
    // which re-checks-out every tracked file -- on a "Merge / rebase" restart
    // (by far the common case: the worktree from the last build is still
    // fine) this dominated the delay between the button press and any
    // visible progress, for no benefit over a plain `checkout -f -B`, which
    // only touches files that actually differ. Any failure here (relink,
    // rebase abort, or checkout) falls through to the full recovery path
    // unchanged, so every fail-state this function is documented to recover
    // from is still handled -- this is purely a happy-path shortcut.
    if is_valid_linked_worktree(wt) && relink_worktree(root, wt, branch).is_ok() {
        // [State 7] Unlock if locked. No-op when not locked; cheap either way,
        // unlike the removal sequence below.
        let _ = root.git(&["worktree", "unlock", &wt_str]);
        let _ = wt.git(&["rebase", "--abort"]);
        if worktree_has_changes(wt) {
            crate::rlog!(
                WARNING,
                "ralphus [guardian] worktree recovery: discarding uncommitted changes in {wt_str} \
                 before resetting to '{branch}'"
            );
        }
        if wt.git(&["checkout", "-f", "-B", rev, branch]).is_ok() {
            return Ok(());
        }
    }

    // [State 7] Unlock if locked. No-op when not locked.
    let _ = root.git(&["worktree", "unlock", &wt_str]);

    // Remove any existing tracking entry for this path.
    // One `-f` handles dirty/untracked files; a second `-f` handles locked
    // worktrees (belt-and-suspenders after the explicit unlock above).
    let _ = root.git(&["worktree", "remove", "-f", &wt_str]);
    let _ = root.git(&["worktree", "remove", "-f", "-f", &wt_str]);

    // [State 4] Prune stale entries where a tracking path no longer exists on
    // disk, so that `git worktree add` does not reject the path as registered.
    let _ = root.git(&["worktree", "prune"]);

    if !wt.root().exists() {
        // Removal succeeded; add fresh.
        if branch_exists(root, branch) {
            return root
                .git(&["worktree", "add", "-B", rev, &wt_str, branch])
                .map(|_| ());
        }
        return regen_from_feature_worktree(root, rev, wt, branch);
    }

    // [State 2] Inspect the `.git` entry to decide how to repair.
    // `is_valid_linked_worktree` is true only when `.git` is a file starting
    // with `gitdir:`. If it's a directory the path contains a main git repo
    // rather than a linked worktree — wipe and recreate. Otherwise the `.git`
    // file is absent, malformed, or stale; `relink_worktree` will fix it.
    if !is_valid_linked_worktree(wt) && wt.exists(".git") {
        crate::rlog!(
            WARNING,
            "ralphus [guardian] worktree recovery: directory at {wt_str} is a main git \
             repository; its contents will be discarded"
        );
        wt.remove_path(".", true);
        return if branch_exists(root, branch) {
            root.git(&["worktree", "add", "-B", rev, &wt_str, branch])
                .map(|_| ())
        } else {
            regen_from_feature_worktree(root, rev, wt, branch)
        };
    }

    // (Re)register the worktree tracking entry so git commands work inside
    // `wt`. Idempotent for already-valid worktrees — rewrites the same
    // gitdir/commondir pointers and fills in a missing HEAD placeholder.
    relink_worktree(root, wt, branch)
        .map_err(|e| format!("cannot relink worktree at {wt_str}: {e}"))?;

    // Abort any rebase that was left in flight.
    let _ = wt.git(&["rebase", "--abort"]);

    // Warn before discarding local changes.
    if worktree_has_changes(wt) {
        crate::rlog!(
            WARNING,
            "ralphus [guardian] worktree recovery: discarding uncommitted changes in {wt_str} \
             before resetting to '{branch}'"
        );
    }

    // [State 3] Review branch missing: `-B` creates it.
    // [State 5] Branch mismatch: `-B` resets to the correct starting point.
    // [State 6] Detached HEAD: `checkout` reattaches to a named branch.
    // `-f` discards local modifications.
    match wt.git(&["checkout", "-f", "-B", rev, branch]) {
        Ok(_) => Ok(()),
        Err(checkout_err) => {
            // Feature branch may be absent; wipe and regenerate.
            wt.remove_path(".", true);
            if branch_exists(root, branch) {
                root.git(&["worktree", "add", "-B", rev, &wt_str, branch])
                    .map(|_| ())
                    .map_err(|e| format!("{checkout_err}; worktree add: {e}"))
            } else {
                regen_from_feature_worktree(root, rev, wt, branch)
                    .map_err(|e| format!("{checkout_err}; regen: {e}"))
            }
        }
    }
}

/// Resolve a path within git's own admin directory for `rel`, via `git
/// rev-parse --git-path`. Linked worktrees (the layout `wt` always is, here)
/// keep most of their per-worktree state under `<main-repo>/.git/worktrees/
/// <name>/` rather than `wt/.git/`, so this must go through git's own
/// resolution rather than assuming a fixed relative path. Returns `None` on
/// git failure; does not check whether the resolved path actually exists.
fn git_path(wt: &Workspace, rel: &str) -> Option<PathBuf> {
    let p = wt.git(&["rev-parse", "--git-path", rel]).ok()?;
    let p = p.trim();
    if p.is_empty() {
        return None;
    }
    let path = Path::new(p);
    Some(if path.is_absolute() {
        path.to_path_buf()
    } else {
        wt.root().join(path)
    })
}

/// Whether a rebase is currently in progress in `wt` (its state directory
/// exists). Used to decide whether `rebase --continue` still has work to do.
///
/// `pub(crate)`: also read by `server.rs`'s live conflicting-files endpoint
/// (RAL-148) to report whether a branch's worktree is mid-rebase.
pub(crate) fn rebase_in_progress(wt: &Workspace) -> bool {
    ["rebase-merge", "rebase-apply"]
        .iter()
        .any(|d| git_path(wt, d).is_some_and(|p| p.exists()))
}

/// Git's own interactive-rebase todo-list progress for an in-progress rebase
/// in `wt`: `(commands done, commands total)`. Reads `rebase-merge/done` and
/// `rebase-merge/git-rebase-todo` directly -- the same files `git status`
/// summarizes as "Last commands done" / "Next commands to do" -- counting
/// non-blank, non-comment lines rather than parsing any command output.
/// Returns `None` when no rebase is in progress, or if the files can't be
/// read (e.g. a race right after `--continue`/`--skip` clears them); callers
/// must treat that as "no progress to report", not an error.
pub fn rebase_command_progress(wt: &Workspace) -> Option<(i64, i64)> {
    let state_dir = git_path(wt, "rebase-merge")?;
    let done = count_command_lines(&state_dir.join("done"))?;
    let remaining = count_command_lines(&state_dir.join("git-rebase-todo")).unwrap_or(0);
    Some((done, done + remaining))
}

/// Count non-blank, non-comment lines in a rebase-todo-style file. `None` if
/// the file can't be read (missing, or a transient race).
fn count_command_lines(path: &Path) -> Option<i64> {
    let content = std::fs::read_to_string(path).ok()?;
    Some(
        content
            .lines()
            .filter(|l| {
                let t = l.trim();
                !t.is_empty() && !t.starts_with('#')
            })
            .count() as i64,
    )
}

/// Files with unresolved merge conflicts in a worktree.
///
/// `pub(crate)`: also called directly by `server.rs`'s live conflicting-files
/// endpoint (RAL-148), which polls this on demand for the board rather than
/// waiting on `resolve_conflicts_with_agent`'s own loop below.
pub(crate) fn conflicted_files(wt: &Workspace) -> Vec<String> {
    wt.git(&["diff", "--name-only", "--diff-filter=U"])
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

/// Count remaining `<<<<<<<` conflict markers across the given files.
fn count_markers(wt: &Workspace, files: &[String]) -> usize {
    files
        .iter()
        .map(|f| {
            wt.read_file(f)
                .map(|c| c.lines().filter(|l| l.starts_with("<<<<<<<")).count())
                .ok_or(())
                .unwrap_or(0)
        })
        .sum()
}

/// Advance a paused rebase with `git rebase --continue`. On failure, only
/// falls back to `--skip` when no conflict is actually present — i.e. the
/// failure was genuinely about the just-applied commit becoming empty (or
/// some other non-conflict error), not because `--continue` immediately ran
/// into the *next* commit's conflict. `--empty=drop` already discards
/// patch-equal commits on its own, so a `--continue` failure with fresh
/// conflicted files present means a new commit needs the resolver loop to
/// pick it up — skipping it here would silently discard that commit's
/// changes instead of ever resolving them.
fn advance_rebase(wt: &Workspace) {
    if wt.git(&["rebase", "--continue"]).is_err() && conflicted_files(wt).is_empty() {
        let _ = wt.git(&["rebase", "--skip"]);
    }
}

/// The agent backend used to resolve conflicts: the review's own `stored` agent
/// (from `[[review]]`), else the `RALPHUS_RESOLVER_AGENT` env
/// override, else `ollama`.
pub(crate) fn resolver_agent(stored: Option<&str>) -> String {
    stored
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
        .or_else(|| std::env::var("RALPHUS_RESOLVER_AGENT").ok())
        .unwrap_or_else(|| "ollama".to_string())
}

/// The model the resolver runs: the review's own `stored` model, else the
/// `RALPHUS_RESOLVER_MODEL` env override, else `qwen3:8b` for the ollama backend.
/// claude, claude-code, and codex each pick their own default when unset → `None`.
pub(crate) fn resolver_model(stored: Option<&str>, agent: &str) -> Option<String> {
    stored
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
        .or_else(|| std::env::var("RALPHUS_RESOLVER_MODEL").ok())
        .or_else(|| match agent {
            "ollama" => Some("qwen3:8b".to_string()),
            _ => None, // codex, claude, claude-code: each picks its own default
        })
}

/// Resolves this review's resolver backend/model (RAL-149/168) -- cheap, one
/// DB read, no LLM call. Split out from [`resolve_conflicts_with_agent`] so
/// [`drive_rebase`]'s clean (no-conflict) path can also resolve it, without
/// paying for the (possibly LLM-backed) quality-bar synthesis unless a verify
/// call actually ends up running -- see [`verify_extras`].
fn resolver_backend(store: &Arc<Mutex<Store>>, id: &str) -> (String, Option<String>) {
    let guard = store.lock().expect("poisoned");
    let g = guard.get_guardian(id).ok();
    let stored_agent = g.as_ref().and_then(|g| g.resolver_agent.clone());
    let stored_model = g.and_then(|g| g.resolver_model.clone());
    let agent = resolver_agent(stored_agent.as_deref());
    let model = resolver_model(stored_model.as_deref(), &agent);
    (agent, model)
}

/// Quality-bar instructions + ghost-memory prefix for a branch's dedicated
/// final-verification call (RAL-149/168). Deliberately lazy: callers compute
/// this only once they've already decided [`run_final_verify`] will actually
/// run for this branch, since `synthesize_verify_instructions` may itself
/// invoke an LLM call -- under RAL-168's "nothing"/"final_branch" scopes (or
/// "each_branch" with auto-clean-skip), most branches never call this at all.
fn verify_extras(
    store: &Arc<Mutex<Store>>,
    id: &str,
    branch: &str,
    branch_id: &str,
    runner: &dyn Runner,
    agent: &str,
    model: &Option<String>,
) -> (String, String) {
    let quality_note = synthesize_verify_instructions(store, id, branch, runner, agent, model);
    let ghost_uri = crate::ghost::review_uri(id, Some(branch_id));
    let ghost_prefix = {
        let guard = store.lock().expect("poisoned");
        guard
            .get_ghost(&ghost_uri)
            .ok()
            .flatten()
            .and_then(|g| crate::ghost::format_context_block(Some(&g), &[]))
            .unwrap_or_default()
    };
    (quality_note, ghost_prefix)
}

/// Resolved "Verify" scope settings for one branch attempt (RAL-168): whether
/// and how often the dedicated LLM-based final-verify call
/// ([`run_final_verify`]) should fire, replacing the old `verify_mid_resolution`
/// flag outright. Computed once per branch from
/// `Guardian::effective_verify_scope`/`effective_verify_skip_auto_clean` (already
/// resolved against the project-level `.ralphus.toml` default) plus whether
/// this is the last branch in the stack.
#[derive(Debug, Clone)]
struct VerifyGate {
    /// `"each_branch"` | `"final_branch"` | `"nothing"`.
    scope: String,
    /// Only meaningful under `"each_branch"`.
    skip_auto_clean: bool,
    /// Whether this branch is the last (by position) enabled branch in the
    /// stack -- the only branch `"final_branch"` scope verifies.
    is_final_branch: bool,
}

impl VerifyGate {
    /// Resolve a guardian's Verify-scope settings for one branch attempt,
    /// combining the guardian-level `effective_verify_scope`/
    /// `effective_verify_skip_auto_clean` with whether this particular
    /// branch is the last one in the stack.
    fn resolve(store: &Arc<Mutex<Store>>, id: &str, is_final_branch: bool) -> Self {
        let guard = store.lock().expect("poisoned");
        let g = guard.get_guardian(id).ok();
        VerifyGate {
            scope: g
                .as_ref()
                .map(|g| g.effective_verify_scope.clone())
                .unwrap_or_else(|| "each_branch".to_string()),
            skip_auto_clean: g
                .as_ref()
                .is_some_and(|g| g.effective_verify_skip_auto_clean),
            is_final_branch,
        }
    }

    /// Whether [`run_final_verify`] should fire for a branch whose conflicts
    /// the agent just resolved. A branch that hit real conflicts is never
    /// "auto-clean" (`skip_auto_clean` is irrelevant here, matching RAL-168's
    /// own Q2 resolution: "if a rebase occurred [with conflicts], ... under
    /// Each branch verification must happen").
    fn allows_after_conflict(&self) -> bool {
        match self.scope.as_str() {
            "nothing" => false,
            "final_branch" => self.is_final_branch,
            _ => true, // "each_branch" (and any unrecognized value, defensively)
        }
    }

    /// Whether [`run_final_verify`] should fire for a branch that rebased
    /// cleanly (no conflict at all) but contributed real changes -- the new
    /// RAL-168 "each_branch" behavior, unless `skip_auto_clean` opts back
    /// into the old lighter-weight default.
    fn allows_for_clean_branch(&self) -> bool {
        match self.scope.as_str() {
            "nothing" => false,
            "final_branch" => self.is_final_branch,
            _ => !self.skip_auto_clean,
        }
    }
}

/// The enabled branch with the highest position -- unambiguous "last branch
/// in the stack" for `VerifyScope::FinalBranch` (RAL-168; see
/// `OrderedBranch`'s doc comment on why position, not list order, is
/// authoritative). `None` for a guardian with no enabled branches.
fn final_branch_id(branches: &[crate::guardian::OrderedBranch]) -> Option<&str> {
    branches
        .iter()
        .filter(|b| b.enabled)
        .max_by_key(|b| b.position)
        .map(|b| b.id.as_str())
}

/// Derives a concise quality-bar instruction for the conflict-resolver agent.
///
/// When the guardian has a task linkage, an LLM synthesises the raw verify steps
/// into a single paragraph — deduplicating retry policies and stripping anything
/// that conflicts with the rebase flow (commit, push, abort). The guardian's
/// explicit check commands (if any) are folded in as additional command-kind
/// inputs so nothing is lost.
///
/// Falls back (without an LLM call) to the existing `checks_note` format when
/// there are explicit checks but no task linkage, or to a static project-discovery
/// prompt when nothing is configured at all.
///
/// Significant decisions are written to the guardian event log so they surface
/// in the Reviews UI under the affected branch.
fn synthesize_verify_instructions(
    store: &Arc<Mutex<Store>>,
    id: &str,
    branch: &str,
    runner: &dyn Runner,
    agent: &str,
    model: &Option<String>,
) -> String {
    let log = |msg: &str| {
        let _ = store.lock().expect("poisoned").log_event(
            None,
            Some(id),
            "guardian",
            Some(branch),
            msg,
        );
    };

    let (skip_worktree_checks, explicit_checks, git_root) = {
        let guard = store.lock().expect("poisoned");
        let skip = guard.guardian_skip_worktree_checks(id).unwrap_or(false);
        let checks = guard.guardian_checks(id).unwrap_or_default();
        let root = guard
            .get_guardian(id)
            .map(|g| g.git_root)
            .unwrap_or_default();
        (skip, checks, root)
    };

    // Opt-out: user has disabled the per-worktree quality-bar prompt (RAL-110).
    if skip_worktree_checks {
        return String::new();
    }

    let (session_verifies, task_verifies) = {
        let guard = store.lock().expect("poisoned");
        match guard.verify_steps_for_review_branch(id, branch) {
            Ok(Some((sv, tv, _))) => (sv, tv),
            _ => (vec![], vec![]),
        }
    };

    let has_task_steps = !session_verifies.is_empty() || !task_verifies.is_empty();
    let has_checks = !explicit_checks.is_empty();

    // No task linkage and no explicit checks → static project-discovery fallback.
    if !has_task_steps && !has_checks {
        log("no task verify steps or check commands found; using project-discovery fallback");
        return " After resolving, verify the code meets project quality standards: \
                check for a CLAUDE.md or AGENTS.md file in the repository root for build, \
                format, lint, and test instructions; run any applicable formatter and linter; \
                ensure the project builds without errors; then stage your changes."
            .to_string();
    }

    // No task linkage but explicit checks exist → keep the original format (no LLM call).
    if !has_task_steps {
        log("no task verify steps; using explicit check commands directly");
        return format!(
            " After resolving, your edits must keep these project checks passing: {}.",
            explicit_checks.join("; ")
        );
    }

    // Build the synthesis prompt from all available inputs.
    log(&format!(
        "synthesizing verify instructions from {} task + {} session verify steps{}",
        task_verifies.len(),
        session_verifies.len(),
        if has_checks {
            format!(" + {} explicit checks", explicit_checks.len())
        } else {
            String::new()
        },
    ));

    let mut lines: Vec<String> = Vec::new();
    if !task_verifies.is_empty() {
        lines.push("Task-level verify steps:".to_string());
        for v in &task_verifies {
            lines.push(format!("  [{}] {}", v.kind, v.spec));
        }
    }
    if !session_verifies.is_empty() {
        lines.push("Session-level verify steps:".to_string());
        for v in &session_verifies {
            lines.push(format!("  [{}] {}", v.kind, v.spec));
        }
    }
    if has_checks {
        lines.push("Additional check commands (from review configuration):".to_string());
        for c in &explicit_checks {
            lines.push(format!("  [command] {c}"));
        }
    }

    const SYNTHESIS_SYSTEM: &str = "\
        You are preparing verification instructions for a git rebase \
        conflict-resolution agent. The agent can run shell commands (formatters, \
        linters, tests) but CANNOT and MUST NOT commit, push, or abort the rebase \
        — the orchestrator handles those steps.\n\n\
        From the verify steps provided, produce a single concise paragraph (under \
        120 words) telling the agent what quality bar it must meet after resolving \
        conflicts. Rules:\n\
        - command-kind step: instruct the agent to run the command and fix any failures\n\
        - prompt-kind step: rephrase as a descriptive criterion (what \"done\" looks like)\n\
        - Deduplicate overlapping retry policies across steps — state each policy once\n\
        - Remove or adapt anything that conflicts with the rebase flow: no commit, \
          no push, no abort, no \"do not stage\", no task-failure side-effects\n\
        Output ONLY the instruction paragraph. No headers, labels, or commentary.";

    let spec = RunnerSpec {
        // RAL-102: run_id/session_id together key the tmux session name
        // (see `crate::tmux::session_name`) — must be unique per guardian so
        // concurrent guardians' agent invocations never collide on the same
        // tmux session.
        run_id: format!("guardian-{id}"),
        task: "verify-synthesis".to_string(),
        session_id: format!("synthesizer-{}", branch.replace(['/', '.'], "-")),
        cwd: git_root,
        prompt: Some(lines.join("\n")),
        command: None,
        agent: agent.to_string(),
        model: model.clone(),
        system_prompt: Some(SYNTHESIS_SYSTEM.to_string()),
        system_prompt_position: None,
        timeout_sec: Some(120),
        budget_tokens: Some(1000),
        maximum_budget_usd: None,
        verify: false,
        trace_context: None,
        resume_agent_session_id: None,
        env_overrides: std::collections::BTreeMap::new(),
        machine: None,
    };
    let result = runner.run(&spec);

    if result.is_done() && !result.summary.trim().is_empty() {
        let synthesized = result.summary.trim().to_string();
        log(&format!(
            "verify synthesis complete ({} chars, in={} out={} tokens)",
            synthesized.len(),
            result.tokens_in,
            result.tokens_out,
        ));
        return format!(" After resolving and staging, meet this quality bar: {synthesized}");
    }

    // Synthesis LLM call failed — fall back gracefully.
    log(&format!(
        "verify synthesis failed ({}); falling back to {}",
        result
            .error
            .as_deref()
            .unwrap_or("no output from synthesis agent"),
        if has_checks {
            "explicit checks"
        } else {
            "project-discovery prompt"
        },
    ));
    if has_checks {
        format!(
            " After resolving, your edits must keep these project checks passing: {}.",
            explicit_checks.join("; ")
        )
    } else {
        " After resolving, verify the code meets project quality standards: \
          check for a CLAUDE.md or AGENTS.md file in the repository root for build, \
          format, lint, and test instructions; run any applicable formatter and linter; \
          ensure the project builds without errors; then stage your changes."
            .to_string()
    }
}

/// Drive an agent to resolve the in-progress rebase conflicts in `wt`, then
/// `git add` + `rebase --continue`, looping until the rebase completes or a cap
/// is hit. Once every conflict marker is resolved and committed, runs a
/// dedicated final-verification agent call (RAL-149) before returning --
/// see [`run_final_verify`]. Returns `Ok((agent_session_id, verify_detail))`
/// when fully resolved and verified (pass or fail; verify never blocks the
/// rebase from completing -- see [`run_final_verify`]'s doc comment).
#[allow(clippy::too_many_arguments)]
fn resolve_conflicts_with_agent(
    store: &Arc<Mutex<Store>>,
    id: &str,
    position: i64,
    branch_id: &str,
    runner: &dyn Runner,
    wt: &Workspace,
    branch: &str,
    agent: &str,
    model: &Option<String>,
    gate: &VerifyGate,
) -> std::result::Result<(Option<String>, String), String> {
    let agent = agent.to_string();
    let model = model.clone();

    // RAL-136: ghost memory for review worktrees. Reviews aren't part of the
    // task dependency graph (Q2's "one level up" lookup is task-graph only),
    // so the only context to inject here is this branch's *own* prior ghost --
    // e.g. from an earlier resolve pass or a rebuild after the base branch
    // shifted (`rebuild_on_base_shift`). Computed once and reused across loop
    // iterations, same as `quality_note` above.
    let ghost_uri = crate::ghost::review_uri(id, Some(branch_id));
    let ghost_prefix = {
        let guard = store.lock().expect("poisoned");
        guard
            .get_ghost(&ghost_uri)
            .ok()
            .flatten()
            .and_then(|g| crate::ghost::format_context_block(Some(&g), &[]))
            .unwrap_or_default()
    };

    // Seed the live progress (RAL-72): found = current conflicting commit's marker
    // block count, fixed = files resolved in working tree (not staged), committed =
    // hunks staged for the *current* conflicting commit. Both found and committed are
    // scoped to whichever commit the rebase is presently stopped on: found is
    // recomputed fresh from disk every loop iteration below, and committed is reset to
    // 0 every time the rebase advances to its next commit (RAL-144) -- neither value
    // accumulates across commits within the branch's rebase.
    let mut found = i64::try_from(count_markers(wt, &conflicted_files(wt))).unwrap_or(i64::MAX);
    let mut committed = 0i64;
    let mut last_session_id: Option<String> = None;
    crate::rlog!(
        INFO,
        "ralphus [guardian] review {id} conflicts starting branch={branch:?} found={found} \
         agent={agent:?} model={model:?}"
    );
    {
        let guard = store.lock().expect("poisoned");
        let _ = guard.set_guardian_conflicts(id, Some(found), Some(0), Some(0));
        let _ = guard.set_branch_conflicts(id, branch_id, Some(found), Some(0), Some(0));
        let _ = guard.cartographer_log(crate::cartographer::CartographerEntry {
            level: crate::logging::LogLevel::INFO,
            source: "guardian",
            message: "conflicts starting",
            scope: Some("branch"),
            run_id: None,
            guardian_id: Some(id),
            session_id: None,
            task: None,
            payload: serde_json::json!({
                "branch": branch,
                "found": found,
                "agent": agent,
                "model": model,
            }),
        });
    }

    // Side-channel file where the Python backend writes the claude session ID as
    // soon as the stream-json init event arrives — before the session completes.
    // Located in the system temp dir (never inside the worktree) so git can never
    // track it.  The watcher thread below polls this file and writes the session ID
    // to the DB immediately, enabling Watch Live access during a long resolve pass.
    let wt_basename = wt
        .root()
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "unknown".to_string());
    let sid_path = std::env::temp_dir()
        .join("ralphus")
        .join(format!("{wt_basename}.live_session"));

    for _ in 0..32 {
        let files = conflicted_files(wt);
        if files.is_empty() {
            // No conflicts left: the rebase has either finished or auto-advanced
            // through clean commits. If it is still in progress, drive it forward;
            // once it reports no rebase in progress we are done.
            if rebase_in_progress(wt) {
                advance_rebase(wt);
                // RAL-144: advancing to the next commit -- nothing was found or
                // committed for it yet.
                committed = 0;
                continue;
            }
            {
                let guard = store.lock().expect("poisoned");
                let _ = guard.set_guardian_conflicts(id, Some(found), Some(0), Some(committed));
                let _ = guard.set_branch_conflicts(
                    id,
                    branch_id,
                    Some(found),
                    Some(0),
                    Some(committed),
                );
            }
            crate::rlog!(
                INFO,
                "ralphus [guardian] review {id} conflicts resolved branch={branch:?} committed={committed}"
            );
            {
                let guard = store.lock().expect("poisoned");
                let _ = guard.cartographer_log(crate::cartographer::CartographerEntry {
                    level: crate::logging::LogLevel::INFO,
                    source: "guardian",
                    message: "conflicts resolved",
                    scope: Some("branch"),
                    run_id: None,
                    guardian_id: Some(id),
                    session_id: None,
                    task: None,
                    payload: serde_json::json!({"branch": branch, "committed": committed}),
                });
            }
            // RAL-168: gated by Verify scope -- a branch that just had real
            // conflicts resolved is never "auto-clean", so only `scope`
            // (not `skip_auto_clean`) matters here.
            if !gate.allows_after_conflict() {
                crate::rlog!(
                    INFO,
                    "ralphus [guardian] review {id} final verification skipped branch={branch:?} \
                     scope={:?}",
                    gate.scope
                );
                return Ok((
                    last_session_id,
                    "resolved by agent; final verification skipped (Verify scope)".to_string(),
                ));
            }
            let (quality_note, ghost_prefix) =
                verify_extras(store, id, branch, branch_id, runner, &agent, &model);
            let (verify_session_id, verify_detail) = run_final_verify(
                store,
                id,
                position,
                branch_id,
                runner,
                wt,
                branch,
                &agent,
                &model,
                &quality_note,
                &ghost_prefix,
            );
            return Ok((verify_session_id.or(last_session_id), verify_detail));
        }
        // Fast path: rerere (or a prior agent pass) may have already resolved
        // the file content even though the index still shows UU entries.
        // count_markers reads the actual files; if zero, stage and continue
        // without invoking the agent at all.
        let markers_before = i64::try_from(count_markers(wt, &files)).unwrap_or(i64::MAX);
        // RAL-144: found is scoped to the commit the rebase is presently stopped
        // on -- recompute it fresh each iteration rather than letting the
        // pre-loop seed go stale as the rebase advances through commits.
        found = markers_before;
        if markers_before == 0 {
            crate::rlog!(
                INFO,
                "ralphus [guardian] review {id} rerere fast-path branch={branch:?} \
                 (content resolved, staging without agent)"
            );
            committed += i64::try_from(files.len()).unwrap_or(0);
            {
                let guard = store.lock().expect("poisoned");
                let _ = guard.set_guardian_conflicts(id, Some(found), Some(0), Some(committed));
                let _ = guard.set_branch_conflicts(
                    id,
                    branch_id,
                    Some(found),
                    Some(0),
                    Some(committed),
                );
                let _ = guard.cartographer_log(crate::cartographer::CartographerEntry {
                    level: crate::logging::LogLevel::INFO,
                    source: "guardian",
                    message: "rerere fast-path (content resolved, staging without agent)",
                    scope: Some("branch"),
                    run_id: None,
                    guardian_id: Some(id),
                    session_id: None,
                    task: None,
                    payload: serde_json::json!({"branch": branch, "files": files.len()}),
                });
            }
            wt.git(&["add", "-A"])?;
            advance_rebase(wt);
            // RAL-144: advancing to the next commit -- nothing committed for it yet.
            committed = 0;
            continue;
        }

        let prompt = format!(
            "{ghost_prefix}Resolve all merge conflict markers in these files from branch '{branch}': {}. \
             Read each file, intelligently merge both sides of every conflict block \
             (<<<<<<<...=======...>>>>>>>), and write the resolved content back with \
             ALL markers removed.",
            files.join(", ")
        );
        // RAL-168: this fix pass never runs formatters/linters/tests, even if
        // the reviewer's own project uses them heavily -- that responsibility
        // belongs solely to the dedicated final-verify call
        // (`run_final_verify`/[`VerifyGate`] above), which already handles it
        // plus auto-fix. Running (and paying for) the same checks twice per
        // conflict-resolution cycle was the redundancy this ticket removes;
        // see the module-level RAL-168 notes.
        let system_prompt = "You are a git merge-conflict resolver running inside a checked-out worktree \
             during an active `git rebase`. Your job is to eliminate every conflict marker and \
             produce correctly merged files -- nothing more.\n\
             \n\
             Step-by-step:\n\
             1. For each conflicted file named in the prompt: call read_file to get its \
                current content.\n\
             2. Locate every conflict block delimited by <<<<<<< ... ======= ... >>>>>>>. \
                Understand what each side contributes and write the correct merged result — \
                preserving the intent of both sides, with ALL markers removed.\n\
             3. Call write_file with the fully resolved content. Repeat for every file.\n\
             4. Once every file is marker-free, call run_bash with exactly: git add -A\n\
             5. After git add -A succeeds, output the following line and stop:\n\
                RALPHUS_STAGE: DONE\n\
             \n\
             Do NOT run formatters, linters, or tests, and do NOT attempt to fix quality \
             issues beyond resolving the conflict markers themselves -- a dedicated \
             verification pass runs afterward and will handle formatting/linting/testing, \
             including auto-fixing any failures it finds. Do NOT call `git rebase --continue`, \
             `git commit`, `git push`, or any other git command besides `git add -A`. The \
             orchestrator advances the rebase as soon as it sees RALPHUS_STAGE: DONE in your \
             output.";
        let spec = RunnerSpec {
            // RAL-102: unique per (guardian, branch position) so the tmux
            // session this resolves through (see `crate::tmux::session_name`)
            // never collides with another guardian's or branch's resolver —
            // and so the Review tab's capture-pane endpoint can address this
            // exact invocation via the same (guardian id, position) pair the
            // route already carries.
            run_id: format!("guardian-{id}"),
            task: RESOLVER_TASK.to_string(),
            session_id: format!("resolver-{position}"),
            cwd: wt.root().to_string_lossy().into_owned(),
            prompt: Some(prompt),
            command: None,
            agent: agent.clone(),
            model: model.clone(),
            system_prompt: Some(system_prompt.to_string()),
            system_prompt_position: None,
            timeout_sec: None,
            budget_tokens: None,
            maximum_budget_usd: None,
            verify: false,
            trace_context: None,
            resume_agent_session_id: None,
            env_overrides: std::collections::BTreeMap::new(),
            machine: None,
        };

        // Clean up any stale file from a previous pass so the watcher does not
        // read an old session ID.
        let _ = std::fs::remove_file(&sid_path);

        // Spawn a thread that polls the side-channel file and writes the session
        // ID to the DB as soon as the Python backend creates it (on the init
        // event, before the session finishes).  This is what makes Watch Live
        // available immediately rather than only after the full pass completes.
        let stop = Arc::new(AtomicBool::new(false));
        let stop_clone = Arc::clone(&stop);
        let store_clone = Arc::clone(store);
        let id_str = id.to_string();
        let branch_id_str = branch_id.to_string();
        let sid_path_clone = sid_path.clone();
        let watcher = std::thread::spawn(move || {
            while !stop_clone.load(Ordering::Relaxed) {
                if let Ok(raw) = std::fs::read_to_string(&sid_path_clone) {
                    let sid = raw.trim();
                    if !sid.is_empty() {
                        let guard = store_clone.lock().expect("poisoned");
                        let _ = guard.set_branch_resolver_session_id(&id_str, &branch_id_str, sid);
                        break;
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(500));
            }
        });

        let result = runner.run(&spec);

        stop.store(true, Ordering::Relaxed);
        let _ = watcher.join();

        if let Some(sid) = result.agent_session_id.clone() {
            last_session_id = Some(sid);
        }
        if !result.is_done() {
            let err = result
                .error
                .as_deref()
                .unwrap_or("resolver subprocess produced no output");
            crate::rlog!(
                ERROR,
                "ralphus [guardian] review {id} conflicts failed branch={branch:?}: {err}"
            );
            {
                let guard = store.lock().expect("poisoned");
                let _ = guard.cartographer_log(crate::cartographer::CartographerEntry {
                    level: crate::logging::LogLevel::ERROR,
                    source: "guardian",
                    message: "conflicts failed",
                    scope: Some("branch"),
                    run_id: None,
                    guardian_id: Some(id),
                    session_id: None,
                    task: None,
                    payload: serde_json::json!({"branch": branch, "error": err}),
                });
            }
            return Err(format!("conflict resolver failed: {err}"));
        }

        // RAL-136: persist the resolver's self-summarized handoff note, if it
        // produced one (same `RALPHUS_GHOST:` marker/system-prompt path as a
        // task session -- see `cli/src/ralphus/runner/execute.py`). Merges
        // onto whatever this branch already published rather than
        // overwriting it, so notes from earlier passes/rebuilds accumulate.
        if let Some(ghost_text) = result.ghost.as_deref().map(str::trim) {
            if !ghost_text.is_empty() {
                let revision = crate::ghost::current_revision(&wt.root().to_string_lossy());
                let guard = store.lock().expect("poisoned");
                if guard
                    .upsert_ghost(
                        &ghost_uri,
                        crate::ghost::KIND_REVIEW,
                        None,
                        Some(id),
                        ghost_text,
                        revision.as_deref(),
                    )
                    .is_ok()
                {
                    let _ = guard.cartographer_log(crate::cartographer::CartographerEntry {
                        level: crate::logging::LogLevel::INFO,
                        source: "guardian",
                        message: "ghost published",
                        scope: Some("branch"),
                        run_id: None,
                        guardian_id: Some(id),
                        session_id: None,
                        task: None,
                        payload: serde_json::json!({"branch": branch, "len": ghost_text.len()}),
                    });
                }
            }
        }

        // Fast path: agent signalled it staged everything.  Trust it and advance
        // the rebase immediately without re-scanning for markers.
        let stage_done = result.summary.contains(STAGE_DONE_MARKER);
        if stage_done {
            committed += markers_before;
            {
                let guard = store.lock().expect("poisoned");
                let _ = guard.set_guardian_conflicts(id, Some(found), Some(0), Some(committed));
                let _ = guard.set_branch_conflicts(
                    id,
                    branch_id,
                    Some(found),
                    Some(0),
                    Some(committed),
                );
            }
            crate::rlog!(
                INFO,
                "ralphus [guardian] review {id} stage-done signal received branch={branch:?}"
            );
            {
                let guard = store.lock().expect("poisoned");
                let _ = guard.cartographer_log(crate::cartographer::CartographerEntry {
                    level: crate::logging::LogLevel::INFO,
                    source: "guardian",
                    message: "stage-done signal received",
                    scope: Some("branch"),
                    run_id: None,
                    guardian_id: Some(id),
                    session_id: None,
                    task: None,
                    payload: serde_json::json!({"branch": branch, "committed": committed}),
                });
            }
            advance_rebase(wt);
            // RAL-144: advancing to the next commit -- nothing committed for it yet.
            committed = 0;
            continue;
        }

        // Fallback: agent did not emit the signal (older model, partial run, etc.).
        // Count remaining markers and advance when they are gone.
        let remaining = i64::try_from(count_markers(wt, &files)).unwrap_or(i64::MAX);
        // Fixed = hunks the agent cleared in the working tree, not yet staged.
        let fixed = markers_before.saturating_sub(remaining);
        {
            let guard = store.lock().expect("poisoned");
            let _ = guard.set_guardian_conflicts(id, Some(found), Some(fixed), Some(committed));
            let _ = guard.set_branch_conflicts(
                id,
                branch_id,
                Some(found),
                Some(fixed),
                Some(committed),
            );
        }
        if remaining > 0 {
            // Markers still present — let the loop retry up to the cap rather than
            // failing immediately. The agent may need more than one pass to fully
            // clear all conflicts (e.g. partial resolution or a multi-file case).
            continue;
        }
        wt.git(&["add", "-A"])?;
        // All hunks in this batch are now staged; accumulate them as committed.
        committed += markers_before;
        {
            let guard = store.lock().expect("poisoned");
            let _ = guard.set_guardian_conflicts(id, Some(found), Some(0), Some(committed));
            let _ =
                guard.set_branch_conflicts(id, branch_id, Some(found), Some(0), Some(committed));
        }
        // Advance the rebase to the next commit. The loop re-checks at the top
        // and routes any newly-surfaced conflict back through the resolver.
        // `--continue` may fail if the resolved commit is now empty (its change
        // already applied) — `advance_rebase` falls back to `--skip` in that case.
        // Either way the loop re-checks and resolves any further conflicting
        // commits.
        advance_rebase(wt);
        // RAL-144: advancing to the next commit -- nothing committed for it yet.
        committed = 0;
    }
    Err("exceeded conflict-resolution attempts".to_string())
}

/// RAL-149/168: dedicated final-verification agent call. Runs once a
/// branch's conflict markers are all resolved and committed (from
/// `resolve_conflicts_with_agent`'s loop above), or -- per RAL-168's Verify
/// scope -- against a branch that rebased cleanly with no conflict at all
/// (from `drive_rebase` directly) -- a separate LLM call from the fix pass so
/// the board's "final verification pending" indicator reflects a real,
/// distinct step rather than something bundled into the fix call. Its system
/// prompt does not assume a conflict occurred, so it inspects the worktree
/// itself rather than assuming what state the code is in.
///
/// Sets the branch's merge status to [`MergeStatus::VerifyPending`] for the
/// call's duration -- the caller clears it (to `conflict_resolved`) once this
/// returns. Reuses the `verify: true` `RunnerSpec` contract (same
/// `RALPHUS_VERIFY: PASS/FAIL` marker-parsing, fail-closed on no verdict) that
/// `agent`-kind task verify steps already use.
///
/// Never fails the branch: like the quality-bar instructions it carries, this
/// call is advisory. A FAIL verdict (or a runner error) is folded into the
/// returned detail message for a reviewer to see, not treated as a rebase
/// failure -- a verify retry/blocking policy is explicitly out of scope for
/// RAL-149.
#[allow(clippy::too_many_arguments)]
fn run_final_verify(
    store: &Arc<Mutex<Store>>,
    id: &str,
    position: i64,
    branch_id: &str,
    runner: &dyn Runner,
    wt: &Workspace,
    branch: &str,
    agent: &str,
    model: &Option<String>,
    quality_note: &str,
    ghost_prefix: &str,
) -> (Option<String>, String) {
    {
        let guard = store.lock().expect("poisoned");
        let _ = guard.set_branch_status(id, branch_id, MergeStatus::VerifyPending, None);
        let _ = guard.cartographer_log(crate::cartographer::CartographerEntry {
            level: crate::logging::LogLevel::INFO,
            source: "guardian",
            message: "final verification starting",
            scope: Some("branch"),
            run_id: None,
            guardian_id: Some(id),
            session_id: None,
            task: None,
            payload: serde_json::json!({"branch": branch}),
        });
    }
    crate::rlog!(
        INFO,
        "ralphus [guardian] review {id} final verification starting branch={branch:?} agent={agent:?} model={model:?}"
    );

    let prompt = format!(
        "{ghost_prefix}Confirm branch '{branch}' is ready in this worktree, after being \
         rebased onto the current stack -- whether or not that rebase hit a conflict, an \
         earlier fix pass may or may not have made changes here to satisfy the project's \
         quality bar, so do not assume what state the code is in; inspect it yourself.{quality_note}"
    );
    let system_prompt = "You are running the dedicated final-verification pass of a git rebase \
         conflict-resolution cycle, in a checked-out worktree. Confirm the code meets the \
         quality bar described in the prompt, fixing anything you reasonably can. If you edit \
         any files, run `git add -A` with run_bash to stage them before you finish. Do NOT call \
         `git rebase --continue`, `git commit`, `git push`, `git rebase --abort`, or any other \
         rebase-affecting git command -- the orchestrator owns the rebase and has already \
         advanced past the conflict this branch was resolving.";
    let spec = RunnerSpec {
        // Mirrors the fix pass's `resolver-{position}` session id (RAL-102) --
        // distinct so the two calls never collide on the same tmux session.
        run_id: format!("guardian-{id}"),
        task: RESOLVER_VERIFY_TASK.to_string(),
        session_id: format!("resolver-verify-{position}"),
        cwd: wt.root().to_string_lossy().into_owned(),
        prompt: Some(prompt),
        command: None,
        agent: agent.to_string(),
        model: model.clone(),
        system_prompt: Some(system_prompt.to_string()),
        system_prompt_position: None,
        timeout_sec: None,
        budget_tokens: None,
        maximum_budget_usd: None,
        verify: true,
        trace_context: None,
        resume_agent_session_id: None,
        env_overrides: std::collections::BTreeMap::new(),
        machine: None,
    };
    let result = runner.run(&spec);
    let passed = result.verify_passed();

    crate::rlog!(
        INFO,
        "ralphus [guardian] review {id} final verification done branch={branch:?} passed={passed}"
    );
    {
        let guard = store.lock().expect("poisoned");
        let _ = guard.cartographer_log(crate::cartographer::CartographerEntry {
            level: if passed {
                crate::logging::LogLevel::INFO
            } else {
                crate::logging::LogLevel::WARNING
            },
            source: "guardian",
            message: "final verification done",
            scope: Some("branch"),
            run_id: None,
            guardian_id: Some(id),
            session_id: None,
            task: None,
            payload: serde_json::json!({"branch": branch, "passed": passed}),
        });
    }

    let detail = if passed {
        "resolved by agent; final verification passed".to_string()
    } else {
        let reason = result
            .error
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .or_else(|| Some(result.summary.trim()).filter(|s| !s.is_empty()))
            .unwrap_or("no verdict reported");
        format!("resolved by agent; final verification failed: {reason}")
    };
    (result.agent_session_id, detail)
}

/// CCTL-134: run the review's check gates against a single stacked commit's
/// worktree. Returns `Err` with the failing command on the first failure. A
/// review that opted out of checks (CCTL-130) or declares none passes trivially.
///
/// On a full pass, RAL-152 folds a ground-truth "this was validated" note
/// onto `branch_id`'s own ghost, mirroring what `run_verifies`/
/// `note_verify_outcome` do for task sessions in `scheduler.rs` — a resolver
/// restarted for this branch (e.g. after `rebuild_on_base_shift`) then has a
/// reliable signal that the last full check pass actually succeeded, not
/// just that the agent said so. No note when `checks` is empty (nothing was
/// actually validated) or `skip_auto_build` is set.
fn run_commit_checks(
    store: &Arc<Mutex<Store>>,
    id: &str,
    branch_id: &str,
    wt: &Workspace,
    branch: &str,
) -> std::result::Result<(), String> {
    let (skip_auto_build, checks) = {
        let guard = store.lock().expect("poisoned");
        (
            guard.guardian_skip_auto_build(id).unwrap_or(false),
            guard.guardian_checks(id).unwrap_or_default(),
        )
    };
    if skip_auto_build {
        return Ok(());
    }
    for cmd in &checks {
        if !wt.run_command(cmd).0 {
            return Err(format!("check failed after '{branch}': {cmd}"));
        }
    }
    let wt_str = wt.root().to_string_lossy().into_owned();
    if !checks.is_empty() {
        let uri = crate::ghost::review_uri(id, Some(branch_id));
        let note = crate::ghost::verify_outcome_note(checks.len(), checks.len());
        let revision = crate::ghost::current_revision(&wt_str);
        let guard = store.lock().expect("poisoned");
        if guard
            .upsert_ghost(
                &uri,
                crate::ghost::KIND_REVIEW,
                None,
                Some(id),
                &note,
                revision.as_deref(),
            )
            .is_ok()
        {
            crate::cartographer::Note::new("guardian")
                .guardian(id)
                .scope("branch")
                .emit(
                    &guard,
                    "ghost check-outcome note recorded",
                    serde_json::json!({"branch": branch, "checks": checks.len()}),
                );
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Restack helpers (RAL-35)
// ---------------------------------------------------------------------------

/// Re-stack every branch whose `position > from_position` onto the review branch
/// at `from_position` (which is assumed to already have the desired HEAD). Runs
/// check gates on each branch; finalises the combined worktree at the end.
///
/// Extracted so both [`run_feedback`] and [`dispatch_routes`] can share the
/// downstream-rebase logic without duplication.
#[allow(clippy::too_many_arguments)]
fn restack_from_position<F: Fn(GuardianStatus, Option<&str>)>(
    store: &Arc<Mutex<Store>>,
    runner: &dyn Runner,
    id: &str,
    root: &Workspace,
    wt_base: &Workspace,
    base_branch: &str,
    from_position: i64,
    set_status: &F,
) {
    // RAL-103: this is a forced regeneration (feedback routing or a detected
    // manual push) -- clear the stale manual-checks commands up front so
    // `checks_state` drops out of "ready" for the whole restack, instead of
    // showing the previous build's commands as current until
    // `generate_manual_commands` overwrites them at the end.
    let _ = store
        .lock()
        .expect("poisoned")
        .clear_guardian_manual_commands(id);
    let base_sha = match resolve_base(root, base_branch) {
        Ok(s) => s,
        Err(e) => {
            set_status(
                GuardianStatus::MergeFailed,
                Some(&format!("base branch '{base_branch}': {e}")),
            );
            return;
        }
    };
    let branches = store
        .lock()
        .expect("poisoned")
        .guardian_branches(id)
        .unwrap_or_default();
    // RAL-91: resolve each branch's effective project so squash can be applied
    // per-project during the re-stack. `guardian_branches` returns no project, so
    // build the map from the full guardian view.
    let (squash_set, proj_by_branch) = {
        let g = store.lock().expect("poisoned").get_guardian(id).ok();
        let squash_set: std::collections::HashSet<String> = g
            .as_ref()
            .map(|g| g.squash_projects.iter().cloned().collect())
            .unwrap_or_default();
        let git_root = g.as_ref().map(|g| g.git_root.clone()).unwrap_or_default();
        let map: std::collections::HashMap<String, String> = g
            .map(|g| {
                g.branches
                    .into_iter()
                    .map(|b| {
                        (
                            b.branch.clone(),
                            b.project.unwrap_or_else(|| git_root.clone()),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        (squash_set, map)
    };
    // RAL-168: unambiguous "last branch in the stack" for `VerifyScope::FinalBranch`,
    // computed once against the FULL branch list (not the `from_position`-filtered
    // re-stack subset below) -- a re-stack starting mid-stack must still recognize
    // the true final branch even when it isn't touched by this particular pass.
    let final_id = final_branch_id(&branches).map(str::to_string);
    let mut prev_ref = branches
        .iter()
        .find(|b| b.position == from_position)
        .map(|b| format!("guardian/{id}/wt-{}", b.branch))
        .unwrap_or_default();
    for ob in branches.iter().filter(|b| b.position > from_position) {
        let squash = proj_by_branch
            .get(&ob.branch)
            .is_some_and(|p| squash_set.contains(p));
        let _ = store.lock().expect("poisoned").set_branch_status(
            id,
            &ob.id,
            MergeStatus::InProgress,
            None,
        );
        let rev = format!("guardian/{id}/wt-{}", ob.branch);
        let wt_j = wt_base.join(format!("wt-{}", ob.branch));
        let wt_j_str = wt_j.root().to_string_lossy().to_string();
        if let Err(e) = worktree_add_or_reset(root, &rev, &wt_j, &ob.branch) {
            fail_branch(store, id, &ob.id, &ob.branch, &e, set_status);
            return;
        }
        let _ = store
            .lock()
            .expect("poisoned")
            .set_branch_review(id, &ob.id, &rev, &wt_j_str);
        let gate = VerifyGate::resolve(store, id, Some(&ob.id) == final_id.as_ref());
        if stack_pick(
            store,
            runner,
            id,
            ob.position,
            &ob.id,
            &ob.branch,
            &base_sha,
            &prev_ref,
            &rev,
            &wt_j,
            squash,
            &gate,
        )
        .is_err()
        {
            return;
        }
        if let Err(e) = run_commit_checks(store, id, &ob.id, &wt_j, &ob.branch) {
            fail_branch(store, id, &ob.id, &ob.branch, &e, set_status);
            return;
        }
        prev_ref = rev;
    }
    match finalize_review(store, root, wt_base, id, &prev_ref) {
        Ok(note) => {
            // RAL-53: regenerate summary from subject lines after re-stacking.
            let completed: Vec<(i64, String)> = branches
                .iter()
                .filter(|b| b.enabled)
                .map(|b| (b.position, b.branch.clone()))
                .collect();
            generate_summary(
                store, runner, id, root, &base_sha, &completed, &prev_ref, true,
            );
            // RAL-27/RAL-110: regenerate manual review commands after
            // re-stacking, and try an AI-inferred auto-build if nothing else
            // covered finalize-time verification.
            let build_note = generate_manual_commands(
                store,
                runner,
                id,
                root,
                &base_sha,
                &prev_ref,
                Some(&wt_base.join(format!("{id}-review"))),
            );
            // RAL-92: re-baseline every branch's review-branch tip now that the
            // stack has settled, so the restacked downstream branches are not
            // mistaken for a manual push on the next maintenance sweep.
            snapshot_review_heads(store, id);
            set_status(GuardianStatus::InReview, note.or(build_note).as_deref());
        }
        Err(e) => set_status(GuardianStatus::MergeFailed, Some(&e)),
    }
}

/// Dispatch a list of `(branch_name, instructions)` route blocks to fresh agents
/// in their respective review worktrees (RAL-35). Blocks until every agent
/// finishes, then re-stacks all branches downstream of the lowest modified
/// position and rebuilds the combined worktree.
fn dispatch_routes(
    store: &Arc<Mutex<Store>>,
    runner: Arc<dyn Runner>,
    id: &str,
    routes: &[(String, String)],
    guardian: &crate::guardian::GuardianView,
    no_commit: bool,
) {
    if routes.is_empty() {
        return;
    }
    crate::rlog!(
        INFO,
        "ralphus [guardian] review {id} routing feedback to {} branch(es) no_commit={no_commit}",
        routes.len()
    );
    {
        let guard = store.lock().expect("poisoned");
        let _ = guard.cartographer_log(crate::cartographer::CartographerEntry {
            level: crate::logging::LogLevel::INFO,
            source: "guardian",
            message: "routing feedback to branches",
            scope: Some("guardian"),
            run_id: None,
            guardian_id: Some(id),
            session_id: None,
            task: None,
            payload: serde_json::json!({"routes": routes.len(), "no_commit": no_commit}),
        });
    }
    let git_root = Workspace::for_guardian(store, id, PathBuf::from(&guardian.git_root));
    let wt_base = git_root.at(worktree_dir(&guardian.git_root, id));
    let r_agent = resolver_agent(guardian.resolver_agent.as_deref());
    let r_model = resolver_model(guardian.resolver_model.as_deref(), &r_agent);

    struct Target {
        position: i64,
        branch: String,
        wt: Workspace,
        pre_hash: String,
        instructions: String,
    }

    // Map each route block to a branch that has a review worktree, recording the
    // pre-dispatch HEAD hash so changes can be detected after the agent finishes.
    let targets: Vec<Target> = routes
        .iter()
        .filter_map(|(branch_name, instructions)| {
            let bv = guardian
                .branches
                .iter()
                .find(|b| &b.branch == branch_name)?;
            let wt_str = bv.worktree.as_ref()?;
            let wt = Workspace::for_guardian(store, id, PathBuf::from(wt_str));
            let pre_hash = head_hash(&wt)?;
            Some(Target {
                position: bv.position,
                branch: branch_name.clone(),
                wt,
                pre_hash,
                instructions: instructions.clone(),
            })
        })
        .collect();

    if targets.is_empty() {
        return;
    }

    // Spawn one thread per target; each thread uses a clone of the shared runner.
    let handles: Vec<std::thread::JoinHandle<()>> = targets
        .iter()
        .map(|t| {
            let guardian_id = id.to_string();
            let position = t.position;
            let branch = t.branch.clone();
            let wt_cwd = t.wt.root().to_string_lossy().into_owned();
            let wt_for_thread = t.wt.clone();
            let instructions = t.instructions.clone();
            let r_agent = r_agent.clone();
            let r_model = r_model.clone();
            let runner_clone = runner.clone();
            std::thread::spawn(move || {
                // Stash any pre-existing dirty state so we only commit agent-made
                // changes (not leftovers from a prior no-commit turn).
                let stashed = if !no_commit {
                    let pre = wt_for_thread
                        .git(&["status", "--porcelain"])
                        .unwrap_or_default();
                    if !pre.trim().is_empty() {
                        wt_for_thread.git(&["stash", "--include-untracked"]).is_ok()
                    } else {
                        false
                    }
                } else {
                    false
                };
                let prompt = if no_commit {
                    format!(
                        "You are implementing reviewer feedback on the feature branch \
                         '{branch}' in its review worktree.\n\n\
                         Task:\n{instructions}\n\n\
                         Apply the required changes to the files. \
                         Do not run any git commands.",
                    )
                } else {
                    format!(
                        "You are implementing reviewer feedback on the feature branch \
                         '{branch}' in its review worktree.\n\n\
                         Task:\n{instructions}\n\n\
                         After making the required changes, commit them with:\n\
                           git add -A && git commit --amend --no-edit\n\
                         Amend the existing HEAD commit — do NOT create a new commit on \
                         top. Do not push.",
                    )
                };
                let spec = RunnerSpec {
                    // RAL-102: unique per (guardian, branch position) — see the
                    // comment on the resolver `RunnerSpec` above.
                    run_id: format!("guardian-{guardian_id}"),
                    task: "route".to_string(),
                    session_id: format!("route-{position}-{}", branch.replace(['/', '.'], "-")),
                    cwd: wt_cwd,
                    prompt: Some(prompt),
                    command: None,
                    agent: r_agent,
                    model: r_model,
                    system_prompt: None,
                    system_prompt_position: None,
                    timeout_sec: None,
                    budget_tokens: None,
                    maximum_budget_usd: None,
                    verify: false,
                    trace_context: None,
                    resume_agent_session_id: None,
                    env_overrides: std::collections::BTreeMap::new(),
                    machine: None,
                };
                let _ = runner_clone.run(&spec);
                // Defensive amend: if the agent left uncommitted changes and we are
                // allowed to commit, finalize them now.
                if !no_commit {
                    let status = wt_for_thread
                        .git(&["status", "--porcelain"])
                        .unwrap_or_default();
                    if !status.trim().is_empty() {
                        let _ = wt_for_thread.git(&["add", "-A"]);
                        let _ = wt_for_thread.git(&["commit", "--amend", "--no-edit"]);
                    }
                }
                // Restore any pre-existing (no-commit) changes to the working tree.
                if stashed {
                    let _ = wt_for_thread.git(&["stash", "pop"]);
                }
            })
        })
        .collect();

    // Block until ALL dispatched agents finish before touching the git graph.
    for handle in handles {
        let _ = handle.join();
    }

    // Detect which review branches changed (agent committed/amended).
    let mut dirty_positions: Vec<i64> = Vec::new();
    for t in &targets {
        let post_hash = head_hash(&t.wt).unwrap_or_default();
        if post_hash != t.pre_hash {
            dirty_positions.push(t.position);
        }
    }

    if dirty_positions.is_empty() {
        return;
    }

    let from_position = *dirty_positions.iter().min().expect("non-empty");
    let set_status = |s: GuardianStatus, detail: Option<&str>| {
        let _ = store
            .lock()
            .expect("poisoned")
            .set_guardian_status(id, s, detail);
    };
    set_status(GuardianStatus::Merging, Some("applying routed feedback"));
    restack_from_position(
        store,
        runner.as_ref(),
        id,
        &git_root,
        &wt_base,
        &guardian.base_branch,
        from_position,
        &set_status,
    );
}

/// The worktree directory a guardian's review stack is built in.
///
/// Lives inside `.git/` (which git already excludes from the working tree) so
/// the review worktrees never appear at the repo root and need no gitignore
/// entry. Git worktrees checked out under `.git` resolve normally — the admin
/// entry in `.git/worktrees/<name>` and the `commondir` back-pointer are
/// independent of where the checkout itself lives.
pub(crate) fn worktree_dir(git_root: &str, guardian_id: &str) -> PathBuf {
    Path::new(git_root)
        .join(".git")
        .join(".ralphus_guardian")
        .join(guardian_id)
}

/// Best-effort removal of every review worktree/branch a guardian created, used
/// when the guardian is deleted. All git operations are ignored on failure (a
/// bare/fake `git_root`, e.g. in tests, simply removes nothing).
///
/// Also called on the *source* guardian by `Store::move_guardian_branch`'s
/// caller (`server::guardian_move_branch`, RAL-118) when the guardian is NOT
/// being deleted, just losing a branch: the branches that stay behind may
/// have been rebased on top of the moved-out branch in a prior build, and
/// `run_merge`'s carry-forward mechanism (the `old_review` snapshot near the
/// top of `run_merge`, keyed only by branch name) has no way to tell that
/// from a `guardian/<id>/wt-<branch>` ref alone -- it would otherwise replay
/// a remaining branch's stale review commit, which still contains the moved
/// branch's changes baked into its history, onto the new stack. Purging
/// every worktree/branch/carry-ref here (before the next `run_merge` even
/// starts) forces that rebuild to fall back to its from-scratch path and
/// re-derive every remaining branch purely from its own feature-branch tip.
pub fn purge_worktrees(store: &Arc<Mutex<Store>>, git_root: &str, id: &str) {
    let num = id.replace("guardian-", "");
    let root = Workspace::for_guardian(store, id, Path::new(git_root));
    let wt_base = root.at(worktree_dir(git_root, id));
    cleanup_review_worktrees(&root, &wt_base, id, &num, &[]);
    // Also drop any carry-forward protection refs so a deleted guardian leaves
    // nothing pinning otherwise-unreachable commits.
    purge_carry_refs(&root, id);
}

/// Validate the guardian and kick off a background merge. Returns immediately.
/// Kick off a background merge for `id`. The spawned worker acquires a slot
/// from `sem` before doing any work, so the review counts against the same
/// global concurrency cap as sessions and task-level verifies.
pub fn start_merge(
    store: Arc<Mutex<Store>>,
    runner: Arc<dyn Runner>,
    id: &str,
    sem: Arc<Semaphore>,
) -> Reply {
    let guardian = {
        let guard = store.lock().expect("store mutex poisoned");
        guard.get_guardian(id)
    };
    let guardian = match guardian {
        Ok(g) => g,
        Err(e) => {
            return reply(404, &error_body("not_found", &e.to_string()));
        }
    };
    if guardian.branches.is_empty() {
        return reply(
            400,
            &error_body("no_branches", "guardian has no branches to merge"),
        );
    }

    // Atomically transition collecting, merge_failed, or in_review → merging
    // (RAL-108: in_review is included so "Merge / rebase" forces a fresh rebase
    // even on an already-done review). Two concurrent requests can both pass
    // the guardian-exists check above, but only one can win this SQL UPDATE;
    // the other gets false and a 409.
    let claimed = match store
        .lock()
        .expect("store mutex poisoned")
        .claim_guardian_merge(id)
    {
        Ok(c) => c,
        Err(e) => return reply(500, &error_body("store_error", &e.to_string())),
    };
    if !claimed {
        crate::rlog!(
            DEBUG,
            "ralphus [guardian] review {id} merge claim rejected (already merging, or in a terminal state)"
        );
        {
            let guard = store.lock().expect("store mutex poisoned");
            let _ = guard.cartographer_log(crate::cartographer::CartographerEntry {
                level: crate::logging::LogLevel::DEBUG,
                source: "guardian",
                message: "merge claim rejected (already merging, or in a terminal state)",
                scope: Some("guardian"),
                run_id: None,
                guardian_id: Some(id),
                session_id: None,
                task: None,
                payload: serde_json::json!({}),
            });
        }
        return reply(
            409,
            &error_body(
                "already_in_progress",
                "a rebase is already in progress; cancel it before starting a new one",
            ),
        );
    }
    crate::rlog!(
        INFO,
        "ralphus [guardian] review {id} merge starting branches={}",
        guardian.branches.len()
    );
    {
        let guard = store.lock().expect("store mutex poisoned");
        let _ = guard.cartographer_log(crate::cartographer::CartographerEntry {
            level: crate::logging::LogLevel::INFO,
            source: "guardian",
            message: "merge starting",
            scope: Some("guardian"),
            run_id: None,
            guardian_id: Some(id),
            session_id: None,
            task: None,
            payload: serde_json::json!({"branches": guardian.branches.len()}),
        });
    }
    let sid = id.to_string();
    std::thread::spawn(move || {
        let _permit = sem.acquire();
        run_merge(&store, runner.as_ref(), &sid);
    });
    reply(202, "{\"status\":\"merging\"}")
}

/// Kick off a background "set it for me" resolution of one named
/// [`CheckInput`] (RAL-164). Looks the input up across the guardian's
/// `manual_commands`/`action_hints` (first match wins) to recover the
/// command it's used in and its declared message/default, atomically claims
/// it via [`Store::claim_guardian_input_resolution`] so a concurrent
/// duplicate request 409s instead of spawning a second LLM call, then
/// spawns a background worker (gated by `sem`, same global concurrency cap
/// as merges/sessions) and returns `202` immediately.
pub fn start_resolve_input(
    store: Arc<Mutex<Store>>,
    runner: Arc<dyn Runner>,
    guardian_id: &str,
    input_name: &str,
    sem: Arc<Semaphore>,
) -> Reply {
    let guardian = {
        let guard = store.lock().expect("store mutex poisoned");
        guard.get_guardian(guardian_id)
    };
    let guardian = match guardian {
        Ok(g) => g,
        Err(e) => return reply(404, &error_body("not_found", &e.to_string())),
    };

    let found = guardian
        .manual_commands
        .iter()
        .chain(guardian.action_hints.iter())
        .find_map(|check| {
            check
                .inputs
                .iter()
                .find(|i| i.name == input_name)
                .map(|input| {
                    let command = check
                        .command
                        .clone()
                        .or_else(|| check.prompt.clone())
                        .unwrap_or_default();
                    (command, input.clone())
                })
        });
    let Some((command, input)) = found else {
        return reply(
            400,
            &error_body(
                "unknown_input",
                "no check on this review declares that input",
            ),
        );
    };

    let claimed = match store
        .lock()
        .expect("store mutex poisoned")
        .claim_guardian_input_resolution(guardian_id, input_name)
    {
        Ok(c) => c,
        Err(e) => return reply(500, &error_body("store_error", &e.to_string())),
    };
    if !claimed {
        return reply(
            409,
            &error_body(
                "already_in_progress",
                "a resolution for this input is already in progress",
            ),
        );
    }

    let gid = guardian_id.to_string();
    std::thread::spawn(move || {
        let _permit = sem.acquire();
        resolve_check_input(&store, runner.as_ref(), &gid, &command, &input);
    });
    reply(202, "{\"ok\":true}")
}

/// Validate that a branch position has a review worktree, then kick off a
/// background feedback application. Returns immediately.
pub fn start_feedback(
    store: Arc<Mutex<Store>>,
    runner: Arc<dyn Runner>,
    id: &str,
    branch_id: &str,
    feedback: String,
) -> Reply {
    let guardian = {
        let guard = store.lock().expect("store mutex poisoned");
        guard.get_guardian(id)
    };
    let guardian = match guardian {
        Ok(g) => g,
        Err(e) => return reply(404, &error_body("not_found", &e.to_string())),
    };
    match guardian.branches.iter().find(|b| b.id == branch_id) {
        Some(b) if b.worktree.is_some() => {}
        Some(_) => {
            return reply(
                409,
                &error_body("not_ready", "run the review merge before giving feedback"),
            );
        }
        None => return reply(404, &error_body("not_found", "no such branch")),
    }
    let sid = id.to_string();
    let bid = branch_id.to_string();
    std::thread::spawn(move || run_feedback(&store, runner.as_ref(), &sid, &bid, &feedback));
    reply(202, "{\"status\":\"applying_feedback\"}")
}

/// Run the global feedback triage agent for a guardian (RAL-22). It works in
/// the COMBINED (all-branches-rebased) review worktree so it can see the whole
/// stack, then replies in the thread — asking a clarifying question, or stating
/// which branch(es) it is routing each reviewer instruction to. Synchronous;
/// spawned by [`start_chat`].
///
/// RAL-33: for `claude`/`anthropic` and `ollama` backends the LLM API is called
/// directly (no subprocess spawn), eliminating the 1–3 s Python cold-start that
/// was the dominant per-message latency cost. Other backends fall back to the
/// subprocess runner unchanged. The conversation history is passed as a proper
/// messages array rather than a flat concatenated prompt.
pub fn run_chat(
    store: &Arc<Mutex<Store>>,
    runner: Arc<dyn Runner>,
    id: &str,
    message: &str,
    image: Option<&str>,
) {
    let guardian = match store.lock().expect("poisoned").get_guardian(id) {
        Ok(g) => g,
        Err(_) => return,
    };
    let branches = guardian
        .branches
        .iter()
        .map(|b| b.branch.clone())
        .collect::<Vec<_>>()
        .join(", ");

    // RAL-35: the triage prompt documents the XML route-block convention.  When
    // the Guardian is confident about which branch needs a change, it embeds one
    // or more <route> blocks in its reply.  The daemon strips those blocks before
    // storing the message, so the reviewer never sees the raw XML.
    let system = format!(
        "You are the review Guardian, triaging reviewer feedback for a stacked \
         set of feature branches: [{branches}]. You are in the COMBINED review \
         worktree, which has every branch rebased together, so you can see the \
         whole change at once.\n\n\
         TRIAGE the reviewer's latest message: work out which branch(es) each \
         request applies to. Then EITHER ask one concise clarifying question if \
         it is ambiguous, OR describe which branch(es) you are routing each \
         instruction to and include a <route> block for each one.\n\n\
         Route-block convention (blocks are hidden from the reviewer — only your \
         plain-text reply is shown):\n\
         <route branch=\"EXACT_BRANCH_NAME\">\n\
         Precise, self-contained instructions for the agent implementing this \
         change on that branch. Say exactly which files to edit and what to \
         change.\n\
         </route>\n\n\
         Rules:\n\
         • Only emit a <route> block when you are certain which branch needs the \
           change and what change is required.\n\
         • You may include multiple <route> blocks — one per branch — in a single \
           reply.\n\
         • Do NOT run git commands yourself."
    );

    let r_agent = resolver_agent(guardian.resolver_agent.as_deref());
    let r_model = resolver_model(guardian.resolver_model.as_deref(), &r_agent);

    // RAL-88: capture the resolved agent/model actually used for this reply so the
    // reviewer can inspect it. When no model is configured, `call_direct` applies a
    // backend default (below) — mirror it so the recorded model matches what ran.
    let chat_agent_used = r_agent.clone();
    let chat_model_used = r_model
        .clone()
        .or_else(|| match r_agent.to_lowercase().as_str() {
            "claude" | "anthropic" => Some("claude-haiku-4-5".to_string()),
            "ollama" => Some("qwen3:8b".to_string()),
            _ => None,
        });

    // Fetch the full thread. `start_chat` already persisted the latest reviewer
    // message before spawning this thread, so the history ends with it — we do
    // not need to append it separately (doing so was a double-send bug in the
    // original flat-prompt approach).
    let history = store
        .lock()
        .expect("poisoned")
        .guardian_messages(id)
        .unwrap_or_default();

    crate::rlog!(
        DEBUG,
        "ralphus [guardian] review {id} chat triage start backend={r_agent:?} \
         model={r_model:?} messages={} has_image={}",
        history.len(),
        image.is_some()
    );
    {
        let guard = store.lock().expect("poisoned");
        let _ = guard.cartographer_log(crate::cartographer::CartographerEntry {
            level: crate::logging::LogLevel::DEBUG,
            source: "guardian",
            message: "chat triage start",
            scope: Some("guardian"),
            run_id: None,
            guardian_id: Some(id),
            session_id: None,
            task: None,
            payload: serde_json::json!({
                "backend": r_agent,
                "model": r_model,
                "messages": history.len(),
                "has_image": image.is_some(),
            }),
        });
    }

    // Map DB roles to API roles for the direct call.
    let chat_messages: Vec<crate::chat_client::ChatMessage> = history
        .iter()
        .map(|m| crate::chat_client::ChatMessage {
            role: if m.role == "reviewer" {
                "user"
            } else {
                "assistant"
            },
            content: m.text.clone(),
            image: m.image.clone(),
        })
        .collect();

    // Try a direct HTTP call first (no subprocess). Falls back to the subprocess
    // runner for unsupported agent types (e.g. claude-code, codex-cli, harness backends).
    let raw_reply = match crate::chat_client::call_direct(
        &r_agent,
        r_model.as_deref(),
        &system,
        &chat_messages,
    ) {
        Ok(text) => text,
        Err(direct_err) => {
            crate::rlog!(
                WARNING,
                "ralphus [guardian] review {id} chat-api fallback to subprocess: {direct_err}"
            );
            {
                let guard = store.lock().expect("poisoned");
                let _ = guard.cartographer_log(crate::cartographer::CartographerEntry {
                    level: crate::logging::LogLevel::WARNING,
                    source: "guardian",
                    message: "chat-api fallback to subprocess",
                    scope: Some("guardian"),
                    run_id: None,
                    guardian_id: Some(id),
                    session_id: None,
                    task: None,
                    payload: serde_json::json!({"error": direct_err}),
                });
            }
            // Subprocess fallback: build the old flat-text prompt and spawn the runner.
            // `message` is re-appended here because the subprocess backend does not
            // receive the structured messages array.
            let transcript = history
                .iter()
                .map(|m| format!("{}: {}", m.role, m.text))
                .collect::<Vec<_>>()
                .join("\n");
            let cwd = guardian
                .combined_worktree
                .clone()
                .unwrap_or_else(|| guardian.git_root.clone());

            // RAL-169: the combined review worktree is torn down and rebuilt
            // while a merge/rebase is running (see `run_merge`'s
            // `cleanup_review_worktrees` call), and `combined_worktree` in the
            // DB isn't cleared/updated until the rebuild finishes — so `cwd`
            // can point at a directory that transiently doesn't exist for the
            // whole duration of a merge. Spawning the subprocess runner
            // against a missing cwd fails with a raw filesystem error
            // ("workspace directory does not exist: ...") that would
            // otherwise be stored verbatim as the guardian's chat reply.
            // Detect that up front and reply with a friendly status message
            // instead, so chat stays usable while a merge is in progress.
            if !std::path::Path::new(&cwd).is_dir() {
                crate::rlog!(
                    WARNING,
                    "ralphus [guardian] review {id} chat workspace unavailable, \
                     skipping subprocess: cwd={cwd}"
                );
                {
                    let guard = store.lock().expect("poisoned");
                    let _ = guard.cartographer_log(crate::cartographer::CartographerEntry {
                        level: crate::logging::LogLevel::WARNING,
                        source: "guardian",
                        message: "chat workspace unavailable",
                        scope: Some("guardian"),
                        run_id: None,
                        guardian_id: Some(id),
                        session_id: None,
                        task: None,
                        payload: serde_json::json!({"cwd": cwd, "status": guardian.status}),
                    });
                }
                "A merge is currently rebuilding this review's workspace, so I can't look \
                 anything up right now. This is usually quick — please try again in a \
                 moment."
                    .to_string()
            } else {
                let prompt = format!(
                    "{system}\n\nConversation so far:\n{transcript}\n\nReviewer: {message}"
                );
                let spec = RunnerSpec {
                    // RAL-102: unique per guardian — see the comment on the
                    // resolver `RunnerSpec` in `resolve_conflicts_with_agent`.
                    run_id: format!("guardian-{id}"),
                    task: "chat".to_string(),
                    session_id: "triage".to_string(),
                    cwd,
                    prompt: Some(prompt),
                    command: None,
                    agent: r_agent,
                    model: r_model,
                    system_prompt: None,
                    system_prompt_position: None,
                    timeout_sec: None,
                    budget_tokens: None,
                    maximum_budget_usd: None,
                    verify: false,
                    trace_context: None,
                    resume_agent_session_id: None,
                    env_overrides: std::collections::BTreeMap::new(),
                    machine: None,
                };
                let result = runner.run(&spec);
                if result.is_done() && !result.summary.trim().is_empty() {
                    result.summary
                } else {
                    // RAL-169: don't surface a raw internal/subprocess error
                    // (e.g. a filesystem or backend failure message) directly
                    // in the reviewer-facing chat thread. Log the real detail
                    // for diagnosis and reply with a friendly message.
                    if let Some(err) = &result.error {
                        crate::rlog!(
                            WARNING,
                            "ralphus [guardian] review {id} chat triage failed: {err}"
                        );
                    }
                    "Sorry, I ran into a problem answering that — please try again in a \
                     moment."
                        .to_string()
                }
            }
        }
    };

    // Parse route blocks from the raw reply, then strip them so only the
    // human-readable text lands in the feedback thread.
    let routes = parse_route_blocks(&raw_reply);
    let visible_text = if routes.is_empty() {
        raw_reply
    } else {
        strip_route_blocks(&raw_reply)
    };

    {
        let guard = store.lock().expect("poisoned");
        let _ = guard.add_guardian_message(id, "guardian", &visible_text, None);
        // RAL-88: record which resolved agent/model produced this reply.
        let _ = guard.set_guardian_chat_agent(id, &chat_agent_used, chat_model_used.as_deref());
        // RAL-167: the reviewer-facing content the chat UI's post-send poll is
        // actually waiting on -- push a guardian-scoped event now so a
        // connected SSE client refreshes the thread immediately instead of
        // relying solely on that bounded poll.
        let _ = guard.cartographer_log(crate::cartographer::CartographerEntry {
            level: crate::logging::LogLevel::INFO,
            source: "guardian",
            message: "chat reply posted",
            scope: Some("guardian"),
            run_id: None,
            guardian_id: Some(id),
            session_id: None,
            task: None,
            payload: serde_json::json!({"has_routes": !routes.is_empty()}),
        });
    }

    // Dispatch each route block to a fresh agent in the target review worktree,
    // wait for all agents to finish, then restack downstream branches.
    if !routes.is_empty() {
        let route_branches: Vec<&str> = routes.iter().map(|(b, _)| b.as_str()).collect();
        crate::rlog!(
            INFO,
            "ralphus [guardian] review {id} chat routes={} branches=[{}]",
            routes.len(),
            route_branches.join(", ")
        );
        {
            let guard = store.lock().expect("poisoned");
            let _ = guard.cartographer_log(crate::cartographer::CartographerEntry {
                level: crate::logging::LogLevel::INFO,
                source: "guardian",
                message: "chat routes dispatched",
                scope: Some("guardian"),
                run_id: None,
                guardian_id: Some(id),
                session_id: None,
                task: None,
                payload: serde_json::json!({"routes": routes.len(), "branches": route_branches}),
            });
        }
        let no_commit = is_no_commit_intent(message);
        dispatch_routes(store, runner, id, &routes, &guardian, no_commit);
    }
}

/// Append the reviewer's message to the thread and kick off the triage agent's
/// reply in the background. Returns immediately (RAL-22).
///
/// `image` is an optional base64 data-URI attached to this message (RAL-59).
pub fn start_chat(
    store: Arc<Mutex<Store>>,
    runner: Arc<dyn Runner>,
    id: &str,
    message: String,
    image: Option<String>,
) -> Reply {
    if let Err(e) = store.lock().expect("poisoned").get_guardian(id) {
        return reply(404, &error_body("not_found", &e.to_string()));
    }
    // Persist the reviewer's message synchronously so the UI shows it at once.
    if let Err(e) = store.lock().expect("poisoned").add_guardian_message(
        id,
        "reviewer",
        &message,
        image.as_deref(),
    ) {
        return reply(500, &error_body("internal", &e.to_string()));
    }
    let sid = id.to_string();
    let img = image.clone();
    std::thread::spawn(move || run_chat(&store, runner, &sid, &message, img.as_deref()));
    reply(202, "{\"status\":\"triaging\"}")
}

/// Build the review stack for a guardian (synchronous; called on a worker thread
/// or directly in tests). Conflicts are resolved with `runner`.
///
/// For single-project guardians each feature branch gets its OWN review worktree
/// and branch, stacked one on top of the previous. For multi-project guardians
/// (RAL-29) the branches are first partitioned by project root; each project runs
/// its own independent stacking sequence. All projects must succeed for the
/// guardian to reach `InReview`. A final, read-only *combined* worktree points at
/// the head of the last branch in the last project.
pub fn run_merge(store: &Arc<Mutex<Store>>, runner: &dyn Runner, id: &str) {
    let guardian = match store.lock().expect("poisoned").get_guardian(id) {
        Ok(g) => g,
        Err(_) => return,
    };
    // RAL-185 D5: the review's machine, resolved once. Every workspace below is
    // derived from this one, so a path can never lose track of which host it
    // belongs to on the way down.
    let ws_root = Workspace::for_guardian(store, id, &guardian.git_root);
    let num = id.replace("guardian-", "");
    let base = guardian.base_branch.clone();
    crate::rlog!(
        INFO,
        "ralphus [guardian] review {id} merge executing base={base:?} branches={}",
        guardian.branches.iter().filter(|b| b.enabled).count()
    );
    {
        let guard = store.lock().expect("poisoned");
        let _ = guard.cartographer_log(crate::cartographer::CartographerEntry {
            level: crate::logging::LogLevel::INFO,
            source: "guardian",
            message: "merge executing",
            scope: Some("guardian"),
            run_id: None,
            guardian_id: Some(id),
            session_id: None,
            task: None,
            payload: serde_json::json!({
                "base": base,
                "branches": guardian.branches.iter().filter(|b| b.enabled).count(),
            }),
        });
    }

    let set_status = |s: GuardianStatus, detail: Option<&str>| {
        let _ = store
            .lock()
            .expect("poisoned")
            .set_guardian_status(id, s, detail);
    };
    set_status(GuardianStatus::Merging, None);
    {
        let guard = store.lock().expect("poisoned");
        // RAL-103: the change summary is deliberately NOT cleared here -- the
        // last computed summary (preliminary or final) stays visible until
        // `generate_summary` overwrites it once the stack rebuilds, instead of
        // showing a misleading empty/"generating" gap for the whole rebuild.
        let _ = guard.clear_guardian_manual_commands(id);
        let _ = guard.set_guardian_conflicts(id, None, None, None);
        let _ = guard.clear_all_branch_conflicts(id);
    }

    // Partition branches by their effective project root, preserving position order
    // within each project and preserving the order in which projects first appear.
    let branches = {
        let g = store.lock().expect("poisoned");
        g.get_guardian(id)
            .unwrap_or_else(|_| guardian.clone())
            .branches
    };

    // RAL-168: unambiguous "last branch in the stack" for `VerifyScope::FinalBranch`,
    // computed once here across EVERY project -- position is global across the
    // whole guardian, not reset per project, so a multi-project guardian's final
    // branch is whichever enabled branch has the highest position overall, not
    // the last one within whichever project happens to build last.
    let final_branch_id: Option<String> = branches
        .iter()
        .filter(|b| b.enabled)
        .max_by_key(|b| b.position)
        .map(|b| b.id.clone());

    // RAL-54: reset all enabled branches to Pending before starting, so the board
    // never shows stale terminal statuses (Done, Failed) from a prior build while
    // the new merge is in progress. Done before the per-branch loop so the reset
    // is as close to atomic with the first git operation as SQLite allows.
    {
        let guard = store.lock().expect("poisoned");
        let _ = guard.reset_all_enabled_branches_to_pending(id);
    }

    // RAL-43: disabled branches are skipped in the stacking sequence but their
    // rows stay in the DB. Reset them to Pending so stale review-branch/worktree
    // fields from a prior build do not linger after cleanup.
    for ob in branches.iter().filter(|b| !b.enabled) {
        let _ = store
            .lock()
            .expect("poisoned")
            .reset_branch_to_pending(id, &ob.id);
    }
    let branches: Vec<_> = branches.into_iter().filter(|b| b.enabled).collect();

    // Insertion-ordered: Vec<(project, Vec<branch>)>.
    let mut project_order: Vec<String> = Vec::new();
    let mut project_branches: std::collections::HashMap<String, Vec<crate::guardian::BranchView>> =
        std::collections::HashMap::new();
    for b in branches {
        let proj = b
            .project
            .clone()
            .unwrap_or_else(|| guardian.git_root.clone());
        if !project_branches.contains_key(&proj) {
            project_order.push(proj.clone());
        }
        project_branches.entry(proj).or_default().push(b);
    }
    let project_branches: Vec<(String, Vec<crate::guardian::BranchView>)> = project_order
        .into_iter()
        .map(|p| {
            let bs = project_branches.remove(&p).unwrap_or_default();
            (p, bs)
        })
        .collect();

    // RAL-43: if all branches are disabled the review is a no-op — surface it
    // clearly rather than erroring.
    if project_branches.is_empty() {
        set_status(
            GuardianStatus::InReview,
            Some("all branches disabled — review is a no-op"),
        );
        return;
    }
    // Carry-forward snapshot: record each enabled branch's prior resolved
    // review-branch commit and the prior per-project base BEFORE cleanup deletes
    // those refs. On a rebuild (e.g. the base branch moved) the already-resolved
    // commits are replayed onto the new base instead of re-deriving from the
    // feature tips — so a conflict resolved on an earlier build is not resolved
    // again, even without git rerere. Empty on the first build (no prior refs),
    // which makes this path identical to a from-scratch merge.
    let mut old_base_by_proj: std::collections::HashMap<String, String> =
        guardian.base_commits.clone();
    if let Some(bc) = &guardian.base_commit {
        old_base_by_proj
            .entry(guardian.git_root.clone())
            .or_insert_with(|| bc.clone());
    }
    let mut old_review: std::collections::HashMap<(String, String), String> =
        std::collections::HashMap::new();

    // Pin every commit we may carry forward under `refs/ralphus/carry/<id>/…`
    // BEFORE cleanup deletes the `guardian/<id>/*` branches, so a carried commit
    // is never even briefly dangling — and thus never prunable by a concurrent
    // `git gc` — in the window between cleanup and its re-checkout below. `carry`
    // deletes every ref it creates when this function returns (success, early
    // return, or panic); any leftovers from a merge killed before its guard ran
    // are purged first.
    let mut carry = CarryRefs::new();
    for (proj, proj_branches) in &project_branches {
        // RAL-185: every path derived below carries the review's machine, so
        // each git command and file operation lands where the review was
        // assigned rather than on whichever host happens to be running this.
        let proot = ws_root.at(PathBuf::from(proj));
        purge_carry_refs(&proot, id);
        if let Some(sha) = old_base_by_proj.get(proj) {
            carry.pin(proot.root(), id, "base", sha);
        }
        for ob in proj_branches {
            let rev = format!("guardian/{id}/wt-{}", ob.branch);
            if let Ok(sha) = proot.git(&["rev-parse", "--verify", &rev]) {
                let sha = sha.trim().to_string();
                carry.pin(proot.root(), id, &ob.position.to_string(), &sha);
                old_review.insert((proj.clone(), ob.branch.clone()), sha);
            }
        }
    }

    // Clean up prior worktrees for ALL projects before starting fresh.
    for (proj, _) in &project_branches {
        let root = ws_root.at(PathBuf::from(proj));
        let wt_base = ws_root.at(worktree_dir(proj, id));
        cleanup_review_worktrees(&root, &wt_base, id, &num, &[]);
    }

    // Track the last combined worktree (and its project root, for RAL-101
    // auto-build config resolution) across all projects, used for final checks.
    let mut last_combined: Option<String> = None;
    let mut last_root: Option<Workspace> = None;
    let mut last_build_note: Option<String> = None;

    for (proj, proj_branches) in &project_branches {
        let root = ws_root.at(PathBuf::from(proj));
        let wt_base = ws_root.at(worktree_dir(proj, id));

        // Snapshot the base branch to a single immutable commit for this project.
        let base_sha = match resolve_base(&root, &base) {
            Ok(s) => s,
            Err(e) => {
                set_status(
                    GuardianStatus::MergeFailed,
                    Some(&format!("{proj}: base branch '{base}': {e}")),
                );
                return;
            }
        };
        let _ = store
            .lock()
            .expect("poisoned")
            .set_guardian_project_base_commit(id, proj, &base_sha);

        // CCTL-156: large repos can opt out of per-branch worktrees.
        if guardian.skip_worktrees {
            // RAL-91: squash applies per-project in the shared-worktree path too.
            let squash = guardian.squash_projects.iter().any(|p| p == proj);
            let ordered: Vec<crate::guardian::OrderedBranch> = proj_branches
                .iter()
                .map(|b| crate::guardian::OrderedBranch {
                    id: b.id.clone(),
                    position: b.position,
                    branch: b.branch.clone(),
                    enabled: b.enabled,
                })
                .collect();
            run_merge_shared(
                store,
                runner,
                id,
                &root,
                &wt_base,
                &base_sha,
                &ordered,
                squash,
                &set_status,
                final_branch_id.as_deref(),
            );
            // On failure, set_status was already called inside run_merge_shared.
            let cur_status = store
                .lock()
                .expect("poisoned")
                .get_guardian(id)
                .map(|g| g.status)
                .unwrap_or_default();
            if cur_status == GuardianStatus::MergeFailed.as_str() {
                return;
            }
            let combined_str = store
                .lock()
                .expect("poisoned")
                .get_guardian(id)
                .ok()
                .and_then(|g| g.combined_worktree)
                .unwrap_or_default();
            last_combined = Some(combined_str);
            last_root = Some(root.clone());
            continue;
        }

        // RAL-91: whether this project's task branches are squashed to one commit.
        let squash = guardian.squash_projects.iter().any(|p| p == proj);

        // Per-branch worktree path: stack each branch on top of the previous.
        // `prev_ref` is the NEW stack tip each branch rebases onto; `prev_old` is
        // the matching tip from the PREVIOUS build (the old base for the first
        // branch), used to carry a prior resolution forward.
        let mut prev_ref = base_sha.clone();
        let mut prev_old: Option<String> = old_base_by_proj.get(proj).cloned();
        for ob in proj_branches {
            let _ = store.lock().expect("poisoned").set_branch_status(
                id,
                &ob.id,
                MergeStatus::InProgress,
                None,
            );
            // RAL-185 Phase 3b: a branch whose session ran on another machine
            // has its commits over there, not here -- pull them in before the
            // stack tries to use them.
            if let Err(e) = fetch_branch_for_remote_session(store, id, ob) {
                fail_branch(store, id, &ob.id, &ob.branch, &e, &set_status);
                return;
            }
            let rev = format!("guardian/{id}/wt-{}", ob.branch);
            let wt = wt_base.join(format!("wt-{}", ob.branch));
            let wt_str = wt.root().to_string_lossy().to_string();
            if let Err(e) = worktree_add_or_reset(&root, &rev, &wt, &ob.branch) {
                fail_branch(store, id, &ob.id, &ob.branch, &e, &set_status);
                return;
            }
            // Carry-forward: when this branch has a prior resolved review commit
            // whose old upstream is still an ancestor of it, point the review
            // branch at that commit and rebase ITS own (already conflict-resolved)
            // changes onto the new stack tip — passing the old upstream as the
            // rebase boundary. The replay then conflicts only on genuine new base
            // deltas, so a previously-resolved conflict is not resolved again.
            // Any mismatch (no prior commit, broken chain, checkout failure) falls
            // back to `base_sha` — the from-feature-tip behaviour.
            let this_old = old_review.get(&(proj.clone(), ob.branch.clone())).cloned();
            let upstream = match (&this_old, &prev_old) {
                (Some(src), Some(up))
                    if is_ancestor(&root, up, src)
                        && wt.git(&["checkout", "-B", &rev, src]).is_ok() =>
                {
                    up.clone()
                }
                _ => base_sha.clone(),
            };
            let _ = store
                .lock()
                .expect("poisoned")
                .set_branch_review(id, &ob.id, &rev, &wt_str);
            let gate = VerifyGate::resolve(
                store,
                id,
                Some(ob.id.as_str()) == final_branch_id.as_deref(),
            );
            if stack_pick(
                store,
                runner,
                id,
                ob.position,
                &ob.id,
                &ob.branch,
                &upstream,
                &prev_ref,
                &rev,
                &wt,
                squash,
                &gate,
            )
            .is_err()
            {
                return;
            }
            if let Err(e) = run_commit_checks(store, id, &ob.id, &wt, &ob.branch) {
                fail_branch(store, id, &ob.id, &ob.branch, &e, &set_status);
                return;
            }
            if note_if_branch_is_empty(store, id, &ob.id, &ob.branch, &root, &upstream) {
                // A task may legitimately produce no changes, but a branch in a
                // review stack is there to contribute something -- an empty one
                // means the review would approve work it does not contain.
                fail_branch(
                    store,
                    id,
                    &ob.id,
                    &ob.branch,
                    "branch is empty: it adds no changes over the branch beneath it in the stack.                      Its task most likely never committed its work -- check that session, then re-run                      it. If this branch is meant to be empty, disable it to drop it from the stack.",
                    &set_status,
                );
                return;
            }
            // The next branch extracts its own OLD commits relative to THIS
            // branch's old resolved tip, independent of the carry decision above.
            prev_old = this_old;
            prev_ref = rev;
        }

        // Build per-project combined worktree at the top of this project's stack.
        match rebuild_combined(store, &root, &wt_base, id, &prev_ref) {
            Ok(combined_str) => {
                let combined_wt = std::path::PathBuf::from(&combined_str);
                last_combined = Some(combined_str);
                last_root = Some(root.clone());
                // RAL-53: generate summary from commit subject lines once the
                // full stack is assembled.
                let completed: Vec<(i64, String)> = proj_branches
                    .iter()
                    .map(|ob| (ob.position, ob.branch.clone()))
                    .collect();
                generate_summary(
                    store, runner, id, &root, &base_sha, &completed, &prev_ref, true,
                );
                // RAL-27/RAL-110: regenerate manual review commands once the
                // stack is ready, and try an AI-inferred auto-build for this
                // project. Only the last project's note is surfaced below,
                // matching the final-check-gates pass which also only covers
                // the last project.
                last_build_note = generate_manual_commands(
                    store,
                    runner,
                    id,
                    &root,
                    &base_sha,
                    &prev_ref,
                    Some(&root.at(&combined_wt)),
                );
            }
            Err(e) => {
                set_status(GuardianStatus::MergeFailed, Some(&e));
                return;
            }
        }
    }

    // Run final check gates against the last combined worktree (all-projects pass).
    let note = if let (Some(combined_str), Some(root)) = (&last_combined, &last_root) {
        match final_checks(store, id, root, combined_str) {
            Ok(n) => n,
            Err(e) => {
                set_status(GuardianStatus::MergeFailed, Some(&e));
                return;
            }
        }
    } else {
        None
    };
    // RAL-92: record the freshly-built review-branch tip of every branch as the
    // baseline for manual-push detection, so this build (or a base-shift rebuild)
    // is never itself detected as a reviewer's manual push.
    snapshot_review_heads(store, id);
    set_status(
        GuardianStatus::InReview,
        note.or(last_build_note).as_deref(),
    );
}

/// CCTL-156 skip-worktrees path: rebase every branch, in order, onto a single
/// shared worktree (the combined review branch) instead of one worktree per
/// branch — avoiding a worktree copy per branch on large repos. Each branch still
/// gets its merge status and check gates; they all point at the shared worktree,
/// so per-branch review feedback lands there too.
#[allow(clippy::too_many_arguments)]
fn run_merge_shared<F: Fn(GuardianStatus, Option<&str>)>(
    store: &Arc<Mutex<Store>>,
    runner: &dyn Runner,
    id: &str,
    root: &Workspace,
    wt_base: &Workspace,
    base_sha: &str,
    branches: &[crate::guardian::OrderedBranch],
    squash: bool,
    set_status: &F,
    final_branch_id: Option<&str>,
) {
    let combined_branch = format!("guardian/{id}/review");
    let wt_name = format!("{id}-review");
    let wt = wt_base.join(&wt_name);
    let wt_str = wt.root().to_string_lossy().to_string();
    if let Err(e) = worktree_add_or_reset(root, &combined_branch, &wt, base_sha) {
        set_status(GuardianStatus::MergeFailed, Some(&e));
        return;
    }
    let mut completed: Vec<(i64, String)> = Vec::new();
    for ob in branches {
        let _ = store.lock().expect("poisoned").set_branch_status(
            id,
            &ob.id,
            MergeStatus::InProgress,
            None,
        );
        // Every branch shares the one combined worktree/branch; record it now so
        // the UI can show the expand row and feedback widget even if this branch fails.
        let _ = store.lock().expect("poisoned").set_branch_review(
            id,
            &ob.id,
            &combined_branch,
            &wt_str,
        );
        // Detach at the feature tip and rebase its own commits onto the current
        // combined head; then advance the combined branch to the result.
        if let Err(e) = wt.git(&["checkout", "--detach", &ob.branch]) {
            let _ = wt.git(&["checkout", "--force", &combined_branch]);
            fail_branch(store, id, &ob.id, &ob.branch, &e, set_status);
            return;
        }
        let gate = VerifyGate::resolve(store, id, Some(ob.id.as_str()) == final_branch_id);
        match drive_rebase(
            store,
            id,
            ob.position,
            &ob.id,
            runner,
            &ob.branch,
            &wt,
            &combined_branch,
            base_sha,
            "HEAD",
            &gate,
        ) {
            Ok((outcome, session_id)) => {
                // RAL-91: squash this branch's commits before advancing the shared
                // combined branch, so its contribution lands as a single commit.
                if squash {
                    if let Err(e) = squash_review_commits(&wt, &combined_branch, &ob.branch) {
                        let _ = wt.git(&["checkout", "--force", &combined_branch]);
                        fail_branch(store, id, &ob.id, &ob.branch, &e, set_status);
                        return;
                    }
                }
                let nothing = contributed_nothing(&wt, &combined_branch, "HEAD");
                if let Err(e) = wt.git(&["checkout", "-B", &combined_branch, "HEAD"]) {
                    fail_branch(store, id, &ob.id, &ob.branch, &e, set_status);
                    return;
                }
                let (status, detail): (MergeStatus, Option<String>) = match outcome {
                    RebaseOutcome::Resolved(note) => (MergeStatus::ConflictResolved, Some(note)),
                    // RAL-168: verified but no conflict occurred -- still `Done`.
                    RebaseOutcome::CleanVerified(note) => (MergeStatus::Done, Some(note)),
                    RebaseOutcome::Clean => (
                        MergeStatus::Done,
                        nothing.then(|| "no new commits over base (already merged?)".to_string()),
                    ),
                };
                let guard = store.lock().expect("poisoned");
                let _ = guard.set_branch_status(id, &ob.id, status, detail.as_deref());
                if let Some(ref sid) = session_id {
                    let _ = guard.set_branch_resolver_session_id(id, &ob.id, sid);
                }
            }
            Err(e) => {
                // drive_rebase already aborted; restore the combined branch.
                let _ = wt.git(&["checkout", "--force", &combined_branch]);
                fail_branch(store, id, &ob.id, &ob.branch, &e, set_status);
                return;
            }
        }
        if let Err(e) = run_commit_checks(store, id, &ob.id, &wt, &ob.branch) {
            fail_branch(store, id, &ob.id, &ob.branch, &e, set_status);
            return;
        }
        completed.push((ob.position, ob.branch.clone()));
        // RAL-39: update interim summary after each branch is stacked.
        generate_summary(
            store,
            runner,
            id,
            root,
            base_sha,
            &completed,
            &combined_branch,
            false,
        );
    }
    {
        let guard = store.lock().expect("poisoned");
        let _ = guard.set_guardian_review_branch(id, &combined_branch);
        let _ = guard.set_guardian_combined_worktree(id, &wt_str);
    }
    match final_checks(store, id, root, &wt_str) {
        Ok(note) => {
            // RAL-39: final summary once the whole stack is ready.
            generate_summary(
                store,
                runner,
                id,
                root,
                base_sha,
                &completed,
                &combined_branch,
                true,
            );
            // RAL-27/RAL-110: generate manual review commands once the stack is
            // ready, and try an AI-inferred auto-build if nothing else covered
            // finalize-time verification.
            let build_note = generate_manual_commands(
                store,
                runner,
                id,
                root,
                base_sha,
                &combined_branch,
                Some(&wt),
            );
            // RAL-92: baseline the shared review branch's tip (all branches share
            // it here) so the daemon's own build is not read as a manual push.
            snapshot_review_heads(store, id);
            set_status(GuardianStatus::InReview, note.or(build_note).as_deref());
        }
        Err(e) => set_status(GuardianStatus::MergeFailed, Some(&e)),
    }
}

/// Apply reviewer `feedback` to one branch's review worktree (via the agent),
/// commit it onto that branch's review branch, then re-stack the downstream
/// branches on top and rebuild the combined worktree. The task worktrees are
/// never touched. Runs synchronously (spawned by [`start_feedback`]).
pub fn run_feedback(
    store: &Arc<Mutex<Store>>,
    runner: &dyn Runner,
    id: &str,
    branch_id: &str,
    feedback: &str,
) {
    let guardian = match store.lock().expect("poisoned").get_guardian(id) {
        Ok(g) => g,
        Err(_) => return,
    };
    let base = guardian.base_branch.clone();
    let Some(branch) = guardian.branches.iter().find(|b| b.id == branch_id) else {
        return;
    };
    // Stack-order math (downstream filtering) below is legitimately
    // position-based; resolve it once here from the addressed branch_id.
    let position = branch.position;
    crate::rlog!(
        INFO,
        "ralphus [guardian] review {id} feedback applying position={position}"
    );
    {
        let guard = store.lock().expect("poisoned");
        let _ = guard.cartographer_log(crate::cartographer::CartographerEntry {
            level: crate::logging::LogLevel::INFO,
            source: "guardian",
            message: "feedback applying",
            scope: Some("branch"),
            run_id: None,
            guardian_id: Some(id),
            session_id: None,
            task: None,
            payload: serde_json::json!({"position": position}),
        });
    }

    let set_status = |s: GuardianStatus, detail: Option<&str>| {
        let _ = store
            .lock()
            .expect("poisoned")
            .set_guardian_status(id, s, detail);
    };

    // Resolve this branch's effective project root (RAL-29: may differ from primary).
    let branch_project = branch
        .project
        .clone()
        .unwrap_or_else(|| guardian.git_root.clone());
    let root = Workspace::for_guardian(store, id, PathBuf::from(&branch_project));
    let wt_base = root.at(worktree_dir(&branch_project, id));

    let Some(wt_str) = branch.worktree.clone() else {
        set_status(
            GuardianStatus::MergeFailed,
            Some("no review worktree yet; run the merge first"),
        );
        return;
    };
    let feature = branch.branch.clone();
    let wt = root.at(PathBuf::from(&wt_str));
    set_status(GuardianStatus::Merging, None);

    // The agent edits the review worktree; we commit onto its review branch.
    let prompt = format!(
        "You are revising branch '{feature}' in response to reviewer feedback. \
         Edit the files in this worktree to satisfy the feedback, then stop. \
         Feedback: {feedback}. Do not run any git commands."
    );
    let r_agent = resolver_agent(guardian.resolver_agent.as_deref());
    let r_model = resolver_model(guardian.resolver_model.as_deref(), &r_agent);
    let spec = RunnerSpec {
        // RAL-102: unique per guardian — a bare "guardian" run_id collides
        // with every other guardian's tmux session name (see the identical
        // fix on the `generate_summary` spec below).
        run_id: format!("guardian-{id}"),
        task: "feedback".to_string(),
        session_id: "reviewer".to_string(),
        cwd: wt_str,
        prompt: Some(prompt),
        command: None,
        agent: r_agent,
        model: r_model,
        system_prompt: None,
        system_prompt_position: None,
        timeout_sec: None,
        budget_tokens: None,
        maximum_budget_usd: None,
        verify: false,
        trace_context: None,
        resume_agent_session_id: None,
        env_overrides: std::collections::BTreeMap::new(),
        machine: None,
    };
    let no_commit = is_no_commit_intent(feedback);
    // Stash any pre-existing dirty state so we only include the agent's own
    // changes in the new commit (not leftovers from a prior no-commit turn).
    let stashed = if !no_commit {
        let pre = wt.git(&["status", "--porcelain"]).unwrap_or_default();
        if !pre.trim().is_empty() {
            wt.git(&["stash", "--include-untracked"]).is_ok()
        } else {
            false
        }
    } else {
        false
    };
    let _ = runner.run(&spec);
    let dirty = wt.git(&["status", "--porcelain"]).unwrap_or_default();
    if !dirty.trim().is_empty() && !no_commit {
        let _ = wt.git(&["add", "-A"]);
        let _ = git(
            wt.root(),
            &["commit", "-m", &format!("review feedback: {feedback}")],
        );
    }
    // Restore any pre-existing (no-commit) changes to the working tree.
    if stashed {
        let _ = wt.git(&["stash", "pop"]);
    }
    let _ = store
        .lock()
        .expect("poisoned")
        .set_branch_detail(id, branch_id, "feedback applied");
    crate::rlog!(
        INFO,
        "ralphus [guardian] review {id} feedback done position={position} no_commit={no_commit}"
    );
    {
        let guard = store.lock().expect("poisoned");
        let _ = guard.cartographer_log(crate::cartographer::CartographerEntry {
            level: crate::logging::LogLevel::INFO,
            source: "guardian",
            message: "feedback done",
            scope: Some("branch"),
            run_id: None,
            guardian_id: Some(id),
            session_id: None,
            task: None,
            payload: serde_json::json!({"position": position, "no_commit": no_commit}),
        });
    }
    if no_commit {
        // RAL-92: no commit was created so the review-branch tips are unchanged;
        // re-baseline anyway to keep manual-push detection consistent.
        snapshot_review_heads(store, id);
        set_status(GuardianStatus::InReview, None);
        return;
    }

    // RAL-103: applying feedback is a forced regeneration too -- clear the
    // stale manual-checks commands now so `checks_state` drops out of "ready"
    // for the downstream restack below, instead of showing the previous
    // build's commands as current until `generate_manual_commands` overwrites
    // them at the end.
    let _ = store
        .lock()
        .expect("poisoned")
        .clear_guardian_manual_commands(id);

    // Re-stack only the downstream branches in the SAME project (cross-project
    // rebasing is impossible). Snapshot the base commit once for consistency.
    let base_sha = match resolve_base(&root, &base) {
        Ok(s) => s,
        Err(e) => {
            set_status(
                GuardianStatus::MergeFailed,
                Some(&format!("base branch '{base}': {e}")),
            );
            return;
        }
    };
    let all_branches = store
        .lock()
        .expect("poisoned")
        .get_guardian(id)
        .map(|g| g.branches)
        .unwrap_or_default();
    // RAL-168: unambiguous "last branch in the stack" for `VerifyScope::FinalBranch`
    // -- computed against every branch in the guardian, not just this re-stack's
    // downstream subset (see `run_merge`'s identical computation for why).
    let final_branch_id: Option<String> = all_branches
        .iter()
        .filter(|b| b.enabled)
        .max_by_key(|b| b.position)
        .map(|b| b.id.clone());
    // Downstream branches in the same project, in position order.
    let downstream: Vec<_> = all_branches
        .iter()
        .filter(|b| {
            b.position > position
                && b.project.as_deref().unwrap_or(&guardian.git_root) == branch_project
        })
        .collect();
    // RAL-91: downstream branches share this branch's project, so squash is
    // constant across the re-stack.
    let squash = guardian
        .squash_projects
        .iter()
        .any(|p| p == &branch_project);
    let mut prev_ref = branch
        .review_branch
        .clone()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| format!("guardian/{id}/wt-{}", branch.branch));
    for ob in &downstream {
        let _ = store.lock().expect("poisoned").set_branch_status(
            id,
            &ob.id,
            MergeStatus::InProgress,
            None,
        );
        let rev = ob
            .review_branch
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| format!("guardian/{id}/wt-{}", ob.branch));
        let wt_j = wt_base.join(format!("wt-{}", ob.branch));
        let wt_j_str = wt_j.root().to_string_lossy().to_string();
        // Reset the review branch to the feature tip; drive_rebase replays its
        // own commits onto the revised upstream (`prev_ref`).
        if let Err(e) = worktree_add_or_reset(&root, &rev, &wt_j, &ob.branch) {
            fail_branch(store, id, &ob.id, &ob.branch, &e, &set_status);
            return;
        }
        let _ = store
            .lock()
            .expect("poisoned")
            .set_branch_review(id, &ob.id, &rev, &wt_j_str);
        let gate = VerifyGate::resolve(
            store,
            id,
            Some(ob.id.as_str()) == final_branch_id.as_deref(),
        );
        if stack_pick(
            store,
            runner,
            id,
            ob.position,
            &ob.id,
            &ob.branch,
            &base_sha,
            &prev_ref,
            &rev,
            &wt_j,
            squash,
            &gate,
        )
        .is_err()
        {
            return;
        }
        if let Err(e) = run_commit_checks(store, id, &ob.id, &wt_j, &ob.branch) {
            fail_branch(store, id, &ob.id, &ob.branch, &e, &set_status);
            return;
        }
        prev_ref = rev;
    }

    match finalize_review(store, &root, &wt_base, id, &prev_ref) {
        Ok(note) => {
            // RAL-53: regenerate summary from subject lines after the re-stack.
            let completed: Vec<(i64, String)> = all_branches
                .iter()
                .filter(|b| {
                    b.enabled
                        && b.project.as_deref().unwrap_or(&guardian.git_root) == branch_project
                })
                .map(|b| (b.position, b.branch.clone()))
                .collect();
            generate_summary(
                store, runner, id, &root, &base_sha, &completed, &prev_ref, true,
            );
            // RAL-27/RAL-110: regenerate manual review commands after the
            // re-stack, and try an AI-inferred auto-build if nothing else
            // covered finalize-time verification.
            let build_note = generate_manual_commands(
                store,
                runner,
                id,
                &root,
                &base_sha,
                &prev_ref,
                Some(&wt_base.join(format!("{id}-review"))),
            );
            // RAL-92: re-baseline after applying feedback so the new tips (the
            // edited branch and its restacked downstream) are the reference for
            // future manual-push detection.
            snapshot_review_heads(store, id);
            set_status(GuardianStatus::InReview, note.or(build_note).as_deref());
        }
        Err(e) => set_status(GuardianStatus::MergeFailed, Some(&e)),
    }
}

/// Poll every review for a base-branch shift and rebuild any that drifted, each
/// on its own thread. Called periodically by the scheduler loop so that new
/// commits landing on a review's base branch are picked up automatically.
/// Sweep all `in_review`/`merge_failed` guardians and rebuild any whose base
/// branch has shifted. Each spawned worker acquires a slot from `sem` only if
/// it actually decides to rebuild, so this never blocks unnecessarily.
pub fn review_maintenance(store: &Arc<Mutex<Store>>, sem: &Arc<Semaphore>) {
    let straggler_ids: Vec<String> = {
        let guard = store.lock().expect("poisoned");
        guard.guardians_with_ready_stragglers().unwrap_or_default()
    };
    for id in straggler_ids {
        let store = Arc::clone(store);
        let sem = Arc::clone(sem);
        std::thread::spawn(move || {
            let runner = crate::runner::SubprocessRunner::from_env();
            reopen_straggler(&store, &runner, &id, &sem);
        });
    }

    let ids: Vec<String> = {
        let guard = store.lock().expect("poisoned");
        guard
            .list_guardians()
            .unwrap_or_default()
            .into_iter()
            .filter(|g| matches!(g.status.as_str(), "in_review" | "merge_failed"))
            .map(|g| g.id)
            .collect()
    };
    for id in ids {
        let store = Arc::clone(store);
        let sem = Arc::clone(sem);
        std::thread::spawn(move || {
            let runner: Arc<dyn Runner> = Arc::new(
                crate::runner::SubprocessRunner::from_env().with_cartographer(Arc::clone(&store)),
            );
            // A base-shift rebuild (full re-derive) subsumes any manual push via
            // carry-forward, so only look for a manual push when no rebuild ran.
            if !rebuild_on_base_shift(&store, runner.as_ref(), &id, &sem) {
                rebase_on_manual_push(&store, runner.as_ref(), &id, &sem);
            }
        });
    }
}

/// Reopen a single guardian stuck out of `collecting` (`in_review` or
/// `merge_failed`) with a straggler branch: a linked review (RAL-97/98) whose
/// branches arrive from separate runs can leave the guardian's later branch at
/// `merge_status = 'pending'` forever, because `try_start_ready_reviews_for_task`
/// only re-examines guardians still in `collecting` when a task completes
/// (`guardian.rs::collecting_guardians_for_sessions`). If the guardian already
/// moved on (e.g. to `in_review`) before the straggler's run finished, nothing
/// else ever revisits it. This periodic self-heal (called from
/// [`review_maintenance`]) promotes the now-done branch to `ready` and, if the
/// reopen claim wins, reruns [`run_merge`] from scratch — which rebuilds every
/// enabled branch and so picks the straggler up with no per-branch
/// special-casing. Returns whether a reopen actually happened.
pub fn reopen_straggler(
    store: &Arc<Mutex<Store>>,
    runner: &dyn Runner,
    id: &str,
    sem: &Semaphore,
) -> bool {
    let claimed = {
        let guard = store.lock().expect("poisoned");
        // Only reopen if there was actually a straggler to promote — otherwise
        // this would reopen (and rebuild) every in_review/merge_failed guardian
        // on every maintenance sweep for no reason.
        let promoted = guard
            .mark_ready_branches_with_done_sessions(id)
            .unwrap_or(0);
        promoted > 0 && guard.reopen_guardian_merge(id).unwrap_or(false)
    };
    if claimed {
        crate::rlog!(
            INFO,
            "ralphus [guardian] review {id} reopened: straggler branch ready"
        );
        let _permit = sem.acquire();
        run_merge(store, runner, id);
    }
    claimed
}

/// Read each enabled branch's current review-branch tip (RAL-92). Returns
/// `(position, sha)` for every branch whose review-branch ref still resolves in
/// its project. Branches with no review branch (never built, disabled, or reset)
/// are skipped. Each branch's ref is resolved in its own project root so
/// multi-project guardians (RAL-29) are handled correctly.
fn current_review_heads(
    store: &Arc<Mutex<Store>>,
    guardian: &crate::guardian::GuardianView,
) -> Vec<(i64, String, String)> {
    let mut out = Vec::new();
    for b in guardian.branches.iter().filter(|b| b.enabled) {
        let Some(rev) = b.review_branch.as_deref().filter(|s| !s.is_empty()) else {
            continue;
        };
        let proj = b
            .project
            .clone()
            .unwrap_or_else(|| guardian.git_root.clone());
        if let Ok(sha) = Workspace::on(Path::new(&proj), guardian.machine.as_deref())
            .with_store(Arc::clone(store))
            .git(&["rev-parse", "--verify", rev])
        {
            out.push((b.position, b.id.clone(), sha.trim().to_string()));
        }
    }
    out
}

/// Record every branch's current review-branch tip as its `review_head` baseline
/// (RAL-92). Called at every point the stack settles into `in_review` (initial
/// build, base-shift rebuild, feedback, and manual-push restack) so that only a
/// *reviewer's* subsequent move of a review-branch ref reads as a manual push —
/// never the daemon's own write.
fn snapshot_review_heads(store: &Arc<Mutex<Store>>, id: &str) {
    let guardian = match store.lock().expect("poisoned").get_guardian(id) {
        Ok(g) => g,
        Err(_) => return,
    };
    for (_position, branch_id, sha) in current_review_heads(store, &guardian) {
        let guard = store.lock().expect("poisoned");
        let _ = guard.set_branch_review_head(id, &branch_id, &sha);
    }
}

/// Detect a reviewer's manual push/amend to any of a review's per-branch review
/// worktrees and, if found, rebase the touched branch's downstream stack — the
/// same one-at-a-time restack that pressing "Merge / rebase" performs (RAL-92).
///
/// The manually-edited branch itself is left as the reviewer pushed it; every
/// branch after it is rebased onto it in order, with conflicts flowing through the
/// existing agent conflict-resolution path. Divergence is measured against the
/// `review_head` baseline the daemon records after every build, so the daemon's
/// own writes never trigger this — only a ref moved outside the daemon does.
///
/// Only idle `in_review` guardians are eligible; a merge/rebuild in flight owns
/// the worktrees and moves the refs itself. Each guardian is checked independently
/// by the maintenance sweep, so every review that has a touched worktree is
/// updated — including the (unusual) case of more than one review over the same
/// branch. The shared-worktree path (CCTL-156) has no per-branch worktrees to
/// watch and is skipped (baseline only). Returns whether a restack ran.
pub fn rebase_on_manual_push(
    store: &Arc<Mutex<Store>>,
    runner: &dyn Runner,
    id: &str,
    sem: &Semaphore,
) -> bool {
    let guardian = match store.lock().expect("poisoned").get_guardian(id) {
        Ok(g) => g,
        Err(_) => return false,
    };
    if guardian.status.as_str() != "in_review" {
        return false;
    }
    if guardian.skip_worktrees {
        // No per-branch worktrees; keep the baseline current but never restack.
        snapshot_review_heads(store, id);
        return false;
    }

    // Compare each branch's current review-branch tip against its baseline. A
    // first-ever observation (no baseline) is just recorded — not treated as a push.
    let mut changed: Vec<i64> = Vec::new();
    for (position, branch_id, current) in current_review_heads(store, &guardian) {
        let stored = store
            .lock()
            .expect("poisoned")
            .get_branch_review_head(id, &branch_id)
            .unwrap_or(None);
        match stored {
            None => {
                let guard = store.lock().expect("poisoned");
                let _ = guard.set_branch_review_head(id, &branch_id, &current);
            }
            Some(prev) if prev == current => {}
            Some(prev) => {
                crate::rlog!(
                    INFO,
                    "ralphus [guardian] review {id} manual push detected position={position} \
                     old={prev} new={current}"
                );
                {
                    let guard = store.lock().expect("poisoned");
                    let _ = guard.cartographer_log(crate::cartographer::CartographerEntry {
                        level: crate::logging::LogLevel::INFO,
                        source: "guardian",
                        message: "manual push detected",
                        scope: Some("branch"),
                        run_id: None,
                        guardian_id: Some(id),
                        session_id: None,
                        task: None,
                        payload: serde_json::json!({"position": position, "old": prev, "new": current}),
                    });
                }
                changed.push(position);
            }
        }
    }
    if changed.is_empty() {
        return false;
    }

    // Claim the review (in_review → merging) under one lock so a concurrent
    // maintenance pass or an explicit merge request cannot also start rebuilding it.
    let claimed = {
        let g = store.lock().expect("poisoned");
        matches!(g.get_guardian(id), Ok(gv) if gv.status.as_str() == "in_review")
            && g.set_guardian_status(
                id,
                GuardianStatus::Merging,
                Some("manual push detected; rebasing downstream"),
            )
            .is_ok()
    };
    if !claimed {
        return false;
    }
    let _permit = sem.acquire();

    // Restack downstream of the lowest touched branch. `restack_from_position`
    // leaves that branch untouched and rebases each later branch onto it in turn,
    // then re-baselines all branches (so the moved downstream tips are not read as
    // a fresh manual push next sweep) and sets the final status.
    let from_position = *changed.iter().min().expect("non-empty");
    let git_root = Workspace::for_guardian(store, id, PathBuf::from(&guardian.git_root));
    let wt_base = git_root.at(worktree_dir(&guardian.git_root, id));
    let set_status = |s: GuardianStatus, detail: Option<&str>| {
        let _ = store
            .lock()
            .expect("poisoned")
            .set_guardian_status(id, s, detail);
    };
    crate::rlog!(
        INFO,
        "ralphus [guardian] review {id} rebasing downstream after manual push from_position={from_position}"
    );
    {
        let guard = store.lock().expect("poisoned");
        let _ = guard.cartographer_log(crate::cartographer::CartographerEntry {
            level: crate::logging::LogLevel::INFO,
            source: "guardian",
            message: "rebasing downstream after manual push",
            scope: Some("guardian"),
            run_id: None,
            guardian_id: Some(id),
            session_id: None,
            task: None,
            payload: serde_json::json!({"from_position": from_position}),
        });
    }
    restack_from_position(
        store,
        runner,
        id,
        &git_root,
        &wt_base,
        &guardian.base_branch,
        from_position,
        &set_status,
    );
    true
}

/// If the guardian's base branch has moved in ANY of its projects since the stack
/// was last built, rebuild it against the new base. Returns whether a rebuild ran.
///
/// For multi-project guardians (RAL-29), each project is checked independently;
/// a shift in any one project triggers a full rebuild. Only reviews `in_review`
/// or `merge_failed` are eligible. The first base commit seen for a project is
/// just recorded (no rebuild) so an existing review is not rebuilt merely because
/// the per-project column was previously unset.
pub fn rebuild_on_base_shift(
    store: &Arc<Mutex<Store>>,
    runner: &dyn Runner,
    id: &str,
    sem: &Semaphore,
) -> bool {
    let guardian = match store.lock().expect("poisoned").get_guardian(id) {
        Ok(g) => g,
        Err(_) => return false,
    };
    if !matches!(guardian.status.as_str(), "in_review" | "merge_failed") {
        return false;
    }

    // Check every project in the guardian for a base-branch shift.
    let mut any_shifted = false;
    let mut all_have_baseline = true;
    for proj in &guardian.projects {
        let current = match resolve_base(
            &Workspace::for_guardian(store, id, Path::new(proj)),
            &guardian.base_branch,
        ) {
            Ok(s) => s,
            Err(_) => continue, // branch gone/unresolvable: skip this project
        };
        match guardian.base_commits.get(proj) {
            None => {
                // No baseline for this project yet — record it without rebuilding.
                let _ = store
                    .lock()
                    .expect("poisoned")
                    .set_guardian_project_base_commit(id, proj, &current);
                all_have_baseline = false;
            }
            Some(prev) if prev == &current => {} // unchanged
            Some(_) => {
                any_shifted = true;
            }
        }
    }

    // Also fall back to the legacy single-project base_commit for existing rows
    // that were created before multi-project support was added.
    if !any_shifted && !all_have_baseline {
        // Some projects got a first-time baseline; don't rebuild.
        return false;
    }
    if !any_shifted {
        return false;
    }
    // Claim the review under one lock (flip to Merging) so a concurrent
    // maintenance pass cannot also start rebuilding it.
    let claimed = {
        let g = store.lock().expect("poisoned");
        matches!(g.get_guardian(id), Ok(gv) if matches!(gv.status.as_str(), "in_review" | "merge_failed"))
            && g.set_guardian_status(
                id,
                GuardianStatus::Merging,
                Some("base branch changed; rebuilding"),
            )
            .is_ok()
    };
    if claimed {
        let _permit = sem.acquire();
        run_merge(store, runner, id);
        true
    } else {
        false
    }
}

/// Rebase `feature_branch`'s own commits (`base_sha..feature`) onto `newbase` in
/// the worktree `wt` — which must already be checked out on the review branch
/// `rev` (created at the feature tip) — resolving conflicts with the agent, and
/// record the branch's merge status. On failure it marks the branch + guardian
/// failed and returns `Err`.
#[allow(clippy::too_many_arguments)]
fn stack_pick(
    store: &Arc<Mutex<Store>>,
    runner: &dyn Runner,
    id: &str,
    position: i64,
    branch_id: &str,
    feature_branch: &str,
    base_sha: &str,
    newbase: &str,
    rev: &str,
    wt: &Workspace,
    squash: bool,
    gate: &VerifyGate,
) -> std::result::Result<(), ()> {
    let set_status = |s: GuardianStatus, d: Option<&str>| {
        let _ = store
            .lock()
            .expect("poisoned")
            .set_guardian_status(id, s, d);
    };
    match drive_rebase(
        store,
        id,
        position,
        branch_id,
        runner,
        feature_branch,
        wt,
        newbase,
        base_sha,
        rev,
        gate,
    ) {
        Ok((outcome, session_id)) => {
            let (status, detail): (MergeStatus, Option<String>) = match outcome {
                RebaseOutcome::Resolved(note) => (MergeStatus::ConflictResolved, Some(note)),
                // RAL-168: verified but no conflict occurred -- still `Done`.
                RebaseOutcome::CleanVerified(note) => (MergeStatus::Done, Some(note)),
                RebaseOutcome::Clean => (
                    MergeStatus::Done,
                    // Surface a branch that added nothing over the base rather than
                    // reporting a silent, work-free "done".
                    contributed_nothing(wt, newbase, rev)
                        .then(|| "no new commits over base (already merged?)".to_string()),
                ),
            };
            {
                let guard = store.lock().expect("poisoned");
                let _ = guard.set_branch_status(id, branch_id, status, detail.as_deref());
                if let Some(ref sid) = session_id {
                    let _ = guard.set_branch_resolver_session_id(id, branch_id, sid);
                }
            }
            // RAL-91: collapse this branch's commits to one when its project opts in.
            if squash {
                if let Err(e) = squash_review_commits(wt, newbase, feature_branch) {
                    fail_branch(store, id, branch_id, feature_branch, &e, &set_status);
                    return Err(());
                }
            }
            Ok(())
        }
        Err(e) => {
            fail_branch(store, id, branch_id, feature_branch, &e, &set_status);
            Err(())
        }
    }
}

/// Rebuild the combined review worktree at `prev_ref` and run the *deterministic*
/// check gates (see [`final_checks`] for the full RAL-110 precedence — this does
/// not attempt the AI-inferred build tier; that runs alongside
/// [`generate_manual_commands`] at each call site).
///
/// Returns an optional informational note for the `InReview` status: `Some(...)`
/// when the check gates were opted out (CCTL-130) or a config auto-build ran in
/// place of explicit checks (RAL-101), so the UI can distinguish those from a
/// plain "checks passed"; `None` when explicit checks ran and passed, or nothing
/// ran at all (no checks, no project auto-build default configured).
fn finalize_review(
    store: &Arc<Mutex<Store>>,
    root: &Workspace,
    wt_base: &Workspace,
    id: &str,
    prev_ref: &str,
) -> std::result::Result<Option<String>, String> {
    let combined_str = rebuild_combined(store, root, wt_base, id, prev_ref)?;
    final_checks(store, id, root, &combined_str)
}

pub(crate) fn remote_clone_url(root: &Path) -> std::result::Result<String, String> {
    let remote_name = crate::config::resolve_forge(root)
        .remote
        .unwrap_or_else(|| "origin".to_string());
    git(root, &["remote", "get-url", &remote_name]).map(|s| s.trim().to_string())
}

/// Run the review's check gates against the finished combined worktree.
///
/// Returns `Some(note)` when checks were opted out, or the config auto-build
/// (local or remote) ran (so the UI can show what happened), `None` when
/// explicit checks ran and passed or nothing ran at all, or `Err` on the
/// first failure.
fn final_checks(
    store: &Arc<Mutex<Store>>,
    id: &str,
    root: &Workspace,
    combined_str: &str,
) -> std::result::Result<Option<String>, String> {
    let (skip_auto_build, checks) = {
        let guard = store.lock().expect("poisoned");
        (
            guard.guardian_skip_auto_build(id).unwrap_or(false),
            guard.guardian_checks(id).unwrap_or_default(),
        )
    };

    if skip_auto_build {
        return Ok((!checks.is_empty()).then(|| "check gates skipped (opt-out)".to_string()));
    }
    if !checks.is_empty() {
        for cmd in &checks {
            if !root.at(combined_str).run_command(cmd).0 {
                return Err(format!("check failed: {cmd}"));
            }
        }
        return Ok(None);
    }
    // RAL-101: no explicit checks — fall back to the project's default
    // build/test command, if one is configured, so "in review" still means
    // "testable" rather than "merged and never built". If this isn't
    // configured either, `generate_manual_commands` (RAL-110) tries AI
    // inference next.
    match crate::config::resolve(root.root()).auto_build {
        Some(cmd) => {
            if !root.at(combined_str).run_command(&cmd).0 {
                return Err(format!("auto-build failed: {cmd}"));
            }
            Ok(Some(format!("auto-built via project default: {cmd}")))
        }
        None => Ok(None),
    }
}

/// Fetch a branch produced on another machine into this repository, so the
/// review can stack it (RAL-185 Phase 3b).
///
/// Per **D2** the daemon never *publishes* — deciding what to commit is
/// judgment, and a generic `add -A && commit && push` would sweep up build
/// artifacts and contradict the per-session control task files already
/// exercise. The task's own session is responsible for pushing. Fetching a
/// branch whose name and remote are both already known is the opposite: fully
/// deterministic, so it belongs here.
///
/// A no-op for a locally-produced branch, which is every pre-RAL-185 branch.
///
/// **This is the check that catches a task that never pushed.** Without it the
/// review would either fail deep inside `worktree add` with an opaque "invalid
/// reference" message, or — worse, when a *previous* run did push — quietly
/// stack that older revision and present a plausible but stale review. The
/// error names the branch, its machine, and the fact that publishing is the
/// task's own job.
///
/// Note the limit, honestly: this proves the branch *exists* on the remote,
/// not that it is the newest thing the task machine has. Detecting "pushed,
/// but stale" would require asking that machine for its own worktree HEAD,
/// which the provider contract has no verb for. The empty-branch check
/// ([`note_if_branch_is_empty`]) covers the common consequence — a branch that
/// contributes nothing — but a genuinely stale non-empty push is not detected
/// today.
fn fetch_branch_for_remote_session(
    store: &Arc<Mutex<Store>>,
    guardian_id: &str,
    branch: &crate::guardian::BranchView,
) -> std::result::Result<(), String> {
    let Some(machine) = branch
        .source_session_machine
        .as_deref()
        .map(str::trim)
        .filter(|m| !m.is_empty() && !m.eq_ignore_ascii_case(ralphus_core::schema::LOCAL_MACHINE))
    else {
        return Ok(());
    };
    let root = branch
        .project
        .clone()
        .or_else(|| {
            store
                .lock()
                .expect("poisoned")
                .get_guardian(guardian_id)
                .ok()
                .map(|g| g.git_root)
        })
        .ok_or_else(|| format!("branch {} has no project root to fetch into", branch.branch))?;
    let root = Path::new(&root);
    let remote = crate::config::resolve_forge(root)
        .remote
        .unwrap_or_else(|| "origin".to_string());
    let vcs = {
        let guard = store.lock().expect("poisoned");
        crate::vcs::for_project_root(&guard, root)?
    };

    if let Err(e) = vcs.fetch_branch(root, &remote, &branch.branch) {
        return Err(format!(
            "branch \"{}\" was produced on machine \"{machine}\" but could not be fetched from \
             \"{remote}\": {e}. The task that owns this branch is responsible for pushing it \
             before it completes — ralphus never commits or pushes on a session's behalf. Check \
             that session's output, confirm it pushed, then restart this review.",
            branch.branch
        ));
    }
    let sha = vcs.revision_of(root, &branch.branch).unwrap_or_default();
    crate::rlog!(
        INFO,
        "ralphus [guardian] review {guardian_id} fetched remote-produced branch {} from {remote} at {sha}",
        branch.branch
    );
    let guard = store.lock().expect("poisoned");
    crate::cartographer::Note::new("guardian")
        .guardian(guardian_id)
        .scope("guardian")
        .emit(
            &guard,
            format!("fetched remote-produced branch {}", branch.branch),
            serde_json::json!({
                "branch": branch.branch,
                "machine": machine,
                "remote": remote,
                "sha": sha,
            }),
        );
    Ok(())
}

/// Flag a branch whose *own, pre-rebase* commits added nothing over `upstream`
/// (RAL-190). Returns whether it is empty.
///
/// This is the check that catches the most quietly-wrong review there is: a
/// task whose session never committed produces a branch identical to its base,
/// which rebases perfectly and merges perfectly, so the review reaches
/// `in_review` looking entirely healthy while containing none of that task's
/// work. Nothing else in the pipeline notices — verify steps check the *code*,
/// not whether it was committed.
///
/// Deliberately compares the **original** feature branch (`branch`, untouched
/// by this build's rebase) against `upstream` (the same boundary the rebase
/// itself replayed from) rather than the post-rebase review ref. A branch
/// whose real commits are already present on the (possibly since-advanced)
/// stack tip rebases with `--empty=drop` dropping them as patch-equal, which
/// makes the *post*-rebase review ref diff-empty against the stack tip too —
/// but that is "already merged", not "never committed", and must not be
/// conflated with it (RAL-193): the task did commit, the work just predates
/// it in history now. Comparing the untouched original branch to the same
/// lower bound the rebase used sidesteps that entirely.
///
/// A *task* is allowed to produce no changes (a read-only analysis session, a
/// no-op run), but a branch sitting in a **review stack** is there to
/// contribute something — so the caller treats `true` as a merge failure, not
/// a warning. The escape hatch for a legitimately-empty branch is to disable
/// it (it stays visible in the stack and can be re-enabled), which is what the
/// failure message points at.
///
/// `git diff --quiet` exits 0 when there is no difference, 1 when there is;
/// any other outcome (a bad ref, git missing) leaves the flag alone and
/// reports `false` rather than guessing a review into failure.
fn note_if_branch_is_empty(
    store: &Arc<Mutex<Store>>,
    guardian_id: &str,
    branch_id: &str,
    branch: &str,
    root: &Workspace,
    upstream: &str,
) -> bool {
    // Through the VCS adapter, never a raw git call: a project may be
    // registered as something other than git, and this check runs on every
    // review (see `crate::vcs`).
    let vcs = {
        let guard = store.lock().expect("poisoned");
        match crate::vcs::for_project_root(&guard, root.root()) {
            Ok(v) => v,
            Err(e) => {
                crate::rlog!(
                    WARNING,
                    "ralphus [guardian] review {guardian_id} cannot check whether branch {branch} is empty: {e}"
                );
                return false;
            }
        }
    };
    // An unanswerable comparison must never fail a review on a guess, so it
    // reports "not empty" and leaves the stored flag untouched.
    let is_empty = match vcs.differs(root.root(), upstream, branch) {
        Ok(differs) => !differs,
        Err(_) => return false,
    };
    {
        let guard = store.lock().expect("poisoned");
        let _ = guard.set_branch_empty(guardian_id, branch_id, is_empty);
    }
    if !is_empty {
        return false;
    }
    crate::rlog!(
        ERROR,
        "ralphus [guardian] review {guardian_id} branch {branch} is empty — it adds no changes over the branch beneath it"
    );
    let guard = store.lock().expect("poisoned");
    crate::cartographer::Note::new("guardian")
        .guardian(guardian_id)
        .scope("guardian")
        .level(crate::logging::LogLevel::ERROR)
        .emit(
            &guard,
            format!("review branch {branch} is empty"),
            serde_json::json!({
                "branch": branch,
                "branch_id": branch_id,
                "reason": "no diff against the stack tip beneath it — its task likely never committed",
            }),
        );
    true
}

/// (Re)create the stable, read-only combined worktree at `prev_ref`, pointing at
/// the `review` branch (the head of the full stacked review).
fn rebuild_combined(
    store: &Arc<Mutex<Store>>,
    root: &Workspace,
    wt_base: &Workspace,
    id: &str,
    prev_ref: &str,
) -> std::result::Result<String, String> {
    let combined_branch = format!("guardian/{id}/review");
    let wt_name = format!("{id}-review");
    let combined_wt = wt_base.join(&wt_name);
    let combined_str = combined_wt.root().to_string_lossy().to_string();
    worktree_add_or_reset(root, &combined_branch, &combined_wt, prev_ref)?;
    let guard = store.lock().expect("poisoned");
    let _ = guard.set_guardian_review_branch(id, &combined_branch);
    let _ = guard.set_guardian_combined_worktree(id, &combined_str);
    Ok(combined_str)
}

/// Remove every review worktree/branch this guardian created previously, so a
/// re-merge starts from a clean slate. Worktrees are matched by the guardian id
/// appearing in their path. Branches are removed via `for-each-ref` covering the
/// current naming (`guardian/<id>/*`) and the legacy `guardian/<num>/*` scheme for
/// reviews built before RAL-63, plus the interim `review`/`review-*` names.
fn cleanup_review_worktrees(
    root: &Workspace,
    wt_base: &Workspace,
    id: &str,
    num: &str,
    old_review_branches: &[String],
) {
    let list = root
        .git(&["worktree", "list", "--porcelain"])
        .unwrap_or_default();
    for line in list.lines() {
        if let Some(path) = line.strip_prefix("worktree ") {
            let path = path.trim();
            if path.contains(id) {
                // Unlock first so that a locked worktree does not block removal.
                let _ = root.git(&["worktree", "unlock", path]);
                // Two --force flags handle dirty/untracked (first) and locked (second).
                let _ = root.git(&["worktree", "remove", "-f", "-f", path]);
            }
        }
    }
    let _ = root.git(&["worktree", "prune"]);
    wt_base.remove_path(".", true);
    // Migration (RAL-93): review worktrees used to live at the repo root under
    // .ralphus_guardian/<id>; they now live under .git/. Best-effort remove any
    // leftover from the old location so the two layouts don't both linger, then
    // drop the now-empty legacy parent (remove_dir is a no-op unless empty, so a
    // still-populated dir belonging to another guardian is left untouched).
    root.remove_path(root.root().join(".ralphus_guardian").join(id), true);
    root.remove_path(root.root().join(".ralphus_guardian"), true);
    for branch in old_review_branches {
        let _ = root.git(&["branch", "-D", branch]);
    }
    let refs = git(
        root.root(),
        &[
            "for-each-ref",
            "--format=%(refname:short)",
            // Current naming: guardian/<id>/wt-* and guardian/<id>/review.
            &format!("refs/heads/guardian/{id}"),
            &format!("refs/heads/guardian/{id}/*"),
            // Legacy pre-RAL-63 naming: guardian/<num>/b*.
            &format!("refs/heads/guardian/{num}"),
            &format!("refs/heads/guardian/{num}/*"),
            // Interim naming used between the two schemes: bare `review` and `review-*`.
            "refs/heads/review",
            "refs/heads/review-*",
        ],
    )
    .unwrap_or_default();
    for branch in refs.lines().map(str::trim).filter(|b| !b.is_empty()) {
        let _ = root.git(&["branch", "-D", branch]);
    }
}

/// Mark a branch failed and the guardian merge-failed with a reason.
fn fail_branch<F: Fn(GuardianStatus, Option<&str>)>(
    store: &Arc<Mutex<Store>>,
    id: &str,
    branch_id: &str,
    branch: &str,
    err: &str,
    set_status: &F,
) {
    let _ = store.lock().expect("poisoned").set_branch_status(
        id,
        branch_id,
        MergeStatus::Failed,
        Some(err),
    );
    set_status(
        GuardianStatus::MergeFailed,
        Some(&format!("branch {branch}: {err}")),
    );
}

/// How a branch's commits landed on the stack.
enum RebaseOutcome {
    /// Rebased with no conflicts, and no dedicated verify call ran (a true
    /// no-op, or Verify scope skipped it -- RAL-168).
    Clean,
    /// Rebased with no conflicts, but a dedicated verify call ran anyway
    /// (RAL-168 "each_branch"/"final_branch" scope, on a branch that
    /// contributed real changes). Carries the branch detail message to
    /// record, same shape as [`RebaseOutcome::Resolved`]'s -- still reported
    /// as `Done`, not `ConflictResolved`, since no conflict actually occurred.
    CleanVerified(String),
    /// Rebased after the agent resolved conflicts (and, per Verify scope,
    /// possibly ran the RAL-149 final-verification call). Carries the branch
    /// detail message to record (e.g. "resolved by agent; final verification
    /// passed/failed: ...", or "...skipped (Verify scope)").
    Resolved(String),
}

/// List candidate base branches for a guardian, scoped to the remote that owns
/// the current `base_branch`.  If `base_branch` looks like a remote-tracking ref
/// (e.g. `origin/main`) only refs under that remote are returned; otherwise local
/// branches are returned.  Returns an empty list on any git error.
pub fn list_base_branches(git_root: &str, base_branch: &str) -> Vec<String> {
    let root = Path::new(git_root);
    let ref_prefix = if let Some(slash) = base_branch.find('/') {
        let remote = &base_branch[..slash];
        format!("refs/remotes/{remote}/")
    } else {
        "refs/heads/".to_string()
    };
    match git(
        root,
        &["for-each-ref", "--format=%(refname:short)", &ref_prefix],
    ) {
        Ok(s) => s
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// Resolve `base_branch` (a branch name — mutable, may be local or a remote
/// tracking ref) to the immutable commit it currently points at, so a single
/// build snapshots one base and a later shift is detectable. Returns the short-ish
/// full SHA, or an error if the ref does not resolve.
pub(crate) fn resolve_base(
    root: &Workspace,
    base_branch: &str,
) -> std::result::Result<String, String> {
    let spec = format!("{base_branch}^{{commit}}");
    Ok(root
        .git(&["rev-parse", "--verify", &spec])?
        .trim()
        .to_string())
}

/// Rebase the checked-out review branch's own commits (`base_sha..HEAD-of-branch`)
/// onto `newbase`, driving the agent through any conflicts. The worktree must
/// already be checked out on the branch/commit being rebased.
///
/// `branch_arg` is what git rebases: the review branch name, or `"HEAD"` for the
/// detached shared-worktree path. Returns the outcome, or an error after aborting
/// the rebase when it cannot be completed.
#[allow(clippy::too_many_arguments)]
fn drive_rebase(
    store: &Arc<Mutex<Store>>,
    id: &str,
    position: i64,
    branch_id: &str,
    runner: &dyn Runner,
    feature: &str,
    wt: &Workspace,
    newbase: &str,
    base_sha: &str,
    branch_arg: &str,
    gate: &VerifyGate,
) -> std::result::Result<(RebaseOutcome, Option<String>), String> {
    // Remove untracked files before rebasing. `git rebase --onto <newbase>` fails
    // with "untracked working tree files would be overwritten by checkout" when the
    // worktree contains a file that is tracked in `newbase` but untracked here —
    // a common leftover from a prior agent session that didn't stage everything.
    // This is especially likely on Windows where a CWD lock prevents
    // `ensure_worktree` from deleting and recreating the directory cleanly.
    let _ = wt.git(&["clean", "-fd"]);

    // `--empty=drop` discards commits already present on `newbase` (patch-equal),
    // which is exactly why rebase — not a range cherry-pick — is used here: a
    // shared or already-merged commit is dropped instead of halting the stack.
    let args = [
        "rebase",
        "--onto",
        newbase,
        "--empty=drop",
        "--no-fork-point",
        base_sha,
        branch_arg,
    ];
    match wt.git(&args) {
        Ok(_) => {
            // RAL-168: "each_branch" (without auto-clean-skip) or
            // "final_branch" (on the last branch) also verifies a branch that
            // rebased cleanly -- not just one whose conflicts the agent
            // resolved -- as long as it actually contributed real changes (a
            // true no-op always skips verify, no setting needed).
            let nothing = contributed_nothing(wt, newbase, branch_arg);
            if nothing || !gate.allows_for_clean_branch() {
                return Ok((RebaseOutcome::Clean, None));
            }
            let (agent, model) = resolver_backend(store, id);
            let (quality_note, ghost_prefix) =
                verify_extras(store, id, feature, branch_id, runner, &agent, &model);
            let (verify_session_id, verify_detail) = run_final_verify(
                store,
                id,
                position,
                branch_id,
                runner,
                wt,
                feature,
                &agent,
                &model,
                &quality_note,
                &ghost_prefix,
            );
            Ok((
                RebaseOutcome::CleanVerified(verify_detail),
                verify_session_id,
            ))
        }
        Err(e) => {
            let conflicts = conflicted_files(wt);
            if conflicts.is_empty() && !rebase_in_progress(wt) {
                // Genuine failure with no conflict to resolve (e.g. a bad ref): abort clean.
                let _ = wt.git(&["rebase", "--abort"]);
                Err(e)
            } else {
                if conflicts.is_empty() {
                    // rerere.autoupdate staged every conflicted file automatically —
                    // the rebase is still paused but the index is already clean.
                    // The resolver loop drives it forward without invoking an agent.
                    crate::rlog!(
                        INFO,
                        "ralphus [guardian] review {id} rerere-autoupdate fast-path \
                         branch={feature:?} (staged by rerere, no agent needed)"
                    );
                    let guard = store.lock().expect("poisoned");
                    let _ = guard.cartographer_log(crate::cartographer::CartographerEntry {
                        level: crate::logging::LogLevel::INFO,
                        source: "guardian",
                        message: "rerere-autoupdate fast-path (staged by rerere, no agent needed)",
                        scope: Some("branch"),
                        run_id: None,
                        guardian_id: Some(id),
                        session_id: None,
                        task: None,
                        payload: serde_json::json!({"branch": feature}),
                    });
                }
                let (agent, model) = resolver_backend(store, id);
                match resolve_conflicts_with_agent(
                    store, id, position, branch_id, runner, wt, feature, &agent, &model, gate,
                ) {
                    Ok((session_id, verify_detail)) => {
                        Ok((RebaseOutcome::Resolved(verify_detail), session_id))
                    }
                    Err(re) => {
                        let _ = wt.git(&["rebase", "--abort"]);
                        Err(re)
                    }
                }
            }
        }
    }
}

/// RAL-91: collapse the review branch's own commits (`newbase..HEAD`) into a
/// single commit when per-project squashing is enabled. The worktree must be
/// checked out on the review branch (or detached) at the tip that was just
/// rebased onto `newbase`.
///
/// No-op unless there are 2+ commits over `newbase`, so a branch that already
/// landed as one commit — or contributed nothing — is left untouched. The
/// squashed commit preserves the feature branch name and the original subject
/// lines in its message. `reset --soft` rewinds the branch/HEAD to the stack tip
/// while keeping the fully-merged tree staged; a single commit then re-lands all
/// the changes. On a commit failure the pre-squash state is restored via
/// `ORIG_HEAD` so the stack is never left with a dirty index.
fn squash_review_commits(
    wt: &Workspace,
    newbase: &str,
    feature: &str,
) -> std::result::Result<(), String> {
    let range = format!("{newbase}..HEAD");
    let count = wt
        .git(&["rev-list", "--count", &range])
        .ok()
        .and_then(|s| s.trim().parse::<usize>().ok())
        .unwrap_or(0);
    if count <= 1 {
        return Ok(());
    }
    let subjects = wt
        .git(&["log", "--reverse", "--format=%s", &range])
        .unwrap_or_default();
    let body: String = subjects
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(|l| format!("- {l}"))
        .collect::<Vec<_>>()
        .join("\n");
    let msg = format!("{feature} (squashed {count} commits)\n\n{body}");
    wt.git(&["reset", "--soft", newbase])?;
    if let Err(e) = wt.git(&["commit", "--no-verify", "-m", &msg]) {
        // Restore the pre-squash tip so the stack is not left in a dirty state.
        let _ = wt.git(&["reset", "--soft", "ORIG_HEAD"]);
        return Err(e);
    }
    crate::rlog!(
        INFO,
        "ralphus [guardian] squashed branch={feature:?} {count} commits → 1 over {newbase}"
    );
    Ok(())
}

/// Whether a just-built review branch (`rev`) contributed no commits over
/// `newbase` — i.e. all of the feature's changes were already present. Returned
/// as a branch detail so a silently-empty stack entry is surfaced, not hidden.
fn contributed_nothing(wt: &Workspace, newbase: &str, rev: &str) -> bool {
    let range = format!("{newbase}..{rev}");
    wt.git(&["rev-list", "--count", &range])
        .ok()
        .and_then(|c| c.trim().parse::<i64>().ok())
        .is_some_and(|n| n == 0)
}

/// RAL-103: Recompute a preliminary, git-log-only change summary from each
/// not-yet-reviewed ready branch's OWN task worktree (the source session's
/// `cwd` — never a review-owned worktree). Unlike [`generate_summary`], this
/// never calls an LLM, so it is cheap enough to recompute synchronously every
/// time another branch reaches `Ready`, while the guardian is still
/// `collecting` (before any review worktree exists for those branches).
///
/// A branch stops contributing here — and starts being covered by
/// [`generate_summary`]'s agent-authored final summary instead — once it has
/// a review worktree (`BranchView.worktree.is_some()`), i.e. once its stacked
/// rebase has run at least once.
///
/// A no-op (leaves `change_summary` untouched) when no qualifying branch has
/// any commits to show — that "nothing ready yet" state is instead surfaced
/// by `summary_state == "waiting"`.
///
/// RAL-121: only the DB reads/writes below take the store lock; the `git log`
/// subprocess calls run with it released. This used to run entirely under a
/// lock held by the caller (the scheduler's task-completion handler), which
/// meant every other daemon request — including a review page's own reads —
/// blocked on the store mutex for the duration of one `git log` per branch.
/// Called from a [`crate::summary_worker::SummaryQueue`] background worker,
/// never inline on a request or scheduler thread.
pub(crate) fn recompute_preliminary_summary(store: &Arc<Mutex<Store>>, id: &str) {
    struct Candidate {
        branch: String,
        cwd: String,
    }
    let (base_branch, candidates) = {
        let guard = store.lock().expect("store mutex poisoned");
        let Ok(guardian) = guard.get_guardian(id) else {
            return;
        };
        let candidates = guardian
            .branches
            .iter()
            .filter(|b| b.enabled && b.worktree.is_none() && b.merge_status != "pending")
            .filter_map(|b| {
                guard
                    .session_cwd_for_branch(&b.branch)
                    .ok()
                    .flatten()
                    .map(|cwd| Candidate {
                        branch: b.branch.clone(),
                        cwd,
                    })
            })
            .collect::<Vec<_>>();
        (guardian.base_branch, candidates)
    };

    // RAL-147: each candidate's worktree HEAD is built on top of every
    // earlier branch in the stack (a stacked review rebases branch N onto
    // branch N-1), so diffing every branch against the shared `base_branch`
    // makes each subsequent section accumulate all prior branches' commits
    // too. Diff against a running `prev_sha` instead -- seeded to
    // `base_branch`, then advanced to each candidate's own HEAD after it's
    // processed -- mirroring `generate_summary`'s `prev..branch_ref` dedup.
    let mut sections: Vec<String> = Vec::new();
    let mut prev_sha = base_branch.clone();
    for c in &candidates {
        let log = git(
            Path::new(&c.cwd),
            &["log", "--format=%s", &format!("{prev_sha}..HEAD")],
        )
        .unwrap_or_default();
        let log = log.trim();
        if !log.is_empty() {
            sections.push(format!("{}:\n{log}", c.branch));
        }
        if let Ok(head_sha) =
            Workspace::for_guardian(store, id, Path::new(&c.cwd)).git(&["rev-parse", "HEAD"])
        {
            prev_sha = head_sha.trim().to_string();
        }
    }
    if sections.is_empty() {
        return;
    }
    // No agent/model recorded — this is plain git-log formatting, not an LLM
    // call, which distinguishes a preliminary summary from a final one.
    let guard = store.lock().expect("store mutex poisoned");
    let _ = guard.set_guardian_summary(id, &sections.join("\n\n"), None, None);
}

/// RAL-124: extract a `TICKET-123`-style label from the start of a branch
/// name (e.g. `RAL-124-bullet_change_summary` -> `RAL-124`), used to label
/// that branch's bullet in a bullet-format change summary. Falls back to the
/// full branch name when it doesn't start with `<letters>-<digits>`.
fn branch_summary_label(branch: &str) -> String {
    let mut parts = branch.splitn(3, '-');
    if let (Some(prefix), Some(number)) = (parts.next(), parts.next()) {
        let is_ticket = !prefix.is_empty()
            && prefix.chars().all(|c| c.is_ascii_alphabetic())
            && !number.is_empty()
            && number.chars().all(|c| c.is_ascii_digit());
        if is_ticket {
            return format!("{prefix}-{number}");
        }
    }
    branch.to_string()
}

/// Generate the cross-branch change summary for a guardian using the resolver
/// agent. Called after the stack is fully assembled (`is_final=true` organises
/// output per-branch when individual refs exist; otherwise uses the combined log).
/// A single call covers every branch in `completed` at once (never one call per
/// branch), so the agent can synthesize/collapse redundant items across branches.
///
/// The result is stored as `change_summary` on the guardian and surfaced in the
/// review detail pane. Failures are silent — a missing summary is better than a
/// crashed merge thread.
///
/// RAL-53: uses commit subject lines only (no diffs) so the output describes
/// developer intent rather than low-level file changes.
///
/// RAL-124: whether the output is a one-bullet-per-branch list (default) or the
/// original prose paragraph is controlled by `[review] summary_format` in
/// `.ralphus.toml`/global config (see [`crate::config::ReviewConfig::bullet_summary`]).
/// In bullet mode, each branch is labelled with [`branch_summary_label`] --
/// its ticket id when the branch name starts with one, else the branch name
/// itself -- and the agent is instructed to use that exact label per bullet.
#[allow(clippy::too_many_arguments)]
fn generate_summary(
    store: &Arc<Mutex<Store>>,
    runner: &dyn Runner,
    id: &str,
    root: &Workspace,
    base_sha: &str,
    completed: &[(i64, String)],
    tip_ref: &str,
    is_final: bool,
) {
    if completed.is_empty() {
        return;
    }

    // Collect per-branch subject lines using the per-branch review refs
    // (`guardian/<id>/wt-<branch>`). Falls back to the combined log if the refs
    // don't exist (e.g. skip-worktrees path, where all branches share one ref).
    let (has_per_branch_refs, context) = if is_final {
        let first_ref = completed
            .first()
            .map(|(_, branch)| format!("guardian/{id}/wt-{branch}"));
        let has = first_ref.is_some_and(|r| root.git(&["rev-parse", "--verify", &r]).is_ok());
        if has {
            let mut prev = base_sha.to_string();
            let mut lines: Vec<String> = Vec::new();
            for (_, branch_name) in completed {
                let branch_ref = format!("guardian/{id}/wt-{branch_name}");
                let b_log = git(
                    root.root(),
                    &["log", "--format=%s", &format!("{prev}..{branch_ref}")],
                )
                .unwrap_or_default();
                if !b_log.trim().is_empty() {
                    let label = branch_summary_label(branch_name);
                    lines.push(format!("{label}:\n{b_log}"));
                }
                prev = branch_ref;
            }
            let ctx = if lines.is_empty() {
                return;
            } else {
                lines.join("\n\n")
            };
            (true, ctx)
        } else {
            (false, String::new())
        }
    } else {
        (false, String::new())
    };

    let branch_labels = completed
        .iter()
        .map(|(_, n)| branch_summary_label(n))
        .collect::<Vec<_>>()
        .join(", ");

    let context = if has_per_branch_refs {
        context
    } else {
        let log = git(
            root.root(),
            &["log", "--format=%s", &format!("{base_sha}..{tip_ref}")],
        )
        .unwrap_or_default();
        if log.trim().is_empty() {
            return;
        }
        format!("Branches: [{branch_labels}]\n\n{log}")
    };

    let (agent, model) = {
        let guard = store.lock().expect("poisoned");
        let g = guard.get_guardian(id).ok();
        let stored_agent = g.as_ref().and_then(|g| g.resolver_agent.clone());
        let stored_model = g.and_then(|g| g.resolver_model.clone());
        let a = resolver_agent(stored_agent.as_deref());
        let m = resolver_model(stored_model.as_deref(), &a);
        (a, m)
    };
    let cwd = root.root().to_string_lossy().into_owned();

    let prompt = if crate::config::resolve(root.root()).bullet_summary() {
        format!(
            "You are summarising a stacked code review made up of the branches \
             [{branch_labels}]. The following are commit subject lines for each \
             branch — one line per commit. Write the summary as a bullet list \
             with EXACTLY one bullet per branch, each on its own line in the \
             form `- <label>: <description>`, where <label> is exactly one of \
             the branch labels given above (do not invent or reformat it). \
             Each description must be a single concise line focused on \
             developer intent, not file-level details. You may simplify or \
             collapse redundant detail within a bullet, but every branch \
             listed above must be represented by exactly one bullet. Respond \
             with ONLY the bullet list — no preamble, no trailing remarks.\
             \n\n{context}"
        )
    } else {
        format!(
            "You are summarising a stacked code review. The following are commit \
             subject lines for branches [{branch_labels}] — one line per commit. \
             Write a compact 2-3 sentence summary in plain language describing \
             what was changed and why. Focus on developer intent, not file-level \
             details.\n\n{context}"
        )
    };
    let spec = RunnerSpec {
        // RAL-102: unique per guardian — a bare "guardian" run_id collides
        // with every other guardian's tmux session name (observed in CI as
        // cross-test contamination when two live-Ollama tests generate a
        // summary concurrently and clobber each other's tmux session).
        run_id: format!("guardian-{id}"),
        task: "summary".to_string(),
        session_id: "summarizer".to_string(),
        cwd,
        prompt: Some(prompt),
        command: None,
        agent: agent.clone(),
        model: model.clone(),
        system_prompt: None,
        system_prompt_position: None,
        timeout_sec: None,
        budget_tokens: None,
        maximum_budget_usd: None,
        verify: false,
        trace_context: None,
        resume_agent_session_id: None,
        env_overrides: std::collections::BTreeMap::new(),
        machine: None,
    };
    let result = runner.run(&spec);
    if result.is_done() && !result.summary.trim().is_empty() {
        // RAL-88: record which resolved agent/model produced this summary.
        let _ = store.lock().expect("poisoned").set_guardian_summary(
            id,
            &result.summary,
            Some(agent.as_str()),
            model.as_deref(),
        );
    }
}

/// A single named input inside the RAL-164 structured manual-check shape --
/// mirrors [`CheckInput`], kept separate so the wire format asked of the LLM
/// (loose/untrusted) doesn't leak straight into the persisted type.
#[derive(Deserialize)]
struct ManualCheckInputItem {
    name: String,
    message: String,
    #[serde(default)]
    default: String,
}

impl From<ManualCheckInputItem> for CheckInput {
    fn from(i: ManualCheckInputItem) -> Self {
        Self {
            name: i.name,
            message: i.message,
            default: i.default,
        }
    }
}

/// One element of the `manual_commands` array asked of the LLM (RAL-164): a
/// plain command string (no variable/colliding values, back-compat with
/// small local models and the pre-RAL-164 shape), or a structured object
/// naming inputs referenced in `command`/`cleanup_command` as `{name}`
/// placeholders.
#[derive(Deserialize)]
#[serde(untagged)]
enum ManualCheckItem {
    Command(String),
    Structured {
        command: String,
        #[serde(default)]
        cleanup_command: Option<String>,
        #[serde(default)]
        inputs: Vec<ManualCheckInputItem>,
    },
}

impl From<ManualCheckItem> for GuardianCheck {
    fn from(item: ManualCheckItem) -> Self {
        match item {
            ManualCheckItem::Command(command) => Self {
                label: None,
                command: Some(command),
                prompt: None,
                cleanup_command: None,
                inputs: Vec::new(),
            },
            ManualCheckItem::Structured {
                command,
                cleanup_command,
                inputs,
            } => Self {
                label: None,
                command: Some(command),
                prompt: None,
                cleanup_command,
                inputs: inputs.into_iter().map(CheckInput::from).collect(),
            },
        }
    }
}

/// JSON shape asked of the resolver agent when it also needs to infer a build
/// command (RAL-110) — see [`generate_manual_commands`]. `build_command` is a
/// single shell command that prepares the worktree (compile/bundle/install) so
/// the proposed `manual_commands` are fast to run by hand, or `null` when
/// nothing needs pre-building.
#[derive(Deserialize)]
struct ManualChecksInference {
    manual_commands: Vec<ManualCheckItem>,
    #[serde(default)]
    build_command: Option<String>,
}

/// Parse the resolver agent's manual-commands response. Tries the RAL-110
/// `{"manual_commands": [...], "build_command": ...}` object shape first (with
/// a `{...}`-substring fallback for chatty models), then falls back to the
/// original bare `["...", ...]` array shape (no build command) for backward
/// compatibility and for small local models that ignore the object-shape
/// instruction. Each `manual_commands` element is either a bare string or a
/// RAL-164 structured object (see [`ManualCheckItem`]). Returns `(checks,
/// build_command)`; both empty/`None` when nothing parseable was found.
fn parse_manual_commands_response(text: &str) -> (Vec<GuardianCheck>, Option<String>) {
    let as_object = serde_json::from_str::<ManualChecksInference>(text)
        .ok()
        .or_else(|| {
            let start = text.find('{')?;
            let end = text.rfind('}').unwrap_or(text.len().saturating_sub(1));
            serde_json::from_str::<ManualChecksInference>(&text[start..=end]).ok()
        });
    if let Some(obj) = as_object {
        return (
            obj.manual_commands.into_iter().map(Into::into).collect(),
            obj.build_command.filter(|s| !s.trim().is_empty()),
        );
    }
    let as_array = serde_json::from_str::<Vec<ManualCheckItem>>(text)
        .ok()
        .or_else(|| {
            let start = text.find('[')?;
            let end = text.rfind(']').unwrap_or(text.len().saturating_sub(1));
            serde_json::from_str::<Vec<ManualCheckItem>>(&text[start..=end]).ok()
        });
    (
        as_array
            .unwrap_or_default()
            .into_iter()
            .map(Into::into)
            .collect(),
        None,
    )
}

/// Shared prompt body for [`generate_manual_commands`]. `ask_build` selects
/// between the plain bare-array instruction and the RAL-110 JSON-object
/// instruction that also asks for an inferred build command.
fn manual_commands_prompt(tail: &str, ask_build: bool) -> String {
    let focus = "You are preparing a code review. Based on the changed files and commit \
         messages below, produce 1-5 shell command strings that a human reviewer should \
         run to manually verify these changes. Focus on hands-on, observable steps: \
         launching the app and inspecting it visually, running a build script, or \
         exercising a CLI feature by hand. Do NOT suggest unit tests or automated checks \
         that could be scripted — the goal is human eyes and hands on the actual result. \
         If a command depends on a value that could vary or collide between runs -- a \
         port number, a file path, a branch name, anything where running the same \
         command twice concurrently would conflict -- do NOT hardcode it. Instead make \
         that element an object: {\"command\": \"...{name}...\", \"inputs\": [{\"name\": \
         \"...\", \"message\": \"shown to the user\", \"default\": \"...\"}]}, using a \
         `{name}` placeholder in \"command\" for each declared input. When such a command \
         leaves something running that a rerun would collide with (e.g. a bound port, a \
         background process), also set \"cleanup_command\" on that same object to a \
         command that stops/frees it first. A command with nothing variable and nothing \
         left running can stay a plain string.";
    let format = if ask_build {
        " Also decide whether a separate build/compile/bundle/install step must run \
          first so those commands are fast to use once a human runs them (e.g. \
          `cargo build --release`, `npm install && npm run build`) — if so, put that \
          single shell command in \"build_command\"; otherwise use null. Return ONLY a \
          valid JSON object of the shape {\"manual_commands\": [...], \"build_command\": \
          \"...\"|null}, where each element of \"manual_commands\" is either a plain \
          string or the object shape described above — no markdown fences, no \
          explanation, no other text."
    } else {
        " Return ONLY a valid JSON array where each element is either a plain string or \
          the object shape described above — no markdown fences, no explanation, no \
          other text."
    };
    format!("{focus}{format}\n\n{tail}")
}

/// Generate LLM-suggested shell commands for manually testing or verifying the
/// changes in the review branch (RAL-27), and — when nothing already covers
/// finalize-time verification (see [`final_checks`]'s precedence) — an
/// AI-inferred build command run against the combined worktree in advance, so
/// a human pressing "Run all" on the manual checks sees an already-built
/// worktree instead of eating a cold build live (RAL-110). Both come from the
/// same LLM call: the build a manual check needs is exactly the kind of thing
/// the model is already inferring context for. Manual-commands generation
/// failures are silent — a missing command list is better than a crash; an
/// inferred build that runs and fails is recorded in the returned note rather
/// than failing the whole finalize (unlike explicit checks/config auto_build,
/// since this tier is a guess, not something the user configured).
///
/// `worktree` is the combined review worktree path (preferred). When present the
/// LLM runs inside the worktree with its `run_bash` tool so it can inspect the
/// diff itself — no diff content is embedded in the prompt, which avoids OS
/// command-line length limits in harness backends. Falls back to a file-name
/// list from `root` when no worktree is available (in which case AI-build
/// inference/execution never happens — there is nowhere safe to build).
///
/// Returns `Some(note)` when an inferred build ran (or failed trying) so the
/// caller can fold it into the `InReview` status detail alongside
/// [`final_checks`]'s note; `None` otherwise.
fn generate_manual_commands(
    store: &Arc<Mutex<Store>>,
    runner: &dyn Runner,
    id: &str,
    root: &Workspace,
    base_sha: &str,
    tip_ref: &str,
    worktree: Option<&Workspace>,
) -> Option<String> {
    // RAL-110: only attempt AI build inference/execution when nothing else
    // already covers finalize-time verification and there's a worktree to
    // build in — mirrors `final_checks`'s own precedence so the two never
    // double-build regardless of call-site ordering.
    let (skip_auto_build, has_explicit_checks) = {
        let guard = store.lock().expect("poisoned");
        (
            guard.guardian_skip_auto_build(id).unwrap_or(false),
            !guard.guardian_checks(id).unwrap_or_default().is_empty(),
        )
    };
    let has_config_auto_build = crate::config::resolve(root.root()).auto_build.is_some();
    let skip_ai_build =
        worktree.is_none() || skip_auto_build || has_explicit_checks || has_config_auto_build;

    let (cwd, prompt) = if let Some(wt) = worktree {
        // Worktree path: embed only the --stat output (always compact — one line
        // per changed file). Never embed the full diff; it can be arbitrarily
        // large and would blow OS command-line limits in harness backends.
        let stat = wt.git(&["diff", "--stat", base_sha]).unwrap_or_default();
        if stat.trim().is_empty() {
            return None;
        }
        let log = git(
            root.root(),
            &["log", "--format=%s", &format!("{base_sha}..{tip_ref}")],
        )
        .unwrap_or_default();
        let tail = format!("Changed files (stat):\n{stat}\n\nCommit messages:\n{log}");
        (
            wt.root().to_string_lossy().into_owned(),
            manual_commands_prompt(&tail, !skip_ai_build),
        )
    } else {
        // Fallback: list changed file names from the repository root. The file
        // list is always small, so it is safe to embed directly.
        let files = match git(
            root.root(),
            &["diff", "--name-only", &format!("{base_sha}..{tip_ref}")],
        ) {
            Ok(s) if !s.trim().is_empty() => s,
            _ => return None,
        };
        let log = git(
            root.root(),
            &["log", "--format=%s", &format!("{base_sha}..{tip_ref}")],
        )
        .unwrap_or_default();
        let tail = format!("Changed files:\n{files}\n\nCommit messages:\n{log}");
        (
            root.root().to_string_lossy().into_owned(),
            manual_commands_prompt(&tail, false),
        )
    };

    let (agent, model) = {
        let guard = store.lock().expect("poisoned");
        let g = guard.get_guardian(id).ok();
        let stored_agent = g.as_ref().and_then(|g| g.resolver_agent.clone());
        let stored_model = g.and_then(|g| g.resolver_model.clone());
        let a = resolver_agent(stored_agent.as_deref());
        let m = resolver_model(stored_model.as_deref(), &a);
        (a, m)
    };

    let spec = RunnerSpec {
        // RAL-102/RAL-88 follow-up: unique per guardian (see the comment on
        // the resolver `RunnerSpec` in `resolve_conflicts_with_agent`) so this
        // generation's tmux session never collides with another guardian's.
        run_id: format!("guardian-{id}"),
        task: MANUAL_COMMANDS_TASK.to_string(),
        session_id: MANUAL_COMMANDS_SESSION.to_string(),
        cwd,
        prompt: Some(prompt),
        command: None,
        agent: agent.clone(),
        model: model.clone(),
        system_prompt: None,
        system_prompt_position: None,
        timeout_sec: None,
        budget_tokens: None,
        maximum_budget_usd: None,
        verify: false,
        trace_context: None,
        resume_agent_session_id: None,
        env_overrides: std::collections::BTreeMap::new(),
        machine: None,
    };

    // Side-channel file where the Python backend writes the claude session ID as
    // soon as the stream-json init event arrives — before generation completes.
    // Mirrors `resolve_conflicts_with_agent`'s watcher, so the "Open Agent"
    // terminal action becomes available while generation is still running,
    // not only once it finishes.
    let sid_path = std::env::temp_dir()
        .join("ralphus")
        .join(format!("guardian-{id}-manual.live_session"));
    let _ = std::fs::remove_file(&sid_path);
    let stop = Arc::new(AtomicBool::new(false));
    let stop_clone = Arc::clone(&stop);
    let store_clone = Arc::clone(store);
    let id_str = id.to_string();
    let sid_path_clone = sid_path.clone();
    let watcher = std::thread::spawn(move || {
        while !stop_clone.load(Ordering::Relaxed) {
            if let Ok(raw) = std::fs::read_to_string(&sid_path_clone) {
                let sid = raw.trim();
                if !sid.is_empty() {
                    let guard = store_clone.lock().expect("poisoned");
                    let _ = guard.set_guardian_manual_commands_session_id(&id_str, sid);
                    break;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
    });

    let result = runner.run(&spec);

    stop.store(true, Ordering::Relaxed);
    let _ = watcher.join();

    if let Some(sid) = result.agent_session_id.as_deref() {
        let _ = store
            .lock()
            .expect("poisoned")
            .set_guardian_manual_commands_session_id(id, sid);
    }

    if !result.is_done() || result.summary.trim().is_empty() {
        return None;
    }

    let (commands, build_command) = parse_manual_commands_response(result.summary.trim());

    if !commands.is_empty() {
        // RAL-88: record which resolved agent/model produced these commands.
        let _ = store
            .lock()
            .expect("poisoned")
            .set_guardian_manual_commands(id, &commands, Some(agent.as_str()), model.as_deref());
    }

    if skip_ai_build {
        return None;
    }
    let cmd = build_command?;
    let wt = worktree?.clone();
    let ok = wt.run_command(&cmd).0;
    let _ =
        store
            .lock()
            .expect("poisoned")
            .cartographer_log(crate::cartographer::CartographerEntry {
                level: if ok {
                    crate::logging::LogLevel::INFO
                } else {
                    crate::logging::LogLevel::WARNING
                },
                source: "guardian",
                message: if ok {
                    "auto-build (AI-inferred) succeeded"
                } else {
                    "auto-build (AI-inferred) failed"
                },
                scope: Some("guardian"),
                run_id: None,
                guardian_id: Some(id),
                session_id: None,
                task: None,
                payload: serde_json::json!({"command": cmd}),
            });
    Some(if ok {
        format!("auto-built via inferred build command: {cmd}")
    } else {
        format!("auto-build failed: {cmd}")
    })
}

/// Prompt asked of the resolver agent for "set it for me" (RAL-164): propose
/// a concrete value for one named [`CheckInput`], given the command that
/// references it.
fn resolve_input_prompt(command: &str, input: &CheckInput) -> String {
    format!(
        "A reviewer is about to run this shell command as part of manually verifying a \
         code review:\n\n{command}\n\nIt references a value named \"{}\" ({}). The current \
         default is \"{}\". Propose a good concrete value for \"{}\" for this run. Respond \
         with ONLY the value itself -- no explanation, no quotes, no markdown, nothing else.",
        input.name, input.message, input.default, input.name
    )
}

/// Resolve a named [`CheckInput`]'s value via the resolver agent ("set it
/// for me", RAL-164). Meant to be called from a freshly spawned background
/// thread — mirrors [`generate_manual_commands`]'s own call sites — *after*
/// the HTTP handler has already won the atomic claim via
/// [`Store::claim_guardian_input_resolution`]; this function only resolves
/// and records, it does not claim. Records `failed` on any error (LLM call
/// errored, timed out, or returned nothing usable) so the UI never shows a
/// permanently-stuck spinner; on success also folds the value into
/// [`Store::merge_guardian_input_values`] so it becomes the new default.
pub(crate) fn resolve_check_input(
    store: &Arc<Mutex<Store>>,
    runner: &dyn Runner,
    guardian_id: &str,
    command: &str,
    input: &CheckInput,
) {
    let Some((cwd, agent, model)) = ({
        let guard = store.lock().expect("poisoned");
        guard.get_guardian(guardian_id).ok().map(|g| {
            let cwd = g.combined_worktree.clone().unwrap_or(g.git_root.clone());
            let a = resolver_agent(g.resolver_agent.as_deref());
            let m = resolver_model(g.resolver_model.as_deref(), &a);
            (cwd, a, m)
        })
    }) else {
        let guard = store.lock().expect("poisoned");
        let _ = guard.set_guardian_input_resolution_failed(guardian_id, &input.name);
        return;
    };

    let spec = RunnerSpec {
        // Unique per (guardian, input) so concurrent resolutions for
        // different inputs on the same guardian -- or the guardian's own
        // manual-commands generation -- never collide on one tmux session.
        run_id: format!("guardian-{guardian_id}-input-{}", input.name),
        task: RESOLVE_INPUT_TASK.to_string(),
        session_id: format!("resolve-input-{}", input.name),
        cwd,
        prompt: Some(resolve_input_prompt(command, input)),
        command: None,
        agent,
        model,
        system_prompt: None,
        system_prompt_position: None,
        timeout_sec: None,
        budget_tokens: None,
        maximum_budget_usd: None,
        verify: false,
        trace_context: None,
        resume_agent_session_id: None,
        env_overrides: std::collections::BTreeMap::new(),
        machine: None,
    };

    let result = runner.run(&spec);
    let guard = store.lock().expect("poisoned");
    if !result.is_done() || result.summary.trim().is_empty() {
        let _ = guard.set_guardian_input_resolution_failed(guardian_id, &input.name);
        return;
    }

    // Small/chatty models sometimes wrap the value in quotes or pad it with
    // a trailing sentence -- take just the first non-empty line, stripped of
    // surrounding quotes, as the value.
    let value = result
        .summary
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or_default()
        .trim()
        .trim_matches(|c| c == '"' || c == '\'')
        .to_string();

    if value.is_empty() {
        let _ = guard.set_guardian_input_resolution_failed(guardian_id, &input.name);
    } else {
        let _ = guard.set_guardian_input_resolution_ready(guardian_id, &input.name, &value);
        let _ = guard.merge_guardian_input_values(
            guardian_id,
            &std::collections::HashMap::from([(input.name.clone(), value)]),
        );
    }
}

fn reply(status: u16, body: &str) -> Reply {
    Reply {
        status,
        body: body.to_string(),
    }
}

fn error_body(code: &str, message: &str) -> String {
    format!(
        "{{\"error\":{{\"code\":\"{code}\",\"message\":\"{}\"}}}}",
        message.replace('"', "'")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::RunnerResult;
    use std::sync::atomic::{AtomicU32, Ordering};

    // -----------------------------------------------------------------------
    // Helpers shared by worktree-recovery tests
    // -----------------------------------------------------------------------

    static TEST_N: AtomicU32 = AtomicU32::new(0);

    fn tmp_dir(tag: &str) -> PathBuf {
        let n = TEST_N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("wt-rec-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir tmp_dir");
        dir
    }

    fn g(root: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(root)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .env("GIT_EDITOR", "true")
            .status()
            .expect("git");
        assert!(
            status.success(),
            "git {args:?} in {} failed",
            root.display()
        );
    }

    // -----------------------------------------------------------------------
    // Manual-checks response parsing (RAL-164)
    // -----------------------------------------------------------------------

    #[test]
    fn parse_manual_commands_bare_string_array_back_compat() {
        let (checks, build) = parse_manual_commands_response(r#"["cargo test", "npm run e2e"]"#);
        assert_eq!(checks.len(), 2);
        assert_eq!(checks[0].command.as_deref(), Some("cargo test"));
        assert!(checks[0].inputs.is_empty());
        assert!(checks[0].cleanup_command.is_none());
        assert_eq!(checks[1].command.as_deref(), Some("npm run e2e"));
        assert!(build.is_none());
    }

    #[test]
    fn parse_manual_commands_object_shape_with_structured_input() {
        let text = r#"{
            "manual_commands": [
                "cargo test",
                {
                    "command": "ralphus-daemon serve --port {port}",
                    "cleanup_command": "ralphus-daemon stop --port {port}",
                    "inputs": [{"name": "port", "message": "Port for the daemon", "default": "7890"}]
                }
            ],
            "build_command": "cargo build"
        }"#;
        let (checks, build) = parse_manual_commands_response(text);
        assert_eq!(checks.len(), 2);
        assert_eq!(checks[0].command.as_deref(), Some("cargo test"));
        assert!(checks[0].inputs.is_empty());

        assert_eq!(
            checks[1].command.as_deref(),
            Some("ralphus-daemon serve --port {port}")
        );
        assert_eq!(
            checks[1].cleanup_command.as_deref(),
            Some("ralphus-daemon stop --port {port}")
        );
        assert_eq!(checks[1].inputs.len(), 1);
        assert_eq!(checks[1].inputs[0].name, "port");
        assert_eq!(checks[1].inputs[0].message, "Port for the daemon");
        assert_eq!(checks[1].inputs[0].default, "7890");

        assert_eq!(build.as_deref(), Some("cargo build"));
    }

    #[test]
    fn parse_manual_commands_tolerates_chatty_model_wrapping_json_in_prose() {
        let text = "Sure, here you go:\n```json\n{\"manual_commands\": [\"cargo test\"]}\n```\nHope that helps!";
        let (checks, _build) = parse_manual_commands_response(text);
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].command.as_deref(), Some("cargo test"));
    }

    #[test]
    fn parse_manual_commands_unparseable_text_is_empty() {
        let (checks, build) = parse_manual_commands_response("not json at all");
        assert!(checks.is_empty());
        assert!(build.is_none());
    }

    // -----------------------------------------------------------------------
    // "Set it for me" input resolution (RAL-164)
    // -----------------------------------------------------------------------

    struct FixedValueRunner(&'static str);
    impl Runner for FixedValueRunner {
        fn run(&self, _spec: &RunnerSpec) -> RunnerResult {
            RunnerResult {
                status: "done".into(),
                tokens_in: 1,
                tokens_out: 1,
                cost_usd: 0.0,
                summary: self.0.to_string(),
                error: None,
                verified: None,
                agent_session_id: None,
                ghost: None,
            }
        }
    }

    fn guardian_with_port_input(store: &Store) -> String {
        let id = store.create_guardian("r", "main", "/repo").unwrap();
        store
            .set_guardian_manual_commands(
                &id,
                &[GuardianCheck {
                    label: None,
                    command: Some("ralphus-daemon serve --port {port}".to_string()),
                    prompt: None,
                    cleanup_command: None,
                    inputs: vec![CheckInput {
                        name: "port".to_string(),
                        message: "Port for the daemon".to_string(),
                        default: "7890".to_string(),
                    }],
                }],
                None,
                None,
            )
            .unwrap();
        id
    }

    #[test]
    fn start_resolve_input_success_records_ready_and_new_default() {
        let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
        let id = guardian_with_port_input(&store.lock().unwrap());
        let runner: Arc<dyn Runner> = Arc::new(FixedValueRunner("9001"));
        let sem = Arc::new(Semaphore::new(4));

        let r = start_resolve_input(Arc::clone(&store), runner, &id, "port", sem);
        assert_eq!(r.status, 202);

        // The background thread runs synchronously fast enough in tests
        // (FixedValueRunner does no real I/O), but poll briefly for
        // robustness against scheduling jitter.
        let mut g = store.lock().unwrap().get_guardian(&id).unwrap();
        for _ in 0..50 {
            if g.input_resolutions.get("port").map(|r| r.status.as_str()) != Some("resolving") {
                break;
            }
            drop(g);
            std::thread::sleep(std::time::Duration::from_millis(20));
            g = store.lock().unwrap().get_guardian(&id).unwrap();
        }

        assert_eq!(g.input_resolutions["port"].status, "ready");
        assert_eq!(g.input_resolutions["port"].value.as_deref(), Some("9001"));
        assert_eq!(g.input_values.get("port"), Some(&"9001".to_string()));
    }

    #[test]
    fn start_resolve_input_unknown_input_is_400() {
        let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
        let id = guardian_with_port_input(&store.lock().unwrap());
        let runner: Arc<dyn Runner> = Arc::new(FixedValueRunner("9001"));
        let sem = Arc::new(Semaphore::new(4));

        let r = start_resolve_input(store, runner, &id, "does-not-exist", sem);
        assert_eq!(r.status, 400);
        assert!(r.body.contains("unknown_input"));
    }

    #[test]
    fn start_resolve_input_concurrent_duplicate_is_409() {
        let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
        let id = guardian_with_port_input(&store.lock().unwrap());
        // Pre-claim, simulating a resolution already in flight from a
        // concurrent request.
        store
            .lock()
            .unwrap()
            .claim_guardian_input_resolution(&id, "port")
            .unwrap();

        let runner: Arc<dyn Runner> = Arc::new(FixedValueRunner("9001"));
        let sem = Arc::new(Semaphore::new(4));
        let r = start_resolve_input(store, runner, &id, "port", sem);
        assert_eq!(r.status, 409);
        assert!(r.body.contains("already_in_progress"));
    }

    /// Returns `(base_dir, repo_root, feature_worktree_path)`.
    ///
    /// Sets up: `main` branch with one commit, and a linked worktree on
    /// `feature/a` with one additional commit.
    fn make_repo(tag: &str) -> (PathBuf, PathBuf, PathBuf) {
        let base = tmp_dir(tag);
        let repo = base.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        g(&repo, &["init", "-b", "main"]);
        std::fs::write(repo.join("base.txt"), "base\n").unwrap();
        g(&repo, &["add", "."]);
        g(&repo, &["commit", "-m", "base"]);
        let fwt = base.join("fwt");
        g(
            &repo,
            &["worktree", "add", "-b", "feature/a", fwt.to_str().unwrap()],
        );
        std::fs::write(fwt.join("feat.txt"), "feat\n").unwrap();
        g(&fwt, &["add", "."]);
        g(&fwt, &["commit", "-m", "feature"]);
        (base, repo, fwt)
    }

    fn current_branch(wt: &Path) -> String {
        git(wt, &["symbolic-ref", "--short", "HEAD"])
            .unwrap_or_default()
            .trim()
            .to_string()
    }

    fn assert_on_branch(wt: &Path, expected: &str) {
        assert_eq!(
            current_branch(wt),
            expected,
            "expected worktree at {} to be on '{expected}'",
            wt.display()
        );
    }

    // -----------------------------------------------------------------------
    // Fail-state 1 — directory missing entirely
    // -----------------------------------------------------------------------

    #[test]
    fn worktree_recovery_state1_directory_missing() {
        let (base, repo, _fwt) = make_repo("s1");
        let rwt = base.join("rwt");
        assert!(!rwt.exists(), "precondition: rwt must not exist");

        worktree_add_or_reset(
            &Workspace::local(&repo),
            "guardian/g/wt-feature-a",
            &Workspace::local(&rwt),
            "feature/a",
        )
        .expect("state 1 recovery");

        assert!(rwt.exists());
        assert_on_branch(&rwt, "guardian/g/wt-feature-a");
        assert!(branch_exists(
            &Workspace::local(&repo),
            "guardian/g/wt-feature-a"
        ));
        let _ = std::fs::remove_dir_all(&base);
    }

    // -----------------------------------------------------------------------
    // Fail-state 2 — directory exists but is not a git worktree
    // -----------------------------------------------------------------------

    #[test]
    fn worktree_recovery_state2_directory_not_a_worktree() {
        let (base, repo, _fwt) = make_repo("s2");
        let rwt = base.join("rwt");
        // Create a plain directory with some files but no git tracking.
        std::fs::create_dir_all(&rwt).unwrap();
        std::fs::write(rwt.join("stray.txt"), "stray\n").unwrap();
        assert!(!rwt.join(".git").exists(), "precondition: no .git");

        worktree_add_or_reset(
            &Workspace::local(&repo),
            "guardian/g/wt-feature-a",
            &Workspace::local(&rwt),
            "feature/a",
        )
        .expect("state 2 recovery");

        assert!(rwt.exists());
        assert_on_branch(&rwt, "guardian/g/wt-feature-a");
        let _ = std::fs::remove_dir_all(&base);
    }

    // -----------------------------------------------------------------------
    // Fail-state 3 — worktree directory survives but review branch is deleted
    // -----------------------------------------------------------------------

    #[test]
    fn worktree_recovery_state3_review_branch_deleted() {
        let (base, repo, _fwt) = make_repo("s3");
        let rwt = base.join("rwt");

        // Initial setup.
        worktree_add_or_reset(
            &Workspace::local(&repo),
            "guardian/g/wt-feature-a",
            &Workspace::local(&rwt),
            "feature/a",
        )
        .expect("initial setup");

        // Simulate an external tool deleting the review branch:
        // 1. Remove the git tracking entry so `git branch -D` won't complain
        //    about the branch being checked out.
        // 2. Delete the branch.
        // 3. Directory `rwt` survives (CWD lock / external process).
        let tracking = repo.join(".git").join("worktrees").join("rwt");
        std::fs::remove_dir_all(&tracking).ok();
        g(&repo, &["branch", "-D", "guardian/g/wt-feature-a"]);
        // rwt directory still exists with its .git file.

        worktree_add_or_reset(
            &Workspace::local(&repo),
            "guardian/g/wt-feature-a",
            &Workspace::local(&rwt),
            "feature/a",
        )
        .expect("state 3 recovery");

        assert!(rwt.exists());
        assert_on_branch(&rwt, "guardian/g/wt-feature-a");
        assert!(branch_exists(
            &Workspace::local(&repo),
            "guardian/g/wt-feature-a"
        ));
        let _ = std::fs::remove_dir_all(&base);
    }

    // -----------------------------------------------------------------------
    // Fail-state 4 — branch exists, directory absent (stale tracking entry)
    // -----------------------------------------------------------------------

    #[test]
    fn worktree_recovery_state4_stale_tracking_entry() {
        let (base, repo, _fwt) = make_repo("s4");
        let rwt = base.join("rwt");

        // Initial setup.
        worktree_add_or_reset(
            &Workspace::local(&repo),
            "guardian/g/wt-feature-a",
            &Workspace::local(&rwt),
            "feature/a",
        )
        .expect("initial setup");

        // Simulate an OS crash or manual deletion of the directory without
        // going through `git worktree remove` — leaves a stale tracking entry.
        std::fs::remove_dir_all(&rwt).expect("delete rwt dir");
        assert!(!rwt.exists(), "precondition: rwt gone");
        // git worktree list should report rwt as prunable now.

        worktree_add_or_reset(
            &Workspace::local(&repo),
            "guardian/g/wt-feature-a",
            &Workspace::local(&rwt),
            "feature/a",
        )
        .expect("state 4 recovery");

        assert!(rwt.exists());
        assert_on_branch(&rwt, "guardian/g/wt-feature-a");
        let _ = std::fs::remove_dir_all(&base);
    }

    // -----------------------------------------------------------------------
    // Fail-state 5 — branch mismatch (directory survives, wrong branch)
    // -----------------------------------------------------------------------

    #[test]
    fn worktree_recovery_state5_branch_mismatch() {
        let (base, repo, _fwt) = make_repo("s5");
        let rwt = base.join("rwt");

        // Initial setup.
        worktree_add_or_reset(
            &Workspace::local(&repo),
            "guardian/g/wt-feature-a",
            &Workspace::local(&rwt),
            "feature/a",
        )
        .expect("initial setup");

        // Simulate the directory surviving with the wrong branch checked out.
        // Order matters: change the branch FIRST (while the tracking entry
        // exists so git commands work inside rwt), THEN remove the tracking
        // entry to simulate a CWD-lock scenario where the directory outlived
        // the git worktree removal.
        g(&rwt, &["checkout", "-b", "wrong-branch"]);
        let tracking = repo.join(".git").join("worktrees").join("rwt");
        std::fs::remove_dir_all(&tracking).ok();
        // Verify precondition: rwt still has files but git can't work in it
        // (tracking is gone), and its HEAD points to wrong-branch.
        assert!(rwt.exists(), "rwt directory must survive");
        // Read the HEAD content from the (now deleted) tracking entry... we
        // can't run git in rwt, so just confirm the directory exists with files.
        assert!(
            rwt.join(".git").is_file(),
            "rwt/.git pointer file must remain"
        );

        worktree_add_or_reset(
            &Workspace::local(&repo),
            "guardian/g/wt-feature-a",
            &Workspace::local(&rwt),
            "feature/a",
        )
        .expect("state 5 recovery");

        assert_on_branch(&rwt, "guardian/g/wt-feature-a");
        let _ = std::fs::remove_dir_all(&base);
    }

    // -----------------------------------------------------------------------
    // Fail-state 6 — detached HEAD in worktree
    // -----------------------------------------------------------------------

    #[test]
    fn worktree_recovery_state6_detached_head() {
        let (base, repo, _fwt) = make_repo("s6");
        let rwt = base.join("rwt");

        // Initial setup.
        worktree_add_or_reset(
            &Workspace::local(&repo),
            "guardian/g/wt-feature-a",
            &Workspace::local(&rwt),
            "feature/a",
        )
        .expect("initial setup");

        // Simulate the directory surviving in detached-HEAD state.
        // Order matters: detach HEAD FIRST (while the tracking entry is valid),
        // THEN remove the entry to simulate the CWD-lock survival scenario.
        let sha = git(&rwt, &["rev-parse", "HEAD"])
            .expect("rev-parse HEAD")
            .trim()
            .to_string();
        g(&rwt, &["checkout", "--detach", &sha]);
        let tracking = repo.join(".git").join("worktrees").join("rwt");
        std::fs::remove_dir_all(&tracking).ok();
        assert!(rwt.exists(), "rwt directory must survive");

        worktree_add_or_reset(
            &Workspace::local(&repo),
            "guardian/g/wt-feature-a",
            &Workspace::local(&rwt),
            "feature/a",
        )
        .expect("state 6 recovery");

        assert_on_branch(&rwt, "guardian/g/wt-feature-a");
        let _ = std::fs::remove_dir_all(&base);
    }

    // -----------------------------------------------------------------------
    // RAL-171 — a healthy restart reuses the worktree in place
    // -----------------------------------------------------------------------

    #[test]
    fn worktree_add_or_reset_healthy_restart_preserves_untracked_files() {
        let (base, repo, _fwt) = make_repo("fastpath");
        let rwt = base.join("rwt");

        // Initial setup.
        worktree_add_or_reset(
            &Workspace::local(&repo),
            "guardian/g/wt-feature-a",
            &Workspace::local(&rwt),
            "feature/a",
        )
        .expect("initial setup");

        // An untracked file (e.g. a build cache) that only the destructive
        // remove+recreate path would wipe -- `checkout -f -B` never touches
        // files git doesn't know about.
        std::fs::write(rwt.join("untracked-cache.txt"), "cache\n").unwrap();

        // A "restart": call again with nothing wrong. This must take the fast
        // in-place path, not the destructive remove/prune/re-add fallback.
        worktree_add_or_reset(
            &Workspace::local(&repo),
            "guardian/g/wt-feature-a",
            &Workspace::local(&rwt),
            "feature/a",
        )
        .expect("restart on healthy worktree");

        assert!(
            rwt.join("untracked-cache.txt").exists(),
            "a healthy restart must reuse the worktree in place, not wipe and recreate it"
        );
        assert_on_branch(&rwt, "guardian/g/wt-feature-a");
        let _ = std::fs::remove_dir_all(&base);
    }

    // -----------------------------------------------------------------------
    // Fail-state 7 — worktree locked externally
    // -----------------------------------------------------------------------

    #[test]
    fn worktree_recovery_state7_locked_worktree() {
        let (base, repo, _fwt) = make_repo("s7");
        let rwt = base.join("rwt");

        // Initial setup.
        worktree_add_or_reset(
            &Workspace::local(&repo),
            "guardian/g/wt-feature-a",
            &Workspace::local(&rwt),
            "feature/a",
        )
        .expect("initial setup");

        // Lock the worktree (simulates external tooling holding a lock).
        g(&repo, &["worktree", "lock", rwt.to_str().unwrap()]);

        // Recovery must unlock and succeed.
        worktree_add_or_reset(
            &Workspace::local(&repo),
            "guardian/g/wt-feature-a",
            &Workspace::local(&rwt),
            "feature/a",
        )
        .expect("state 7 recovery");

        assert_on_branch(&rwt, "guardian/g/wt-feature-a");
        // Verify the worktree is no longer locked.
        let list = git(&repo, &["worktree", "list", "--porcelain"]).unwrap_or_default();
        assert!(
            !list.contains("locked"),
            "worktree should not be locked after recovery; list:\n{list}"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    // -----------------------------------------------------------------------
    // Feature branch absent — regenerate from live feature worktree
    // -----------------------------------------------------------------------

    /// When the feature branch ref is absent but a registered worktree is
    /// checked out on it, `regen_from_feature_worktree` must create the review
    /// branch at the feature worktree's HEAD and add the review worktree.
    ///
    /// We test the function directly because triggering it through
    /// `worktree_add_or_reset` would require the feature branch ref to be
    /// absent while `git rev-parse HEAD` still works — a state that requires
    /// packing refs and deleting them at the filesystem level, which is too
    /// fragile for a portable test.
    #[test]
    fn regen_from_feature_worktree_creates_review_worktree_at_feature_head() {
        let (base, repo, _fwt) = make_repo("fbregen");
        let rwt = base.join("rwt");

        // fwt is already registered and `feature/a` is live. Call the regen
        // function directly — it uses `find_worktree_for_branch` to locate fwt
        // by searching git worktree list for anything on `feature/a`.
        regen_from_feature_worktree(
            &Workspace::local(&repo),
            "guardian/g/wt-feature-a",
            &Workspace::local(&rwt),
            "feature/a",
        )
        .expect("regen from feature worktree");

        assert!(rwt.exists());
        assert_on_branch(&rwt, "guardian/g/wt-feature-a");
        // Content from fwt (the feature worktree) must be present.
        assert!(
            rwt.join("feat.txt").exists(),
            "feat.txt from feature worktree should be present after regen"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    // -----------------------------------------------------------------------
    // Helper function unit tests
    // -----------------------------------------------------------------------

    #[test]
    fn is_valid_linked_worktree_false_for_absent_dir() {
        let dir = tmp_dir("vld-absent");
        assert!(!is_valid_linked_worktree(&Workspace::local(
            dir.join("nonexistent")
        )));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn is_valid_linked_worktree_false_for_plain_dir() {
        let dir = tmp_dir("vld-plain");
        std::fs::create_dir_all(dir.join("wt")).unwrap();
        assert!(!is_valid_linked_worktree(&Workspace::local(dir.join("wt"))));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn is_valid_linked_worktree_false_for_git_dir() {
        let dir = tmp_dir("vld-gitdir");
        let wt = dir.join("wt");
        std::fs::create_dir_all(wt.join(".git")).unwrap(); // directory, not file
        assert!(!is_valid_linked_worktree(&Workspace::local(&wt)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn is_valid_linked_worktree_true_for_gitdir_file() {
        let dir = tmp_dir("vld-file");
        let wt = dir.join("wt");
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(wt.join(".git"), "gitdir: /some/path\n").unwrap();
        assert!(is_valid_linked_worktree(&Workspace::local(&wt)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn find_worktree_for_branch_finds_correct_worktree() {
        let (base, repo, fwt) = make_repo("find-wt");
        let found = find_worktree_for_branch(&Workspace::local(&repo), "feature/a");
        // On Windows, git may report paths with long names while the PathBuf
        // from temp_dir() uses short (8.3) names. Canonicalize both before
        // comparing so they resolve to the same physical path.
        let found_canon = found.and_then(|p| p.canonicalize().ok());
        let fwt_canon = fwt.canonicalize().ok();
        assert_eq!(
            found_canon,
            fwt_canon,
            "should find fwt at {}",
            fwt.display()
        );
        assert!(find_worktree_for_branch(&Workspace::local(&repo), "no-such-branch").is_none());
        let _ = std::fs::remove_dir_all(&base);
    }

    // -----------------------------------------------------------------------
    // XML route-block parsing (pre-existing tests)
    // -----------------------------------------------------------------------

    #[test]
    fn parse_route_blocks_extracts_branch_and_instructions() {
        let text = r#"I'll route this.
<route branch="feature/foo">
Rename the variable in src/lib.rs.
</route>
Let me know if you need anything else."#;
        let routes = parse_route_blocks(text);
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].0, "feature/foo");
        assert!(routes[0].1.contains("Rename the variable"));
    }

    #[test]
    fn parse_route_blocks_handles_multiple_blocks() {
        let text = concat!(
            r#"<route branch="feat/a">Fix A.</route>"#,
            "\n",
            r#"<route branch="feat/b">Fix B.</route>"#
        );
        let routes = parse_route_blocks(text);
        assert_eq!(routes.len(), 2);
        assert_eq!(routes[0].0, "feat/a");
        assert_eq!(routes[1].0, "feat/b");
        assert_eq!(routes[0].1, "Fix A.");
        assert_eq!(routes[1].1, "Fix B.");
    }

    #[test]
    fn parse_route_blocks_empty_when_no_blocks() {
        assert!(parse_route_blocks("Nothing here.").is_empty());
    }

    #[test]
    fn parse_route_blocks_ignores_unclosed_block() {
        let text = "<route branch=\"feat/x\">Missing close tag";
        assert!(parse_route_blocks(text).is_empty());
    }

    #[test]
    fn strip_route_blocks_removes_blocks_and_trims() {
        let text = "Plain text.\n<route branch=\"feat/a\">Do something.</route>\nMore text.";
        assert_eq!(strip_route_blocks(text), "Plain text.\n\nMore text.");
    }

    #[test]
    fn strip_route_blocks_leaves_no_route_text_unchanged() {
        let text = "No route blocks here.";
        assert_eq!(strip_route_blocks(text), text);
    }

    #[test]
    fn strip_route_blocks_handles_multiple_blocks() {
        let text = "A<route branch=\"x\">1</route>B<route branch=\"y\">2</route>C";
        assert_eq!(strip_route_blocks(text), "ABC");
    }

    #[test]
    fn extract_xml_attr_returns_value() {
        assert_eq!(
            extract_xml_attr(r#"<route branch="main">"#, "branch"),
            Some("main".to_string())
        );
    }

    #[test]
    fn extract_xml_attr_returns_none_when_missing() {
        assert_eq!(extract_xml_attr("<route>", "branch"), None);
    }

    /// Regression test for the production failure:
    ///   "git checkout -f -B <rev> <branch> failed: fatal: not a git
    ///   repository: .git/worktrees/<name>"
    ///
    /// Root cause: `git worktree remove --force` on Windows can delete the
    /// admin entry (`.git/worktrees/<name>/`) while leaving the checkout
    /// directory locked by another process's CWD.  When `worktree_add_or_reset`
    /// then calls `relink_worktree` to recreate the entry, it wrote `gitdir`
    /// and `commondir` but omitted `HEAD`.  Without `HEAD`, git refuses to
    /// open the gitdir and the subsequent `checkout -f -B` fails.
    #[test]
    fn worktree_add_or_reset_recovers_when_admin_entry_missing() {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!("ralphus-relink-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("mkdir temp root");

        // Set up a repo with a feature branch.
        git(&root, &["init", "-b", "main"]).expect("git init");
        git(&root, &["config", "user.email", "t@t.com"]).expect("git config email");
        git(&root, &["config", "user.name", "t"]).expect("git config name");
        git(&root, &["commit", "--allow-empty", "-m", "initial"]).expect("initial commit");
        git(&root, &["checkout", "-b", "feat-test"]).expect("create branch");
        std::fs::write(root.join("f.txt"), "hello\n").expect("write f.txt");
        git(&root, &["add", "f.txt"]).expect("git add");
        git(&root, &["commit", "-m", "add f"]).expect("git commit");
        git(&root, &["checkout", "main"]).expect("checkout main");

        // Create a worktree the normal way — this produces a valid admin entry.
        let wt = root.join("wt-feat-test");
        let rev = "guardian/g-001/wt-feat-test";
        git(
            &root,
            &[
                "worktree",
                "add",
                "-B",
                rev,
                wt.to_str().unwrap(),
                "feat-test",
            ],
        )
        .expect("git worktree add");

        // Sanity: HEAD exists after a normal `git worktree add`.
        let admin = root.join(".git").join("worktrees").join("wt-feat-test");
        assert!(
            admin.join("HEAD").exists(),
            "HEAD must be present after git worktree add"
        );

        // Simulate the failure state: the admin entry is removed (as happens
        // when `git worktree remove --force` deletes the entry but cannot
        // delete the checkout directory due to a Windows CWD lock) while the
        // checkout directory itself survives.
        std::fs::remove_dir_all(&admin).expect("delete admin entry");
        assert!(!admin.exists(), "admin entry must be gone");
        assert!(wt.exists(), "checkout directory must survive");

        // Before the fix, this call produced:
        //   "git checkout -f -B … failed: fatal: not a git repository: …/wt-feat-test"
        let result = worktree_add_or_reset(
            &Workspace::local(&root),
            rev,
            &Workspace::local(&wt),
            "feat-test",
        );
        assert!(result.is_ok(), "recovery must succeed; got: {:?}", result);

        // After recovery the worktree must be on the review branch.
        let head = git(&wt, &["rev-parse", "--abbrev-ref", "HEAD"]).expect("rev-parse HEAD");
        assert_eq!(head.trim(), rev, "worktree HEAD after recovery");

        let _ = std::fs::remove_dir_all(&root);
    }

    // Regression: when the guardian worktree has an untracked file that is now
    // tracked in the new base commit (e.g. because main added the file after a
    // previous agent session left it behind), `git rebase --onto <new_base>` fails
    // with "untracked working tree files would be overwritten by checkout".
    //
    // This test calls `drive_rebase` directly (bypassing `ensure_worktree`) so it
    // exercises exactly the code path that broke in production — where Windows
    // CWD-lock prevents `ensure_worktree` from deleting the worktree directory.
    #[test]
    fn drive_rebase_cleans_untracked_file_that_blocks_rebase_onto() {
        use crate::runner::RunnerResult;

        let base = tmp_dir("drive-rebase-blocker");
        let repo = base.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        g(&repo, &["init", "-b", "main"]);

        // Initial base commit — no blocker.txt yet.
        std::fs::write(repo.join("base.txt"), "base\n").unwrap();
        g(&repo, &["add", "."]);
        g(&repo, &["commit", "-m", "initial"]);
        let old_base_str = git(&repo, &["rev-parse", "HEAD"]).unwrap();
        let old_base = old_base_str.trim();

        // Feature branch adds feat.txt (NOT blocker.txt).
        g(&repo, &["checkout", "-b", "feature/a"]);
        std::fs::write(repo.join("feat.txt"), "feat\n").unwrap();
        g(&repo, &["add", "."]);
        g(&repo, &["commit", "-m", "add feature"]);
        g(&repo, &["checkout", "main"]);

        // Main moves forward and introduces blocker.txt.
        std::fs::write(repo.join("blocker.txt"), "on main\n").unwrap();
        g(&repo, &["add", "."]);
        g(&repo, &["commit", "-m", "main adds blocker.txt"]);
        let new_base_str = git(&repo, &["rev-parse", "HEAD"]).unwrap();
        let new_base = new_base_str.trim();

        // Set up a review worktree on feature/a.
        let rev = "guardian/test/wt-feature-a";
        let wt = base.join("review-wt");
        g(
            &repo,
            &[
                "worktree",
                "add",
                "-b",
                rev,
                wt.to_str().unwrap(),
                "feature/a",
            ],
        );

        // Simulate a stale agent leftover: an untracked copy of blocker.txt in the
        // worktree. Without the fix, `git rebase --onto new_base` fails here because
        // new_base has blocker.txt tracked and git refuses to overwrite the untracked
        // file.
        std::fs::write(wt.join("blocker.txt"), "stale agent output\n").unwrap();

        let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
        let guardian_id = store
            .lock()
            .unwrap()
            .create_guardian("test", "main", repo.to_str().unwrap())
            .unwrap();
        store
            .lock()
            .unwrap()
            .add_guardian_branch(&guardian_id, "feature/a")
            .unwrap();
        let branch_id = store
            .lock()
            .unwrap()
            .get_guardian(&guardian_id)
            .unwrap()
            .branches[0]
            .id
            .clone();

        struct NopRunner;
        impl Runner for NopRunner {
            fn run(&self, _: &RunnerSpec) -> RunnerResult {
                RunnerResult::failure("should not be called")
            }
        }

        // "nothing" scope: this test is about untracked-file cleanup during the
        // rebase itself, not verify gating -- NopRunner must never be called.
        let gate = VerifyGate {
            scope: "nothing".to_string(),
            skip_auto_clean: false,
            is_final_branch: false,
        };
        let result = drive_rebase(
            &store,
            &guardian_id,
            0,
            &branch_id,
            &NopRunner,
            "feature/a",
            &Workspace::local(&wt),
            new_base,
            old_base,
            rev,
            &gate,
        );
        assert!(
            result.is_ok(),
            "drive_rebase must succeed despite untracked blocker.txt; got Err({:?})",
            result.err()
        );
        assert!(
            matches!(result.unwrap().0, RebaseOutcome::Clean),
            "expected Clean rebase outcome"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    // -----------------------------------------------------------------------
    // RAL-103 — preliminary (git-log-only) change summary
    // -----------------------------------------------------------------------

    /// Insert a minimal run/task/session row so `recompute_preliminary_summary`
    /// can resolve `branch`'s contributing session back to its task worktree
    /// `cwd` via `sessions.review_branch`.
    fn insert_done_session(store: &Store, run_id: &str, cwd: &Path, branch: &str) {
        store
            .conn
            .execute(
                "INSERT INTO runs (id, label, state, depends_on, created_at_ms, updated_at_ms) \
                 VALUES (?, NULL, 'done', '[]', 0, 0)",
                rusqlite::params![run_id],
            )
            .unwrap();
        store
            .conn
            .execute(
                "INSERT INTO tasks (run_id, idx, name, state, depends_on) \
                 VALUES (?, 0, 't', 'done', '[]')",
                rusqlite::params![run_id],
            )
            .unwrap();
        store
            .conn
            .execute(
                "INSERT INTO sessions \
                 (run_id, task_idx, idx, sid, cwd, agent, state, depends_on, review_branch) \
                 VALUES (?, 0, 0, 's', ?, 'claude', 'done', '[]', ?)",
                rusqlite::params![run_id, cwd.to_str().unwrap(), branch],
            )
            .unwrap();
    }

    #[test]
    fn a_remote_review_dispatches_to_its_machine_rather_than_merging_here() {
        // The failure this guards against is the worst kind: a merge that
        // rebased, resolved conflicts, and reported success -- all on the wrong
        // host. With no provider program actually present, the merge must fail
        // *trying to reach the machine*, never by quietly succeeding locally.
        let (base, repo, _fwt) = make_repo("remote-review-dispatch");
        let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
        let id = {
            let guard = store.lock().unwrap();
            let id = guard
                .create_guardian("r", "main", repo.to_str().unwrap())
                .unwrap();
            guard.add_guardian_branch(&id, "feature/a").unwrap();
            guard
                .register_machine_provider(
                    "ib",
                    "",
                    "/definitely/not/a/real/provider",
                    &[],
                    crate::machines::PROTOCOL_VERSION,
                    false,
                )
                .unwrap();
            guard.set_guardian_machine(&id, Some("ib:A")).unwrap();
            id
        };
        let runner: Arc<dyn Runner> = Arc::new(CapturingRunner::new());
        run_merge(&store, runner.as_ref(), &id);

        let g = store.lock().unwrap().get_guardian(&id).unwrap();
        assert_eq!(
            g.status, "merge_failed",
            "an unreachable machine must fail the merge, not silently run it here"
        );
        let detail = g.detail.unwrap_or_default();
        assert!(
            detail.contains("machine provider") || detail.contains("ib"),
            "the failure must name the machine it could not reach: {detail}"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_review_left_local_still_merges_normally() {
        // Regression guard: the gate must be inert for every existing review.
        let (base, repo, _fwt) = make_repo("local-review-ok");
        let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
        let id = {
            let guard = store.lock().unwrap();
            let id = guard
                .create_guardian("r", "main", repo.to_str().unwrap())
                .unwrap();
            guard.add_guardian_branch(&id, "feature/a").unwrap();
            // Explicitly local -- must be treated exactly like unset.
            guard.set_guardian_machine(&id, Some("local")).unwrap();
            id
        };
        let runner: Arc<dyn Runner> = Arc::new(CapturingRunner::new());
        run_merge(&store, runner.as_ref(), &id);

        let g = store.lock().unwrap().get_guardian(&id).unwrap();
        assert_ne!(
            g.status, "merge_failed",
            "a local review must not be caught by the remote gate: {:?}",
            g.detail
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_locally_produced_branch_is_never_fetched() {
        // Regression guard: every pre-RAL-185 branch has no machine, and the
        // bridge must be completely inert for it -- including in a repo with
        // no remote configured at all, where a fetch would fail.
        let (base, repo, _fwt) = make_repo("fetch-local");
        let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
        let id = {
            let guard = store.lock().unwrap();
            let id = guard
                .create_guardian("r", "main", repo.to_str().unwrap())
                .unwrap();
            guard.add_guardian_branch(&id, "feature/a").unwrap();
            id
        };
        let b = store.lock().unwrap().get_guardian(&id).unwrap().branches[0].clone();
        assert!(b.source_session_machine.is_none());
        assert!(
            fetch_branch_for_remote_session(&store, &id, &b).is_ok(),
            "a local branch must not attempt any fetch"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_remote_branch_that_was_never_pushed_fails_with_an_actionable_message() {
        // The quietly-wrong case Q6 exists for: the task ran on another machine
        // and never pushed, so its commits are nowhere this repo can see them.
        // Failing here beats an opaque `worktree add` error -- or, when a
        // previous run *did* push, silently stacking that stale revision.
        let (base, repo, _fwt) = make_repo("fetch-never-pushed");
        let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
        let id = {
            let guard = store.lock().unwrap();
            let id = guard
                .create_guardian("r", "main", repo.to_str().unwrap())
                .unwrap();
            guard.add_guardian_branch(&id, "feature/a").unwrap();
            id
        };
        let mut b = store.lock().unwrap().get_guardian(&id).unwrap().branches[0].clone();
        b.source_session_machine = Some("incredibuild:A".to_string());
        // `make_repo` configures no remote, so the fetch cannot succeed --
        // exactly what an unpushed branch looks like from here.
        let err = fetch_branch_for_remote_session(&store, &id, &b)
            .expect_err("an unfetchable remote branch must fail the merge");
        assert!(err.contains("feature/a"), "must name the branch: {err}");
        assert!(
            err.contains("incredibuild:A"),
            "must name the machine: {err}"
        );
        assert!(
            err.contains("responsible for pushing"),
            "must say whose job publishing is: {err}"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn note_if_branch_is_empty_flags_a_branch_that_adds_nothing() {
        // The quietly-wrong case this exists for: a task whose session never
        // committed leaves a branch identical to its base. It rebases cleanly,
        // merges cleanly, and the review reaches `in_review` looking healthy
        // while containing none of that task's work.
        let (base, repo, _fwt) = make_repo("empty-branch");
        let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
        let id = {
            let guard = store.lock().unwrap();
            let id = guard
                .create_guardian("r", "main", repo.to_str().unwrap())
                .unwrap();
            guard.add_guardian_branch(&id, "feature/empty").unwrap();
            id
        };
        let branch_id = store.lock().unwrap().get_guardian(&id).unwrap().branches[0]
            .id
            .clone();

        // A branch pointing at the very same commit as the tip beneath it.
        g(&repo, &["branch", "feature/empty", "main"]);
        assert!(
            note_if_branch_is_empty(
                &store,
                &id,
                &branch_id,
                "feature/empty",
                &Workspace::local(&repo),
                "main"
            ),
            "an empty branch must report true so the caller can fail the merge"
        );
        assert!(
            store.lock().unwrap().get_guardian(&id).unwrap().branches[0].is_empty,
            "a branch with no diff over its base must be flagged empty"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn note_if_branch_is_empty_leaves_a_real_branch_alone() {
        let (base, repo, fwt) = make_repo("nonempty-branch");
        let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
        let id = {
            let guard = store.lock().unwrap();
            let id = guard
                .create_guardian("r", "main", repo.to_str().unwrap())
                .unwrap();
            guard.add_guardian_branch(&id, "feature/a").unwrap();
            id
        };
        let branch_id = store.lock().unwrap().get_guardian(&id).unwrap().branches[0]
            .id
            .clone();
        // `make_repo` already committed a real change on `feature/a` in `fwt`.
        assert!(fwt.join("feat.txt").exists());
        note_if_branch_is_empty(
            &store,
            &id,
            &branch_id,
            "feature/a",
            &Workspace::local(&repo),
            "main",
        );
        assert!(
            !store.lock().unwrap().get_guardian(&id).unwrap().branches[0].is_empty,
            "a branch carrying real commits must not be flagged"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn note_if_branch_is_empty_does_not_flag_a_branch_already_merged_upstream() {
        // Reproduces a real false-positive (guardian-000000000099): a review
        // gets rebuilt (e.g. a resurrected/re-run stack) after its branch's
        // commit has *already* landed on the base branch -- by an earlier
        // successful pass of this very guardian, or by any other route. The
        // branch's own pre-rebase commit is real work, but by the time the
        // check runs, the *current* base already contains it (and more),
        // which is exactly the shape `--empty=drop` reports as a clean,
        // content-empty rebase. The caller must compare against the boundary
        // the rebase actually used (`upstream`, honoring carry-forward) --
        // never the current, possibly-already-advanced base -- or a task that
        // genuinely committed gets told it "most likely never committed".
        let (base, repo, _fwt) = make_repo("already-merged");
        let old_main = git(&repo, &["rev-parse", "main"])
            .unwrap()
            .trim()
            .to_string();
        // Advance `main` past the point where feature/a's own commit already
        // applies cleanly, simulating a base that has since absorbed this
        // branch's work (directly or via a prior guardian run).
        g(&repo, &["merge", "--no-edit", "feature/a"]);

        let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
        let id = {
            let guard = store.lock().unwrap();
            let id = guard
                .create_guardian("r", "main", repo.to_str().unwrap())
                .unwrap();
            guard.add_guardian_branch(&id, "feature/a").unwrap();
            id
        };
        let branch_id = store.lock().unwrap().get_guardian(&id).unwrap().branches[0]
            .id
            .clone();

        // Comparing against the stale, pre-merge upstream (what carry-forward
        // resolves to) correctly finds feature/a's real contribution.
        assert!(
            !note_if_branch_is_empty(
                &store,
                &id,
                &branch_id,
                "feature/a",
                &Workspace::local(&repo),
                &old_main
            ),
            "a branch whose commit already landed upstream must not be reported \
             empty when checked against the boundary it was actually built from"
        );
        assert!(
            !store.lock().unwrap().get_guardian(&id).unwrap().branches[0].is_empty,
            "must not flag a branch that committed real work, even if that \
             work is now also present further up the current base"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn note_if_branch_is_empty_says_nothing_when_git_cannot_answer() {
        // An unknown ref makes `git diff` exit with neither 0 nor 1. Recording
        // "not empty" there would be a guess dressed up as a fact.
        let (base, repo, _fwt) = make_repo("unknown-ref");
        let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
        let id = {
            let guard = store.lock().unwrap();
            let id = guard
                .create_guardian("r", "main", repo.to_str().unwrap())
                .unwrap();
            guard.add_guardian_branch(&id, "feature/a").unwrap();
            id
        };
        let branch_id = store.lock().unwrap().get_guardian(&id).unwrap().branches[0]
            .id
            .clone();
        store
            .lock()
            .unwrap()
            .set_branch_empty(&id, &branch_id, true)
            .unwrap();
        assert!(
            !note_if_branch_is_empty(
                &store,
                &id,
                &branch_id,
                "feature/a",
                &Workspace::local(&repo),
                "no-such-ref"
            ),
            "an unanswerable diff must never fail a review on a guess"
        );
        assert!(
            store.lock().unwrap().get_guardian(&id).unwrap().branches[0].is_empty,
            "an unanswerable diff must leave the existing flag untouched"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn recompute_preliminary_summary_uses_task_worktree_git_log_not_llm() {
        let (base, repo, fwt) = make_repo("prelim");
        let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
        let id = {
            let guard = store.lock().unwrap();
            let id = guard
                .create_guardian("r", "main", repo.to_str().unwrap())
                .unwrap();
            guard.add_guardian_branch(&id, "feature/a").unwrap();
            insert_done_session(&guard, "run-1", &fwt, "feature/a");
            id
        };
        let branch_id = store.lock().unwrap().get_guardian(&id).unwrap().branches[0]
            .id
            .clone();

        // A branch still `pending` (no session done yet) contributes nothing.
        recompute_preliminary_summary(&store, &id);
        assert!(
            store
                .lock()
                .unwrap()
                .get_guardian(&id)
                .unwrap()
                .change_summary
                .is_none()
        );

        // Session done -> mark the branch Ready (as the scheduler now does
        // per-branch, independent of any sibling) and recompute.
        store
            .lock()
            .unwrap()
            .set_branch_status(&id, &branch_id, MergeStatus::Ready, None)
            .unwrap();
        recompute_preliminary_summary(&store, &id);
        let g = store.lock().unwrap().get_guardian(&id).unwrap();
        let summary = g.change_summary.expect("preliminary summary computed");
        assert!(summary.contains("feature/a:"), "summary: {summary:?}");
        assert!(summary.contains("feature"), "summary: {summary:?}");
        // No agent/model recorded -- distinguishes preliminary (pure git log)
        // from the LLM-authored final summary.
        assert!(g.summary_agent.is_none());
        assert_eq!(g.summary_state, "ready");

        // Once the branch has a review worktree (its first rebase has run),
        // it is no longer this function's concern -- a no-op leaves whatever
        // `generate_summary` last wrote untouched.
        {
            let guard = store.lock().unwrap();
            guard
                .set_branch_review(
                    &id,
                    &branch_id,
                    "guardian/x/wt-feature-a",
                    "/some/review/wt",
                )
                .unwrap();
            guard
                .set_guardian_summary(&id, "final summary from the agent", Some("ollama"), None)
                .unwrap();
        }
        recompute_preliminary_summary(&store, &id);
        let g = store.lock().unwrap().get_guardian(&id).unwrap();
        assert_eq!(
            g.change_summary.as_deref(),
            Some("final summary from the agent")
        );
        assert_eq!(g.summary_agent.as_deref(), Some("ollama"));

        let _ = std::fs::remove_dir_all(&base);
    }

    /// RAL-147: reproduce a stacked review's worktree topology -- branch B's
    /// worktree HEAD is built on top of branch A's commits (rebased, exactly
    /// as the real merge flow leaves it), and branch C's on top of B's. Each
    /// candidate's `base_branch..HEAD` log would therefore naturally include
    /// every earlier branch's commits too; assert the preliminary summary
    /// instead shows each branch's section containing only its own commit.
    #[test]
    fn recompute_preliminary_summary_dedupes_stacked_branch_commits() {
        let (base, repo, awt) = make_repo("prelim-stack");

        // feature/a already has its "feature" commit from make_repo. Add one
        // more so it has a distinctive, greppable subject.
        std::fs::write(awt.join("a-extra.txt"), "a\n").unwrap();
        g(&awt, &["add", "."]);
        g(&awt, &["commit", "-m", "commit-a-only"]);

        // feature/b is stacked on top of feature/a's tip -- its worktree HEAD
        // contains commit-a-only plus its own new commit.
        let bwt = base.join("bwt");
        g(
            &repo,
            &[
                "worktree",
                "add",
                "-b",
                "feature/b",
                bwt.to_str().unwrap(),
                "feature/a",
            ],
        );
        std::fs::write(bwt.join("b.txt"), "b\n").unwrap();
        g(&bwt, &["add", "."]);
        g(&bwt, &["commit", "-m", "commit-b-only"]);

        // feature/c is stacked on top of feature/b's tip.
        let cwt = base.join("cwt");
        g(
            &repo,
            &[
                "worktree",
                "add",
                "-b",
                "feature/c",
                cwt.to_str().unwrap(),
                "feature/b",
            ],
        );
        std::fs::write(cwt.join("c.txt"), "c\n").unwrap();
        g(&cwt, &["add", "."]);
        g(&cwt, &["commit", "-m", "commit-c-only"]);

        let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
        let id = {
            let guard = store.lock().unwrap();
            let id = guard
                .create_guardian("r", "main", repo.to_str().unwrap())
                .unwrap();
            for branch in ["feature/a", "feature/b", "feature/c"] {
                guard.add_guardian_branch(&id, branch).unwrap();
            }
            insert_done_session(&guard, "run-a", &awt, "feature/a");
            insert_done_session(&guard, "run-b", &bwt, "feature/b");
            insert_done_session(&guard, "run-c", &cwt, "feature/c");
            id
        };
        let branch_ids: Vec<String> = store
            .lock()
            .unwrap()
            .get_guardian(&id)
            .unwrap()
            .branches
            .iter()
            .map(|b| b.id.clone())
            .collect();
        {
            let guard = store.lock().unwrap();
            for branch_id in &branch_ids {
                guard
                    .set_branch_status(&id, branch_id, MergeStatus::Ready, None)
                    .unwrap();
            }
        }

        recompute_preliminary_summary(&store, &id);
        let g_row = store.lock().unwrap().get_guardian(&id).unwrap();
        let summary = g_row.change_summary.expect("preliminary summary computed");

        // Each commit subject appears exactly once across the whole summary
        // -- not once per downstream branch's section.
        for subject in ["commit-a-only", "commit-b-only", "commit-c-only"] {
            assert_eq!(
                summary.matches(subject).count(),
                1,
                "expected '{subject}' to appear exactly once in summary: {summary:?}"
            );
        }

        // And each section is scoped to only the commit(s) that branch
        // actually introduced.
        let sections: std::collections::HashMap<&str, &str> = summary
            .split("\n\n")
            .filter_map(|s| s.split_once(":\n"))
            .collect();
        // feature/a also carries make_repo's pre-existing "feature" commit
        // (git log lists newest first).
        assert_eq!(sections.get("feature/a"), Some(&"commit-a-only\nfeature"));
        assert_eq!(sections.get("feature/b"), Some(&"commit-b-only"));
        assert_eq!(sections.get("feature/c"), Some(&"commit-c-only"));

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn recompute_preliminary_summary_is_noop_with_no_ready_branches() {
        let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
        let id = {
            let guard = store.lock().unwrap();
            let id = guard.create_guardian("r", "main", "/repo").unwrap();
            guard.add_guardian_branch(&id, "feature/a").unwrap();
            id
        };

        recompute_preliminary_summary(&store, &id);
        assert!(
            store
                .lock()
                .unwrap()
                .get_guardian(&id)
                .unwrap()
                .change_summary
                .is_none()
        );
    }

    // -----------------------------------------------------------------------
    // RAL-124 — bullet-format change summary
    // -----------------------------------------------------------------------

    #[test]
    fn branch_summary_label_extracts_ticket_id_or_falls_back() {
        assert_eq!(
            branch_summary_label("RAL-124-bullet_change_summary"),
            "RAL-124"
        );
        assert_eq!(branch_summary_label("PIPE-1234-implement-x"), "PIPE-1234");
        assert_eq!(branch_summary_label("DEV-443-fix-y"), "DEV-443");
        assert_eq!(
            branch_summary_label("just-a-branch-name"),
            "just-a-branch-name"
        );
        assert_eq!(branch_summary_label("no-ticket-here"), "no-ticket-here");
        assert_eq!(branch_summary_label("feature/RAL-9"), "feature/RAL-9");
    }

    struct CapturingRunner {
        last_prompt: Mutex<Option<String>>,
    }

    impl CapturingRunner {
        fn new() -> Self {
            Self {
                last_prompt: Mutex::new(None),
            }
        }
    }

    impl Runner for CapturingRunner {
        fn run(&self, spec: &RunnerSpec) -> RunnerResult {
            *self.last_prompt.lock().unwrap() = spec.prompt.clone();
            RunnerResult {
                status: "done".to_string(),
                tokens_in: 0,
                tokens_out: 0,
                cost_usd: 0.0,
                summary: "captured".to_string(),
                error: None,
                verified: None,
                agent_session_id: None,
                ghost: None,
            }
        }
    }

    #[test]
    fn generate_summary_default_prompt_requests_bullet_list() {
        let (base, repo, _fwt) = make_repo("gensum-bullet");
        let base_sha = git(&repo, &["rev-parse", "main"])
            .unwrap()
            .trim()
            .to_string();
        let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
        let runner = CapturingRunner::new();

        generate_summary(
            &store,
            &runner,
            "guardian-x",
            &Workspace::local(&repo),
            &base_sha,
            &[(0, "feature/a".to_string())],
            "feature/a",
            false,
        );

        let prompt = runner
            .last_prompt
            .lock()
            .unwrap()
            .clone()
            .expect("prompt captured");
        assert!(prompt.contains("bullet list"), "prompt: {prompt}");
        assert!(prompt.contains("feature/a"), "prompt: {prompt}");

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn generate_summary_prose_config_disables_bullet_list() {
        let (base, repo, _fwt) = make_repo("gensum-prose");
        std::fs::write(
            repo.join(".ralphus.toml"),
            "[review]\nsummary_format = \"prose\"\n",
        )
        .unwrap();
        let base_sha = git(&repo, &["rev-parse", "main"])
            .unwrap()
            .trim()
            .to_string();
        let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
        let runner = CapturingRunner::new();

        generate_summary(
            &store,
            &runner,
            "guardian-x",
            &Workspace::local(&repo),
            &base_sha,
            &[(0, "feature/a".to_string())],
            "feature/a",
            false,
        );

        let prompt = runner
            .last_prompt
            .lock()
            .unwrap()
            .clone()
            .expect("prompt captured");
        assert!(!prompt.contains("bullet list"), "prompt: {prompt}");
        assert!(prompt.contains("2-3 sentence"), "prompt: {prompt}");

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn generate_summary_bullet_prompt_uses_ticket_label_not_raw_branch_name() {
        let (base, repo, _fwt) = make_repo("gensum-label");
        let base_sha = git(&repo, &["rev-parse", "main"])
            .unwrap()
            .trim()
            .to_string();
        let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
        let runner = CapturingRunner::new();

        // The actual git ref (`tip_ref`) stays `feature/a` -- only the
        // `completed` label fed to the prompt is ticket-shaped, so this
        // isolates label substitution from git-log resolution.
        generate_summary(
            &store,
            &runner,
            "guardian-x",
            &Workspace::local(&repo),
            &base_sha,
            &[(0, "RAL-124-bullet_change_summary".to_string())],
            "feature/a",
            false,
        );

        let prompt = runner
            .last_prompt
            .lock()
            .unwrap()
            .clone()
            .expect("prompt captured");
        assert!(prompt.contains("RAL-124"), "prompt: {prompt}");
        assert!(
            !prompt.contains("RAL-124-bullet_change_summary"),
            "prompt should use the extracted ticket label, not the full branch name: {prompt}"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    // -----------------------------------------------------------------------
    // RAL-148 -- `conflicted_files`/`rebase_in_progress` live-conflicts read
    // -----------------------------------------------------------------------

    #[test]
    fn conflicted_files_and_rebase_in_progress_reflect_a_real_conflicting_rebase() {
        let base = tmp_dir("live-conflicts");
        let repo = base.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        g(&repo, &["init", "-b", "main"]);
        std::fs::write(repo.join("shared.txt"), "base\n").unwrap();
        g(&repo, &["add", "."]);
        g(&repo, &["commit", "-m", "base"]);

        // Feature branch changes shared.txt one way...
        g(&repo, &["checkout", "-b", "feature/a"]);
        std::fs::write(repo.join("shared.txt"), "feature\n").unwrap();
        g(&repo, &["add", "."]);
        g(&repo, &["commit", "-m", "feature change"]);

        // ...while main changes it another way, so rebasing feature/a onto
        // main conflicts on shared.txt.
        g(&repo, &["checkout", "main"]);
        std::fs::write(repo.join("shared.txt"), "main\n").unwrap();
        g(&repo, &["add", "."]);
        g(&repo, &["commit", "-m", "main change"]);

        g(&repo, &["checkout", "feature/a"]);
        let status = std::process::Command::new("git")
            .args(["rebase", "main"])
            .current_dir(&repo)
            .env("GIT_EDITOR", "true")
            .status()
            .expect("git rebase");
        assert!(
            !status.success(),
            "rebase should fail with a conflict for this test to be meaningful"
        );

        assert!(
            rebase_in_progress(&Workspace::local(&repo)),
            "rebase should be mid-flight"
        );
        assert_eq!(
            conflicted_files(&Workspace::local(&repo)),
            vec!["shared.txt".to_string()]
        );

        // Resolve and continue -- both should reflect the cleared conflict.
        std::fs::write(repo.join("shared.txt"), "resolved\n").unwrap();
        g(&repo, &["add", "."]);
        let status = std::process::Command::new("git")
            .args(["rebase", "--continue"])
            .current_dir(&repo)
            .env("GIT_EDITOR", "true")
            .env("GIT_SEQUENCE_EDITOR", "true")
            .status()
            .expect("git rebase --continue");
        assert!(status.success());
        assert!(conflicted_files(&Workspace::local(&repo)).is_empty());
        assert!(!rebase_in_progress(&Workspace::local(&repo)));

        let _ = std::fs::remove_dir_all(&base);
    }

    // -----------------------------------------------------------------------
    // Remote build machines (RAL-175)
    // -----------------------------------------------------------------------

    /// A guardian with an explicit `checks` command, in a repo whose
    /// `.ralphus.toml` enables remote build against a fake endpoint --
    /// `final_checks` only takes the remote path when it resolves a
    #[test]
    fn final_checks_runs_local_checks_and_surfaces_a_failure() {
        let (base, repo, _fwt) = make_repo("finalchecks-remote-off");
        let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
        let id = {
            let guard = store.lock().unwrap();
            let id = guard
                .create_guardian("r", "main", &repo.to_string_lossy())
                .unwrap();
            guard
                .set_guardian_checks(&id, &["exit 1".to_string()])
                .unwrap();
            id
        };

        let result = final_checks(
            &store,
            &id,
            &Workspace::local(&repo),
            &repo.to_string_lossy(),
        );
        // The configured `exit 1` check gate fails the review.
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("check failed"));

        let _ = std::fs::remove_dir_all(&base);
    }
}
